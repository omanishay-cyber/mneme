//! Local sentence embedding for mneme.
//!
//! # Two-tier backend strategy
//!
//! Mneme embeds code/text into 384-dim vectors for semantic recall. The
//! quality of those vectors directly controls retrieval hit-rate
//! (`recall_concept`, `blast_radius` secondary ranking, `find_references`
//! across renames, etc).
//!
//! The embedder selects its backend at runtime:
//!
//! 1. **`real-embeddings` feature ON** — tries to load BGE-small-en-v1.5
//!    via the `ort` crate (ONNX Runtime) + `tokenizers`. Requires:
//!    * an `onnxruntime.dll` / `libonnxruntime.so` on `PATH`, **or**
//!    * the `ORT_DYLIB_PATH` env var pointing at the shared library, **and**
//!    * the model + tokenizer at `$MNEME_HOME/models/` (or the default
//!      `~/.mneme/models/`).
//!
//!    If any of those are missing, we log `warn!` and fall back to the
//!    hashing-trick backend — **never panic**.
//!
//! 2. **`real-embeddings` feature OFF** (the default on Windows, and on
//!    any tier where ORT DLL loading is unreliable): the embedder always
//!    uses the pure-Rust hashing-trick backend. Zero native deps, zero
//!    DLLs, identical public API. Quality is lower on abstract paraphrase
//!    but competitive on code (where token overlap dominates).
//!
//! Switching backends requires no caller change — the public API
//! ([`Embedder::embed`], [`Embedder::embed_batch`]) is identical.
//!
//! # Offline-first
//!
//! Mneme never makes unsolicited network calls. The BGE model must be
//! installed explicitly by the user (`mneme models install <path.onnx>`).
//! After that, everything is local.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashMap;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;
use tracing::{debug, info, warn};

use crate::error::{BrainError, BrainResult};

/// BGE-Small-En-v1.5 produces 384-dim embeddings. The hashing-trick fallback
/// uses the same dimension for drop-in compatibility.
pub const EMBEDDING_DIM: usize = 384;

/// Maximum tokens fed to the model at once. BGE-small was trained on 512.
const MAX_TOKENS: usize = 512;

/// Cap on the per-Embedder text->vector cache. Long-running daemons embedding
/// many distinct strings would otherwise leak unbounded memory.
/// BUG-A2-007 fix.
const EMBED_CACHE_CAPACITY: usize = 10_000;

/// Process-wide registry of backends keyed by `(model_path, tokenizer_path)`.
/// BUG-A2-003 fix: a single `OnceCell` keyed by nothing meant the FIRST call
/// (often a bogus probe) decided the backend forever; subsequent calls with
/// real model paths were ignored. Now distinct path pairs get distinct
/// backends and a `models install` followed by a fresh `Embedder::new` engages
/// the real BGE backend without a daemon restart.
static GLOBAL_BACKENDS: OnceCell<DashMap<(PathBuf, PathBuf), Arc<Mutex<Backend>>>> =
    OnceCell::new();

/// Public embedder handle. Cheap to clone.
#[derive(Clone)]
pub struct Embedder {
    inner: Arc<Inner>,
}

struct Inner {
    backend: Arc<Mutex<Backend>>,
    cache: DashMap<[u8; 32], Vec<f32>>,
    /// Insertion order tracker for the LRU eviction in [`Inner::cache`].
    /// BUG-A2-007: a parallel deque guards the cache from unbounded growth.
    /// Held under its own short-lived mutex so cache reads stay lock-free
    /// on the DashMap fast path.
    cache_order: Mutex<std::collections::VecDeque<[u8; 32]>>,
    model_path: PathBuf,
    tokenizer_path: PathBuf,
}

impl std::fmt::Debug for Embedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Embedder")
            .field("model_path", &self.inner.model_path)
            .field("tokenizer_path", &self.inner.tokenizer_path)
            .field("backend", &self.backend_name())
            .field("cached", &self.inner.cache.len())
            .finish()
    }
}

impl Embedder {
    /// Build an embedder from the default `$MNEME_HOME/models/` path
    /// (or `~/.mneme/models/` if `MNEME_HOME` is unset).
    pub fn from_default_path() -> BrainResult<Self> {
        let base = default_model_dir();
        Self::new(
            &base.join("bge-small-en-v1.5.onnx"),
            &base.join("tokenizer.json"),
        )
    }

    /// Build an embedder from explicit paths. Missing files are tolerated —
    /// the embedder falls back to the pure-Rust hashing-trick backend and
    /// logs a warning.
    ///
    /// BUG-A2-003 fix: the backend is keyed by `(model_path, tokenizer_path)`
    /// so a probe with bogus paths can no longer poison the cache for a
    /// later real install.
    pub fn new(model_path: &Path, tokenizer_path: &Path) -> BrainResult<Self> {
        let registry = GLOBAL_BACKENDS.get_or_init(DashMap::new);
        let key = (model_path.to_path_buf(), tokenizer_path.to_path_buf());

        // Fast path: backend already loaded for these paths.
        let backend = if let Some(entry) = registry.get(&key) {
            entry.clone()
        } else {
            // Slow path: insert if absent. `entry().or_insert_with` keeps
            // the construction inside the dashmap's per-shard lock.
            registry
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(Backend::Uninitialized)))
                .clone()
        };

        // Eager init. Try real model first, fall back to hashing trick.
        {
            let mut guard = backend.lock();
            if matches!(*guard, Backend::Uninitialized) {
                *guard = Backend::load(model_path, tokenizer_path);
                match &*guard {
                    Backend::Real(_) => info!(
                        model = %model_path.display(),
                        "BGE embedder loaded - real transformer path active"
                    ),
                    Backend::Fallback(_) => warn!(
                        model = %model_path.display(),
                        "BGE model missing or ORT unavailable - embedder running in \
                         hashing-trick fallback mode. Run `mneme models install <path>` \
                         and (if needed) set ORT_DYLIB_PATH for full retrieval quality."
                    ),
                    Backend::Uninitialized => unreachable!(),
                }
            }
        }

        Ok(Self {
            inner: Arc::new(Inner {
                backend,
                cache: DashMap::new(),
                cache_order: Mutex::new(std::collections::VecDeque::with_capacity(
                    EMBED_CACHE_CAPACITY,
                )),
                model_path: model_path.to_path_buf(),
                tokenizer_path: tokenizer_path.to_path_buf(),
            }),
        })
    }

    /// True iff the real transformer backend is active. When false, the
    /// embedder is in fallback (hashing-trick) mode — still functional but
    /// lower retrieval quality.
    pub fn is_ready(&self) -> bool {
        matches!(*self.inner.backend.lock(), Backend::Real(_))
    }

    /// Name of the active backend: `"bge-small-en-v1.5"` or `"hashing-trick"`.
    pub fn backend_name(&self) -> &'static str {
        match *self.inner.backend.lock() {
            Backend::Real(_) => "bge-small-en-v1.5",
            Backend::Fallback(_) => "hashing-trick",
            Backend::Uninitialized => "uninitialized",
        }
    }

    /// Embed a single text. Returns a 384-element vector.
    pub fn embed(&self, text: &str) -> BrainResult<Vec<f32>> {
        let key = hash_key(text);
        if let Some(v) = self.inner.cache.get(&key) {
            return Ok(v.clone());
        }

        let vec = {
            let mut guard = self.inner.backend.lock();
            guard.embed_one(text)?
        };
        self.cache_put(key, vec.clone());
        Ok(vec)
    }

    /// Batched embedding. Order of returned vectors matches `texts`.
    pub fn embed_batch(&self, texts: &[&str]) -> BrainResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Resolve cached entries first; only run uncached items through the
        // backend.
        let mut out: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        let mut to_compute_idx: Vec<usize> = Vec::new();
        let mut to_compute_text: Vec<&str> = Vec::new();

        for (i, t) in texts.iter().enumerate() {
            let k = hash_key(t);
            if let Some(v) = self.inner.cache.get(&k) {
                out[i] = Some(v.clone());
            } else {
                to_compute_idx.push(i);
                to_compute_text.push(t);
            }
        }

        if !to_compute_text.is_empty() {
            let computed = {
                let mut guard = self.inner.backend.lock();
                guard.embed_batch(&to_compute_text)?
            };
            for (slot, vec) in to_compute_idx.into_iter().zip(computed) {
                let k = hash_key(texts[slot]);
                self.cache_put(k, vec.clone());
                out[slot] = Some(vec);
            }
        }

        Ok(out
            .into_iter()
            .map(|v| v.unwrap_or_else(zero_vec))
            .collect())
    }

    /// Drop the cache. Useful for memory-pressure callbacks.
    pub fn clear_cache(&self) {
        self.inner.cache.clear();
        self.inner.cache_order.lock().clear();
    }

    /// BUG-A2-042 fix: caller-driven cache invalidation. Lets the worker
    /// invalidate the cache entry for a text when downstream `EmbedStore`
    /// upsert fails, so the cached vector cannot drift from the persisted
    /// store.
    pub fn invalidate_cache_for(&self, text: &str) {
        let k = hash_key(text);
        self.inner.cache.remove(&k);
        let mut order = self.inner.cache_order.lock();
        if let Some(pos) = order.iter().position(|x| x == &k) {
            order.remove(pos);
        }
    }

    /// LRU insertion: drop the oldest entry once we exceed the cap.
    /// BUG-A2-007 fix.
    fn cache_put(&self, key: [u8; 32], vec: Vec<f32>) {
        let mut order = self.inner.cache_order.lock();
        if self.inner.cache.insert(key, vec).is_none() {
            order.push_back(key);
        } else if let Some(pos) = order.iter().position(|x| x == &key) {
            // Existing entry — move to back (most recently used).
            order.remove(pos);
            order.push_back(key);
        }
        while order.len() > EMBED_CACHE_CAPACITY {
            if let Some(stale) = order.pop_front() {
                self.inner.cache.remove(&stale);
            }
        }
    }

    pub fn model_path(&self) -> &Path {
        &self.inner.model_path
    }
}

// ---------------------------------------------------------------------------
// Backend enum — real or fallback
// ---------------------------------------------------------------------------

enum Backend {
    Uninitialized,
    Real(RealBackend),
    Fallback(FallbackBackend),
}

impl Backend {
    fn load(model_path: &Path, tokenizer_path: &Path) -> Self {
        #[cfg(feature = "real-embeddings")]
        {
            // Bug-2026-05-02: runtime opt-out for BGE. Even when the
            // `real-embeddings` feature is compiled in and `try_new`
            // succeeds, `session.run()` can hang indefinitely on
            // certain Windows environments (observed: WinDev2407 VM
            // with onnxruntime.dll 1.20 + ort 2.0.0-rc.12). Setting
            // MNEME_FORCE_HASH_EMBED=1 bypasses BGE entirely so the
            // build always completes. Use this on any machine where
            // `mneme build` reports `phase=embed processed=0/N` for
            // more than a minute.
            let force_hash = std::env::var("MNEME_FORCE_HASH_EMBED")
                .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
                .unwrap_or(false);
            if force_hash {
                warn!("MNEME_FORCE_HASH_EMBED=1 set — skipping BGE, using hashing-trick");
            } else {
                match RealBackend::try_new(model_path, tokenizer_path) {
                    Ok(b) => return Backend::Real(b),
                    Err(e) => {
                        warn!(
                            error = %e,
                            "ort/BGE init failed — using hashing-trick fallback"
                        );
                    }
                }
            }
        }
        let _ = (model_path, tokenizer_path); // silence unused warnings on default build
        Backend::Fallback(FallbackBackend::try_load(tokenizer_path))
    }

    fn embed_one(&mut self, text: &str) -> BrainResult<Vec<f32>> {
        match self {
            Backend::Real(b) => b.embed_one(text),
            Backend::Fallback(b) => Ok(b.embed_one(text)),
            Backend::Uninitialized => Ok(zero_vec()),
        }
    }

    fn embed_batch(&mut self, texts: &[&str]) -> BrainResult<Vec<Vec<f32>>> {
        match self {
            Backend::Real(b) => b.embed_batch(texts),
            Backend::Fallback(b) => Ok(b.embed_batch(texts)),
            Backend::Uninitialized => Ok(vec![zero_vec(); texts.len()]),
        }
    }
}

// ---------------------------------------------------------------------------
// Real backend (direct `ort` + `tokenizers`, BGE-Small-En-v1.5)
// ---------------------------------------------------------------------------

#[cfg(feature = "real-embeddings")]
struct RealBackend {
    session: ort::session::Session,
    tokenizer: Tokenizer,
}

#[cfg(feature = "real-embeddings")]
impl RealBackend {
    fn try_new(model_path: &Path, tokenizer_path: &Path) -> BrainResult<Self> {
        use ort::session::Session;

        if !model_path.exists() {
            return Err(BrainError::ModelMissing {
                path: model_path.display().to_string(),
            });
        }
        if !tokenizer_path.exists() {
            return Err(BrainError::TokenizerMissing {
                path: tokenizer_path.display().to_string(),
            });
        }

        // Bug-2026-05-02: pin ORT_DYLIB_PATH to the bundled onnxruntime.dll
        // BEFORE Session::builder triggers the lazy ort::init().
        //
        // Background: ort 2.0.0-rc.12 is built against ONNX Runtime 1.24
        // (default `api-24` feature). We bundle a matching 1.24.4 DLL in
        // ~/.mneme/bin/onnxruntime.dll. But Windows DLL search order
        // checks System32 BEFORE the application directory, and Windows
        // 11 24H2 ships its OWN onnxruntime.dll in System32 (used by
        // Copilot/WinML). That system DLL is typically a different
        // version than what ort rc.12 expects, leading to a vtable
        // mismatch on the first session.run() call — process hangs on
        // EventPairLow with all threads in Wait state. Reproducible on
        // 5 trivial files. See sherpa-onnx#3059 + onnxruntime#11799 for
        // the same hijack class of bug.
        //
        // Resolution: set ORT_DYLIB_PATH to the absolute path of OUR
        // bundled DLL before any Session is built. The `load-dynamic`
        // feature in the ort crate honors this env var unconditionally
        // and skips the default Windows search order entirely.
        //
        // We compute the bundled path as `<exe_dir>/onnxruntime.dll`
        // because mneme.exe lives in ~/.mneme/bin/ and the install
        // pipeline copies onnxruntime.dll alongside it (see
        // scripts/test/stage-release-zip.ps1::stage bin/).
        // BUG-A2-004 fix: gate the env mutation behind a one-shot OnceCell
        // so even if `try_new` is called from multiple threads (post-A2-003
        // any thread can hit a fresh path), the env-set executes EXACTLY
        // ONCE for the whole process. POSIX `setenv`/Windows
        // `SetEnvironmentVariable` are not thread-safe with concurrent
        // `getenv`; serialising through OnceCell + the existing
        // GLOBAL_BACKENDS mutex ensures `ort::init()` (called by
        // `Session::builder`) cannot race a writer.
        static ORT_DYLIB_INIT: OnceCell<()> = OnceCell::new();
        ORT_DYLIB_INIT.get_or_init(|| {
            if std::env::var_os("ORT_DYLIB_PATH").is_some() {
                return;
            }
            let Ok(exe) = std::env::current_exe() else {
                return;
            };
            let Some(dir) = exe.parent() else {
                return;
            };
            let candidate = dir.join("onnxruntime.dll");
            if !candidate.exists() {
                return;
            }
            // SAFETY: gated by `ORT_DYLIB_INIT.get_or_init` (runs at most
            // once) and by the outer `GLOBAL_BACKENDS` per-key Mutex
            // (held across this `Backend::load` call). No other code in
            // brain reads or writes ORT_DYLIB_PATH, and no thread can
            // observe `ort::init()` (which reads the env) until the
            // current thread releases the Mutex below.
            unsafe {
                std::env::set_var("ORT_DYLIB_PATH", &candidate);
            }
            tracing::info!(
                ort_dylib_path = %candidate.display(),
                "pinned ORT_DYLIB_PATH to bundled onnxruntime.dll"
            );
        });

        // Dynamic ORT loading. Honors ORT_DYLIB_PATH (set above on
        // Windows) if present, otherwise searches PATH / LD_LIBRARY_PATH
        // for `onnxruntime.{dll,so,dylib}`. We do NOT call `ort::init()`
        // ourselves — the crate's global init is lazy and picks up
        // ORT_DYLIB_PATH on first session build.

        let tokenizer = Tokenizer::from_file(tokenizer_path).map_err(|e| {
            BrainError::Tokenizer(format!("load {}: {e}", tokenizer_path.display()))
        })?;

        let session = Session::builder()
            .map_err(|e| BrainError::Onnx(format!("Session::builder: {e}")))?
            .commit_from_file(model_path)
            .map_err(|e| {
                BrainError::Onnx(format!("commit_from_file {}: {e}", model_path.display()))
            })?;

        Ok(Self { session, tokenizer })
    }

    fn embed_one(&mut self, text: &str) -> BrainResult<Vec<f32>> {
        let mut out = self.embed_batch(&[text])?;
        Ok(out.pop().unwrap_or_else(zero_vec))
    }

    fn embed_batch(&mut self, texts: &[&str]) -> BrainResult<Vec<Vec<f32>>> {
        use ndarray::{Array2, Axis};
        use ort::inputs;
        use ort::value::Value;

        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Tokenize. We truncate to MAX_TOKENS and pad to the longest in batch.
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| BrainError::Tokenizer(format!("encode_batch: {e}")))?;

        let batch = encodings.len();
        let max_len = encodings
            .iter()
            .map(|e| e.get_ids().len().min(MAX_TOKENS))
            .max()
            .unwrap_or(0)
            .max(1);

        let mut ids = Array2::<i64>::zeros((batch, max_len));
        let mut attn = Array2::<i64>::zeros((batch, max_len));
        let mut toks = Array2::<i64>::zeros((batch, max_len));

        for (i, enc) in encodings.iter().enumerate() {
            let src_ids = enc.get_ids();
            let src_attn = enc.get_attention_mask();
            let src_toks = enc.get_type_ids();
            let n = src_ids.len().min(max_len);
            for j in 0..n {
                ids[[i, j]] = src_ids[j] as i64;
                attn[[i, j]] = src_attn[j] as i64;
                toks[[i, j]] = src_toks[j] as i64;
            }
        }

        // BUG-A2-005 fix: avoid the triple-clone of full Array2<i64> arrays
        // (256 KB * 3 = 768 KB per call * 50k jobs = ~37 GB throwaway).
        // We only need `attn` post-tensor for the CLS-pool path (BUG-A2-008
        // uses position 0 only; we no longer scan the seq dim), so we can
        // pass all three arrays into ORT by-move — no clones at all.
        let ids_val = Value::from_array(ids)
            .map_err(|e| BrainError::Onnx(format!("Value::from_array ids: {e}")))?;
        let attn_val = Value::from_array(attn)
            .map_err(|e| BrainError::Onnx(format!("Value::from_array attn: {e}")))?;
        let toks_val = Value::from_array(toks)
            .map_err(|e| BrainError::Onnx(format!("Value::from_array toks: {e}")))?;

        let outputs = self
            .session
            .run(inputs![
                "input_ids" => ids_val,
                "attention_mask" => attn_val,
                "token_type_ids" => toks_val,
            ])
            .map_err(|e| BrainError::Onnx(format!("session.run: {e}")))?;

        // BGE-small output: "last_hidden_state" shape [batch, seq, 384].
        // Some exports use index 0; try name first, fall back to positional.
        let (_name, out_val) = outputs
            .iter()
            .next()
            .ok_or_else(|| BrainError::Onnx("model produced no outputs".into()))?;

        let (shape, data) = out_val
            .try_extract_tensor::<f32>()
            .map_err(|e| BrainError::Onnx(format!("extract_tensor f32: {e}")))?;

        if shape.len() != 3 || shape[2] as usize != EMBEDDING_DIM {
            return Err(BrainError::Onnx(format!(
                "unexpected output shape {:?}, want [batch, seq, {}]",
                shape, EMBEDDING_DIM
            )));
        }
        let seq = shape[1] as usize;

        // BUG-A2-008 fix: BGE-Small-En-v1.5 was trained for CLS pooling.
        // Take the first-token (`[CLS]`) embedding per row instead of the
        // masked mean over all tokens. Higher retrieval quality on the
        // BGE benchmark and matches the reference implementation.
        let mut results: Vec<Vec<f32>> = Vec::with_capacity(batch);
        for b in 0..batch {
            let base = b * seq * EMBEDDING_DIM;
            let mut pooled = vec![0f32; EMBEDDING_DIM];
            for d in 0..EMBEDDING_DIM {
                pooled[d] = data[base + d];
            }
            l2_normalise(&mut pooled);
            results.push(pooled);
        }

        // ndarray `Axis` unused marker to keep import list stable if model
        // export changes later. (Not load-bearing.)
        let _ = Axis(0);

        Ok(results)
    }
}

#[cfg(not(feature = "real-embeddings"))]
struct RealBackend;

#[cfg(not(feature = "real-embeddings"))]
impl RealBackend {
    #[allow(dead_code)]
    fn try_new(_model_path: &Path, _tokenizer_path: &Path) -> BrainResult<Self> {
        Err(BrainError::Embedding(
            "`real-embeddings` feature disabled at compile time".into(),
        ))
    }

    #[allow(dead_code)]
    fn embed_one(&mut self, _text: &str) -> BrainResult<Vec<f32>> {
        Ok(zero_vec())
    }

    #[allow(dead_code)]
    fn embed_batch(&mut self, texts: &[&str]) -> BrainResult<Vec<Vec<f32>>> {
        Ok(vec![zero_vec(); texts.len()])
    }
}

// ---------------------------------------------------------------------------
// Fallback backend (pure-Rust hashing trick)
// ---------------------------------------------------------------------------

/// Pure-Rust embedder. Uses the hashing trick: tokens are hashed into
/// `EMBEDDING_DIM` buckets with signed counts, then L2-normalised. This
/// preserves the property that *similar bags of tokens produce similar
/// vectors* without requiring an ONNX runtime or any native DLL.
///
/// Quality is lower than BGE-small on abstract paraphrase benchmarks but
/// is excellent on code (where exact-token overlap dominates similarity)
/// and is fully deterministic, online-free, and platform-portable.
struct FallbackBackend {
    tokenizer: Option<Tokenizer>,
}

impl FallbackBackend {
    fn try_load(tokenizer: &Path) -> Self {
        // Tokenizer is optional; if present we use it for better word
        // segmentation, otherwise fall back to whitespace tokenising.
        let tk = if tokenizer.exists() {
            Tokenizer::from_file(tokenizer).ok()
        } else {
            None
        };
        debug!(has_tokenizer = tk.is_some(), "fallback embedder ready");
        Self { tokenizer: tk }
    }

    fn embed_one(&self, text: &str) -> Vec<f32> {
        hashing_embed(text, self.tokenizer.as_ref())
    }

    fn embed_batch(&self, texts: &[&str]) -> Vec<Vec<f32>> {
        texts
            .iter()
            .map(|t| hashing_embed(t, self.tokenizer.as_ref()))
            .collect()
    }
}

/// FNV-1a hash of a token string. Stable, fast, well-distributed across buckets.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Embed one string into `EMBEDDING_DIM` floats via hashing trick.
fn hashing_embed(text: &str, tk: Option<&Tokenizer>) -> Vec<f32> {
    let mut out = vec![0f32; EMBEDDING_DIM];
    let lower = text.to_lowercase();

    let tokens: Vec<String> = match tk {
        Some(t) => {
            let enc = t.encode(lower.as_str(), false);
            match enc {
                Ok(e) => e.get_tokens().to_vec(),
                Err(_) => whitespace_tokens(&lower),
            }
        }
        None => whitespace_tokens(&lower),
    };

    let mut n_tokens: u32 = 0;
    for tok in &tokens {
        if tok.len() < 2 {
            continue;
        }
        n_tokens += 1;
        let h = fnv1a(tok.as_bytes());
        let bucket = (h as usize) % EMBEDDING_DIM;
        let sign = if h & 1 == 0 { 1.0 } else { -1.0 };
        out[bucket] += sign;
    }
    // Character trigrams add shape-of-word info; helps for rare tokens.
    let bytes = lower.as_bytes();
    for pair in bytes.windows(3) {
        let h = fnv1a(pair);
        let bucket = (h as usize) % EMBEDDING_DIM;
        let sign = if h & 1 == 0 { 1.0 } else { -1.0 };
        out[bucket] += sign * 0.5;
    }

    if n_tokens > MAX_TOKENS as u32 && n_tokens > 0 {
        let scale = (MAX_TOKENS as f32) / (n_tokens as f32);
        for v in &mut out {
            *v *= scale;
        }
    }

    l2_normalise(&mut out);
    out
}

fn whitespace_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn zero_vec() -> Vec<f32> {
    vec![0f32; EMBEDDING_DIM]
}

fn hash_key(text: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    let out = h.finalize();
    let mut k = [0u8; 32];
    k.copy_from_slice(&out);
    k
}

fn l2_normalise(v: &mut [f32]) {
    if v.is_empty() {
        return;
    }
    let sq: f32 = v.iter().map(|x| x * x).sum();
    let norm = sq.sqrt();
    if norm > 1e-12 {
        for x in v {
            *x /= norm;
        }
    } else {
        // BUG-A2-006 fix: the cosine similarity in `EmbedStore::nearest`
        // assumes unit-length vectors. When the pooled vector has near-zero
        // magnitude (rare, but possible for very short or sparse inputs)
        // we substitute a deterministic unit vector so downstream math
        // keeps its invariant.
        for x in v.iter_mut() {
            *x = 0.0;
        }
        v[0] = 1.0;
    }
}

/// Default model directory: `<mneme-root>/models/`.
///
/// HOME-bypass-brain (embeddings) fix: the previous duplicate
/// `MNEME_HOME` check + `dirs::home_dir()` fallback is now centralized
/// in `PathManager::default_root()`. Resolution order is the canonical
/// `MNEME_HOME` -> `~/.mneme` -> OS-default chain implemented in
/// `mneme_common::PathManager`.
pub fn default_model_dir() -> PathBuf {
    common::paths::PathManager::default_root()
        .root()
        .join("models")
}

/// Explicit model-install entry point, used by `mneme models install`.
///
/// As of v0.2 this function only *validates* that files exist on the
/// machine — it does **not** download anything. The CLI handler copies
/// user-supplied local files into `default_model_dir()`. Network downloads,
/// if ever added, must be behind an explicit `--from-url` flag with user
/// opt-in.
pub fn install_default_model() -> BrainResult<()> {
    let dir = default_model_dir();
    // Bug G-5 (2026-05-01): propagate dir-create failures. Previously
    // `.ok()` swallowed permission/disk-full errors, then the existence
    // check below failed misleadingly with "model files not present"
    // when the real cause was that the model dir couldn't be created.
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(BrainError::ModelMissing {
            path: format!(
                "could not create model dir {}: {} — check permissions / disk space",
                dir.display(),
                e
            ),
        });
    }

    let onnx = dir.join("bge-small-en-v1.5.onnx");
    let tok = dir.join("tokenizer.json");

    if !onnx.exists() || !tok.exists() {
        return Err(BrainError::ModelMissing {
            path: format!(
                "{} or {} not present — drop the BGE-small-en-v1.5 files in {} \
                 or use `mneme models install --from-path <dir>`",
                onnx.display(),
                tok.display(),
                dir.display()
            ),
        });
    }
    let marker = dir.join(".installed");
    // Bug G-5 (2026-05-01): propagate marker-write failures. If the
    // marker write silently failed, `mneme doctor` would later report
    // "models not installed" while the install reported success — two
    // contradictory signals to the user. Now we surface the failure.
    if let Err(e) = std::fs::write(&marker, b"v0.2 bge-small-en-v1.5 validated\n") {
        return Err(BrainError::ModelMissing {
            path: format!(
                "model files present but installed-marker write to {} failed: {} — doctor will report 'not installed'",
                marker.display(),
                e
            ),
        });
    }
    Ok(())
}

/// Cosine similarity between two same-length f32 vectors.
///
/// Returns `0.0` when either vector has zero magnitude. Output is in
/// `[-1.0, 1.0]` for nonzero inputs. Exposed publicly because the
/// retrieval and benchmark layers share this math.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom > 1e-12 {
        dot / denom
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    /// Locate the installed BGE model, if any. Returns `None` when the
    /// user hasn't dropped the files in `~/.mneme/models/`.
    fn find_installed_model() -> Option<(PathBuf, PathBuf)> {
        let dir = default_model_dir();
        let onnx = dir.join("bge-small-en-v1.5.onnx");
        let tok = dir.join("tokenizer.json");
        if onnx.exists() && tok.exists() {
            Some((onnx, tok))
        } else {
            None
        }
    }

    #[test]
    fn hashing_trick_is_deterministic_and_normalised() {
        let bogus = PathBuf::from("/definitely/nonexistent/model.onnx");
        let toks = PathBuf::from("/definitely/nonexistent/tokenizer.json");
        let e = Embedder::new(&bogus, &toks).expect("build embedder in fallback");
        let v = e.embed("the quick brown fox").unwrap();
        assert_eq!(v.len(), EMBEDDING_DIM);
        // L2 norm == 1 (within float slop).
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (n - 1.0).abs() < 1e-4,
            "fallback vector not L2-normalised: {n}"
        );
    }

    #[test]
    fn cosine_similarity_basic_properties() {
        let a: Vec<f32> = (0..EMBEDDING_DIM).map(|i| (i as f32) * 0.01).collect();
        let mut b = a.clone();
        // Identical vectors → cosine == 1.
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-4);
        // Zero vector → 0.
        b.iter_mut().for_each(|x| *x = 0.0);
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    /// Verifies the real BGE backend produces *semantic* similarity — two
    /// paraphrases should score > 0.7 cosine while the hashing trick would
    /// score near-zero on this pair (no token overlap).
    ///
    /// Skipped with a log line when the model isn't installed so CI and
    /// fresh checkouts don't fail. Enable by running
    /// `mneme models install --from-path <dir>` first.
    #[cfg(feature = "real-embeddings")]
    #[test]
    fn bge_semantic_similarity_exceeds_hashing_floor() {
        let Some((model, tok)) = find_installed_model() else {
            eprintln!(
                "bge_semantic_similarity_exceeds_hashing_floor: \
                 model not installed, skipping"
            );
            return;
        };
        let e = Embedder::new(&model, &tok).expect("build embedder");
        if !e.is_ready() {
            eprintln!(
                "bge_semantic_similarity_exceeds_hashing_floor: \
                 real backend failed to init (likely ORT DLL missing), skipping"
            );
            return;
        }
        let v1 = e.embed("blast radius").unwrap();
        let v2 = e.embed("explosion radius").unwrap();
        let cos = cosine_similarity(&v1, &v2);
        assert!(
            cos > 0.7,
            "expected cos('blast radius','explosion radius') > 0.7 under BGE, got {cos}"
        );
    }

    /// Throughput benchmark — embeds N fixed strings and reports
    /// strings-per-second. The hashing-trick baseline is effectively
    /// memory-bandwidth-bound; the real backend is ONNX-inference-bound.
    ///
    /// The test is a smoke-level check that throughput is at least non-zero
    /// in fallback mode. The printed numbers are useful for manual
    /// comparison against the BGE path.
    #[test]
    fn embed_throughput_smoke() {
        let bogus = PathBuf::from("/definitely/nonexistent/model.onnx");
        let toks = PathBuf::from("/definitely/nonexistent/tokenizer.json");
        let e = Embedder::new(&bogus, &toks).expect("build embedder in fallback");

        let corpus: Vec<&str> = vec![
            "fn compute_blast_radius(node: NodeId) -> BlastReport",
            "pub fn embed(text: &str) -> Vec<f32>",
            "SELECT * FROM edges WHERE src = ?",
            "async fn dispatch_job(req: Request) -> Response",
            "let store = EmbedStore::open(dir)?;",
            "impl Iterator for Graph { type Item = NodeId; }",
            "/// returns nearest K neighbours by cosine",
            "error[E0277]: trait bound not satisfied",
            "use std::collections::HashMap;",
            "return Err(BrainError::ModelMissing { path });",
        ];
        let n_iters = 500usize;
        e.clear_cache();

        let t0 = Instant::now();
        for _ in 0..n_iters {
            for s in &corpus {
                let _ = e.embed(s).unwrap();
            }
        }
        let elapsed = t0.elapsed();
        let total = (n_iters * corpus.len()) as f64;
        let rate = total / elapsed.as_secs_f64();
        eprintln!(
            "hashing-trick throughput ({}): {:.0} strings/s over {} texts in {:?} (cache hit)",
            e.backend_name(),
            rate,
            total as usize,
            elapsed
        );
        assert!(rate > 0.0, "throughput was zero — embedder hung?");
    }

    /// Measure *cold* throughput (first embed, no cache), separately for
    /// hashing trick and BGE. When BGE isn't installed, only the fallback
    /// number is printed.
    #[test]
    fn embed_throughput_cold_uncached() {
        let bogus = PathBuf::from("/definitely/nonexistent/model.onnx");
        let toks = PathBuf::from("/definitely/nonexistent/tokenizer.json");
        let e = Embedder::new(&bogus, &toks).expect("build embedder in fallback");

        // Generate unique strings so the cache is bypassed on every iter.
        let n = 500usize;
        let texts: Vec<String> = (0..n)
            .map(|i| format!("code snippet number {i} end"))
            .collect();
        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();

        let t0 = Instant::now();
        let _ = e.embed_batch(&refs).unwrap();
        let elapsed = t0.elapsed();
        let rate = (n as f64) / elapsed.as_secs_f64();
        eprintln!(
            "COLD throughput ({}): {:.0} strings/s ({} in {:?})",
            e.backend_name(),
            rate,
            n,
            elapsed
        );
        assert!(
            rate > 100.0,
            "hashing-trick should do > 100/s, got {rate:.0}"
        );
    }
}
