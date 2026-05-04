/**
 * Mneme MCP — shared type definitions and zod schemas.
 *
 * Every MCP tool input/output is validated against a zod schema declared here.
 * Hooks emit a strict JSON shape consumed by Claude Code (and 17 other AI
 * harnesses); the schemas in this file are the single source of truth.
 *
 * Conventions:
 *   - All times are RFC3339 strings ("2026-04-23T10:11:12.345Z").
 *   - All file paths are absolute, OS-native (forward slashes on Windows OK).
 *   - All ids ("step_id", "session_id", "snapshot_id") are opaque strings.
 *   - Severity ladder: "info" | "low" | "medium" | "high" | "critical".
 */

import { z } from "zod";

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

export const SeverityEnum = z.enum(["info", "low", "medium", "high", "critical"]);
export type Severity = z.infer<typeof SeverityEnum>;

export const StepStatusEnum = z.enum([
  "not_started",
  "in_progress",
  "completed",
  "blocked",
  "failed",
]);
export type StepStatus = z.infer<typeof StepStatusEnum>;

export const DbLayerEnum = z.enum([
  "history",
  "decisions",
  "constraints",
  "tasks",
  "findings",
  "graph",
  "semantic",
  "memory",
  "tool_cache",
  "audit",
  "telemetry",
  "corpus",
  "multimodal",
  "refactors",
  "wiki",
  "architecture",
]);
export type DbLayer = z.infer<typeof DbLayerEnum>;

// ---------------------------------------------------------------------------
// Tool I/O — Recall family (§5.1)
// ---------------------------------------------------------------------------

export const RecallDecisionInput = z.object({
  query: z.string().min(1),
  since: z.string().optional(),
  limit: z.number().int().positive().max(100).default(10),
});
export type RecallDecisionInput = z.infer<typeof RecallDecisionInput>;

export const Decision = z.object({
  id: z.string(),
  topic: z.string(),
  problem: z.string(),
  chosen: z.string(),
  reasoning: z.string(),
  rejected: z.array(z.string()).default([]),
  timestamp: z.string(),
  source_file: z.string().nullable().default(null),
  confidence: z.number().min(0).max(1).default(1),
});
export type Decision = z.infer<typeof Decision>;

export const RecallDecisionOutput = z.object({
  decisions: z.array(Decision),
  query_id: z.string(),
  latency_ms: z.number(),
});

export const RecallConversationInput = z.object({
  query: z.string().min(1),
  since: z.string().optional(),
  session_id: z.string().optional(),
  limit: z.number().int().positive().max(50).default(10),
});
export type RecallConversationInput = z.infer<typeof RecallConversationInput>;

export const ConversationTurn = z.object({
  turn_id: z.string(),
  session_id: z.string(),
  role: z.enum(["user", "assistant", "system", "tool"]),
  content: z.string(),
  tool_calls: z.array(z.unknown()).default([]),
  timestamp: z.string(),
  similarity: z.number().min(0).max(1).optional(),
});
export type ConversationTurn = z.infer<typeof ConversationTurn>;

export const RecallConversationOutput = z.object({
  turns: z.array(ConversationTurn),
});

export const RecallConceptInput = z.object({
  query: z.string().min(1),
  modality: z
    .enum(["all", "code", "doc", "image", "audio", "video"])
    .default("all"),
  limit: z.number().int().positive().max(50).default(10),
});

export const Concept = z.object({
  id: z.string(),
  label: z.string(),
  modality: z.string(),
  source_file: z.string().nullable(),
  source_location: z.string().nullable(),
  similarity: z.number().min(0).max(1),
  community_id: z.number().int().nullable(),
  // A5-007 (2026-05-04): the `recall_concept` tool emits `community` (kind
  // tag for human-friendly grouping) and `context` (the same kind tag, kept
  // separate for future hover/tooltip use) on every row. zod's default
  // strip-mode silently dropped them, hiding signal from the model. Added
  // them as optional fields here so callers see them when present.
  community: z.string().optional(),
  context: z.string().optional(),
});
export type Concept = z.infer<typeof Concept>;

export const RecallConceptOutput = z.object({
  concepts: z.array(Concept),
});

export const RecallFileInput = z.object({
  path: z.string().min(1),
});

export const FileState = z.object({
  path: z.string(),
  exists: z.boolean(),
  hash: z.string().nullable(),
  size_bytes: z.number().int().nullable(),
  language: z.string().nullable(),
  summary: z.string().nullable(),
  last_read_at: z.string().nullable(),
  last_modified_at: z.string().nullable(),
  blast_radius_count: z.number().int().nullable(),
  test_coverage: z.number().min(0).max(1).nullable(),
});
export type FileState = z.infer<typeof FileState>;

export const RecallTodoInput = z.object({
  filter: z
    .object({
      status: z.enum(["open", "completed", "all"]).default("open"),
      tag: z.string().optional(),
      since: z.string().optional(),
    })
    .default({}),
});

export const Todo = z.object({
  id: z.string(),
  text: z.string(),
  status: z.enum(["open", "completed"]),
  created_at: z.string(),
  completed_at: z.string().nullable(),
  source_file: z.string().nullable(),
  tags: z.array(z.string()).default([]),
});
export type Todo = z.infer<typeof Todo>;

export const RecallTodoOutput = z.object({ todos: z.array(Todo) });

export const RecallConstraintInput = z.object({
  scope: z.enum(["global", "project", "file"]).default("project"),
  file: z.string().optional(),
});

export const Constraint = z.object({
  id: z.string(),
  rule: z.string(),
  scope: z.string(),
  source: z.string(),
  severity: SeverityEnum,
  enforcement: z.enum(["warn", "block"]),
});
export type Constraint = z.infer<typeof Constraint>;

export const RecallConstraintOutput = z.object({
  constraints: z.array(Constraint),
});

// ---------------------------------------------------------------------------
// Tool I/O — Code Graph (§5.2)
// ---------------------------------------------------------------------------

export const BlastRadiusInput = z.object({
  target: z.string().min(1),
  // Default depth=1 to keep responses small enough for an LLM context.
  // Power users opt into the deep walk via `depth: 5` (or larger) or
  // by passing `deep: true` (which expands to `depth=5`).
  depth: z.number().int().positive().max(10).default(1),
  deep: z.boolean().default(false),
  include_tests: z.boolean().default(true),
});

export const BlastRadiusOutput = z.object({
  target: z.string(),
  affected_files: z.array(z.string()),
  affected_symbols: z.array(z.string()),
  test_files: z.array(z.string()),
  total_count: z.number().int(),
  critical_paths: z.array(z.string()).default([]),
});

export const CallGraphInput = z.object({
  function: z.string().min(1),
  direction: z.enum(["callers", "callees", "both"]).default("both"),
  depth: z.number().int().positive().max(10).default(3),
});

export const CallGraphNode = z.object({
  id: z.string(),
  label: z.string(),
  file: z.string(),
  line: z.number().int(),
});

export const CallGraphEdge = z.object({
  source: z.string(),
  target: z.string(),
  call_count: z.number().int().default(1),
});

export const CallGraphOutput = z.object({
  nodes: z.array(CallGraphNode),
  edges: z.array(CallGraphEdge),
});

export const FindReferencesInput = z.object({
  symbol: z.string().min(1),
  scope: z.enum(["project", "workspace"]).default("project"),
});

export const ReferenceHit = z.object({
  file: z.string(),
  line: z.number().int(),
  column: z.number().int(),
  context: z.string(),
  kind: z.enum(["definition", "call", "import", "usage"]),
});

export const FindReferencesOutput = z.object({
  symbol: z.string(),
  hits: z.array(ReferenceHit),
});

export const DependencyChainInput = z.object({
  file: z.string().min(1),
  direction: z.enum(["forward", "reverse", "both"]).default("both"),
});

export const DependencyChainOutput = z.object({
  file: z.string(),
  forward: z.array(z.string()),
  reverse: z.array(z.string()),
});

export const CyclicDepsInput = z.object({
  scope: z.enum(["project", "workspace"]).default("project"),
});

export const CyclicDepsOutput = z.object({
  cycles: z.array(z.array(z.string())),
  count: z.number().int(),
});

// ---------------------------------------------------------------------------
// Tool I/O — Multimodal (§5.3)
// ---------------------------------------------------------------------------

export const GraphifyCorpusInput = z.object({
  path: z.string().optional(),
  mode: z.enum(["fast", "deep"]).default("fast"),
  incremental: z.boolean().default(true),
});

export const GraphifyCorpusOutput = z.object({
  nodes_count: z.number().int(),
  edges_count: z.number().int(),
  hyperedges_count: z.number().int(),
  communities_count: z.number().int(),
  duration_ms: z.number(),
  report_path: z.string(),
});

export const GodNodesInput = z.object({
  project: z.string().optional(),
  top_n: z.number().int().positive().max(100).default(10),
});

export const GodNode = z.object({
  id: z.string(),
  label: z.string(),
  degree: z.number().int(),
  // H3 (Phase A): resolved file path for the node — replaces the opaque
  // `n_f62d…` ids that previously surfaced as the only handle. Null only
  // when the underlying `nodes` row has no file association (rare).
  file_path: z.string().nullable(),
  community_id: z.number().int().nullable(),
});

export const GodNodesOutput = z.object({ gods: z.array(GodNode) });

export const SurprisingConnectionsInput = z.object({
  min_confidence: z.number().min(0).max(1).default(0.7),
  limit: z.number().int().positive().max(50).default(10),
});

export const Surprise = z.object({
  source: z.string(),
  target: z.string(),
  relation: z.string(),
  confidence: z.number(),
  source_community: z.number().int(),
  target_community: z.number().int(),
  reasoning: z.string(),
});

export const SurprisingConnectionsOutput = z.object({
  surprises: z.array(Surprise),
});

export const AuditCorpusInput = z.object({
  path: z.string().optional(),
});

export const AuditCorpusOutput = z.object({
  report_markdown: z.string(),
  report_path: z.string(),
  warnings: z.array(z.string()),
});

// ---------------------------------------------------------------------------
// Tool I/O — Drift & Audit (§5.4)
// ---------------------------------------------------------------------------

export const Finding = z.object({
  id: z.string(),
  scanner: z.string(),
  severity: SeverityEnum,
  file: z.string(),
  line: z.number().int().nullable(),
  rule: z.string(),
  message: z.string(),
  suggestion: z.string().nullable(),
  detected_at: z.string(),
});
export type Finding = z.infer<typeof Finding>;

export const AuditInput = z.object({
  scope: z.enum(["project", "file", "diff"]).default("project"),
  file: z.string().optional(),
  scanners: z.array(z.string()).optional(),
});

export const AuditOutput = z.object({
  findings: z.array(Finding),
  summary: z.object({
    total: z.number().int(),
    by_severity: z.record(z.string(), z.number().int()),
    by_scanner: z.record(z.string(), z.number().int()),
  }),
});

export const DriftFindingsInput = z.object({
  severity: SeverityEnum.optional(),
  scope: z.string().optional(),
  limit: z.number().int().positive().max(500).default(50),
});

export const DriftFindingsOutput = z.object({
  findings: z.array(Finding),
});

export const ScannerInput = z.object({
  file: z.string().optional(),
  scope: z.enum(["project", "file", "diff"]).default("project"),
});

export const ScannerOutput = z.object({
  findings: z.array(Finding),
  scanner: z.string(),
  duration_ms: z.number(),
});

// ---------------------------------------------------------------------------
// Tool I/O — Step Ledger (§5.5)
// ---------------------------------------------------------------------------

export const Step = z.object({
  step_id: z.string(),
  parent_step_id: z.string().nullable(),
  session_id: z.string(),
  description: z.string(),
  acceptance_cmd: z.string().nullable(),
  acceptance_check: z.unknown().nullable(),
  status: StepStatusEnum,
  started_at: z.string().nullable(),
  completed_at: z.string().nullable(),
  verification_proof: z.string().nullable(),
  artifacts: z.unknown().nullable(),
  notes: z.string().nullable(),
  blocker: z.string().nullable(),
  drift_score: z.number().int().default(0),
});
export type Step = z.infer<typeof Step>;

export const StepStatusInput = z.object({
  session_id: z.string().optional(),
});

export const StepStatusOutput = z.object({
  current_step_id: z.string().nullable(),
  steps: z.array(Step),
  drift_score_total: z.number().int(),
  goal_root: z.string().nullable(),
});

export const StepShowInput = z.object({
  step_id: z.string(),
});

export const StepShowOutput = z.object({ step: Step });

export const StepVerifyInput = z.object({
  step_id: z.string(),
  dry_run: z.boolean().default(false),
});

export const StepVerifyOutput = z.object({
  step_id: z.string(),
  passed: z.boolean(),
  proof: z.string(),
  exit_code: z.number().int(),
  duration_ms: z.number(),
});

export const StepCompleteInput = z.object({
  step_id: z.string(),
  force: z.boolean().default(false),
});

export const StepCompleteOutput = z.object({
  step_id: z.string(),
  completed: z.boolean(),
  next_step_id: z.string().nullable(),
  // A5-017 (2026-05-04): when the supervisor IPC is unreachable we cannot
  // actually mark the step complete. The prior shape returned
  // `{ completed: false, next_step_id: <suggested> }` which the model
  // could plausibly read as "the step succeeded; here is the next one".
  // `note` exists to disambiguate — populated only on the failure path.
  note: z.string().optional(),
});

export const StepResumeInput = z.object({
  session_id: z.string().optional(),
});

export const StepResumeOutput = z.object({
  bundle: z.string(),
  current_step_id: z.string().nullable(),
  total_steps: z.number().int(),
});

export const StepPlanFromInput = z.object({
  markdown_path: z.string().min(1),
  session_id: z.string().optional(),
});

export const StepPlanFromOutput = z.object({
  steps_created: z.number().int(),
  root_step_id: z.string(),
});

// ---------------------------------------------------------------------------
// Tool I/O — Step Ledger recall / resume / why  (F1 + F6)
// ---------------------------------------------------------------------------

/** Ledger kind tags in stable string form (matches `StepKind::tag()` in Rust). */
export const LedgerKindEnum = z.enum([
  "decision",
  "impl",
  "bug",
  "open_question",
  "refactor",
  "experiment",
]);
export type LedgerKind = z.infer<typeof LedgerKindEnum>;

export const LedgerEntry = z.object({
  id: z.string(),
  session_id: z.string(),
  timestamp: z.string(), // RFC3339
  kind: LedgerKindEnum,
  summary: z.string(),
  rationale: z.string().nullable(),
  touched_files: z.array(z.string()).default([]),
  touched_concepts: z.array(z.string()).default([]),
  transcript_ref: z
    .object({
      session_id: z.string(),
      turn_index: z.number().int().nullable().optional(),
      message_id: z.string().nullable().optional(),
    })
    .nullable()
    .optional(),
  /** Kind-specific payload, mirrors Rust enum. Kept as opaque JSON here. */
  kind_payload: z.unknown().optional(),
});
export type LedgerEntry = z.infer<typeof LedgerEntry>;

export const MnemeRecallInput = z.object({
  query: z.string().min(1),
  kinds: z.array(LedgerKindEnum).default([]),
  limit: z.number().int().positive().max(50).default(5),
  since_hours: z.number().int().positive().max(24 * 90).optional(),
  session_id: z.string().optional(),
});
export type MnemeRecallInput = z.infer<typeof MnemeRecallInput>;

export const MnemeRecallOutput = z.object({
  entries: z.array(LedgerEntry),
  formatted: z.string(),
});
export type MnemeRecallOutput = z.infer<typeof MnemeRecallOutput>;

export const MnemeResumeInput = z.object({
  since_hours: z.number().int().positive().max(24 * 14).default(48),
  session_id: z.string().optional(),
});
export type MnemeResumeInput = z.infer<typeof MnemeResumeInput>;

export const MnemeResumeOutput = z.object({
  session_id: z.string(),
  generated_at: z.string(),
  recent_decisions: z.array(LedgerEntry),
  recent_implementations: z.array(LedgerEntry),
  open_questions: z.array(LedgerEntry),
  timeline: z.array(LedgerEntry),
  formatted: z.string(),
});
export type MnemeResumeOutput = z.infer<typeof MnemeResumeOutput>;

export const MnemeWhyInput = z.object({
  question: z.string().min(1),
  limit: z.number().int().positive().max(20).default(6),
});
export type MnemeWhyInput = z.infer<typeof MnemeWhyInput>;

export const MnemeWhyOutput = z.object({
  question: z.string(),
  decisions: z.array(LedgerEntry),
  git_commits: z.array(
    z.object({
      sha: z.string(),
      date: z.string(),
      subject: z.string(),
    }),
  ),
  related_concepts: z.array(z.string()),
  formatted: z.string(),
});
export type MnemeWhyOutput = z.infer<typeof MnemeWhyOutput>;

// ---------------------------------------------------------------------------
// Tool I/O — Time Machine (§5.6)
// ---------------------------------------------------------------------------

export const SnapshotInput = z.object({
  label: z.string().optional(),
});

export const SnapshotOutput = z.object({
  snapshot_id: z.string(),
  created_at: z.string(),
  size_bytes: z.number().int(),
});

export const CompareInput = z.object({
  snapshot_a: z.string(),
  snapshot_b: z.string(),
});

export const Diff = z.object({
  files_added: z.array(z.string()),
  files_removed: z.array(z.string()),
  files_modified: z.array(z.string()),
  decisions_added: z.number().int(),
  findings_resolved: z.number().int(),
  findings_introduced: z.number().int(),
});

export const CompareOutput = z.object({ diff: Diff });

export const RewindInput = z.object({
  file: z.string().min(1),
  when: z.string().min(1),
});

export const RewindOutput = z.object({
  file: z.string(),
  when: z.string(),
  // v0.3.x: snapshots store .db files but NOT the underlying file bytes,
  // so we can only return a metadata summary (path, sha, language, line +
  // byte counts, parsed_at). When the snapshot subsystem starts archiving
  // the original file content (planned for v0.4) we'll add a sibling
  // `content` field and flip `content_available` to true.
  // TODO(v0.4): persist file blobs in snapshots and surface raw content.
  content_summary: z.string(),
  content_available: z.boolean(),
  hash: z.string(),
});

// ---------------------------------------------------------------------------
// Tool I/O — Health (§5.7)
// ---------------------------------------------------------------------------

export const HealthInput = z.object({}).default({});

export const HealthOutput = z.object({
  status: z.enum(["green", "yellow", "red"]),
  uptime_seconds: z.number().int(),
  workers: z.array(
    z.object({
      name: z.string(),
      pid: z.number().int().nullable(),
      restarts_24h: z.number().int(),
      rss_mb: z.number(),
      status: z.string(),
    }),
  ),
  cache_hit_rate: z.number().min(0).max(1),
  disk_usage_mb: z.number(),
  queue_depth: z.number().int(),
  // Raw percentiles in milliseconds. Kept for back-compat with any
  // dashboard / script that already parses them.
  p50_ms: z.number(),
  p95_ms: z.number(),
  p99_ms: z.number(),
  // B15 (2026-05-02): human-friendly mirrors. Same numbers as p50_ms /
  // p99_ms with friendlier names. UIs should prefer these.
  typical_response_ms: z.number(),
  slow_response_ms: z.number(),
});

export const DoctorInput = z.object({}).default({});

export const DoctorOutput = z.object({
  ok: z.boolean(),
  checks: z.array(
    z.object({
      name: z.string(),
      passed: z.boolean(),
      detail: z.string(),
    }),
  ),
  recommendations: z.array(z.string()),
});

export const RebuildInput = z.object({
  scope: z.enum(["graph", "semantic", "all"]).default("graph"),
  confirm: z.boolean().default(false),
});

export const RebuildOutput = z.object({
  rebuilt: z.array(z.string()),
  duration_ms: z.number(),
});

// ---------------------------------------------------------------------------
// Tool I/O — Refactor (closes CRG gap)
// ---------------------------------------------------------------------------

export const RefactorProposal = z.object({
  proposal_id: z.string(),
  kind: z.enum([
    "unused-import",
    "unreachable-function",
    "unreferenced-type",
    "rename-function",
    "rename-variable",
    "rename-type",
  ]),
  file: z.string(),
  line_start: z.number().int().nonnegative(),
  line_end: z.number().int().nonnegative(),
  column_start: z.number().int().nonnegative(),
  column_end: z.number().int().nonnegative(),
  symbol: z.string().nullable(),
  original_text: z.string(),
  replacement_text: z.string(),
  rationale: z.string(),
  severity: SeverityEnum,
  confidence: z.number().min(0).max(1),
});
export type RefactorProposal = z.infer<typeof RefactorProposal>;

export const RefactorSuggestInput = z.object({
  scope: z.enum(["project", "file"]).default("project"),
  file: z.string().optional(),
  kinds: z.array(z.string()).optional(),
  limit: z.number().int().positive().max(500).default(100),
});

export const RefactorSuggestOutput = z.object({
  proposals: z.array(RefactorProposal),
  scanned_files: z.number().int(),
  duration_ms: z.number(),
});

export const RefactorApplyInput = z.object({
  proposal_id: z.string().min(1),
  dry_run: z.boolean().default(false),
});

export const RefactorApplyOutput = z.object({
  proposal_id: z.string(),
  applied: z.boolean(),
  backup_path: z.string().nullable(),
  diff_summary: z.string(),
  bytes_written: z.number().int().nonnegative(),
});

// ---------------------------------------------------------------------------
// Tool I/O — Wiki (closes CRG gap)
// ---------------------------------------------------------------------------

export const WikiGenerateInput = z.object({
  project: z.string().optional(),
  force: z.boolean().default(false),
});

export const WikiPageSummary = z.object({
  slug: z.string(),
  title: z.string(),
  community_id: z.number().int(),
  risk_score: z.number(),
  file_count: z.number().int(),
  entry_point_count: z.number().int(),
});

export const WikiGenerateOutput = z.object({
  pages: z.array(WikiPageSummary),
  total_pages: z.number().int(),
  duration_ms: z.number(),
});

export const WikiPageInput = z
  .object({
    slug: z.string().min(1).optional(),
    topic: z.string().min(1).optional(),
    version: z.number().int().positive().optional(),
  })
  .refine((v) => Boolean(v.slug) || Boolean(v.topic), {
    message: "Either slug or topic is required",
  });

export const WikiPageOutput = z.object({
  slug: z.string(),
  title: z.string(),
  community_id: z.number().int(),
  version: z.number().int(),
  markdown: z.string(),
  risk_score: z.number(),
  generated_at: z.string(),
});

// ---------------------------------------------------------------------------
// Tool I/O — Architecture Overview (closes CRG gap)
// ---------------------------------------------------------------------------

export const ArchitectureOverviewInput = z.object({
  project: z.string().optional(),
  refresh: z.boolean().default(false),
  top_k: z.number().int().positive().max(50).default(10),
});

export const CouplingCell = z.object({
  from_community: z.number().int(),
  to_community: z.number().int(),
  edge_count: z.number().int(),
  density: z.number(),
});

export const CommunityRiskEntry = z.object({
  community_id: z.number().int(),
  total_callers: z.number().int(),
  avg_criticality: z.number(),
  security_hits: z.number().int(),
  risk_index: z.number(),
  top_symbols: z.array(z.string()),
});

export const BridgeNodeEntry = z.object({
  qualified_name: z.string(),
  community_id: z.number().int(),
  betweenness: z.number(),
});

export const HubNodeEntry = z.object({
  qualified_name: z.string(),
  community_id: z.number().int(),
  degree: z.number().int(),
  // H3 (Phase A): resolved file path for the hub node so callers don't see
  // opaque internal IDs. Null when the underlying `nodes` row has no
  // file association (rare).
  file_path: z.string().nullable(),
});

export const ArchitectureOverviewOutput = z.object({
  community_count: z.number().int(),
  node_count: z.number().int(),
  edge_count: z.number().int(),
  coupling_matrix: z.array(CouplingCell),
  risk_index: z.array(CommunityRiskEntry),
  bridge_nodes: z.array(BridgeNodeEntry),
  hub_nodes: z.array(HubNodeEntry),
  captured_at: z.string(),
});

// ---------------------------------------------------------------------------
// Hook outputs
// ---------------------------------------------------------------------------

/** Universal hook envelope returned by every hook to the harness. */
export const HookOutput = z.object({
  additional_context: z.string().optional(),
  skip: z.boolean().optional(),
  result: z.string().optional(),
  metadata: z.record(z.string(), z.unknown()).optional(),
});
export type HookOutput = z.infer<typeof HookOutput>;

// ---------------------------------------------------------------------------
// IPC envelope (CLI <-> MCP)
// ---------------------------------------------------------------------------

export interface IpcRequest {
  id: string;
  method: string;
  params: unknown;
}

export interface IpcResponse<T = unknown> {
  id: string;
  ok: boolean;
  data?: T;
  error?: { code: string; message: string; detail?: unknown };
  latency_ms: number;
  cache_hit: boolean;
  source_db?: DbLayer;
  schema_version?: number;
}

// ---------------------------------------------------------------------------
// MCP tool descriptor (used by the registry)
// ---------------------------------------------------------------------------

export interface ToolDescriptor<I = unknown, O = unknown> {
  /** MCP tool name (snake_case). */
  name: string;
  /** Human description shown to the model. */
  description: string;
  /**
   * zod schema for input validation.
   *
   * The third type parameter (`Input`) is intentionally permissive so that
   * schemas using `.default(...)` — whose `z.input<...>` differs from
   * `z.output<...>` — can still be assigned to a descriptor typed by the
   * parsed ("output") shape. `ToolRegistry.invoke()` always parses raw
   * `unknown` input, so accepting `any` here is type-safe at the boundary.
   */
  inputSchema: z.ZodType<I, z.ZodTypeDef, unknown>;
  /** zod schema for output validation. */
  outputSchema: z.ZodType<O, z.ZodTypeDef, unknown>;
  /** Implementation called by the MCP runtime after validation. */
  handler: (input: I, ctx: ToolContext) => Promise<O>;
  /** Optional category (used by /mn-recall, etc.). */
  category?:
    | "recall"
    | "graph"
    | "multimodal"
    | "drift"
    | "step"
    | "time"
    | "health";
}

export interface ToolContext {
  sessionId: string;
  cwd: string;
  /** Set when the tool was triggered via a hook (vs. direct MCP call). */
  hook?: string;
}

export const TokenBudgets = {
  primer: 1500,
  smart_inject: 2500,
  max_total_per_turn: 5000,
} as const;
