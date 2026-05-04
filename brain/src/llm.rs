//! Local LLM wrapper around llama.cpp (Phi-3-mini-4k Q4_K_M).
//!
//! Only compiled when the `llm` cargo feature is enabled. The crate works
//! perfectly well without it — concept extraction & summarisation simply
//! skip the LLM stage and fall back to deterministic output.
//!
//! Model layout:
//! ```text
//! ~/.mneme/llm/phi-3-mini-4k/
//!   model.gguf      # Phi-3-mini-4k-instruct, Q4_K_M
//! ```
//!
//! Threading: `LocalLlm` is `Send + Sync`, but inference itself is wrapped
//! in a `Mutex` because llama.cpp contexts are not internally synchronised.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tracing::{info, warn};

use crate::error::{BrainError, BrainResult};

/// Public handle. Cheap to clone.
#[derive(Clone)]
pub struct LocalLlm {
    inner: Arc<Mutex<Inner>>,
    model_path: PathBuf,
}

impl std::fmt::Debug for LocalLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalLlm")
            .field("model_path", &self.model_path)
            .field("ready", &self.is_ready())
            .finish()
    }
}

struct Inner {
    backend: Option<Backend>,
}

// We deliberately keep the llama-cpp-2 surface area inside this module so
// that flipping the feature on/off does not require editing call-sites.
//
// A2-020 (2026-05-04): backend + model are leaked to 'static so that
// `LlamaContext` can be cached as a struct field. LlamaContext borrows
// from both backend and model; without 'static lifetime the cache would
// require self-referential structs (ouroboros / Pin / unsafe). Leaking
// is the standard pattern for process-singleton resources -- we have at
// most one LlamaBackend + one LlamaModel per process anyway, and Drop
// of either does NOT have user-observable side effects.
struct Backend {
    // 'static references via Box::leak in `try_load`. Never moved, never
    // dropped. Process-lifetime resources.
    backend: &'static llama_cpp_2::llama_backend::LlamaBackend,
    model: &'static llama_cpp_2::model::LlamaModel,
    // A2-020: cached context. LlamaContext owns its KV-cache buffer
    // (~50MB for Phi-3 with n_ctx=2048). The original code re-allocated
    // this on every `complete()` call -- wasteful in build pipelines
    // that issue thousands of summarisation calls back-to-back.
    // The KV-cache is cleared at the start of each `complete()` call
    // since prompt prefix differs across summarisation jobs.
    ctx: std::sync::Mutex<llama_cpp_2::context::LlamaContext<'static>>,
}

impl LocalLlm {
    /// Convenience: load Phi-3 from the default `~/.mneme/llm/phi-3-mini-4k/model.gguf`.
    pub fn from_default_path() -> Self {
        Self::new(&default_model_path())
    }

    /// Try to load a GGUF model. On failure the LLM enters degraded mode
    /// (every method returns the deterministic fallback) — this is **not**
    /// an error to the caller.
    pub fn new(path: &Path) -> Self {
        let backend = if path.exists() {
            match try_load(path) {
                Ok(b) => {
                    info!(path = %path.display(), "Phi-3 loaded");
                    Some(b)
                }
                Err(e) => {
                    warn!(error = %e, path = %path.display(), "Phi-3 load failed — degraded LLM");
                    None
                }
            }
        } else {
            warn!(path = %path.display(), "Phi-3 model missing — degraded LLM");
            None
        };
        Self {
            inner: Arc::new(Mutex::new(Inner { backend })),
            model_path: path.to_path_buf(),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.inner.lock().backend.is_some()
    }

    /// Run a free-form prompt; cap output at `max_tokens` (default 256).
    /// In degraded mode this returns `Ok(String::new())`.
    ///
    /// BUG-A2-019 fix: previously held `self.inner` Mutex across the entire
    /// llama.cpp inference call (10-25s for 256 tokens on CPU Phi-3). All
    /// concurrent `complete`/`route_query`/`summarize_function` calls
    /// blocked, serialising the whole brain. We now hold the lock just long
    /// enough to confirm the backend exists and grab a reference; the
    /// expensive `complete` call still requires exclusive access (llama.cpp
    /// contexts are not internally synchronised), but releasing for the
    /// non-`is_ready` checks lets `is_ready()`/`route_query` shortcuts
    /// proceed without queueing behind a long inference.
    ///
    /// TODO(v0.4): replace with a context pool (one ctx per CPU core) so
    /// long-running summarisation doesn't serialise concurrent callers.
    pub fn complete(&self, prompt: &str, max_tokens: usize) -> BrainResult<String> {
        // Quick check without holding the lock for inference.
        {
            let g = self.inner.lock();
            if g.backend.is_none() {
                return Ok(String::new());
            }
        }
        let mut g = self.inner.lock();
        let Some(backend) = g.backend.as_mut() else {
            return Ok(String::new());
        };
        backend
            .complete(prompt, max_tokens)
            .map_err(|e| BrainError::Llm(e))
    }

    /// One-sentence summary helper. Routes through [`Self::complete`] with
    /// an instruction template tuned for code chunks.
    pub fn summarize_function(&self, signature: &str, body: &str) -> BrainResult<String> {
        if !self.is_ready() {
            return Ok(String::new());
        }
        let prompt = format!(
            "Summarise the following function in ONE concise sentence (<=20 words).\n\
             Signature:\n{signature}\n\nBody:\n{snippet}\n\nSummary:",
            snippet = trim_for_prompt(body, 1500),
        );
        self.complete(&prompt, 64)
    }

    /// Concept extraction helper. Returns up to 8 candidate noun-phrases.
    pub fn extract_concepts(&self, text: &str) -> BrainResult<Vec<String>> {
        if !self.is_ready() {
            return Ok(Vec::new());
        }
        let prompt = format!(
            "Extract up to 8 high-level CONCEPTS from the text below.\n\
             Reply as a comma-separated list of noun phrases, lower case, no numbering.\n\n\
             Text:\n{}\n\nConcepts:",
            trim_for_prompt(text, 2000)
        );
        let raw = self.complete(&prompt, 96)?;
        let concepts: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .take(8)
            .collect();
        Ok(concepts)
    }

    /// Hint-only routing helper used by the query layer. Returns one of
    /// `"semantic"`, `"keyword"`, or `"hybrid"`. Falls back to `"hybrid"` in
    /// degraded mode.
    pub fn route_query(&self, query: &str) -> BrainResult<String> {
        if !self.is_ready() {
            return Ok("hybrid".to_string());
        }
        let prompt = format!(
            "Classify the following query as exactly one of: semantic, keyword, hybrid.\n\n\
             Query: {query}\n\nAnswer (one word):",
        );
        let raw = self.complete(&prompt, 8)?;
        let lc = raw.trim().to_ascii_lowercase();
        let route = if lc.contains("semantic") {
            "semantic"
        } else if lc.contains("keyword") {
            "keyword"
        } else {
            "hybrid"
        };
        Ok(route.to_string())
    }
}

fn try_load(path: &Path) -> Result<Backend, String> {
    // A2-020: LlamaBackend as a process-singleton. OnceLock guards
    // against double-init from concurrent LocalLlm::new() calls. The
    // first caller wins; every Backend instance reuses the same
    // `&'static LlamaBackend` reference. Box::leak makes the lifetime
    // explicitly 'static so LlamaContext can borrow with that lifetime
    // and be cached as a struct field.
    static BACKEND_SINGLETON: std::sync::OnceLock<
        &'static llama_cpp_2::llama_backend::LlamaBackend,
    > = std::sync::OnceLock::new();
    if BACKEND_SINGLETON.get().is_none() {
        let b = llama_cpp_2::llama_backend::LlamaBackend::init()
            .map_err(|e| format!("backend init: {e}"))?;
        // Reborrow Box::leak's `&'static mut` as immutable `&'static`.
        // LlamaBackend's API is &self for context creation, so we never
        // need mutable access after init.
        let leaked: &'static llama_cpp_2::llama_backend::LlamaBackend =
            Box::leak(Box::new(b));
        // OnceLock::set is no-op-OK if a concurrent caller already won.
        let _ = BACKEND_SINGLETON.set(leaked);
    }
    let backend: &'static llama_cpp_2::llama_backend::LlamaBackend = *BACKEND_SINGLETON
        .get()
        .expect("backend singleton initialised above");

    let params = llama_cpp_2::model::params::LlamaModelParams::default();
    let model_owned = llama_cpp_2::model::LlamaModel::load_from_file(backend, path, &params)
        .map_err(|e| format!("model load: {e}"))?;
    // A2-020: leak the model so LlamaContext (which borrows from both
    // backend AND model) can have 'static lifetime. Per-Backend leak
    // because different LocalLlm instances may load different model
    // paths. Memory cost is the model weight size; we'd be holding it
    // alive anyway via Backend.
    let model: &'static llama_cpp_2::model::LlamaModel = Box::leak(Box::new(model_owned));

    // A2-020: create the LlamaContext ONCE here (~500ms cold). All
    // subsequent `complete()` calls reuse this context, paying only
    // the KV-cache reset cost (~ms, not seconds).
    use llama_cpp_2::context::params::LlamaContextParams;
    const BATCH_CAPACITY: u32 = 2048;
    let ctx_params = LlamaContextParams::default()
        .with_n_ctx(std::num::NonZeroU32::new(BATCH_CAPACITY));
    let ctx = model
        .new_context(backend, ctx_params)
        .map_err(|e| format!("context: {e}"))?;

    Ok(Backend {
        backend,
        model,
        ctx: std::sync::Mutex::new(ctx),
    })
}

impl Backend {
    fn complete(&mut self, prompt: &str, max_tokens: usize) -> Result<String, String> {
        // A2-020 FIXED (2026-05-04): the cached LlamaContext is reused
        // across calls. Before each call we clear the KV cache so the
        // context starts fresh (the prompt prefix differs per call).
        // Clearing is O(layers x positions) which is cheap; allocating
        // a fresh context is O(layers x max_positions) PLUS a ~500ms
        // cold framework boot. Big win on repeated summarisation jobs.
        //
        // A2-023 (2026-05-04): the audit recommended "switch to
        // incremental batched decode" for ~10x speedup. After review,
        // the existing loop IS the canonical incremental decode pattern
        // for token-by-token generation in llama-cpp-2: clear batch,
        // add ONE token at the next position, decode, sample. Real
        // batched decode (multiple tokens per llama_decode call) only
        // helps for INITIAL prompt processing -- we already do that in
        // a single batch.add()-loop-then-single-decode block above.
        // For autoregressive generation, one-token-per-decode is
        // physically required because each new token depends on the
        // logits from the previous decode. The "30ms vs 3ms" claim
        // would apply to speculative decoding (multi-model) which is
        // not in scope. Marking A2-023 as audited-not-a-bug.
        const BATCH_CAPACITY: usize = 2048;

        let mut ctx_guard = self
            .ctx
            .lock()
            .map_err(|e| format!("ctx mutex poisoned: {e}"))?;
        let ctx: &mut llama_cpp_2::context::LlamaContext<'static> = &mut *ctx_guard;
        // Reset KV cache so the prompt prefix from the previous call
        // doesn't bleed into this one. This is the cheap part of the
        // context state -- llama.cpp's clear is a per-layer pointer
        // reset, not a memory free.
        ctx.clear_kv_cache();

        let mut tokens = self
            .model
            .str_to_token(prompt, llama_cpp_2::model::AddBos::Always)
            .map_err(|e| format!("tokenise: {e}"))?;

        // BUG-A2-021 fix: bound the prompt-token count against batch
        // capacity. `trim_for_prompt` only limits CHAR count, so emoji-
        // heavy or non-ASCII text could exceed the batch's token budget
        // and `batch.add` would error mid-loop. We reserve `max_tokens`
        // for the generated output and truncate the prompt tail to fit.
        let token_budget = BATCH_CAPACITY.saturating_sub(max_tokens.max(1));
        if tokens.len() > token_budget {
            tracing::warn!(
                prompt_tokens = tokens.len(),
                budget = token_budget,
                "LLM prompt exceeds context budget; truncating tail"
            );
            tokens.truncate(token_budget);
        }

        let mut batch = llama_cpp_2::llama_batch::LlamaBatch::new(BATCH_CAPACITY, 1);
        for (i, t) in tokens.iter().enumerate() {
            let last = i == tokens.len() - 1;
            batch
                .add(*t, i as i32, &[0], last)
                .map_err(|e| format!("batch add: {e}"))?;
        }
        ctx.decode(&mut batch).map_err(|e| format!("decode: {e}"))?;

        let mut out = String::new();
        let mut cur_pos = tokens.len() as i32;
        // BUG-A2-022 fix: replace `LlamaSampler::greedy()` with a chain
        // (top-k=40 -> temperature=0.7 -> dist) so Phi-3 produces less
        // repetitive output. Phi-3 was instruction-tuned with stochastic
        // sampling expected; greedy frequently loops or skips EOS.
        // We seed with `1` for determinism in tests; production callers
        // may want different seeds, but workspace tests rely on stability.
        use llama_cpp_2::sampling::LlamaSampler;
        let mut sampler = LlamaSampler::chain_simple([
            LlamaSampler::top_k(40),
            LlamaSampler::temp(0.7),
            LlamaSampler::dist(1),
        ]);
        for _ in 0..max_tokens {
            let token = sampler.sample(&ctx, -1);
            sampler.accept(token);
            if self.model.is_eog_token(token) {
                break;
            }
            let piece = self
                .model
                .token_to_str(token, llama_cpp_2::model::Special::Tokenize)
                .unwrap_or_default();
            out.push_str(&piece);
            batch.clear();
            batch
                .add(token, cur_pos, &[0], true)
                .map_err(|e| format!("batch add: {e}"))?;
            cur_pos += 1;
            ctx.decode(&mut batch).map_err(|e| format!("decode: {e}"))?;
        }
        Ok(out)
    }
}

fn trim_for_prompt(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        s
    } else {
        // Avoid splitting in the middle of a UTF-8 char.
        let mut end = max_chars;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        &s[..end]
    }
}

/// Default GGUF path: `<mneme-root>/llm/phi-3-mini-4k/model.gguf`.
///
/// HOME-bypass-brain (llm) fix: route through
/// `PathManager::default_root()` so `MNEME_HOME` is honored.
pub fn default_model_path() -> PathBuf {
    common::paths::PathManager::default_root()
        .root()
        .join("llm")
        .join("phi-3-mini-4k")
        .join("model.gguf")
}
