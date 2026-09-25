//! Mistvale BDS game server core.
//!
//! Owns the fixed-rate game loop, which runs on a dedicated OS thread while tokio
//! handles networking, plus world state (chunks, blocks) and entity management.
//! It wires together `mistvale_net` (transport), `mistvale_protocol` (packets)
//! and `mistvale_plugins` (scripting).

use std::time::Duration;

pub mod session;
pub mod world;

/// Simulation rate of the game loop.
pub const TICKS_PER_SECOND: u32 = 20;

/// Wall-clock budget of a single tick (50 ms at 20 TPS).
pub const TICK_DURATION: Duration = Duration::from_millis(1000 / TICKS_PER_SECOND as u64);
