//! State every session shares, and what plugins ask of it.

use std::sync::Arc;

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
}

impl Server {
    pub fn new(world: FlatWorld, plugins: Dispatcher) -> Self {
        Self {
            world: Arc::new(world),
            players: Players::new(),
            plugins,
        }
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
