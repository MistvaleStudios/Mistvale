//! The world players spawn into: a superflat overworld whose blocks can change.
//!
//! Every column starts as the same generated superflat column, shared as one
//! encoded payload. Changing a block gives that column its own storage, which
//! is kept, and re-encoded whenever it is sent after another change.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use mistvale_protocol::block::{BlockState, StateValue};
use mistvale_protocol::chunk::{self, PalettedStorage, SubChunk};
use mistvale_protocol::packets::LevelChunk;
use mistvale_protocol::types::{BlockPos, ChunkPos};

/// Overworld dimension ID.
pub const OVERWORLD: i32 = 0;
/// Lowest block of the overworld.
pub const MIN_Y: i32 = -64;
/// Highest block of the overworld.
pub const MAX_Y: i32 = 319;
/// Sub-chunks in the overworld's y = -64..=319.
const SUB_CHUNKS: usize = 24;
/// Sub-chunk index of the lowest sub-chunk (y = -64 is sub-chunk -4).
const LOWEST_SUB_CHUNK: i8 = (MIN_Y >> 4) as i8;
/// Biome ID of plains.
const PLAINS: u32 = 1;

/// A chunk column's blocks: one storage per sub-chunk from the bottom up, or
/// `None` for sub-chunks that are all air.
#[derive(Debug, Clone)]
struct Column {
    sub_chunks: Vec<Option<PalettedStorage>>,
    /// The encoded LevelChunk payload, until the next change.
    payload: Option<Vec<u8>>,
}

/// An endless superflat overworld with vanilla's default layers (bedrock, two
/// layers of dirt and grass at y = -64..=-61, all plains) that players can change.
#[derive(Debug)]
pub struct World {
    air: u32,
    /// The generated column, which every unchanged chunk shares.
    generated: Column,
    generated_payload: Vec<u8>,
    /// Columns with changes, by chunk position.
    changed: Mutex<HashMap<ChunkPos, Column>>,
    /// Height of the top (grass) layer.
    surface_y: i32,
}

impl World {
    pub fn new() -> Self {
        let air = BlockState::new("minecraft:air").network_id();
        let dirt = BlockState::new("minecraft:dirt");
        let layers = [
            BlockState::new("minecraft:bedrock").with("infiniburn_bit", StateValue::Byte(0)),
            dirt.clone(),
            dirt,
            BlockState::new("minecraft:grass_block"),
        ];

        let mut blocks = PalettedStorage::filled(air);
        for (y, block) in (0u8..).zip(&layers) {
            let network_id = block.network_id();
            for x in 0..16 {
                for z in 0..16 {
                    blocks.set(x, y, z, network_id);
                }
            }
        }
        let mut generated = Column {
            sub_chunks: vec![None; SUB_CHUNKS],
            payload: None,
        };
        generated.sub_chunks[0] = Some(blocks);
        let generated_payload = generated.encode(air);

        Self {
            air,
            generated,
            generated_payload,
            changed: Mutex::new(HashMap::new()),
            surface_y: MIN_Y + layers.len() as i32 - 1,
        }
    }

    /// Where players spawn: standing on the grass in the middle of chunk (0, 0).
    pub fn spawn(&self) -> BlockPos {
        BlockPos {
            x: 8,
            y: self.surface_y + 1,
            z: 8,
        }
    }

    /// The network ID of air.
    pub fn air(&self) -> u32 {
        self.air
    }

    /// The network ID of the block at `pos`; air outside the world's height.
    pub fn block(&self, pos: BlockPos) -> u32 {
        let Some((sub_chunk, x, y, z)) = locate(pos) else {
            return self.air;
        };
        let changed = self.changed();
        let column = changed
            .get(&ChunkPos::of_block(pos))
            .unwrap_or(&self.generated);
        column.sub_chunks[sub_chunk]
            .as_ref()
            .map_or(self.air, |storage| storage.get(x, y, z))
    }

    /// Sets the block at `pos` to the block with network ID `block`. Returns
    /// whether anything changed; positions outside the world's height never do.
    pub fn set_block(&self, pos: BlockPos, block: u32) -> bool {
        self.update_block(pos, |_| Some(block)).is_some()
    }

    /// Sets the block at `pos` to `block`, returning the block it replaced if
    /// anything changed.
    pub fn replace_block(&self, pos: BlockPos, block: u32) -> Option<u32> {
        self.update_block(pos, |_| Some(block))
    }

    /// Puts `block` at `pos` if it is air there now. Returns whether it did.
    pub fn place_block(&self, pos: BlockPos, block: u32) -> bool {
        let air = self.air;
        self.update_block(pos, |current| (current == air).then_some(block))
            .is_some()
    }

    /// Changes the block at `pos` to what `change` makes of the current one,
    /// under one lock. Returns the previous block if anything changed.
    fn update_block(&self, pos: BlockPos, change: impl FnOnce(u32) -> Option<u32>) -> Option<u32> {
        let (sub_chunk, x, y, z) = locate(pos)?;
        let mut changed = self.changed();
        let chunk = ChunkPos::of_block(pos);
        let current = changed.get(&chunk).unwrap_or(&self.generated).sub_chunks[sub_chunk]
            .as_ref()
            .map_or(self.air, |storage| storage.get(x, y, z));
        let block = change(current).filter(|block| *block != current)?;

        let column = changed
            .entry(chunk)
            .or_insert_with(|| self.generated.clone());
        column.sub_chunks[sub_chunk]
            .get_or_insert_with(|| PalettedStorage::filled(self.air))
            .set(x, y, z, block);
        column.payload = None;
        Some(current)
    }

    /// The chunk column at chunk coordinates (`x`, `z`), with any changes.
    pub fn chunk(&self, x: i32, z: i32) -> LevelChunk {
        let mut changed = self.changed();
        let (sub_chunk_count, payload) = match changed.get_mut(&ChunkPos::new(x, z)) {
            Some(column) => {
                if column.payload.is_none() {
                    column.payload = Some(column.encode(self.air));
                }
                let payload = column.payload.clone().expect("just encoded");
                (column.sent_sub_chunks(), payload)
            }
            None => (
                self.generated.sent_sub_chunks(),
                self.generated_payload.clone(),
            ),
        };
        LevelChunk {
            x,
            z,
            dimension: OVERWORLD,
            sub_chunk_count,
            payload,
        }
    }

    fn changed(&self) -> MutexGuard<'_, HashMap<ChunkPos, Column>> {
        // Columns stay consistent even if a holder panicked.
        self.changed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl Column {
    /// Sub-chunks sent: up to the highest one holding anything but air.
    fn sent_sub_chunks(&self) -> u32 {
        let highest = self
            .sub_chunks
            .iter()
            .rposition(Option::is_some)
            .map_or(0, |index| index + 1);
        u32::try_from(highest).expect("24 sub-chunks")
    }

    /// The LevelChunk payload: sub-chunks from the bottom up to the highest
    /// non-air one (air in between), then plains biomes for the full height.
    fn encode(&self, air: u32) -> Vec<u8> {
        let count = self.sent_sub_chunks() as usize;
        let sub_chunks: Vec<SubChunk> = self.sub_chunks[..count]
            .iter()
            .map(|storage| SubChunk {
                layers: vec![
                    storage
                        .clone()
                        .unwrap_or_else(|| PalettedStorage::filled(air)),
                ],
            })
            .collect();
        let biomes = vec![PalettedStorage::filled(PLAINS); SUB_CHUNKS];
        chunk::level_chunk_payload(LOWEST_SUB_CHUNK, &sub_chunks, &biomes)
    }
}

/// The sub-chunk index (from the bottom) and position within it of `pos`, if
/// `pos` is within the world's height.
fn locate(pos: BlockPos) -> Option<(usize, u8, u8, u8)> {
    if !(MIN_Y..=MAX_Y).contains(&pos.y) {
        return None;
    }
    let sub_chunk = usize::try_from((pos.y - MIN_Y) >> 4).ok()?;
    Some((
        sub_chunk,
        (pos.x & 15) as u8,
        (pos.y & 15) as u8,
        (pos.z & 15) as u8,
    ))
}

#[cfg(test)]
mod tests {
    use mistvale_protocol::chunk::SUB_CHUNK_VERSION;

    use super::*;

    #[test]
    fn players_stand_on_the_grass() {
        assert_eq!(World::new().spawn(), BlockPos { x: 8, y: -60, z: 8 });
    }

    #[test]
    fn chunks_hold_one_sub_chunk_and_a_biome_per_sub_chunk() {
        let world = World::new();
        let chunk = world.chunk(3, -2);
        assert_eq!((chunk.x, chunk.z, chunk.sub_chunk_count), (3, -2, 1));

        let payload = &chunk.payload;
        // Sub-chunk -4 with one layer of 2-bit indices: four palette entries.
        assert_eq!(payload[..4], [SUB_CHUNK_VERSION, 1, 0xFC, (2 << 1) | 1]);
        // Then 256 words of indices, the palette (size and 4 hashed IDs), 24
        // biome storages (plains, then 23 repeats) and no border blocks.
        let palette_start = 4 + 256 * 4;
        assert_eq!(
            payload[palette_start], 8,
            "four palette entries, zigzag encoded"
        );
        assert!(payload.ends_with(&[&[0x01, 0x02][..], &[0xFF; 23], &[0]].concat()));
    }

    #[test]
    fn breaking_a_block_changes_only_its_chunk() {
        let world = World::new();
        let grass = BlockState::new("minecraft:grass_block").network_id();
        let pos = BlockPos {
            x: -3,
            y: -61,
            z: 20,
        };
        assert_eq!(world.block(pos), grass);
        let untouched = world.chunk(-1, 1).payload;

        assert!(world.set_block(pos, world.air()));
        assert!(!world.set_block(pos, world.air()), "already air");
        assert_eq!(world.block(pos), world.air());
        // Chunk (-1, 1) holds (-3, 20); its neighbours are still generated.
        assert_ne!(world.chunk(-1, 1).payload, untouched);
        assert_eq!(world.chunk(0, 1).payload, untouched);
        assert_eq!(world.block(BlockPos { x: -2, ..pos }), grass);
    }

    #[test]
    fn replacing_reports_the_old_block_and_placing_needs_air() {
        let world = World::new();
        let grass = BlockState::new("minecraft:grass_block").network_id();
        let stone = BlockState::new("minecraft:stone").network_id();
        let ground = BlockPos { x: 1, y: -61, z: 1 };
        let above = BlockPos { y: -60, ..ground };

        assert!(!world.place_block(ground, stone), "grass is in the way");
        assert!(world.place_block(above, stone));
        assert!(!world.place_block(above, stone), "now stone is");
        assert_eq!(world.replace_block(ground, world.air()), Some(grass));
        assert_eq!(world.replace_block(ground, world.air()), None);
    }

    #[test]
    fn building_high_sends_more_sub_chunks_and_the_height_is_bounded() {
        let world = World::new();
        let stone = BlockState::new("minecraft:stone").network_id();
        assert!(world.set_block(BlockPos { x: 0, y: 40, z: 0 }, stone));
        // y = 40 is in sub-chunk 2 (32..48), the 7th from the bottom.
        let chunk = world.chunk(0, 0);
        assert_eq!(chunk.sub_chunk_count, 7);
        assert_eq!(world.block(BlockPos { x: 0, y: 40, z: 0 }), stone);

        assert!(!world.set_block(BlockPos { x: 0, y: 320, z: 0 }, stone));
        assert!(!world.set_block(BlockPos { x: 0, y: -65, z: 0 }, stone));
        assert_eq!(world.block(BlockPos { x: 0, y: 400, z: 0 }), world.air());
    }
}
