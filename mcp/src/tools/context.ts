/**
 * F2 — mneme_context (phase-c9 wired)
 *
 * Hybrid-retrieval context pack. Fuses BM25 + semantic + 2-hop graph walk
 * in the Rust `brain` crate (see `brain/src/retrieve.rs`) and returns a
 * token-budget-bounded bundle.
 *
 * Write/compute path: supervisor IPC verb `retrieve.hybrid` (see
 * `brain::retrieve::RetrievalEngine`).
 *
 * Graceful degrade: when the supervisor is offline we fall back to
 * `hybridRetrieveFallback` in store.ts which scans recent ledger entries
 * (tasks.db) plus matching graph nodes (graph.db). The response still
 * parses cleanly against the output schema; a `note` field in
 * `formatted` explains which path served the request.
 *
 * NOTE: as of phase-c9 the supervisor in supervisor/src/ipc.rs does NOT
 * yet route `retrieve.hybrid`. Until that verb is added, every call
 * takes the fallback path.
 */

import { z } from "zod";
import type { ToolDescriptor } from "../types.ts";
import { query as dbQuery } from "../db.ts";
import { hybridRetrieveFallback } from "../store.ts";
import { errMsg } from "../errors.ts";

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const ContextInput = z.object({
  task: z.string().min(1),
  budget_tokens: z.number().int().positive().max(32_000).default(2_000),
  anchors: z.array(z.string()).default([]),
});

const RetrievalSource = z.enum(["bm25", "semantic", "graph", "reranker"]);

const ScoredHit = z.object({
  id: z.string(),
  text: z.string(),
  score: z.number().min(0).max(1),
  sources: z.array(RetrievalSource),
});

const ContextOutput = z.object({
  task: z.string(),
  hits: z.array(ScoredHit),
  tokens_used: z.number().int().nonnegative(),
  budget_tokens: z.number().int().positive(),
  latency_ms: z.number().int().nonnegative(),
  formatted: z.string(),
  /** phase-c9: "supervisor" | "fallback:ledger+graph" | "fallback:empty" */
  note: z.string().optional(),
});

type ContextInputT = z.infer<typeof ContextInput>;
type ContextOutputT = z.infer<typeof ContextOutput>;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function formatPack(
  hits: z.infer<typeof ScoredHit>[],
  task: string,
  note?: string,
): string {
  const lines: string[] = [];
  lines.push("<mneme-context>");
  lines.push(`Task: ${task}`);
  if (note) lines.push(`Retrieval: ${note}`);
  lines.push("");
  for (const h of hits) {
    const srcTag = h.sources.map((s) => s.toUpperCase()).join("+");
    lines.push(`## [${h.score.toFixed(3)} ${srcTag}] ${h.id}`);
    lines.push(h.text);
    lines.push("");
  }
  lines.push("</mneme-context>");
  return lines.join("\n");
}

function approxTokens(text: string): number {
  // Cheap heuristic — ~4 chars per token. Good enough for budget display.
  return Math.ceil(text.length / 4);
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

export const tool: ToolDescriptor<ContextInputT, ContextOutputT> = {
  name: "mneme_context",
  description:
    "Hybrid retrieval (BM25 + semantic + 2-hop graph + optional reranker) that returns a token-budgeted context pack for a task. Use this at the start of any non-trivial turn instead of dumping raw files or relying on the model's memory.",
  inputSchema: ContextInput,
  outputSchema: ContextOutput,
  category: "recall",
  async handler(input) {
    const t0 = Date.now();

    // ---- Supervisor path (brain::retrieve via IPC) ------------------------
    type RetrieveResponse = {
      hits: { id: string; text: string; score: number; sources: string[] }[];
      tokens_used: number;
      budget_tokens: number;
      latency_ms: number;
    };

    const resp = await dbQuery
      .raw<RetrieveResponse>("retrieve.hybrid", {
        task: input.task,
        budget_tokens: input.budget_tokens,
        anchors: input.anchors,
      })
      .catch(() => null);

    if (resp && Array.isArray(resp.hits)) {
      const hits = resp.hits.map((h) => ({
        id: h.id,
        text: h.text,
        score: Math.max(0, Math.min(1, h.score)),
        sources: h.sources.filter((s): s is z.infer<typeof RetrievalSource> =>
          (RetrievalSource.options as readonly string[]).includes(s),
        ),
      }));
      return {
        task: input.task,
        hits,
        tokens_used: resp.tokens_used ?? 0,
        budget_tokens: input.budget_tokens,
        latency_ms: resp.latency_ms ?? Date.now() - t0,
        formatted: formatPack(hits, input.task, "supervisor"),
        note: "supervisor",
      };
    }

    // ---- Fallback: local ledger + graph scan ------------------------------
    try {
      const raw = hybridRetrieveFallback(input.task, input.anchors, 10);
      const hits = raw.map((h) => ({
        id: h.id,
        text: h.text,
        score: h.score,
        sources: [h.source] as z.infer<typeof RetrievalSource>[],
      }));
      const tokens = hits.reduce((acc, h) => acc + approxTokens(h.text), 0);
      // A5-003 (2026-05-04): the prior `=== -1 ? hits.length : hits.length`
      // ternary made both branches identical, so the budget was never
      // enforced. Compute the cutoff once and slice up to (but not
      // including) the first hit whose running total tips over the budget.
      // When no hit exceeds the budget, keep them all. Always retain at
      // least one hit so callers see a non-empty pack.
      const cutoff = hits.findIndex((_, i) => {
        const running = hits
          .slice(0, i + 1)
          .reduce((a, h) => a + approxTokens(h.text), 0);
        return running > input.budget_tokens;
      });
      const sliceEnd = cutoff === -1 ? hits.length : Math.max(1, cutoff);
      const capped = hits.slice(0, sliceEnd);
      return {
        task: input.task,
        hits: capped,
        tokens_used: Math.min(tokens, input.budget_tokens),
        budget_tokens: input.budget_tokens,
        latency_ms: Date.now() - t0,
        formatted: formatPack(capped, input.task, "fallback:ledger+graph"),
        note: "fallback:ledger+graph",
      };
    } catch (err) {
      return {
        task: input.task,
        hits: [],
        tokens_used: 0,
        budget_tokens: input.budget_tokens,
        latency_ms: Date.now() - t0,
        formatted:
          `<mneme-context>\n` +
          `Task: ${input.task}\n` +
          `(retrieval offline: ${errMsg(err)})\n` +
          `</mneme-context>`,
        note: "fallback:empty",
      };
    }
  },
};
