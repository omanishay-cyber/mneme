/**
 * F5 — Compaction-Aware Memory.
 *
 * When the host harness approaches its context budget (by default at 80 %
 * utilisation), this guard builds a compact `<mneme-resume>` snapshot and
 * asks the MCP layer to inject it as a system message. The snapshot
 * prefers the Step Ledger (F1) when populated — that's the most
 * compaction-resilient signal we have — and falls back to compressing the
 * last N conversation turns when the ledger is empty or the IPC layer is
 * unreachable.
 *
 * The guard is deliberately idempotent: once a snapshot has been injected
 * for a given usage tier it will not re-fire until usage drops back below
 * the threshold. That keeps the injection from looping forever when the
 * harness lingers near the ceiling.
 *
 * Plumbing: the host harness (Claude Code, for example) calls
 * `onContextMeasurement` from its own pre-compact hook or polling loop.
 * Mneme's Stop hook (`mcp/src/hooks/turn_end.ts`) calls it too, so even
 * harnesses without an explicit pre-compact signal get a snapshot on the
 * turn immediately before a forced compaction.
 */

import { query as dbQuery, livebus } from "./db.ts";
import { errMsg } from "./errors.ts";
import type { ConversationTurn, Step } from "./types.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface ContextMeasurement {
  used: number;
  total: number;
  messages?: Array<{
    role: string;
    content: string;
    timestamp?: string;
  }>;
  sessionId?: string;
}

export interface InjectionSink {
  /**
   * Inject content into the MCP-attached conversation as a system message.
   * In Claude Code this wires up to the Stop / PreCompact hook envelope;
   * other harnesses can plug in whatever is equivalent.
   */
  inject(msg: { kind: "system"; content: string }): Promise<void> | void;
}

// ---------------------------------------------------------------------------
// Guard
// ---------------------------------------------------------------------------

export class CompactionGuard {
  /** Fraction of context usage at which a snapshot fires. */
  public threshold: number = 0.8;

  /** Max step-ledger entries included in the snapshot. */
  public maxLedgerEntries: number = 50;

  /** Max recent messages compacted when the ledger is empty. */
  public maxRecentMessages: number = 20;

  /** Tracks whether we've already fired for the current high-usage episode. */
  private armed = true;

  constructor(private readonly sink: InjectionSink) {}

  /**
   * Call every time the host reports a new context measurement.
   *
   * - Fires at-most-once per "episode" of exceeding the threshold
   *   (re-arms when usage drops back below it).
   * - Never throws: IPC failures degrade to the message-compaction path.
   */
  async onContextMeasurement(m: ContextMeasurement): Promise<void> {
    if (m.total <= 0) return;
    const ratio = m.used / m.total;

    if (ratio < this.threshold) {
      this.armed = true;
      return;
    }
    if (!this.armed) return;
    this.armed = false;

    try {
      const snapshot = await this.buildSnapshot(m);
      await this.sink.inject({
        kind: "system",
        content: `<mneme-resume>\n${snapshot}\n</mneme-resume>`,
      });
    } catch (err) {
      // Swallow — the guard must never take the harness down.
      console.error("[mneme-mcp] CompactionGuard failed:", err);
      // A5-012 (2026-05-04): the snapshot was NOT delivered. Re-arm so the
      // next over-threshold measurement gets another chance instead of
      // sitting silent until usage drops back below the threshold (by
      // which time the harness has already compacted and lost context).
      // Also emit an observability event so scrapers / vision can surface
      // the failure to the user.
      this.armed = true;
      void livebus.emit("compaction.inject_failed", {
        error: errMsg(err),
        ratio,
        sessionId: m.sessionId ?? null,
      });
    }
  }

  /** Force-fire regardless of ratio. Used by the pre-compact hook. */
  async forceSnapshot(m: ContextMeasurement): Promise<void> {
    try {
      const snapshot = await this.buildSnapshot(m);
      await this.sink.inject({
        kind: "system",
        content: `<mneme-resume>\n${snapshot}\n</mneme-resume>`,
      });
    } catch (err) {
      console.error("[mneme-mcp] CompactionGuard.force failed:", err);
    }
  }

  // -------------------------------------------------------------------------
  // Snapshot construction
  // -------------------------------------------------------------------------

  async buildSnapshot(m: ContextMeasurement): Promise<string> {
    // 1. Prefer the Step Ledger. This is the compaction-resilient,
    //    persistently-stored record of what we were doing.
    const ledger = await this.loadRecentLedger(m.sessionId).catch(
      () => [] as Step[],
    );

    if (ledger.length > 0) {
      return this.formatLedger(ledger, m);
    }

    // 2. Fallback: compact the most recent N messages.
    const recent =
      m.messages && m.messages.length > 0
        ? m.messages.slice(-this.maxRecentMessages)
        : await this.loadRecentTurns(m.sessionId);

    return this.formatMessages(recent, m);
  }

  private async loadRecentLedger(sessionId?: string): Promise<Step[]> {
    if (!sessionId) return [];
    const rows = await dbQuery
      .select<Step>(
        "tasks",
        "session_id = ? ORDER BY step_id DESC LIMIT ?",
        [sessionId, this.maxLedgerEntries],
      )
      .catch(() => [] as Step[]);
    // Restore ascending order for readability.
    return rows.slice().reverse();
  }

  private async loadRecentTurns(
    sessionId?: string,
  ): Promise<Array<{ role: string; content: string; timestamp?: string }>> {
    if (!sessionId) return [];
    const turns = await dbQuery
      .select<ConversationTurn>(
        "history",
        "session_id = ? ORDER BY timestamp DESC LIMIT ?",
        [sessionId, this.maxRecentMessages],
      )
      .catch(() => [] as ConversationTurn[]);
    return turns
      .slice()
      .reverse()
      .map((t) => ({ role: t.role, content: t.content, timestamp: t.timestamp }));
  }

  private formatLedger(ledger: Step[], m: ContextMeasurement): string {
    const pct = ((m.used / m.total) * 100).toFixed(0);
    const lines: string[] = [];
    lines.push(`Context at ${pct}% of ${m.total} tokens — snapshot from Step Ledger.`);
    lines.push("");

    const cur = ledger.find((s) => s.status === "in_progress");
    if (cur) {
      lines.push(`## YOU ARE HERE — step ${cur.step_id}`);
      lines.push(cur.description);
      if (cur.blocker) lines.push(`Blocker: ${cur.blocker}`);
      if (cur.acceptance_cmd) lines.push(`Acceptance: \`${cur.acceptance_cmd}\``);
      lines.push("");
    }

    const completed = ledger.filter((s) => s.status === "completed");
    if (completed.length > 0) {
      lines.push(`## Completed (${completed.length})`);
      for (const s of completed.slice(-15)) {
        lines.push(`- [${s.step_id}] ${s.description}`);
      }
      lines.push("");
    }

    const planned = ledger.filter((s) => s.status === "not_started");
    if (planned.length > 0) {
      lines.push(`## Planned (${planned.length})`);
      for (const s of planned.slice(0, 15)) {
        lines.push(`- [${s.step_id}] ${s.description}`);
      }
    }
    return lines.join("\n");
  }

  private formatMessages(
    msgs: Array<{ role: string; content: string; timestamp?: string }>,
    m: ContextMeasurement,
  ): string {
    const pct = ((m.used / m.total) * 100).toFixed(0);
    const lines: string[] = [];
    lines.push(
      `Context at ${pct}% of ${m.total} tokens — compacted ${msgs.length} recent messages.`,
    );
    lines.push("");
    for (const msg of msgs) {
      const head = msg.timestamp
        ? `${msg.role} @ ${msg.timestamp.slice(0, 19)}`
        : msg.role;
      const trimmed = msg.content.length > 400
        ? msg.content.slice(0, 400) + "…"
        : msg.content;
      lines.push(`**${head}**: ${trimmed}`);
    }
    return lines.join("\n");
  }
}

// ---------------------------------------------------------------------------
// Default sink — forwards to the supervisor for injection into the active
// session. Harnesses wire their own sink when they need different framing.
// ---------------------------------------------------------------------------

export const defaultInjectionSink: InjectionSink = {
  async inject(msg) {
    // A5-012 (2026-05-04): the prior implementation `.catch(...)`-swallowed
    // every error, so the guard's `armed` flag stayed flipped to false even
    // when the snapshot was never actually injected. Re-throw so the guard
    // can restore `armed = true` and emit `compaction.inject_failed` on the
    // livebus. Logs are kept for stderr diagnostics.
    try {
      await dbQuery.raw<{ injected: true }>("session.inject", {
        kind: msg.kind,
        content: msg.content,
      });
    } catch (err) {
      console.error(
        "[mneme-mcp] failed to inject compaction snapshot:",
        errMsg(err),
      );
      throw err instanceof Error
        ? err
        : new Error(`session.inject failed: ${String(err)}`);
    }
  },
};
