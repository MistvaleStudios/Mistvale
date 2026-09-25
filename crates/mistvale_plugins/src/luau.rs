//! Luau plugins, each running in its own sandboxed VM.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mlua::{Function, Lua, MultiValue, Value, VmState};
use tracing::Level;

use crate::Output;

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
    plugins: BTreeMap<String, Plugin>,
}

struct Plugin {
    lua: Lua,
    /// When the running call must stop; checked by the VM's interrupt callback.
    deadline: Rc<Cell<Option<Instant>>>,
}

impl LuauEngine {
    pub fn new(limits: Limits, output: Output) -> Self {
        Self {
            limits,
            output,
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

    fn create_plugin(&self, name: &str) -> mlua::Result<Plugin> {
        let lua = Lua::new();
        lua.set_memory_limit(self.limits.memory)?;
        // Globals become read-only once sandboxed, so install ours first.
        install_output(&lua, name, &self.output)?;
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

    fn engine(limits: Limits) -> (LuauEngine, Lines) {
        let lines = Lines::default();
        let sink = Arc::clone(&lines);
        let output: Output = Arc::new(move |plugin, level, message| {
            sink.lock()
                .unwrap()
                .push((plugin.to_owned(), level, message.to_owned()));
        });
        (LuauEngine::new(limits, output), lines)
    }

    fn default_engine() -> (LuauEngine, Lines) {
        engine(Limits {
            memory: 16 * 1024 * 1024,
            execution: Duration::from_millis(250),
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
}
