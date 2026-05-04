//! ParserPool — one `tree_sitter::Parser` per (language, worker) slot.
//!
//! Per §21.3.2 anti-patterns: parsers are NEVER shared across threads. The
//! pool hands out exclusive leases via async [`tokio::sync::Mutex`]. Callers
//! drop the [`ParserLease`] to return the slot.
//!
//! The pool sizes itself from `cpu_count * 4` (§21.3) but accepts an
//! explicit override for tests.

use crate::error::ParserError;
use crate::language::Language;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore};
use tracing::trace;

/// A single borrowed parser. While held, no one else can use this slot.
///
/// Drop = return.
pub struct ParserLease {
    /// Owning guard; dropping it unlocks the slot.
    guard: OwnedMutexGuard<tree_sitter::Parser>,
    language: Language,
    slot: usize,
    _semaphore_permit: tokio::sync::OwnedSemaphorePermit,
}

impl ParserLease {
    /// Mutable access to the underlying parser.
    pub fn parser(&mut self) -> &mut tree_sitter::Parser {
        &mut self.guard
    }

    /// Which language this lease parses.
    pub fn language(&self) -> Language {
        self.language
    }

    /// Which slot inside the pool — surfaced for tracing only.
    pub fn slot(&self) -> usize {
        self.slot
    }
}

impl std::fmt::Debug for ParserLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParserLease")
            .field("language", &self.language)
            .field("slot", &self.slot)
            .finish()
    }
}

/// Per-language array of parsers.
struct LanguageSlots {
    /// `slots[i]` is one parser instance — exclusive via Mutex.
    slots: Vec<Arc<Mutex<tree_sitter::Parser>>>,
    /// Permits = slots.len(); makes acquire() block instead of spinning.
    permits: Arc<Semaphore>,
}

/// Pool of parsers across many languages.
///
/// Construction is fallible because grammar registration can fail (ABI
/// mismatch). Languages whose feature is disabled are silently skipped —
/// `acquire` for them returns [`ParserError::NoParserForLanguage`].
pub struct ParserPool {
    inner: HashMap<Language, LanguageSlots>,
    /// Captured at construction so callers can audit the pool size.
    workers_per_language: usize,
}

impl std::fmt::Debug for ParserPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParserPool")
            .field("languages", &self.inner.keys().collect::<Vec<_>>())
            .field("workers_per_language", &self.workers_per_language)
            .finish()
    }
}

impl ParserPool {
    /// Recommended default: `cpu_count * 4` parsers per enabled language.
    /// See design §21.3 ("3-8 parsers per CPU core; mneme default 4x").
    pub fn with_default_size() -> Result<Self, ParserError> {
        // Workers PER LANGUAGE per PROCESS. The supervisor runs multiple
        // parser-worker processes in parallel, so intra-process parallelism
        // can stay small. 4 is plenty; num_cpus*4 was ~96 which triggered
        // OOM on low-memory machines (18 langs × 96 = 1728 parsers/proc).
        let workers = 4;
        Self::new(workers)
    }

    /// Build a pool with `workers_per_language` parsers per enabled grammar.
    pub fn new(workers_per_language: usize) -> Result<Self, ParserError> {
        assert!(workers_per_language > 0, "workers_per_language must be > 0");
        let mut inner = HashMap::with_capacity(Language::ALL.len());

        for lang in Language::ALL {
            // Skip disabled languages quietly — capabilities are advertised
            // separately by `Language::is_enabled`.
            let ts_lang = match lang.tree_sitter_language() {
                Ok(l) => l,
                Err(ParserError::LanguageNotEnabled(_)) => continue,
                Err(e) => return Err(e),
            };

            let mut slots = Vec::with_capacity(workers_per_language);
            let mut abi_failed = false;
            for _ in 0..workers_per_language {
                let mut p = tree_sitter::Parser::new();
                if let Err(e) = p.set_language(&ts_lang) {
                    tracing::warn!(
                        language = lang.as_str(),
                        error = %e,
                        "grammar ABI mismatch — language disabled at runtime; other grammars continue"
                    );
                    abi_failed = true;
                    break;
                }
                slots.push(Arc::new(Mutex::new(p)));
            }
            if abi_failed {
                continue;
            }

            inner.insert(
                *lang,
                LanguageSlots {
                    slots,
                    permits: Arc::new(Semaphore::new(workers_per_language)),
                },
            );
        }

        trace!(
            languages = inner.len(),
            workers_per_language,
            "ParserPool initialised"
        );
        Ok(Self {
            inner,
            workers_per_language,
        })
    }

    /// Async acquisition of a free parser for `lang`.
    ///
    /// Blocks until a slot is available — exactly as Tokio's MPSC blocks on
    /// a full channel. Cancellation safe: dropping the future before it
    /// resolves releases the permit before any slot is locked.
    pub async fn acquire(&self, lang: Language) -> Result<ParserLease, ParserError> {
        let slots = self
            .inner
            .get(&lang)
            .ok_or_else(|| ParserError::NoParserForLanguage(lang.as_str().to_string()))?;

        // Wait for an available permit first (cheap, async, fair-ish).
        let permit = slots
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ParserError::Internal("pool semaphore closed".into()))?;

        // Then grab whichever slot is currently free. Because we hold a
        // permit there's at least one Mutex that is unlocked, but several
        // could race; try them in declaration order with try_lock_owned()
        // and fall back to a real owned lock on the first slot if none are
        // free yet (rare — only under heavy contention with many languages).
        for (idx, slot) in slots.slots.iter().enumerate() {
            if let Ok(guard) = slot.clone().try_lock_owned() {
                return Ok(ParserLease {
                    guard,
                    language: lang,
                    slot: idx,
                    _semaphore_permit: permit,
                });
            }
        }
        // A3-021 (2026-05-04): rotate the fallback slot index per call.
        // The original implementation always waited on slot[0], which
        // under heavy contention could livelock: thread A waits on
        // slot[0]; meanwhile slot[1] briefly opens, then briefly closes;
        // thread B also waits on slot[0]; ad infinitum -- slot[1] never
        // gets picked up via the fallback path. The semaphore permit
        // guarantees AT LEAST one slot is free; rotating the index
        // ensures that "free slot" is actually selected over time.
        static FALLBACK_RR: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let n = slots.slots.len().max(1);
        let start_idx =
            FALLBACK_RR.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % n;
        let guard = slots.slots[start_idx].clone().lock_owned().await;
        Ok(ParserLease {
            guard,
            language: lang,
            slot: start_idx,
            _semaphore_permit: permit,
        })
    }

    /// Number of parsers per language — surfaced for `/health` reporting.
    pub fn workers_per_language(&self) -> usize {
        self.workers_per_language
    }

    /// Languages backed by at least one parser (i.e. enabled in this build).
    pub fn enabled_languages(&self) -> Vec<Language> {
        let mut v: Vec<_> = self.inner.keys().copied().collect();
        v.sort_by_key(|l| l.as_str());
        v
    }
}
