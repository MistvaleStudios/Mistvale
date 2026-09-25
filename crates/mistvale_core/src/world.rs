//! The world players spawn into: for now a superflat overworld.

use mistvale_protocol::block::{BlockState, StateValue};
use mistvale_protocol::chunk::{self, PalettedStorage, SubChunk};
use mistvale_protocol::packets::LevelChunk;
use mistvale_protocol::types::BlockPos;

/// Overworld dimension ID.
pub const OVERWORLD: i32 = 0;
/// Lowest block of the overworld.
pub const MIN_Y: i32 = -64;
/// Sub-chunks in the overworld's y = -64..=319.
const SUB_CHUNKS: usize = 24;
/// Biome ID of plains.
const PLAINS: u32 = 1;

/// An endless superflat overworld with vanilla's default layers: bedrock, two
/// layers of dirt and grass at y = -64..=-61, all in the plains biome.
#[derive(Debug)]
pub struct FlatWorld {
    /// Every chunk is identical, so the encoded LevelChunk payload is built once.
    payload: Vec<u8>,
    /// Height of the top (grass) layer.
    surface_y: i32,
}

impl FlatWorld {
    pub fn new() -> Self {
        let dirt = BlockState::new("minecraft:dirt");
        let layers = [
            BlockState::new("minecraft:bedrock").with("infiniburn_bit", StateValue::Byte(0)),
            dirt.clone(),
            dirt,
            BlockState::new("minecraft:grass_block"),
        ];

        let mut blocks = PalettedStorage::filled(BlockState::new("minecraft:air").network_id());
        for (y, block) in (0u8..).zip(&layers) {
            let network_id = block.network_id();
            for x in 0..16 {
                for z in 0..16 {
                    blocks.set(x, y, z, network_id);
                }
            }
        }
        let sub_chunk = SubChunk {
            layers: vec![blocks],
        };
        let biomes = vec![PalettedStorage::filled(PLAINS); SUB_CHUNKS];
        let lowest_sub_chunk =
            i8::try_from(MIN_Y >> 4).expect("the overworld floor is sub-chunk -4");

        Self {
            payload: chunk::level_chunk_payload(lowest_sub_chunk, &[sub_chunk], &biomes),
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

    /// The chunk column at chunk coordinates (`x`, `z`).
    pub fn chunk(&self, x: i32, z: i32) -> LevelChunk {
        LevelChunk {
            x,
            z,
            dimension: OVERWORLD,
            // Only the bottom sub-chunk holds blocks; everything above is air.
            sub_chunk_count: 1,
            payload: self.payload.clone(),
        }
    }
}

impl Default for FlatWorld {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use mistvale_protocol::chunk::SUB_CHUNK_VERSION;

    use super::*;

    #[test]
    fn players_stand_on_the_grass() {
        assert_eq!(FlatWorld::new().spawn(), BlockPos { x: 8, y: -60, z: 8 });
    }

    #[test]
    fn chunks_hold_one_sub_chunk_and_a_biome_per_sub_chunk() {
        let world = FlatWorld::new();
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
}
