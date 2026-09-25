//! What plugins and the server say to each other, whatever the engine.
//!
//! The server sends [`Event`]s to plugins, and plugins answer with
//! [`Action`]s for the server to carry out. Both cross threads as messages.

/// A player as plugins see them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Player {
    /// The name shown in game. Players can change it, so key stored data by `uuid`.
    pub name: String,
    /// The player's persistent identity, a UUID string that stays the same
    /// across sessions and name changes.
    pub uuid: String,
}

/// Something that happened in the game, delivered to the plugins listening for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A player finished spawning and is in the world.
    PlayerJoin(Player),
}

impl Event {
    /// Names plugins can listen for.
    pub const NAMES: [&str; 1] = ["player_join"];

    /// The name plugins listen for this event by, e.g. `server.on("player_join", …)`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::PlayerJoin(_) => "player_join",
        }
    }
}

/// Something a plugin asks the server to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Show a message in every player's chat.
    Broadcast(String),
}
