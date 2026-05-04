/**
 * Hook: Stop (between turns) — design §6.5.
 *
 * Runs three things in parallel:
 *   1. Summarizer (existing).
 *   2. Drift-score update (existing).
 *   3. F1 — transcript distillation into the persistent Step Ledger.
 *
 * The transcript watcher is resilient: if the hook JSON doesn't carry the
 * raw messages (e.g. v0.2 supervisor shape), the call is a no-op.
 */

import { query as dbQuery, livebus } from "../db.ts";
import { errMsg } from "../errors.ts";
import type { HookOutput } from "../types.ts";
import { processTurn } from "../transcript_watcher.ts";
import {
  CompactionGuard,
  defaultInjectionSink,
  type ContextMeasurement,
} from "../compaction_guard.ts";

export interface TurnEndArgs {
  sessionId: string;
  /** Optional raw user message for F1 distillation. */
  userMsg?: string;
  /** Optional raw assistant message for F1 distillation. */
  assistantMsg?: string;
  /** Optional turn index (Claude Code provides a monotonic counter). */
  turnIndex?: number;
  /** Optional message id. */
  messageId?: string;
  /** F5: harness-supplied context measurement. */
  context?: ContextMeasurement;
  /** F5: true when the hook is firing right before compaction. */
  preCompact?: boolean;
}

// F5 — one guard per MCP process so threshold arming is shared across turns.
const compactionGuard = new CompactionGuard(defaultInjectionSink);

export async function runTurnEnd(args: TurnEndArgs): Promise<HookOutput> {
  const t0 = Date.now();
  try {
    // A5-013 (2026-05-04): `processTurn` is now synchronous (persistence is
    // fire-and-forget inside the function). Wrap in a try/catch + Promise so
    // the Promise.all aggregation below stays unchanged.
    const ledgerEntryPromise: Promise<ReturnType<typeof processTurn> | null> = (() => {
      if (!args.userMsg || !args.assistantMsg) return Promise.resolve(null);
      try {
        const entry = processTurn({
          sessionId: args.sessionId,
          userMsg: args.userMsg,
          assistantMsg: args.assistantMsg,
          turnIndex: args.turnIndex,
          messageId: args.messageId,
        });
        return Promise.resolve(entry);
      } catch (err) {
        console.error("[mneme-mcp] transcript distillation failed:", err);
        return Promise.resolve(null);
      }
    })();

    // F5 — snapshot-on-compaction. We fire before running the rest of the
    // hook so the resume bundle lands in the transcript ahead of any
    // summariser truncation.
    if (args.context && args.context.total > 0) {
      const measurement: ContextMeasurement = {
        ...args.context,
        sessionId: args.context.sessionId ?? args.sessionId,
      };
      if (args.preCompact) {
        await compactionGuard.forceSnapshot(measurement);
      } else {
        await compactionGuard.onContextMeasurement(measurement);
      }
    }

    const [summarizerResult, driftResult, ledgerEntry] = await Promise.all([
      dbQuery
        .raw<{ summary_id: string; tokens: number }>("summarizer.run", {
          session_id: args.sessionId,
        })
        .catch(() => null),
      dbQuery
        .raw<{ drift_score_delta: number; current_step_id: string | null }>(
          "drift.update_step_score",
          { session_id: args.sessionId },
        )
        .catch(() => null),
      ledgerEntryPromise,
    ]);

    void livebus.emit("turn.ended", {
      session_id: args.sessionId,
      summary_id: summarizerResult?.summary_id ?? null,
      drift_delta: driftResult?.drift_score_delta ?? 0,
      ledger_entry_id: ledgerEntry?.id ?? null,
      duration_ms: Date.now() - t0,
    });

    return {
      metadata: {
        hook: "Stop",
        duration_ms: Date.now() - t0,
        drift_delta: driftResult?.drift_score_delta ?? 0,
        current_step_id: driftResult?.current_step_id ?? null,
        ledger_entry_id: ledgerEntry?.id ?? null,
      },
    };
  } catch (err) {
    console.error("[mneme-mcp] turn_end failed:", err);
    return { metadata: { hook: "Stop", error: errMsg(err) } };
  }
}
