//! State every session shares, and what plugins ask of it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mistvale_plugins::{Action, Dispatcher};
use tokio::sync::mpsc;

use crate::players::Players;
use crate::world::FlatWorld;

/// Plugin actions that may wait before plugins are told the server is busy.
pub const PLUGIN_ACTION_QUEUE: usize = 1024;

/// The world, the players in it and the plugins watching it.
pub struct Server {
    pub world: Arc<FlatWorld>,
    pub players: Players,
    pub plugins: Dispatcher,
    /// The last tick the game loop ran; 0 before the first.
    tick: AtomicU64,
}

impl Server {
    pub fn new(world: FlatWorld, plugins: Dispatcher) -> Self {
        Self {
            world: Arc::new(world),
            players: Players::new(),
            plugins,
            tick: AtomicU64::new(0),
        }
    }

    pub fn current_tick(&self) -> u64 {
        self.tick.load(Ordering::Relaxed)
    }

    /// Advances the world by one tick; called by the game loop.
    pub fn tick(&self, tick: u64) {
        self.tick.store(tick, Ordering::Relaxed);
        self.players.broadcast_movement(tick);
    }

    /// Carries out one plugin action.
    pub fn apply(&self, action: Action) {
        match action {
            Action::Broadcast(message) => self.players.broadcast_message(&message),
        }
    }
}

/// Carries out plugin actions as they arrive, until the plugins stop.
pub async fn apply_plugin_actions(server: Arc<Server>, mut actions: mpsc::Receiver<Action>) {
    while let Some(action) = actions.recv().await {
        server.apply(action);
    }
}
