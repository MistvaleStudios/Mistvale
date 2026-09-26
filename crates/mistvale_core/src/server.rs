//! State every session shares, and what plugins ask of it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use mistvale_plugins::{Action, Dispatcher};
use mistvale_protocol::packet::Encode as _;
use mistvale_protocol::packets::UpdateBlock;
use mistvale_protocol::types::{BlockPos, ChunkPos};
use tokio::sync::mpsc;

use crate::players::Players;
use crate::world::World;

/// Plugin actions that may wait before plugins are told the server is busy.
pub const PLUGIN_ACTION_QUEUE: usize = 1024;

/// The world, the players in it and the plugins watching it.
pub struct Server {
    pub world: Arc<World>,
    pub players: Players,
    pub plugins: Dispatcher,
    /// The last tick the game loop ran; 0 before the first.
    tick: AtomicU64,
}

impl Server {
    pub fn new(world: World, plugins: Dispatcher) -> Self {
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
        self.players.tick(tick);
    }

    /// Replaces the block at `pos` with air and shows the change to every
    /// player whose client has that chunk. Breaking air does nothing.
    pub fn break_block(&self, pos: BlockPos) {
        if !self.world.set_block(pos, self.world.air()) {
            return;
        }
        let update = UpdateBlock {
            position: pos,
            block: self.world.air(),
            flags: UpdateBlock::NETWORK,
            layer: 0,
        };
        self.players
            .send_to_viewers(ChunkPos::of_block(pos), &Bytes::from(update.encode()));
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

#[cfg(test)]
mod tests {
    use mistvale_protocol::packet::{self, id};
    use uuid::Uuid;

    use super::*;
    use crate::players::{EYE_HEIGHT, Joining, Movement, Profile, View};
    use crate::world::World;

    fn join_at<'a>(
        server: &'a Server,
        name: &str,
        chunk: ChunkPos,
    ) -> (crate::players::Membership<'a>, mpsc::Receiver<Bytes>) {
        let (outbound, queue) = mpsc::channel(16);
        let membership = server.players.join(Joining {
            entity_id: server.players.allocate_entity_id(),
            profile: Profile {
                name: name.into(),
                uuid: Uuid::new_v4(),
            },
            movement: Movement {
                position: mistvale_protocol::types::Vec3 {
                    x: chunk.x as f32 * 16.0 + 8.0,
                    y: -60.0 + EYE_HEIGHT,
                    z: chunk.z as f32 * 16.0 + 8.0,
                },
                pitch: 0.0,
                yaw: 0.0,
                head_yaw: 0.0,
                on_ground: true,
            },
            view: View {
                centre: chunk,
                radius: 4,
            },
            outbound,
        });
        (membership, queue)
    }

    #[test]
    fn broken_blocks_reach_players_who_have_the_chunk() {
        let server = Server::new(World::new(), Dispatcher::disconnected());
        let (_near, mut near) = join_at(&server, "Near", ChunkPos::new(0, 0));
        let (_far, mut far) = join_at(&server, "Far", ChunkPos::new(100, 0));
        while near.try_recv().is_ok() {}
        while far.try_recv().is_ok() {}

        let grass = BlockPos { x: 9, y: -61, z: 8 };
        server.break_block(grass);
        assert_eq!(server.world.block(grass), server.world.air());
        let update = near.try_recv().expect("the nearby player sees the change");
        let (header, _) = packet::read_header(&update).unwrap();
        assert_eq!(header.id, id::UPDATE_BLOCK);
        assert!(far.try_recv().is_err(), "the chunk is not loaded out there");

        // Breaking air again changes nothing, so nothing is sent.
        server.break_block(grass);
        assert!(near.try_recv().is_err());
    }
}
