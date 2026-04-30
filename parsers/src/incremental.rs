//! Incremental parsing — keeps a per-file LRU of previous trees.
//!
//! Rules (§21.3.2 anti-patterns):
//! - Always pass `old_tree` to `parse()` if one exists for this file.
//! - Compute a `TSInputEdit` from the byte-range diff between old + new bytes.
//! - Cap the cache at 1000 files (LRU eviction).
//! - Drop trees aggressively when the file's hash matches — no work to do.

use crate::error::ParserError;
use crate::language::Language;
use crate::parser_pool::ParserPool;
use lru::LruCache;
use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::trace;
use tree_sitter::{InputEdit, Point, Tree};

/// Maximum number of trees the LRU keeps alive.
///
/// AI-DNA pace: bumped from 1000 to 4000 (4× headroom). Anish indexes 1000+
/// file projects; AI editing across the whole project burst-cycles all of
/// them. With the legacy 1000-cap the LRU evicts hot trees mid-session,
/// which forces full re-parses on the next save and breaks the
/// "same-speed indexing" promise. 4000 trees × ~10KB ≈ 40MB — well within
/// the resource policy ("no artificial caps on RAM, CPU, or disk", see
/// `docs/design/2026-04-23-resource-policy-addendum.md`). Override at
/// runtime via `MNEME_PARSE_TREE_CACHE` (env, parsed in main.rs).
///
/// See `feedback_mneme_ai_dna_pace.md` Principle B: "Same-speed indexing.
/// When AI is editing 10 files in 30 seconds, the watcher → re-parse →
/// re-embed → graph-update path completes before the next AI tool call."
pub const DEFAULT_TREE_CACHE: usize = 4000;

/// One slot of the LRU.
#[derive(Debug, Clone)]
struct CachedTree {
    tree: Tree,
    /// The bytes the tree was parsed from — needed to compute `InputEdit`.
    bytes: Arc<Vec<u8>>,
    /// Best-effort content hash — short-circuits no-op parses.
    hash: [u8; 32],
}

/// LRU + diff-driven incremental parser orchestrator.
///
/// Holds an `Arc<ParserPool>` so each `parse_file` call leases a parser per
/// the parser-per-worker rule (§21.3.2).
pub struct IncrementalParser {
    pool: Arc<ParserPool>,
    cache: Mutex<LruCache<PathBuf, CachedTree>>,
}

impl std::fmt::Debug for IncrementalParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncrementalParser")
            .field("cache_capacity", &self.cache.lock().cap())
            .field("cache_size", &self.cache.lock().len())
            .finish()
    }
}

/// Result of [`IncrementalParser::parse_file`].
pub struct IncrementalParse {
    pub tree: Tree,
    pub incremental: bool,
    pub unchanged: bool,
}

impl std::fmt::Debug for IncrementalParse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncrementalParse")
            .field("incremental", &self.incremental)
            .field("unchanged", &self.unchanged)
            .field("root_kind", &self.tree.root_node().kind())
            .finish()
    }
}

impl IncrementalParser {
    /// Build with the default 1000-file cache.
    pub fn new(pool: Arc<ParserPool>) -> Self {
        Self::with_capacity(pool, DEFAULT_TREE_CACHE)
    }

    /// Custom-capacity constructor — tests use small values.
    pub fn with_capacity(pool: Arc<ParserPool>, capacity: usize) -> Self {
        // SAFETY: `capacity.max(1)` is guaranteed `>= 1`, so `NonZeroUsize::new`
        // always returns `Some(_)`. Programmer-impossible None.
        let cap = NonZeroUsize::new(capacity.max(1)).expect("capacity.max(1) is always >= 1");
        Self {
            pool,
            cache: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Parse `bytes` for `path`. Reuses the cached tree if one exists.
    ///
    /// `lang` is required because the same file path could in principle be
    /// re-classified (e.g. .ts → .tsx after a rename); we never assume the
    /// cached entry's language.
    pub async fn parse_file(
        &self,
        path: &Path,
        lang: Language,
        bytes: Arc<Vec<u8>>,
    ) -> Result<IncrementalParse, ParserError> {
        // 1. Compute the new content hash up-front; we use it for the
        //    short-circuit and for storing the new cache entry.
        let new_hash = blake3::hash(&bytes);
        let new_hash_bytes: [u8; 32] = *new_hash.as_bytes();

        // 2. Pull the cached entry (if any) without holding the lock across
        //    the await point — promote-on-touch is fine because we'll
        //    re-insert the new tree before returning.
        let cached = {
            let mut cache = self.cache.lock();
            cache.get(path).cloned()
        };

        // 3. Short-circuit: byte-identical content → reuse the cached tree
        //    verbatim.
        if let Some(c) = &cached {
            if c.hash == new_hash_bytes {
                trace!(path = %path.display(), "incremental: bytes unchanged, reusing tree");
                return Ok(IncrementalParse {
                    tree: c.tree.clone(),
                    incremental: true,
                    unchanged: true,
                });
            }
        }

        // 4. Acquire a parser slot for this language.
        let mut lease = self.pool.acquire(lang).await?;
        let parser = lease.parser();

        // 5. Build the InputEdit (if we have an old tree) and apply it.
        let prepared_old = cached.as_ref().map(|c| {
            let mut prev = c.tree.clone();
            let edit = compute_input_edit(&c.bytes, &bytes);
            prev.edit(&edit);
            prev
        });

        // 6. Run the parse — passing `Some(prev)` triggers the incremental
        //    path inside tree-sitter (§21.3 mandatory).
        let new_tree = parser
            .parse(&bytes[..], prepared_old.as_ref())
            .ok_or_else(|| ParserError::ParseFailed(path.to_path_buf()))?;

        // 7. Store the new entry (and bytes) for future calls.
        {
            let mut cache = self.cache.lock();
            cache.put(
                path.to_path_buf(),
                CachedTree {
                    tree: new_tree.clone(),
                    bytes: bytes.clone(),
                    hash: new_hash_bytes,
                },
            );
        }

        Ok(IncrementalParse {
            tree: new_tree,
            incremental: prepared_old.is_some(),
            unchanged: false,
        })
    }

    /// Drop the cached tree for `path`. Called when the file is deleted or
    /// renamed by the file watcher.
    pub fn forget(&self, path: &Path) {
        self.cache.lock().pop(path);
    }

    /// Number of cached trees right now.
    pub fn cached_count(&self) -> usize {
        self.cache.lock().len()
    }

    /// Eviction capacity.
    pub fn capacity(&self) -> usize {
        self.cache.lock().cap().get()
    }
}

// ---------------------------------------------------------------------------
// InputEdit computation
// ---------------------------------------------------------------------------

/// Compute a `TSInputEdit` describing the byte-range diff from `old` to `new`.
///
/// We use a longest-common-prefix + longest-common-suffix scan — sufficient
/// for the typical "user typed in the middle of a file" workload that the
/// 50ms debounce window batches into a single edit (§21.3, §25.10).
///
/// For pathological diffs (every byte changed) the result still parses
/// correctly — tree-sitter's incremental path will simply reuse no nodes.
fn compute_input_edit(old: &[u8], new: &[u8]) -> InputEdit {
    let prefix = common_prefix_len(old, new);
    let suffix = common_suffix_len(&old[prefix..], &new[prefix..]);

    let old_end = old.len() - suffix;
    let new_end = new.len() - suffix;

    InputEdit {
        start_byte: prefix,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: byte_to_point(old, prefix),
        old_end_position: byte_to_point(old, old_end),
        new_end_position: byte_to_point(new, new_end),
    }
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

fn common_suffix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
}

/// Map a byte offset into a tree-sitter [`Point`] (row, column) without
/// re-allocating. Linear scan — adequate for the small (<1MB) source files
/// the editor surface produces; full files use the bulk path in `parse_file`.
fn byte_to_point(bytes: &[u8], offset: usize) -> Point {
    let mut row = 0usize;
    let mut col = 0usize;
    for &b in bytes.iter().take(offset) {
        if b == b'\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    Point { row, column: col }
}

#[cfg(test)]
mod input_edit_tests {
    use super::*;

    #[test]
    fn unchanged_yields_zero_span() {
        let edit = compute_input_edit(b"hello world", b"hello world");
        assert_eq!(edit.start_byte, edit.old_end_byte);
        assert_eq!(edit.start_byte, edit.new_end_byte);
    }

    #[test]
    fn middle_insertion() {
        let edit = compute_input_edit(b"abc def", b"abc XXX def");
        assert_eq!(edit.start_byte, 4);
        assert_eq!(edit.old_end_byte, 4);
        assert_eq!(edit.new_end_byte, 8);
    }

    #[test]
    fn middle_deletion() {
        let edit = compute_input_edit(b"abc XXX def", b"abc def");
        assert_eq!(edit.start_byte, 4);
        assert_eq!(edit.old_end_byte, 8);
        assert_eq!(edit.new_end_byte, 4);
    }

    #[test]
    fn point_is_row_column() {
        let bytes = b"line1\nline2\nthird";
        let p = byte_to_point(bytes, 12); // 't' of "third"
        assert_eq!(p.row, 2);
        assert_eq!(p.column, 0);
    }
}
