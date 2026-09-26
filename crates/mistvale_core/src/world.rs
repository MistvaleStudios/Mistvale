//! The world players spawn into: a superflat overworld whose blocks can change.
//!
//! Every column starts as the same generated superflat column, shared as one
//! encoded payload. Changing a block gives that column its own storage, which
//! is kept, re-encoded whenever it is sent after another change, and marked
//! for saving. A world opened on a directory loads saved columns the first
//! time they are needed and saves changed ones with [`World::save`]; a world
//! from [`World::new`] lives only in memory.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use mistvale_protocol::block::{BlockState, StateValue};
use mistvale_protocol::chunk::{self, PalettedStorage, SubChunk};
use mistvale_protocol::packets::LevelChunk;
use mistvale_protocol::types::{BlockPos, ChunkPos};

use crate::storage::{BLOCKS, ChunkStore, SubChunkBlocks};

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
    /// Changed since it was last saved.
    dirty: bool,
}

/// Columns held in memory, and the saved ones not loaded yet.
#[derive(Debug, Default)]
struct State {
    columns: HashMap<ChunkPos, Column>,
    on_disk: HashSet<ChunkPos>,
}

/// An endless superflat overworld with vanilla's default layers (bedrock, two
/// layers of dirt and grass at y = -64..=-61, all plains) that players can change.
#[derive(Debug)]
pub struct World {
    air: u32,
    /// The generated column, which every unchanged chunk shares.
    generated: Column,
    generated_payload: Vec<u8>,
    state: Mutex<State>,
    /// Where changed columns are saved, if anywhere.
    store: Option<ChunkStore>,
    /// Height of the top (grass) layer.
    surface_y: i32,
}

impl World {
    /// A world that lives only in memory.
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
            dirty: false,
        };
        generated.sub_chunks[0] = Some(blocks);
        let generated_payload = generated.encode(air);

        Self {
            air,
            generated,
            generated_payload,
            state: Mutex::new(State::default()),
            store: None,
            surface_y: MIN_Y + layers.len() as i32 - 1,
        }
    }

    /// The world saved in `directory`, created if it does not exist yet.
    pub fn open(directory: &Path) -> io::Result<Self> {
        let (store, saved) = ChunkStore::open(directory)?;
        let mut world = Self::new();
        world.store = Some(store);
        world
            .state
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .on_disk = saved;
        Ok(world)
    }

    /// How many chunks have saved changes on disk, loaded or not.
    pub fn saved_chunks(&self) -> usize {
        let state = self.state();
        state.on_disk.len()
            + state
                .columns
                .values()
                .filter(|column| !column.dirty)
                .count()
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
        let chunk = ChunkPos::of_block(pos);
        let mut state = self.state();
        self.load(&mut state, chunk);
        let column = state.columns.get(&chunk).unwrap_or(&self.generated);
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
        let chunk = ChunkPos::of_block(pos);
        let mut state = self.state();
        self.load(&mut state, chunk);
        let current = state
            .columns
            .get(&chunk)
            .unwrap_or(&self.generated)
            .sub_chunks[sub_chunk]
            .as_ref()
            .map_or(self.air, |storage| storage.get(x, y, z));
        let block = change(current).filter(|block| *block != current)?;

        let column = state
            .columns
            .entry(chunk)
            .or_insert_with(|| self.generated.clone());
        column.sub_chunks[sub_chunk]
            .get_or_insert_with(|| PalettedStorage::filled(self.air))
            .set(x, y, z, block);
        column.payload = None;
        column.dirty = true;
        Some(current)
    }

    /// The chunk column at chunk coordinates (`x`, `z`), with any changes.
    pub fn chunk(&self, x: i32, z: i32) -> LevelChunk {
        let chunk = ChunkPos::new(x, z);
        let mut state = self.state();
        self.load(&mut state, chunk);
        let (sub_chunk_count, payload) = match state.columns.get_mut(&chunk) {
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

    /// Writes every column changed since the last save. Returns how many were
    /// saved; a column that fails to save stays marked and is tried next time.
    pub fn save(&self) -> io::Result<usize> {
        let Some(store) = &self.store else {
            return Ok(0);
        };
        // Copy the changed columns out, so players are not kept waiting on disk.
        let changed: Vec<(ChunkPos, Vec<SubChunkBlocks>)> = self
            .state()
            .columns
            .iter_mut()
            .filter(|(_, column)| column.dirty)
            .map(|(chunk, column)| {
                column.dirty = false;
                (*chunk, column.blocks())
            })
            .collect();

        let mut saved = 0;
        let mut first_error = None;
        for (chunk, blocks) in changed {
            match store.save(chunk, &blocks) {
                Ok(()) => saved += 1,
                Err(err) => {
                    if let Some(column) = self.state().columns.get_mut(&chunk) {
                        column.dirty = true;
                    }
                    first_error.get_or_insert(err);
                }
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(saved),
        }
    }

    /// Brings a saved column into memory the first time its chunk is used. A
    /// file that cannot be read is logged, and the chunk is generated instead.
    fn load(&self, state: &mut State, chunk: ChunkPos) {
        if !state.on_disk.remove(&chunk) {
            return;
        }
        let Some(store) = &self.store else {
            return;
        };
        let loaded = store
            .load(chunk)
            .map_err(|err| err.to_string())
            .and_then(|blocks| self.column_from(blocks));
        match loaded {
            Ok(column) => {
                state.columns.insert(chunk, column);
            }
            Err(err) => {
                tracing::warn!(chunk = ?(chunk.x, chunk.z), %err, "ignoring a saved chunk that cannot be read");
            }
        }
    }

    fn column_from(&self, blocks: Vec<SubChunkBlocks>) -> Result<Column, String> {
        if blocks.len() != SUB_CHUNKS {
            return Err(format!(
                "{} sub-chunks, expected {SUB_CHUNKS}",
                blocks.len()
            ));
        }
        let sub_chunks = blocks
            .into_iter()
            .map(|blocks| {
                blocks.map(|blocks| {
                    let mut storage = PalettedStorage::filled(self.air);
                    for (index, block) in blocks.into_iter().enumerate() {
                        if block != self.air {
                            let (x, y, z) = position_of(index);
                            storage.set(x, y, z, block);
                        }
                    }
                    storage
                })
            })
            .collect();
        Ok(Column {
            sub_chunks,
            payload: None,
            dirty: false,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Columns stay consistent even if a holder panicked.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl Column {
    /// Every sub-chunk's blocks, in the storage file's order.
    fn blocks(&self) -> Vec<SubChunkBlocks> {
        self.sub_chunks
            .iter()
            .map(|storage| {
                storage.as_ref().map(|storage| {
                    (0..BLOCKS)
                        .map(|index| {
                            let (x, y, z) = position_of(index);
                            storage.get(x, y, z)
                        })
                        .collect()
                })
            })
            .collect()
    }

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

/// The position within a sub-chunk of the `index`th block, in x, z, y order
/// (the order of paletted storage and of chunk files).
fn position_of(index: usize) -> (u8, u8, u8) {
    (
        ((index >> 8) & 15) as u8,
        (index & 15) as u8,
        ((index >> 4) & 15) as u8,
    )
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
    use std::path::PathBuf;

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

    fn temporary_world(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("mistvale-world-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        directory
    }

    #[test]
    fn changes_survive_a_restart() {
        let directory = temporary_world("restart");
        let stone = BlockState::new("minecraft:stone").network_id();
        let hole = BlockPos {
            x: 3,
            y: -61,
            z: -5,
        };
        let tower = BlockPos {
            x: 20,
            y: 100,
            z: 20,
        };

        let world = World::open(&directory).unwrap();
        assert!(world.set_block(hole, world.air()));
        assert!(world.set_block(tower, stone));
        let before = world.chunk(1, 1).payload;
        assert_eq!(world.save().unwrap(), 2);
        assert_eq!(world.save().unwrap(), 0, "nothing changed since");
        drop(world);

        // A fresh server on the same directory sees the same world.
        let world = World::open(&directory).unwrap();
        assert_eq!(world.saved_chunks(), 2);
        assert_eq!(world.block(hole), world.air());
        assert_eq!(world.block(tower), stone);
        assert_eq!(world.chunk(1, 1).payload, before);
        // Untouched chunks are still generated, and not saved.
        assert_eq!(
            world.block(BlockPos {
                x: 40,
                y: -61,
                z: 40
            }),
            BlockState::new("minecraft:grass_block").network_id()
        );
        assert_eq!(world.save().unwrap(), 0);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn an_unreadable_chunk_file_falls_back_to_generation() {
        let directory = temporary_world("damaged");
        let world = World::open(&directory).unwrap();
        world.set_block(BlockPos { x: 0, y: -61, z: 0 }, world.air());
        world.save().unwrap();
        drop(world);
        std::fs::write(directory.join("chunks").join("c.0.0.bin"), b"garbage").unwrap();

        let world = World::open(&directory).unwrap();
        assert_eq!(
            world.block(BlockPos { x: 0, y: -61, z: 0 }),
            BlockState::new("minecraft:grass_block").network_id()
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn block_positions_follow_storage_order() {
        // (x, y, z) from indices ordered x, then z, then y.
        assert_eq!(position_of(0), (0, 0, 0));
        assert_eq!(position_of(1), (0, 1, 0));
        assert_eq!(position_of(16), (0, 0, 1));
        assert_eq!(position_of(256), (1, 0, 0));
        assert_eq!(position_of(4095), (15, 15, 15));
    }
}
