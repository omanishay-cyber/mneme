/**
 * MCP tool: step_complete
 *
 * Marks a step complete.
 *
 * v0.1 (review P2): this tool WRITES — mutations on `tasks.db` must go
 * through the single-writer supervisor. We pre-flight the read (status
 * + next sibling) via `bun:sqlite` to compute `next_step_id` locally,
 * then dispatch `step.complete` over IPC. If IPC is down we still
 * return `completed: false` rather than silently lying.
 */

import {
  StepCompleteInput,
  StepCompleteOutput,
  type ToolDescriptor,
} from "../types.ts";
import { sessionSteps, shardDbPath, singleStep } from "../store.ts";
import { query as dbQuery } from "../db.ts";

export const tool: ToolDescriptor<
  ReturnType<typeof StepCompleteInput.parse>,
  ReturnType<typeof StepCompleteOutput.parse>
> = {
  name: "step_complete",
  description:
    "Mark a step complete. Refuses to advance if the acceptance check has not passed (override with force=true). Returns the next step id if any.",
  inputSchema: StepCompleteInput,
  outputSchema: StepCompleteOutput,
  category: "step",
  async handler(input) {
    // Compute next-step hint from the local shard so we can return a
    // useful answer even if the supervisor is offline.
    let nextStepId: string | null = null;
    if (shardDbPath("tasks")) {
      const row = singleStep(input.step_id);
      if (row) {
        const siblings = sessionSteps(row.session_id);
        const idx = siblings.findIndex((s) => s.step_id === input.step_id);
        if (idx >= 0) {
          const next = siblings
            .slice(idx + 1)
            .find((s) => s.status !== "completed");
          nextStepId = next?.step_id ?? null;
        }
      }
    }

    const result = await dbQuery
      .raw<{ completed: boolean; next_step_id: string | null }>(
        "step.complete",
        { step_id: input.step_id, force: input.force },
      )
      .catch(() => null);

    if (result) {
      return {
        step_id: input.step_id,
        completed: result.completed,
        next_step_id: result.next_step_id ?? nextStepId,
      };
    }
    // A5-017 (2026-05-04): supervisor IPC unreachable — we did NOT actually
    // mark this step complete. Return `next_step_id: null` so the model
    // does not interpret the locally-computed sibling as "the next step to
    // start"; surface the reason via the new `note` field instead.
    return {
      step_id: input.step_id,
      completed: false,
      next_step_id: null,
      note: "supervisor unreachable; no state change made",
    };
  },
};
