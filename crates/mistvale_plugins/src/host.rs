//! The plugin host: an engine thread plus a watcher that reloads changed files.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::luau::{Limits, LuauEngine};
use crate::{Output, tracing_output};

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

enum Command {
    Changed(PathBuf),
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

impl PluginHost {
    /// Loads every plugin in the configured directory, logging their output
    /// through `tracing`, and returns once the initial load has finished.
    pub fn start(config: PluginConfig) -> Result<Self, PluginError> {
        Self::with_output(config, tracing_output())
    }

    /// Like [`PluginHost::start`], sending plugin output to `output`.
    pub fn with_output(config: PluginConfig, output: Output) -> Result<Self, PluginError> {
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
            .spawn(move || run(config, output, receiver, ready))
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
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The engine thread: loads everything once, then applies debounced file changes.
fn run(
    config: PluginConfig,
    output: Output,
    commands: mpsc::Receiver<Command>,
    ready: mpsc::SyncSender<Vec<String>>,
) {
    let limits = Limits {
        memory: config.memory_limit,
        execution: config.execution_limit,
    };
    let mut engine = LuauEngine::new(limits, output);
    for path in plugin_files(&config.directory) {
        load(&mut engine, &path);
    }
    let _ = ready.send(engine.names());

    let mut changed: BTreeSet<PathBuf> = BTreeSet::new();
    loop {
        let command = if changed.is_empty() {
            commands.recv().ok()
        } else {
            match commands.recv_timeout(RELOAD_DEBOUNCE) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => {
                    for path in std::mem::take(&mut changed) {
                        sync(&mut engine, &path);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => None,
            }
        };
        match command {
            // Watcher paths may be absolute; name files relative to the configured directory.
            Some(Command::Changed(path)) => {
                if let Some(file_name) = path.file_name() {
                    changed.insert(config.directory.join(file_name));
                }
            }
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
        notify::recommended_watcher(move |event: notify::Result<Event>| match event {
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
        let host = PluginHost::with_output(config, output).unwrap();
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
}
