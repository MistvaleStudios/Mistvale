//! Changed chunks on disk: one small compressed file per chunk column.
//!
//! Only columns that players changed are stored; everything else is generated.
//! Files live in `<world>/chunks/c.<x>.<z>.bin` and are written to a temporary
//! file first, then renamed over the old one, so a crash mid-save never leaves
//! a half-written chunk.
//!
//! Layout: the magic `MVCH`, a format version byte, then zlib-compressed data:
//! the sub-chunk count, and for each sub-chunk from the bottom a presence byte
//! followed, if present, by 4096 little-endian u32 block network IDs in
//! x, z, y order. Block network IDs are block state hashes, which stay the
//! same as long as a block's name and states do.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use mistvale_protocol::types::ChunkPos;

const MAGIC: &[u8; 4] = b"MVCH";
const VERSION: u8 = 1;
/// Blocks in a sub-chunk.
pub const BLOCKS: usize = 16 * 16 * 16;
/// Largest decompressed chunk accepted: every sub-chunk of a 64-high column.
const MAX_DATA: u64 = 1 + 64 * (1 + BLOCKS as u64 * 4);

/// One sub-chunk's blocks, or `None` if it is all air.
pub type SubChunkBlocks = Option<Vec<u32>>;

/// Why a chunk file could not be read.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("not a Mistvale chunk file")]
    NotAChunk,
    #[error("unsupported chunk format version {0}")]
    Version(u8),
    #[error("truncated or oversized chunk data")]
    Malformed,
}

/// The directory of saved chunks.
#[derive(Debug)]
pub struct ChunkStore {
    directory: PathBuf,
}

impl ChunkStore {
    /// Opens (creating if needed) the chunk directory of the world at `world`,
    /// and lists the chunks saved in it.
    pub fn open(world: &Path) -> io::Result<(Self, HashSet<ChunkPos>)> {
        let directory = world.join("chunks");
        fs::create_dir_all(&directory)?;
        let mut saved = HashSet::new();
        for entry in fs::read_dir(&directory)? {
            let name = entry?.file_name();
            if let Some(chunk) = name.to_str().and_then(parse_file_name) {
                saved.insert(chunk);
            }
        }
        Ok((Self { directory }, saved))
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Reads a saved chunk's sub-chunks, from the bottom up.
    pub fn load(&self, chunk: ChunkPos) -> Result<Vec<SubChunkBlocks>, StoreError> {
        let bytes = fs::read(self.path(chunk))?;
        decode(&bytes)
    }

    /// Writes a chunk's sub-chunks, from the bottom up, replacing any saved copy.
    pub fn save(&self, chunk: ChunkPos, sub_chunks: &[SubChunkBlocks]) -> io::Result<()> {
        let path = self.path(chunk);
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, encode(sub_chunks)?)?;
        fs::rename(&temporary, &path)
    }

    fn path(&self, chunk: ChunkPos) -> PathBuf {
        self.directory
            .join(format!("c.{}.{}.bin", chunk.x, chunk.z))
    }
}

/// `c.<x>.<z>.bin` back to its chunk position.
fn parse_file_name(name: &str) -> Option<ChunkPos> {
    let (x, z) = name
        .strip_prefix("c.")?
        .strip_suffix(".bin")?
        .split_once('.')?;
    Some(ChunkPos::new(x.parse().ok()?, z.parse().ok()?))
}

fn encode(sub_chunks: &[SubChunkBlocks]) -> io::Result<Vec<u8>> {
    let mut file = MAGIC.to_vec();
    file.push(VERSION);
    let mut encoder = ZlibEncoder::new(file, Compression::default());
    let count = u8::try_from(sub_chunks.len()).expect("a column has at most 64 sub-chunks");
    encoder.write_all(&[count])?;
    for sub_chunk in sub_chunks {
        match sub_chunk {
            Some(blocks) => {
                assert_eq!(blocks.len(), BLOCKS, "a sub-chunk has 4096 blocks");
                encoder.write_all(&[1])?;
                for block in blocks {
                    encoder.write_all(&block.to_le_bytes())?;
                }
            }
            None => encoder.write_all(&[0])?,
        }
    }
    encoder.finish()
}

fn decode(bytes: &[u8]) -> Result<Vec<SubChunkBlocks>, StoreError> {
    let (header, compressed) = bytes.split_at_checked(5).ok_or(StoreError::NotAChunk)?;
    if &header[..4] != MAGIC {
        return Err(StoreError::NotAChunk);
    }
    if header[4] != VERSION {
        return Err(StoreError::Version(header[4]));
    }
    // Refuse to inflate more than a chunk can hold.
    let mut data = Vec::new();
    ZlibDecoder::new(compressed)
        .take(MAX_DATA + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > MAX_DATA {
        return Err(StoreError::Malformed);
    }

    let (&count, mut rest) = data.split_first().ok_or(StoreError::Malformed)?;
    let mut sub_chunks = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let (&present, after) = rest.split_first().ok_or(StoreError::Malformed)?;
        rest = after;
        if present == 0 {
            sub_chunks.push(None);
            continue;
        }
        let (blocks, after) = rest
            .split_at_checked(BLOCKS * 4)
            .ok_or(StoreError::Malformed)?;
        rest = after;
        let blocks = blocks
            .as_chunks::<4>()
            .0
            .iter()
            .map(|block| u32::from_le_bytes(*block))
            .collect();
        sub_chunks.push(Some(blocks));
    }
    if !rest.is_empty() {
        return Err(StoreError::Malformed);
    }
    Ok(sub_chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_world(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("mistvale-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        directory
    }

    fn column() -> Vec<SubChunkBlocks> {
        let mut bottom = vec![7; BLOCKS];
        bottom[100] = 42;
        let mut column = vec![None; 24];
        column[0] = Some(bottom);
        column[5] = Some(vec![9; BLOCKS]);
        column
    }

    #[test]
    fn saved_chunks_load_back_and_are_listed_on_open() {
        let world = temporary_world("store");
        let (store, saved) = ChunkStore::open(&world).unwrap();
        assert!(saved.is_empty());
        store.save(ChunkPos::new(-3, 12), &column()).unwrap();
        assert_eq!(store.load(ChunkPos::new(-3, 12)).unwrap(), column());

        // Stray files are ignored when listing.
        fs::write(store.directory().join("notes.txt"), "hi").unwrap();
        let (store, saved) = ChunkStore::open(&world).unwrap();
        assert_eq!(saved, HashSet::from([ChunkPos::new(-3, 12)]));
        // Uniform sub-chunks compress to almost nothing.
        let size = fs::metadata(store.path(ChunkPos::new(-3, 12)))
            .unwrap()
            .len();
        assert!(size < 1024, "{size} bytes");
        fs::remove_dir_all(&world).unwrap();
    }

    #[test]
    fn damaged_files_are_errors_not_panics() {
        assert!(matches!(decode(b"nope"), Err(StoreError::NotAChunk)));
        assert!(matches!(
            decode(b"MVCH\x09..."),
            Err(StoreError::Version(9))
        ));
        let mut truncated = encode(&column()).unwrap();
        truncated.truncate(truncated.len() / 2);
        assert!(decode(&truncated).is_err());
        assert!(parse_file_name("c.1.x.bin").is_none());
        assert_eq!(parse_file_name("c.-1.2.bin"), Some(ChunkPos::new(-1, 2)));
    }
}
