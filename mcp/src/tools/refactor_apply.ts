/**
 * MCP tool: refactor_apply (phase-c10 atomic-rewrite fallback)
 *
 * Apply a single refactor proposal by id. First tries the Rust
 * supervisor's `refactor.apply` IPC verb (which does the atomic
 * rewrite + marks applied_at + returns a diff). When that verb is
 * unavailable, falls back to an in-process atomic rewrite:
 *
 *   1. Read the proposal row from refactors.db.
 *   2. Read the target file from disk.
 *   3. Extract the slice at (line_start:column_start, line_end:column_end).
 *   4. Verify the slice matches proposal.original_text (reject drift).
 *   5. Build new content = before + replacement_text + after.
 *   6. Write new content to <target>.tmp-<proposal_id>.
 *   7. fs.rename tmp -> target (atomic on same-device on Windows/POSIX).
 *   8. UPDATE refactor_proposals SET applied_at = now().
 *
 * Dry-run (input.dry_run === true) does steps 1-5 but skips the
 * rename and the db update. Returns the would-be bytes_written and
 * a preview of the new content.
 *
 * Safety:
 *   - Rejects paths under .git/, target/, node_modules/, ~/.datatree/,
 *     ~/.mneme/ (accidental automation on build artefacts or vendor
 *     code is almost always a bug).
 *   - Rejects paths that resolve outside the project root
 *     (path.relative starts with "..").
 *   - Cleans up orphan temp files on any error after temp write but
 *     before rename.
 *   - Never throws; all error paths return {applied:false, reason}.
 */

import { Buffer } from "node:buffer";
import { existsSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { isAbsolute, join, relative, resolve, sep } from "node:path";
import { z } from "zod";
import {
  RefactorApplyInput,
  RefactorApplyOutput,
  RefactorProposal,
  type ToolDescriptor,
} from "../types.ts";
import { query as dbQuery } from "../db.ts";
import {
  findProjectRoot,
  markRefactorApplied,
  refactorProposalById,
  refactorProposalFullById,
} from "../store.ts";

// Extend the stock output schema additively so older callers that don't
// know about the new fields still parse successfully.
const RefactorApplyOutputExtended = RefactorApplyOutput.extend({
  reason: z.string().optional(),
  proposal: RefactorProposal.nullable().optional(),
  file: z.string().optional(),
  applied_at: z.number().int().optional(),
  expected_preview: z.string().optional(),
  actual_preview: z.string().optional(),
  preview_new_content: z.string().optional(),
  dry_run: z.boolean().optional(),
});

type Input = ReturnType<typeof RefactorApplyInput.parse>;
type Output = z.infer<typeof RefactorApplyOutputExtended>;

const DENY_SEGMENTS = [
  `${sep}.git${sep}`,
  `${sep}target${sep}`,
  `${sep}node_modules${sep}`,
];

function isUnderDenyDir(absPath: string): boolean {
  const normalized = resolve(absPath) + sep;
  for (const seg of DENY_SEGMENTS) {
    if (normalized.includes(seg)) return true;
  }
  const home = homedir();
  const datatreeHome = resolve(join(home, ".datatree")) + sep;
  const mnemeHome = resolve(join(home, ".mneme")) + sep;
  if (normalized.startsWith(datatreeHome)) return true;
  if (normalized.startsWith(mnemeHome)) return true;
  return false;
}

/**
 * Given a 1-indexed (line, column) pair, return the byte offset into `text`
 * that addresses that position. Columns are 0-indexed per the refactor
 * proposal schema (see scanners/refactor_detect). Out-of-range positions
 * clamp to the end of the line / file to stay defensive.
 */
function offsetForLineColumn(text: string, line: number, column: number): number {
  if (line <= 0) return 0;
  let currentLine = 1;
  let i = 0;
  while (i < text.length && currentLine < line) {
    const ch = text.charCodeAt(i);
    i += 1;
    if (ch === 10 /* \n */) currentLine += 1;
  }
  // A5-006 (2026-05-04): the previous `const lineStart = i; ... void lineStart;`
  // captured the line-start offset only to silence an unused-variable warning.
  // The actual clamp behaviour we want — "stop at newline / EOF when walking
  // forward `column` chars" — is already enforced by the loop below, so the
  // captured value was dead code. Removed.
  // Walk forward `column` characters, but stop at newline / EOF.
  let remaining = column;
  while (i < text.length && remaining > 0) {
    const ch = text.charCodeAt(i);
    if (ch === 10) break;
    i += 1;
    remaining -= 1;
  }
  return i;
}

function trimPreview(s: string, max = 200): string {
  if (s.length <= max) return s;
  return `${s.slice(0, max)}... (${s.length - max} more chars)`;
}

function runFallback(input: Input): Output {
  const proposalId = input.proposal_id;
  const dryRun = input.dry_run === true;

  // 1. Fetch the proposal (with column bounds + applied_at).
  const proposal = refactorProposalFullById(proposalId);
  if (!proposal) {
    return {
      proposal_id: proposalId,
      applied: false,
      backup_path: null,
      diff_summary: "",
      bytes_written: 0,
      reason: "proposal not found",
      proposal: null,
    };
  }

  if (proposal.applied_at !== null && proposal.applied_at !== undefined) {
    return {
      proposal_id: proposalId,
      applied: false,
      backup_path: null,
      diff_summary: "",
      bytes_written: 0,
      reason: `already applied at ${String(proposal.applied_at)}`,
    };
  }

  // 2. Safety checks on target path.
  const targetPath = resolve(proposal.file);
  if (isUnderDenyDir(targetPath)) {
    return {
      proposal_id: proposalId,
      applied: false,
      backup_path: null,
      diff_summary: "",
      bytes_written: 0,
      reason: `target path is under a protected directory: ${proposal.file}`,
    };
  }

  // If the proposal stored an absolute path, make sure it lives inside the
  // detected project root. Proposals with relative paths are resolved
  // against cwd (which is the project root inside MCP handlers).
  if (isAbsolute(proposal.file)) {
    const projectRoot = findProjectRoot(targetPath);
    if (projectRoot) {
      const rel = relative(projectRoot, targetPath);
      if (rel.startsWith("..") || isAbsolute(rel)) {
        return {
          proposal_id: proposalId,
          applied: false,
          backup_path: null,
          diff_summary: "",
          bytes_written: 0,
          reason: `target path escapes project root: ${proposal.file}`,
        };
      }
    }
  }

  if (!existsSync(targetPath)) {
    return {
      proposal_id: proposalId,
      applied: false,
      backup_path: null,
      diff_summary: "",
      bytes_written: 0,
      reason: "target file missing",
    };
  }

  // 3. Read the target file.
  let original: string;
  try {
    original = readFileSync(targetPath, "utf8");
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`[refactor_apply] read failed: ${msg}\n`);
    return {
      proposal_id: proposalId,
      applied: false,
      backup_path: null,
      diff_summary: "",
      bytes_written: 0,
      reason: `fs error: ${msg}`,
    };
  }

  // 4. Compute byte offsets for the (line, column) range.
  const startOffset = offsetForLineColumn(
    original,
    proposal.line_start,
    proposal.column_start,
  );
  const endOffset = offsetForLineColumn(
    original,
    proposal.line_end,
    proposal.column_end,
  );
  if (endOffset < startOffset) {
    return {
      proposal_id: proposalId,
      applied: false,
      backup_path: null,
      diff_summary: "",
      bytes_written: 0,
      reason: "invalid proposal range: end < start",
    };
  }

  const actualSlice = original.slice(startOffset, endOffset);
  if (actualSlice !== proposal.original_text) {
    return {
      proposal_id: proposalId,
      applied: false,
      backup_path: null,
      diff_summary: "",
      bytes_written: 0,
      reason: "file changed since proposal created",
      expected_preview: trimPreview(proposal.original_text),
      actual_preview: trimPreview(actualSlice),
    };
  }

  // 5. Build the new content.
  const newContent =
    original.slice(0, startOffset) +
    proposal.replacement_text +
    original.slice(endOffset);
  const bytesWritten = Buffer.byteLength(newContent, "utf8");

  const diffSummary =
    `rewrote ${proposal.file} lines ${proposal.line_start}-${proposal.line_end}: ` +
    `${proposal.original_text.length}->${proposal.replacement_text.length} chars`;

  // 6. Dry-run short-circuit.
  if (dryRun) {
    return {
      proposal_id: proposalId,
      applied: false,
      backup_path: null,
      diff_summary: diffSummary,
      bytes_written: bytesWritten,
      file: targetPath,
      dry_run: true,
      preview_new_content: trimPreview(newContent, 400),
    };
  }

  // 7. Write to temp + atomic rename.
  const tmpPath = `${targetPath}.tmp-${proposalId}`;
  try {
    writeFileSync(tmpPath, newContent, "utf8");
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`[refactor_apply] tmp write failed: ${msg}\n`);
    return {
      proposal_id: proposalId,
      applied: false,
      backup_path: null,
      diff_summary: "",
      bytes_written: 0,
      reason: `fs error: ${msg}`,
    };
  }

  try {
    renameSync(tmpPath, targetPath);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    // Clean up orphan tmp file.
    try {
      if (existsSync(tmpPath)) unlinkSync(tmpPath);
    } catch {
      // ignore
    }
    process.stderr.write(`[refactor_apply] rename failed: ${msg}\n`);
    return {
      proposal_id: proposalId,
      applied: false,
      backup_path: null,
      diff_summary: "",
      bytes_written: 0,
      reason: `fs error: ${msg}`,
    };
  }

  // 8. Mark the proposal applied in the db.
  const appliedAt = Date.now();
  const marked = markRefactorApplied(proposalId, appliedAt);
  if (!marked) {
    process.stderr.write(
      `[refactor_apply] file rewritten but db update failed for ${proposalId}\n`,
    );
  }

  return {
    proposal_id: proposalId,
    applied: true,
    backup_path: null,
    diff_summary: diffSummary,
    bytes_written: bytesWritten,
    file: targetPath,
    applied_at: appliedAt,
  };
}

export const tool: ToolDescriptor<Input, Output> = {
  name: "refactor_apply",
  description:
    "Apply a single refactor proposal by id, rewriting the target file atomically. Tries the supervisor's refactor.apply verb first, then falls back to an in-process atomic rewrite (tmp file + rename + mark applied_at). Set dry_run=true to preview without touching disk. Rejects paths under .git/, target/, node_modules/, ~/.datatree/, ~/.mneme/ or outside the project root.",
  inputSchema: RefactorApplyInput,
  outputSchema: RefactorApplyOutputExtended,
  category: "graph",
  async handler(input) {
    // ---- Supervisor path --------------------------------------------------
    const raw = await dbQuery
      .raw<{
        applied?: boolean;
        backup_path?: string | null;
        diff_summary?: string;
        bytes_written?: number;
      }>("refactor.apply", {
        proposal_id: input.proposal_id,
        dry_run: input.dry_run,
      })
      .catch(() => null);

    if (raw) {
      return {
        proposal_id: input.proposal_id,
        applied: raw.applied ?? false,
        backup_path: raw.backup_path ?? null,
        diff_summary: raw.diff_summary ?? "",
        bytes_written: raw.bytes_written ?? 0,
      };
    }

    // ---- Fallback: atomic rewrite ----------------------------------------
    try {
      return runFallback(input);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      process.stderr.write(`[refactor_apply] unexpected error: ${msg}\n`);
      // Last-ditch: echo the proposal back (preserves phase-c9 behavior).
      const proposal = refactorProposalById(input.proposal_id);
      const severity = proposal?.severity as
        | z.infer<typeof RefactorProposal>["severity"]
        | undefined;
      const kind = proposal?.kind as
        | z.infer<typeof RefactorProposal>["kind"]
        | undefined;
      return {
        proposal_id: input.proposal_id,
        applied: false,
        backup_path: null,
        diff_summary: "",
        bytes_written: 0,
        reason: `fs error: ${msg}`,
        proposal: proposal
          ? {
              proposal_id: proposal.proposal_id,
              kind: kind ?? "unused-import",
              file: proposal.file,
              line_start: proposal.line_start,
              line_end: proposal.line_end,
              column_start: 0,
              column_end: 0,
              symbol: proposal.symbol,
              original_text: proposal.original_text,
              replacement_text: proposal.replacement_text,
              rationale: proposal.rationale,
              severity: severity ?? "info",
              confidence: proposal.confidence,
            }
          : null,
      };
    }
  },
};
