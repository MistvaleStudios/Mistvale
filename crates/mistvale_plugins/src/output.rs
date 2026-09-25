//! Where plugin output (`print` and `log.*`) goes.

use std::sync::Arc;

use tracing::Level;

/// Receives each line a plugin prints or logs: `(plugin, level, message)`.
pub type Output = Arc<dyn Fn(&str, Level, &str) + Send + Sync>;

/// Sends plugin output to `tracing` under the `plugin` target.
pub fn tracing_output() -> Output {
    Arc::new(|plugin, level, message| match level {
        Level::ERROR => tracing::error!(target: "plugin", plugin, "{message}"),
        Level::WARN => tracing::warn!(target: "plugin", plugin, "{message}"),
        Level::INFO => tracing::info!(target: "plugin", plugin, "{message}"),
        Level::DEBUG => tracing::debug!(target: "plugin", plugin, "{message}"),
        _ => tracing::trace!(target: "plugin", plugin, "{message}"),
    })
}
