/**
 * Transcript watcher — F1 hook-side distillation.
 *
 * Subscribes (via the Stop hook — see `hooks/turn_end.ts`) to Claude Code
 * turn events. For every (user_msg, assistant_msg) pair, runs a
 * deterministic distiller that extracts:
 *
 *   - `summary`          one-sentence anchor for recall (first sentence of
 *                        the assistant reply).
 *   - `kind`             Decision / Implementation / Bug / OpenQuestion /
 *                        Refactor / Experiment, chosen by regex cues.
 *   - `rationale`        "because ..." / "so that ..." follow-on.
 *   - `touched_files`    unique absolute paths mentioned in either side.
 *   - `touched_concepts` left empty in v0.2; concept graph hookup happens
 *                        post-v0.2.
 *
 * Every distilled entry is written to the ledger via the Rust supervisor
 * (`ledger.append`) so it survives compaction, restarts, and reboots.
 *
 * Real LLM-backed distillation is a post-v0.2 upgrade; this deterministic
 * extractor is good enough to populate the ledger from day one.
 */

import { randomUUID } from "node:crypto";
import type { LedgerKind } from "./types.ts";
import { query as dbQuery } from "./db.ts";

// ---------------------------------------------------------------------------
// Public shapes
// ---------------------------------------------------------------------------

export interface TurnPair {
  sessionId: string;
  turnIndex?: number;
  userMsg: string;
  assistantMsg: string;
  /** Optional message id Claude Code provides. */
  messageId?: string;
}

export interface DistilledEntry {
  id: string;
  session_id: string;
  timestamp: string;
  kind: LedgerKind;
  summary: string;
  rationale: string | null;
  touched_files: string[];
  touched_concepts: string[];
  transcript_ref: {
    session_id: string;
    turn_index: number | null;
    message_id: string | null;
  };
  kind_payload: Record<string, unknown>;
}

// ---------------------------------------------------------------------------
// Deterministic distiller
// ---------------------------------------------------------------------------

const FIRST_SENTENCE = /^[^.!?\n]{4,}?[.!?]/;
const DECISION_CUES = [
  /\b(?:decid(?:ed|e) to|chose|picked|went with|opting for|selected)\s+([^.\n]{3,80})/i,
  /\b(?:we)\s+over\s+([^.\n]{3,80})/i,
];
const REJECTION_CUE = /\b(?:over|instead of|rather than|not)\s+([^.\n]{3,80})/i;
const BUG_CUE = /\b(?:bug|broken|crash|error|fails?|regression|symptom)\b/i;
const ROOT_CAUSE_CUE = /\b(?:root cause|caused by|because)\s+([^.\n]{3,160})/i;
const REFACTOR_CUE = /\b(?:refactor(?:ed|ing)?|rewrote|extracted|renamed|moved)\b/i;
const EXPERIMENT_CUE = /\b(?:tried|experiment(?:ed|ing)?|prototyped|spiked)\b/i;
const OPEN_QUESTION_CUE = /\?\s*$/;
const RATIONALE_CUE =
  /\b(?:because|so that|to ensure|in order to|rationale[:\s])\s+([^.\n]{3,160})/i;

/** Windows absolute paths and POSIX paths. */
const FILE_PATH_REGEX =
  /(?:[a-zA-Z]:\\(?:[\w.\-]+\\)*[\w.\-]+|\/(?:[\w.\-]+\/)*[\w.\-]+)/g;

export function distillPair(pair: TurnPair): DistilledEntry {
  const firstSentence =
    FIRST_SENTENCE.exec(pair.assistantMsg)?.[0]?.trim() ??
    pair.assistantMsg.slice(0, 160).trim();

  const { kind, payload } = classifyKind(pair.assistantMsg, pair.userMsg);
  const rationale = RATIONALE_CUE.exec(pair.assistantMsg)?.[1]?.trim() ?? null;
  const touched_files = collectPaths(`${pair.userMsg}\n${pair.assistantMsg}`);

  return {
    id: randomUUID().replace(/-/g, ""),
    session_id: pair.sessionId,
    timestamp: new Date().toISOString(),
    kind,
    summary: firstSentence || "(no summary)",
    rationale,
    touched_files,
    touched_concepts: [],
    transcript_ref: {
      session_id: pair.sessionId,
      turn_index: pair.turnIndex ?? null,
      message_id: pair.messageId ?? null,
    },
    kind_payload: payload,
  };
}

function classifyKind(
  assistantMsg: string,
  userMsg: string,
): { kind: LedgerKind; payload: Record<string, unknown> } {
  if (OPEN_QUESTION_CUE.test(assistantMsg.trim())) {
    const questionText = assistantMsg.trim().slice(-200);
    return {
      kind: "open_question",
      payload: { text: questionText, resolved_by: null },
    };
  }

  for (const re of DECISION_CUES) {
    const m = re.exec(assistantMsg);
    if (m && m[1]) {
      const rejected: string[] = [];
      const rej = REJECTION_CUE.exec(assistantMsg);
      if (rej && rej[1]) rejected.push(rej[1].trim());
      return {
        kind: "decision",
        payload: { chosen: m[1].trim(), rejected },
      };
    }
  }

  if (BUG_CUE.test(userMsg) || BUG_CUE.test(assistantMsg)) {
    const root = ROOT_CAUSE_CUE.exec(assistantMsg)?.[1]?.trim() ?? null;
    return {
      kind: "bug",
      payload: {
        symptom: userMsg.slice(0, 200).trim(),
        root_cause: root,
      },
    };
  }

  if (REFACTOR_CUE.test(assistantMsg)) {
    return {
      kind: "refactor",
      payload: { before: "(unknown)", after: assistantMsg.slice(0, 200) },
    };
  }

  if (EXPERIMENT_CUE.test(assistantMsg)) {
    return {
      kind: "experiment",
      payload: { outcome: assistantMsg.slice(0, 200) },
    };
  }

  return { kind: "impl", payload: {} };
}

function collectPaths(text: string): string[] {
  const out = new Set<string>();
  const matches = text.match(FILE_PATH_REGEX);
  if (!matches) return [];
  for (const m of matches) {
    if (m.length < 3) continue;
    out.add(m);
  }
  return Array.from(out);
}

// ---------------------------------------------------------------------------
// Ledger append
// ---------------------------------------------------------------------------

/**
 * Persist a distilled entry to the ledger via the Rust supervisor. Never
 * throws — ledger append is best-effort so a transient supervisor
 * hiccup can't break the Stop hook.
 */
export async function persistDistilled(entry: DistilledEntry): Promise<void> {
  try {
    await dbQuery.raw("ledger.append", entry);
  } catch (err) {
    console.error("[mneme-mcp] ledger.append failed:", err);
  }
}

/**
 * Convenience orchestrator: distill + persist. Returns the distilled entry
 * (even if persistence failed) so callers can log it locally.
 *
 * A5-013 (2026-05-04): persistence is fire-and-forget. The prior `await`
 * defeated the function comment's stated "best-effort so a transient
 * supervisor hiccup can't break the Stop hook" guarantee — it serialised
 * the Stop hook on a 200ms IPC round-trip per turn. `void` the persist
 * promise here so the caller returns immediately with the in-memory
 * distilled entry; `persistDistilled` already swallows its own errors.
 */
export function processTurn(pair: TurnPair): DistilledEntry {
  const entry = distillPair(pair);
  void persistDistilled(entry);
  return entry;
}
