//! State every session shares, and what plugins ask of it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use mistvale_plugins::{Action, Dispatcher};
use mistvale_protocol::packet::Encode as _;
use mistvale_protocol::packets::{LevelEvent, LevelSoundEvent, UpdateBlock};
use mistvale_protocol::types::{BlockPos, ChunkPos, Vec3};
use tokio::sync::mpsc;

use crate::players::Players;
use crate::world::World;

/// Ticks between saves of changed chunks: every 5 seconds.
const SAVE_INTERVAL: u64 = 100;

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
        if tick.is_multiple_of(SAVE_INTERVAL) {
            self.save();
        }
    }

    /// Writes the chunks changed since the last save to disk, logging how it
    /// went. Chunks that fail to save are tried again next time.
    pub fn save(&self) {
        match self.world.save() {
            Ok(0) => {}
            Ok(saved) => tracing::debug!(saved, "saved changed chunks"),
            Err(err) => tracing::error!(%err, "failed to save the world"),
        }
    }

    /// Replaces the block at `pos` with air. Every player whose client has
    /// that chunk sees the change and the breaking particles, and hears it.
    /// Breaking air does nothing.
    pub fn break_block(&self, pos: BlockPos) {
        let Some(broken) = self.world.replace_block(pos, self.world.air()) else {
            return;
        };
        let chunk = ChunkPos::of_block(pos);
        self.players
            .send_to_viewers(chunk, &block_update(pos, self.world.air()));
        let effect = LevelEvent {
            event: LevelEvent::DESTROY_BLOCK,
            position: centre(pos),
            data: broken as i32,
        };
        self.players
            .send_to_viewers(chunk, &Bytes::from(effect.encode()));
    }

    /// Puts `block` at `pos` if it is air there and no player's body is in
    /// the way. Every player whose client has that chunk sees it and hears it
    /// placed. Returns whether it was placed; the caller undoes a refused
    /// placement the client already predicted.
    pub fn place_block(&self, pos: BlockPos, block: u32) -> bool {
        // A block inside a player traps them, and their client fights it.
        if self.players.occupies(pos) || !self.world.place_block(pos, block) {
            return false;
        }
        let chunk = ChunkPos::of_block(pos);
        self.players
            .send_to_viewers(chunk, &block_update(pos, block));
        let sound = LevelSoundEvent {
            sound: LevelSoundEvent::PLACE.into(),
            position: centre(pos),
            data: block as i32,
        };
        self.players
            .send_to_viewers(chunk, &Bytes::from(sound.encode()));
        true
    }

    /// Carries out one plugin action.
    pub fn apply(&self, action: Action) {
        match action {
            Action::Broadcast(message) => self.players.broadcast_message(&message),
        }
    }
}

/// An encoded UpdateBlock setting `pos` to `block`.
pub fn block_update(pos: BlockPos, block: u32) -> Bytes {
    let update = UpdateBlock {
        position: pos,
        block,
        flags: UpdateBlock::NETWORK,
        layer: 0,
    };
    Bytes::from(update.encode())
}

/// The middle of a block, where its particles and sounds come from.
fn centre(pos: BlockPos) -> Vec3 {
    Vec3 {
        x: pos.x as f32 + 0.5,
        y: pos.y as f32 + 0.5,
        z: pos.z as f32 + 0.5,
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
        // The change, then the breaking particles and sound.
        assert_eq!(ids(&mut near), [id::UPDATE_BLOCK, id::LEVEL_EVENT]);
        assert!(
            ids(&mut far).is_empty(),
            "the chunk is not loaded out there"
        );

        // Breaking air again changes nothing, so nothing is sent.
        server.break_block(grass);
        assert!(ids(&mut near).is_empty());
    }

    #[test]
    fn placed_blocks_need_air_and_are_heard() {
        let server = Server::new(World::new(), Dispatcher::disconnected());
        let (_near, mut near) = join_at(&server, "Near", ChunkPos::new(0, 0));
        ids(&mut near);
        let stone = mistvale_protocol::block::BlockState::new("minecraft:stone").network_id();

        let above_grass = BlockPos { x: 9, y: -60, z: 8 };
        assert!(server.place_block(above_grass, stone));
        assert_eq!(server.world.block(above_grass), stone);
        assert_eq!(ids(&mut near), [id::UPDATE_BLOCK, id::LEVEL_SOUND_EVENT]);

        assert!(!server.place_block(above_grass, stone), "occupied");
        assert!(ids(&mut near).is_empty());
    }

    #[test]
    fn blocks_are_never_placed_inside_any_player() {
        let server = Server::new(World::new(), Dispatcher::disconnected());
        // Another player stands in the middle of chunk (0, 0), feet at (8, -60, 8).
        let (_other, mut other) = join_at(&server, "Other", ChunkPos::new(0, 0));
        ids(&mut other);
        let stone = mistvale_protocol::block::BlockState::new("minecraft:stone").network_id();

        // Their feet and their head are both off limits.
        for occupied in [
            BlockPos { x: 8, y: -60, z: 8 },
            BlockPos { x: 8, y: -59, z: 8 },
        ] {
            assert!(!server.place_block(occupied, stone), "{occupied:?}");
            assert_eq!(server.world.block(occupied), server.world.air());
        }
        assert!(ids(&mut other).is_empty(), "nothing changed");

        // Beside them and above their head is fine.
        assert!(server.place_block(BlockPos { x: 9, y: -60, z: 8 }, stone));
        assert!(server.place_block(BlockPos { x: 8, y: -58, z: 8 }, stone));
    }

    fn ids(queue: &mut mpsc::Receiver<Bytes>) -> Vec<u32> {
        std::iter::from_fn(|| queue.try_recv().ok())
            .map(|packet| packet::read_header(&packet).unwrap().0.id)
            .collect()
    }
}
