//! Polyglot plugin engine for Mistvale BDS.
//!
//! Loads plugins as plain source files from the `plugins/` directory and reloads
//! them when they change, with no build step. Each scripting engine runs on its
//! own thread with one isolated VM per plugin and talks to the game loop only
//! through messages.
//!
//! Engines are selected with cargo features:
//! - `luau` (default): Luau via `mlua`
//! - `js`: JavaScript/TypeScript via `deno_core` (TypeScript is transpiled on load)
//! - `python`: Python via RustPython
//!
//! Only the Luau engine exists so far; [`PluginHost`] runs `*.luau` plugins.

#[cfg(feature = "luau")]
mod host;
#[cfg(feature = "luau")]
mod luau;
mod output;

#[cfg(feature = "luau")]
pub use host::{PluginConfig, PluginError, PluginHost};
pub use output::{Output, tracing_output};
