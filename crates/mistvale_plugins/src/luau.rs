//! Luau plugins, each running in its own sandboxed VM.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mlua::{Function, Lua, MultiValue, Table, Value, VmState};
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::Level;

use crate::{Action, Event, Output};

/// Registry key of each VM's table of event handlers: event name → list of functions.
const HANDLERS: &str = "mistvale.handlers";

/// Resource limits applied to every plugin VM.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub memory: usize,
    /// Longest a single call into a plugin may run.
    pub execution: Duration,
}

/// Runs Luau plugins by name. Lives on one thread, since Luau VMs are not `Send`.
pub(crate) struct LuauEngine {
    limits: Limits,
    output: Output,
    actions: mpsc::Sender<Action>,
    plugins: BTreeMap<String, Plugin>,
}

struct Plugin {
    lua: Lua,
    /// When the running call must stop; checked by the VM's interrupt callback.
    deadline: Rc<Cell<Option<Instant>>>,
}

impl LuauEngine {
    pub fn new(limits: Limits, output: Output, actions: mpsc::Sender<Action>) -> Self {
        Self {
            limits,
            output,
            actions,
            plugins: BTreeMap::new(),
        }
    }

    /// Runs `source` as plugin `name` in a fresh VM. The new VM replaces a running
    /// plugin of the same name only if the script succeeds. Returns whether it did.
    pub fn load(&mut self, name: &str, chunk_name: &str, source: &str) -> mlua::Result<bool> {
        let plugin = self.create_plugin(name)?;
        plugin.run(self.limits.execution, || {
            plugin
                .lua
                .load(source)
                .set_name(format!("@{chunk_name}"))
                .exec()
        })?;
        Ok(self.plugins.insert(name.to_owned(), plugin).is_some())
    }

    /// Stops plugin `name`, dropping its VM. Returns whether it was running.
    pub fn unload(&mut self, name: &str) -> bool {
        self.plugins.remove(name).is_some()
    }

    pub fn names(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }

    /// Calls every handler for `event`, plugin by plugin in name order. A
    /// handler that fails is logged and skipped.
    pub fn dispatch(&self, event: &Event) {
        for (name, plugin) in &self.plugins {
            plugin.dispatch(name, event, self.limits.execution);
        }
    }

    fn create_plugin(&self, name: &str) -> mlua::Result<Plugin> {
        let lua = Lua::new();
        lua.set_memory_limit(self.limits.memory)?;
        // Globals become read-only once sandboxed, so install ours first.
        install_output(&lua, name, &self.output)?;
        install_server(&lua, &self.actions)?;
        lua.sandbox(true)?;

        let deadline = Rc::new(Cell::new(None::<Instant>));
        let expiry = Rc::clone(&deadline);
        lua.set_interrupt(move |_| match expiry.get() {
            Some(deadline) if Instant::now() >= deadline => Err(mlua::Error::runtime(
                "plugin exceeded its execution time limit",
            )),
            _ => Ok(VmState::Continue),
        });
        Ok(Plugin { lua, deadline })
    }
}

impl Plugin {
    fn run<T>(&self, limit: Duration, call: impl FnOnce() -> mlua::Result<T>) -> mlua::Result<T> {
        self.deadline.set(Some(Instant::now() + limit));
        let result = call();
        self.deadline.set(None);
        result
    }

    /// Calls this plugin's handlers for `event` in the order they were
    /// registered, each within the execution limit.
    fn dispatch(&self, plugin: &str, event: &Event, limit: Duration) {
        let handlers = match self.handlers(event.name()) {
            Ok(handlers) => handlers,
            Err(err) => {
                tracing::error!(
                    plugin,
                    event = event.name(),
                    "failed to look up handlers: {err}"
                );
                return;
            }
        };
        if handlers.is_empty() {
            return;
        }
        let payload = match event_payload(&self.lua, event) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::error!(
                    plugin,
                    event = event.name(),
                    "failed to build the event: {err}"
                );
                return;
            }
        };
        for handler in handlers {
            if let Err(err) = self.run(limit, || handler.call::<()>(&payload)) {
                tracing::error!(plugin, event = event.name(), "event handler failed: {err}");
            }
        }
    }

    /// A snapshot of the handlers for `event`, so handlers may register more
    /// while being called.
    fn handlers(&self, event: &str) -> mlua::Result<Vec<Function>> {
        let handlers: Table = self.lua.named_registry_value(HANDLERS)?;
        match handlers.raw_get::<Option<Table>>(event)? {
            Some(list) => list.sequence_values().collect(),
            None => Ok(Vec::new()),
        }
    }
}

/// The value handlers receive: a read-only table describing the event.
fn event_payload(lua: &Lua, event: &Event) -> mlua::Result<Table> {
    let payload = match event {
        Event::PlayerJoin(player) => {
            let table = lua.create_table()?;
            table.raw_set("name", player.name.as_str())?;
            table.raw_set("uuid", player.uuid.as_str())?;
            table
        }
    };
    payload.set_readonly(true);
    Ok(payload)
}

/// Adds the `server` table: `server.on(event, handler)` registers an event
/// handler and `server.broadcast(message)` sends a chat message to everyone.
fn install_server(lua: &Lua, actions: &mpsc::Sender<Action>) -> mlua::Result<()> {
    lua.set_named_registry_value(HANDLERS, lua.create_table()?)?;
    let server = lua.create_table()?;

    let on = lua.create_function(|lua, (event, handler): (String, Function)| {
        if !Event::NAMES.contains(&event.as_str()) {
            return Err(mlua::Error::runtime(format!(
                "unknown event {event:?}; expected one of: {}",
                Event::NAMES.join(", ")
            )));
        }
        let handlers: Table = lua.named_registry_value(HANDLERS)?;
        let list = match handlers.raw_get::<Option<Table>>(event.as_str())? {
            Some(list) => list,
            None => {
                let list = lua.create_table()?;
                handlers.raw_set(event, &list)?;
                list
            }
        };
        list.raw_push(handler)
    })?;
    server.set("on", on)?;

    let actions = actions.clone();
    let broadcast = lua.create_function(move |_, message: String| {
        if message.is_empty() {
            return Err(mlua::Error::runtime("cannot broadcast an empty message"));
        }
        match actions.try_send(Action::Broadcast(message)) {
            // A closed channel means the server is shutting down.
            Ok(()) | Err(TrySendError::Closed(_)) => Ok(()),
            Err(TrySendError::Full(_)) => Err(mlua::Error::runtime(
                "the server is not keeping up with plugin actions",
            )),
        }
    })?;
    server.set("broadcast", broadcast)?;

    lua.globals().set("server", server)
}

/// Replaces `print` and adds a `log` table (`log.trace` … `log.error`) that
/// forward to `output`, formatting arguments like Luau's `print`.
fn install_output(lua: &Lua, plugin: &str, output: &Output) -> mlua::Result<()> {
    let globals = lua.globals();
    globals.set("print", output_function(lua, plugin, output, Level::INFO)?)?;

    let log = lua.create_table()?;
    for (name, level) in [
        ("trace", Level::TRACE),
        ("debug", Level::DEBUG),
        ("info", Level::INFO),
        ("warn", Level::WARN),
        ("error", Level::ERROR),
    ] {
        log.set(name, output_function(lua, plugin, output, level)?)?;
    }
    globals.set("log", log)
}

fn output_function(
    lua: &Lua,
    plugin: &str,
    output: &Output,
    level: Level,
) -> mlua::Result<Function> {
    let plugin = plugin.to_owned();
    let output = Arc::clone(output);
    lua.create_function(move |_, args: MultiValue| {
        let message = args
            .iter()
            .map(Value::to_string)
            .collect::<mlua::Result<Vec<_>>>()?
            .join("\t");
        output(&plugin, level, &message);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    type Lines = Arc<Mutex<Vec<(String, Level, String)>>>;

    const DEFAULT_LIMITS: Limits = Limits {
        memory: 16 * 1024 * 1024,
        execution: Duration::from_millis(250),
    };

    fn engine_with_actions(limits: Limits) -> (LuauEngine, Lines, mpsc::Receiver<Action>) {
        let lines = Lines::default();
        let sink = Arc::clone(&lines);
        let output: Output = Arc::new(move |plugin, level, message| {
            sink.lock()
                .unwrap()
                .push((plugin.to_owned(), level, message.to_owned()));
        });
        let (actions, received) = mpsc::channel(16);
        (LuauEngine::new(limits, output, actions), lines, received)
    }

    fn engine(limits: Limits) -> (LuauEngine, Lines) {
        let (engine, lines, _) = engine_with_actions(limits);
        (engine, lines)
    }

    fn default_engine() -> (LuauEngine, Lines) {
        engine(DEFAULT_LIMITS)
    }

    fn steve_joins() -> Event {
        Event::PlayerJoin(crate::Player {
            name: "Steve".into(),
            uuid: "174319cc-f69f-30d8-a279-6ace57f2011e".into(),
        })
    }

    fn messages(lines: &Lines) -> Vec<String> {
        lines
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, m)| m.clone())
            .collect()
    }

    #[test]
    fn print_formats_arguments_like_luau() {
        let (mut engine, lines) = default_engine();
        engine
            .load(
                "hello",
                "hello.luau",
                r#"
                    print("hi", 42, 1.5, true, nil)
                    print(setmetatable({}, { __tostring = function() return "custom" end }))
                "#,
            )
            .unwrap();
        assert_eq!(
            *lines.lock().unwrap(),
            vec![
                ("hello".into(), Level::INFO, "hi\t42\t1.5\ttrue\tnil".into()),
                ("hello".into(), Level::INFO, "custom".into()),
            ]
        );
    }

    #[test]
    fn log_functions_use_their_levels() {
        let (mut engine, lines) = default_engine();
        engine
            .load(
                "levels",
                "levels.luau",
                r#"log.warn("careful") log.error("broken", 1)"#,
            )
            .unwrap();
        let levels: Vec<_> = lines
            .lock()
            .unwrap()
            .iter()
            .map(|(_, l, m)| (*l, m.clone()))
            .collect();
        assert_eq!(
            levels,
            vec![
                (Level::WARN, "careful".into()),
                (Level::ERROR, "broken\t1".into())
            ]
        );
    }

    #[test]
    fn luau_syntax_and_type_annotations_are_supported() {
        let (mut engine, lines) = default_engine();
        engine
            .load(
                "typed",
                "typed.luau",
                "local count: number = 2\ncount += 1\nprint(`count is {count}`)",
            )
            .unwrap();
        assert_eq!(messages(&lines), vec!["count is 3"]);
    }

    #[test]
    fn sandbox_makes_libraries_read_only() {
        let (mut engine, _) = default_engine();
        let err = engine
            .load("vandal", "vandal.luau", "string.upper = nil")
            .unwrap_err();
        assert!(err.to_string().contains("readonly"), "{err}");
    }

    #[test]
    fn plugins_do_not_share_globals() {
        let (mut engine, lines) = default_engine();
        engine.load("a", "a.luau", r#"shared = "from a""#).unwrap();
        engine.load("b", "b.luau", "print(shared)").unwrap();
        assert_eq!(messages(&lines), vec!["nil"]);
    }

    #[test]
    fn runaway_scripts_are_stopped() {
        let (mut engine, _) = default_engine();
        let started = Instant::now();
        let err = engine
            .load("spin", "spin.luau", "while true do end")
            .unwrap_err();
        assert!(err.to_string().contains("execution time limit"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(engine.names().is_empty());
    }

    #[test]
    fn memory_limit_is_enforced() {
        let (mut engine, _) = engine(Limits {
            memory: 4 * 1024 * 1024,
            execution: Duration::from_secs(5),
        });
        let err = engine
            .load(
                "hog",
                "hog.luau",
                "local t = {} for i = 1, 1e7 do t[i] = string.rep('x', 64) .. i end",
            )
            .unwrap_err();
        assert!(matches!(err, mlua::Error::MemoryError(_)), "{err}");
    }

    #[test]
    fn failed_reload_keeps_the_running_version() {
        let (mut engine, _) = default_engine();
        assert!(!engine.load("hello", "hello.luau", "print('v1')").unwrap());
        assert!(engine.load("hello", "hello.luau", "print(").is_err());
        assert_eq!(engine.names(), vec!["hello"]);
        assert!(engine.load("hello", "hello.luau", "print('v2')").unwrap());
        assert!(engine.unload("hello"));
        assert!(!engine.unload("hello"));
    }

    #[test]
    fn join_handlers_get_the_player_and_can_broadcast() {
        let (mut engine, lines, mut actions) = engine_with_actions(DEFAULT_LIMITS);
        engine
            .load(
                "welcome",
                "welcome.luau",
                r#"
                    server.on("player_join", function(player)
                        print(player.name, player.uuid)
                        server.broadcast(`Welcome, {player.name}!`)
                    end)
                "#,
            )
            .unwrap();
        assert!(
            actions.try_recv().is_err(),
            "nothing happens until someone joins"
        );

        engine.dispatch(&steve_joins());
        assert_eq!(
            messages(&lines),
            ["Steve\t174319cc-f69f-30d8-a279-6ace57f2011e"]
        );
        assert_eq!(
            actions.try_recv().unwrap(),
            Action::Broadcast("Welcome, Steve!".into())
        );
    }

    #[test]
    fn unknown_events_are_rejected() {
        let (mut engine, _) = default_engine();
        let err = engine
            .load(
                "typo",
                "typo.luau",
                r#"server.on("player_joined", function() end)"#,
            )
            .unwrap_err();
        assert!(err.to_string().contains("unknown event"), "{err}");
    }

    #[test]
    fn a_failing_handler_does_not_stop_the_others() {
        let (mut engine, lines) = default_engine();
        engine
            .load(
                "a",
                "a.luau",
                r#"
                    server.on("player_join", function(player) player.name = "Alex" end)
                    server.on("player_join", function(player) print("a saw", player.name) end)
                "#,
            )
            .unwrap();
        engine
            .load(
                "b",
                "b.luau",
                r#"server.on("player_join", function(player) print("b saw", player.name) end)"#,
            )
            .unwrap();
        engine.dispatch(&steve_joins());
        assert_eq!(messages(&lines), ["a saw\tSteve", "b saw\tSteve"]);
    }

    #[test]
    fn runaway_handlers_are_stopped() {
        let (mut engine, lines) = default_engine();
        engine
            .load(
                "spin",
                "spin.luau",
                r#"
                    server.on("player_join", function() while true do end end)
                    server.on("player_join", function() print("still here") end)
                "#,
            )
            .unwrap();
        let started = Instant::now();
        engine.dispatch(&steve_joins());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(messages(&lines), ["still here"]);
    }

    #[test]
    fn reloading_replaces_the_handlers() {
        let (mut engine, lines) = default_engine();
        let source = |version: &str| {
            format!(r#"server.on("player_join", function() print("{version}") end)"#)
        };
        engine.load("hello", "hello.luau", &source("v1")).unwrap();
        engine.load("hello", "hello.luau", &source("v2")).unwrap();
        engine.dispatch(&steve_joins());
        assert_eq!(messages(&lines), ["v2"]);
    }

    #[test]
    fn broadcast_rejects_empty_messages_and_the_api_is_read_only() {
        let (mut engine, _) = default_engine();
        let err = engine
            .load("empty", "empty.luau", r#"server.broadcast("")"#)
            .unwrap_err();
        assert!(err.to_string().contains("empty message"), "{err}");
        let err = engine
            .load("vandal", "vandal.luau", "server.broadcast = nil")
            .unwrap_err();
        assert!(err.to_string().contains("readonly"), "{err}");
    }
}
