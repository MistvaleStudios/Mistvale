//! The plugin host: an engine thread plus a watcher that reloads changed files.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::mpsc as tokio_mpsc;

use crate::luau::{Limits, LuauEngine};
use crate::{Action, Event, Output, tracing_output};

/// Quiet period after the last change to a file before it is reloaded; editors
/// often save in several steps.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);
const LUAU_EXTENSION: &str = "luau";

/// Plugin host settings.
#[derive(Debug, Clone)]
pub struct PluginConfig {
    /// Directory holding `*.luau` plugins; created if missing.
    pub directory: PathBuf,
    /// Reload plugins when their files are saved, added or removed.
    pub hot_reload: bool,
    /// Memory each plugin VM may allocate.
    pub memory_limit: usize,
    /// Longest a single call into a plugin may run before it is aborted.
    pub execution_limit: Duration,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("plugins"),
            hot_reload: true,
            memory_limit: 64 * 1024 * 1024,
            execution_limit: Duration::from_secs(1),
        }
    }
}

/// Errors starting a [`PluginHost`]. Individual plugins that fail to load are
/// logged instead, so one broken script never stops the server.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("failed to create plugin directory {}: {source}", .path.display())]
    Directory { path: PathBuf, source: io::Error },
    #[error("failed to watch {} for changes: {source}", .path.display())]
    Watch {
        path: PathBuf,
        source: notify::Error,
    },
    #[error("failed to start the plugin thread: {0}")]
    Thread(io::Error),
    #[error("the plugin thread stopped during startup")]
    Startup,
}

#[derive(Debug)]
enum Command {
    Changed(PathBuf),
    Event(Event),
    Shutdown,
}

/// Runs the plugins in a directory on a dedicated thread, reloading them as
/// their files change. Dropping the host unloads every plugin.
#[derive(Debug)]
pub struct PluginHost {
    commands: mpsc::Sender<Command>,
    thread: Option<thread::JoinHandle<()>>,
    _watcher: Option<RecommendedWatcher>,
    loaded: Vec<String>,
}

/// Delivers game events to the plugins. Cheap to clone and usable from any
/// thread; events sent after the host has stopped are dropped.
#[derive(Debug, Clone)]
pub struct Dispatcher {
    commands: mpsc::Sender<Command>,
}

impl Dispatcher {
    /// A dispatcher with no plugins behind it, which drops every event; for
    /// running without a plugin host, e.g. in tests.
    pub fn disconnected() -> Self {
        let (commands, _) = mpsc::channel();
        Self { commands }
    }

    /// Queues `event` for every plugin listening for it.
    pub fn dispatch(&self, event: Event) {
        let _ = self.commands.send(Command::Event(event));
    }
}

impl PluginHost {
    /// Loads every plugin in the configured directory, logging their output
    /// through `tracing`, and returns once the initial load has finished.
    /// What plugins ask of the server arrives on `actions`.
    pub fn start(
        config: PluginConfig,
        actions: tokio_mpsc::Sender<Action>,
    ) -> Result<Self, PluginError> {
        Self::with_output(config, tracing_output(), actions)
    }

    /// Like [`PluginHost::start`], sending plugin output to `output`.
    pub fn with_output(
        config: PluginConfig,
        output: Output,
        actions: tokio_mpsc::Sender<Action>,
    ) -> Result<Self, PluginError> {
        fs::create_dir_all(&config.directory).map_err(|source| PluginError::Directory {
            path: config.directory.clone(),
            source,
        })?;

        let (commands, receiver) = mpsc::channel();
        // Start watching before the initial scan so no save in between is missed.
        let watcher = if config.hot_reload {
            Some(watch(&config.directory, commands.clone())?)
        } else {
            None
        };

        let (ready, started) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("luau-plugins".into())
            .spawn(move || run(config, output, actions, receiver, ready))
            .map_err(PluginError::Thread)?;
        let loaded = started.recv().map_err(|_| PluginError::Startup)?;

        Ok(Self {
            commands,
            thread: Some(thread),
            _watcher: watcher,
            loaded,
        })
    }

    /// Names of the plugins that loaded successfully at startup.
    pub fn loaded(&self) -> &[String] {
        &self.loaded
    }

    /// A handle for sending game events to the plugins.
    pub fn dispatcher(&self) -> Dispatcher {
        Dispatcher {
            commands: self.commands.clone(),
        }
    }
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The engine thread: loads everything once, then delivers events and applies
/// debounced file changes.
fn run(
    config: PluginConfig,
    output: Output,
    actions: tokio_mpsc::Sender<Action>,
    commands: mpsc::Receiver<Command>,
    ready: mpsc::SyncSender<Vec<String>>,
) {
    let limits = Limits {
        memory: config.memory_limit,
        execution: config.execution_limit,
    };
    let mut engine = LuauEngine::new(limits, output, actions);
    for path in plugin_files(&config.directory) {
        load(&mut engine, &path);
    }
    let _ = ready.send(engine.names());

    let mut changed: BTreeSet<PathBuf> = BTreeSet::new();
    // When to reload `changed`; each change pushes it back, events do not.
    let mut reload_at: Option<Instant> = None;
    loop {
        let command = match reload_at {
            None => commands.recv().ok(),
            Some(at) => match commands.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => {
                    for path in std::mem::take(&mut changed) {
                        sync(&mut engine, &path);
                    }
                    reload_at = None;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => None,
            },
        };
        match command {
            // Watcher paths may be absolute; name files relative to the configured directory.
            Some(Command::Changed(path)) => {
                if let Some(file_name) = path.file_name() {
                    changed.insert(config.directory.join(file_name));
                    reload_at = Some(Instant::now() + RELOAD_DEBOUNCE);
                }
            }
            Some(Command::Event(event)) => engine.dispatch(&event),
            Some(Command::Shutdown) | None => break,
        }
    }
}

/// Loads, reloads or unloads the plugin at `path` to match the file on disk.
fn sync(engine: &mut LuauEngine, path: &Path) {
    if path.is_file() {
        load(engine, path);
    } else if let Some(name) = plugin_name(path)
        && engine.unload(&name)
    {
        tracing::info!(plugin = %name, "unloaded plugin");
    }
}

fn load(engine: &mut LuauEngine, path: &Path) {
    let Some(name) = plugin_name(path) else {
        return;
    };
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) => {
            tracing::error!(plugin = %name, path = %path.display(), %err, "failed to read plugin");
            return;
        }
    };
    let was_running = engine.names().contains(&name);
    match engine.load(&name, &path.display().to_string(), &source) {
        Ok(true) => tracing::info!(plugin = %name, "reloaded plugin"),
        Ok(false) => tracing::info!(plugin = %name, path = %path.display(), "loaded plugin"),
        Err(err) if was_running => {
            tracing::error!(plugin = %name, "failed to reload plugin, keeping the running version: {err}");
        }
        Err(err) => tracing::error!(plugin = %name, "failed to load plugin: {err}"),
    }
}

/// `*.luau` files directly inside `directory`, in name order.
fn plugin_files(directory: &Path) -> Vec<PathBuf> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::error!(directory = %directory.display(), %err, "failed to list plugins");
            return Vec::new();
        }
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| is_plugin_file(path) && path.is_file())
        .collect();
    files.sort();
    files
}

fn is_plugin_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(LUAU_EXTENSION))
}

fn plugin_name(path: &Path) -> Option<String> {
    path.file_stem()?.to_str().map(str::to_owned)
}

fn watch(
    directory: &Path,
    commands: mpsc::Sender<Command>,
) -> Result<RecommendedWatcher, PluginError> {
    let watch_error = |source| PluginError::Watch {
        path: directory.to_owned(),
        source,
    };
    let mut watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event) => {
                if matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) {
                    for path in event.paths.into_iter().filter(|path| is_plugin_file(path)) {
                        let _ = commands.send(Command::Changed(path));
                    }
                }
            }
            Err(err) => tracing::warn!(%err, "plugin file watcher error"),
        })
        .map_err(watch_error)?;
    watcher
        .watch(directory, RecursiveMode::NonRecursive)
        .map_err(watch_error)?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use super::*;

    #[test]
    fn loads_at_startup_and_reloads_saved_files() {
        let directory =
            std::env::temp_dir().join(format!("mistvale-plugins-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let plugin = directory.join("greet.luau");
        fs::write(&plugin, r#"print("v1")"#).unwrap();
        fs::write(directory.join("notes.txt"), "not a plugin").unwrap();

        let messages = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&messages);
        let output: Output = Arc::new(move |plugin, _, message| {
            sink.lock().unwrap().push(format!("{plugin}: {message}"));
        });
        let config = PluginConfig {
            directory: directory.clone(),
            ..PluginConfig::default()
        };
        let (actions, _) = tokio_mpsc::channel(1);
        let host = PluginHost::with_output(config, output, actions).unwrap();
        assert_eq!(host.loaded(), ["greet"]);
        assert_eq!(*messages.lock().unwrap(), ["greet: v1"]);

        fs::write(&plugin, r#"print("v2")"#).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !messages.lock().unwrap().contains(&"greet: v2".to_owned()) {
            assert!(Instant::now() < deadline, "plugin was not reloaded");
            thread::sleep(Duration::from_millis(20));
        }

        drop(host);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn events_reach_plugins_and_their_actions_reach_the_server() {
        let directory =
            std::env::temp_dir().join(format!("mistvale-events-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("welcome.luau"),
            r#"server.on("player_join", function(player) server.broadcast("hi " .. player.name) end)"#,
        )
        .unwrap();

        let config = PluginConfig {
            directory: directory.clone(),
            hot_reload: false,
            ..PluginConfig::default()
        };
        let (actions, mut received) = tokio_mpsc::channel(4);
        let host = PluginHost::with_output(config, Arc::new(|_, _, _| {}), actions).unwrap();
        host.dispatcher().dispatch(Event::PlayerJoin(crate::Player {
            name: "Steve".into(),
            uuid: "174319cc-f69f-30d8-a279-6ace57f2011e".into(),
        }));

        let deadline = Instant::now() + Duration::from_secs(10);
        let action = loop {
            if let Ok(action) = received.try_recv() {
                break action;
            }
            assert!(Instant::now() < deadline, "the plugin did not broadcast");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(action, Action::Broadcast("hi Steve".into()));

        drop(host);
        fs::remove_dir_all(&directory).unwrap();
    }
}
