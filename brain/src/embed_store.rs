//! Disk-backed embedding store with brute-force cosine nearest-neighbour.
//!
//! Storage layout (binary, little-endian):
//! ```text
//! ~/.mneme/cache/embed/index.bin
//!   [u32  magic   = 0x44544231 ("DTB1")]
//!   [u32  dim     = 384]
//!   [u64  count   = N]
//!   N x ([u128 node_id] [f32 * dim])
//! ```
//!
//! This is intentionally simpler than VSS / usearch: zero external native
//! deps, opens cleanly on every platform, and the brute-force scan is well
//! under a millisecond per 1k vectors at 384-D. When the corpus exceeds
//! ~250k vectors callers should swap in an ANN index — until then this is
//! both faster end-to-end and easier to audit.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::embeddings::EMBEDDING_DIM;
use crate::error::{BrainError, BrainResult};
use crate::NodeId;

const MAGIC: u32 = 0x4454_4231; // "DTB1"
const FILE_NAME: &str = "index.bin";

/// One ANN result.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct NearestHit {
    pub node: NodeId,
    /// Cosine similarity in `[-1.0, 1.0]`. Higher is closer.
    pub score: f32,
}

/// Disk-backed vector store. Cheap to clone (internally `Arc`-wrapped).
#[derive(Clone)]
pub struct EmbedStore {
    inner: Arc<Inner>,
}

struct Inner {
    dir: PathBuf,
    state: RwLock<State>,
}

#[derive(Default)]
struct State {
    ids: Vec<NodeId>,
    vectors: Vec<f32>, // flat: ids.len() * EMBEDDING_DIM
    dirty: bool,
}

impl std::fmt::Debug for EmbedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.inner.state.read();
        f.debug_struct("EmbedStore")
            .field("dir", &self.inner.dir)
            .field("count", &s.ids.len())
            .field("dim", &EMBEDDING_DIM)
            .finish()
    }
}

impl EmbedStore {
    /// Open at the default `~/.mneme/cache/embed/` directory.
    pub fn open_default() -> BrainResult<Self> {
        Self::open(&default_dir())
    }

    /// Open (or create) a store at `dir`.
    pub fn open(dir: &Path) -> BrainResult<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        let state = if path.exists() {
            match load_file(&path) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "embed index unreadable — starting empty");
                    State::default()
                }
            }
        } else {
            State::default()
        };
        debug!(count = state.ids.len(), "embed store opened");
        Ok(Self {
            inner: Arc::new(Inner {
                dir: dir.to_path_buf(),
                state: RwLock::new(state),
            }),
        })
    }

    /// Number of vectors currently stored.
    pub fn len(&self) -> usize {
        self.inner.state.read().ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Insert / overwrite one vector. The store keeps **the latest write**
    /// for any given node id.
    pub fn upsert(&self, node: NodeId, vector: &[f32]) -> BrainResult<()> {
        if vector.len() != EMBEDDING_DIM {
            return Err(BrainError::Invalid(format!(
                "vector length {} != EMBEDDING_DIM ({})",
                vector.len(),
                EMBEDDING_DIM
            )));
        }
        let mut s = self.inner.state.write();
        if let Some(pos) = s.ids.iter().position(|n| *n == node) {
            let off = pos * EMBEDDING_DIM;
            s.vectors[off..off + EMBEDDING_DIM].copy_from_slice(vector);
        } else {
            s.ids.push(node);
            s.vectors.extend_from_slice(vector);
        }
        s.dirty = true;
        Ok(())
    }

    /// Bulk upsert — cheaper than calling [`Self::upsert`] in a loop because
    /// the lock is taken once.
    pub fn upsert_many(&self, items: &[(NodeId, Vec<f32>)]) -> BrainResult<()> {
        for (_, v) in items {
            if v.len() != EMBEDDING_DIM {
                return Err(BrainError::Invalid(format!(
                    "vector length {} != EMBEDDING_DIM ({})",
                    v.len(),
                    EMBEDDING_DIM
                )));
            }
        }
        let mut s = self.inner.state.write();
        for (node, v) in items {
            if let Some(pos) = s.ids.iter().position(|n| *n == *node) {
                let off = pos * EMBEDDING_DIM;
                s.vectors[off..off + EMBEDDING_DIM].copy_from_slice(v);
            } else {
                s.ids.push(*node);
                s.vectors.extend_from_slice(v);
            }
        }
        s.dirty = true;
        Ok(())
    }

    /// Remove one node. No-op if absent.
    pub fn remove(&self, node: NodeId) -> bool {
        let mut s = self.inner.state.write();
        if let Some(pos) = s.ids.iter().position(|n| *n == node) {
            s.ids.remove(pos);
            let off = pos * EMBEDDING_DIM;
            s.vectors.drain(off..off + EMBEDDING_DIM);
            s.dirty = true;
            true
        } else {
            false
        }
    }

    /// Return the `k` highest-cosine matches.
    ///
    /// `query` is assumed to be already L2-normalised (the [`crate::Embedder`]
    /// guarantees this); stored vectors are too, so cosine == dot product.
    ///
    /// BUG-A2-002 fix: use a `BinaryHeap<Reverse<...>>` min-heap for O(N log k)
    /// instead of sort-on-every-insert (was O(N k log k)).
    pub fn nearest(&self, query: &[f32], k: usize) -> Vec<NearestHit> {
        if query.len() != EMBEDDING_DIM || k == 0 {
            return Vec::new();
        }
        let s = self.inner.state.read();
        let n = s.ids.len();
        if n == 0 {
            return Vec::new();
        }

        // Min-heap of size k. Wrap NearestHit in `Reverse` so the smallest
        // score is on top; that lets us pop-and-replace in O(log k) per step.
        let mut heap: BinaryHeap<Reverse<HeapHit>> = BinaryHeap::with_capacity(k + 1);
        for i in 0..n {
            let off = i * EMBEDDING_DIM;
            let row = &s.vectors[off..off + EMBEDDING_DIM];
            let mut dot = 0f32;
            for d in 0..EMBEDDING_DIM {
                dot += row[d] * query[d];
            }
            let hit = HeapHit {
                node: s.ids[i],
                score: dot,
            };
            if heap.len() < k {
                heap.push(Reverse(hit));
            } else if let Some(Reverse(min)) = heap.peek() {
                if hit.score > min.score {
                    heap.pop();
                    heap.push(Reverse(hit));
                }
            }
        }

        // Drain heap then sort high-to-low for the caller.
        let mut out: Vec<NearestHit> = heap
            .into_iter()
            .map(|Reverse(h)| NearestHit {
                node: h.node,
                score: h.score,
            })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }

    /// Persist any pending writes to `index.bin` (atomic rename).
    pub fn flush(&self) -> BrainResult<()> {
        let mut s = self.inner.state.write();
        if !s.dirty {
            return Ok(());
        }
        let final_path = self.inner.dir.join(FILE_NAME);
        let tmp_path = self.inner.dir.join(format!("{FILE_NAME}.tmp"));

        {
            let f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)?;
            let mut w = BufWriter::new(f);
            w.write_all(&MAGIC.to_le_bytes())?;
            w.write_all(&(EMBEDDING_DIM as u32).to_le_bytes())?;
            w.write_all(&(s.ids.len() as u64).to_le_bytes())?;
            for (i, id) in s.ids.iter().enumerate() {
                w.write_all(&id.0.to_le_bytes())?;
                let off = i * EMBEDDING_DIM;
                let row = &s.vectors[off..off + EMBEDDING_DIM];
                // WIDE-012: write each f32 little-endian byte-by-byte so
                // the on-disk format is portable across architectures
                // (no host-endian dependency, no `unsafe`). For raw
                // pass-through on LE hosts we can use `bytemuck`, but
                // explicit `to_le_bytes()` is unambiguous and trivially
                // verifiable.
                for f in row {
                    w.write_all(&f.to_le_bytes())?;
                }
            }
            w.flush()?;
        }

        // BUG-A2-001 fix: `fs::rename` is atomic on both Unix and Windows
        // (Rust >= 1.5 uses `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`), so
        // the prior `remove_file` opened a crash window where index.bin
        // could vanish entirely. Just rename — Windows handles overwrite.
        fs::rename(&tmp_path, &final_path)?;
        s.dirty = false;
        Ok(())
    }
}

/// Internal heap entry — a `NearestHit` with `Ord` impl driven by score.
#[derive(Debug, Clone, Copy)]
struct HeapHit {
    node: NodeId,
    score: f32,
}

impl PartialEq for HeapHit {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score
    }
}

impl Eq for HeapHit {}

impl PartialOrd for HeapHit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapHit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Total order on f32 — NaN treated as equal/smallest.
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_file(path: &Path) -> BrainResult<State> {
    let f = File::open(path)?;
    let mut r = BufReader::new(f);

    let mut buf4 = [0u8; 4];
    r.read_exact(&mut buf4)?;
    let magic = u32::from_le_bytes(buf4);
    if magic != MAGIC {
        return Err(BrainError::Store(format!("bad magic: {magic:#010x}")));
    }
    r.read_exact(&mut buf4)?;
    let dim = u32::from_le_bytes(buf4) as usize;
    if dim != EMBEDDING_DIM {
        return Err(BrainError::Store(format!(
            "dim mismatch: file={dim} expected={EMBEDDING_DIM}"
        )));
    }
    let mut buf8 = [0u8; 8];
    r.read_exact(&mut buf8)?;
    let count = u64::from_le_bytes(buf8) as usize;

    let mut ids = Vec::with_capacity(count);
    let mut vectors = Vec::with_capacity(count * EMBEDDING_DIM);
    let mut buf16 = [0u8; 16];
    let row_bytes = EMBEDDING_DIM * std::mem::size_of::<f32>();
    let mut row_buf = vec![0u8; row_bytes];

    for _ in 0..count {
        r.read_exact(&mut buf16)?;
        let id = u128::from_le_bytes(buf16);
        ids.push(NodeId(id));
        r.read_exact(&mut row_buf)?;
        for chunk in row_buf.chunks_exact(4) {
            let mut b = [0u8; 4];
            b.copy_from_slice(chunk);
            vectors.push(f32::from_le_bytes(b));
        }
    }

    // Sanity: cursor at EOF.
    let pos = r.stream_position()?;
    let len = r.get_ref().metadata()?.len();
    if pos != len {
        warn!(pos, len, "trailing bytes in embed index");
    }

    Ok(State {
        ids,
        vectors,
        dirty: false,
    })
}

/// Default cache directory: `<mneme-root>/cache/embed/`.
///
/// HOME-bypass-brain (embed_store) fix: route through
/// `PathManager::default_root()` so `MNEME_HOME` is honored.
/// `PathManager` already implements the
/// `MNEME_HOME` -> `~/.mneme` -> OS-default fallback chain.
pub fn default_dir() -> PathBuf {
    common::paths::PathManager::default_root()
        .root()
        .join("cache")
        .join("embed")
}
