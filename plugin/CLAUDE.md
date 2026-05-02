<!-- mneme-start v1.0 -->
# Mneme - Persistent Project Memory + Live Code Graph

This project has the **mneme** plugin installed (v0.3.2). Mneme is a local,
no-internet daemon that gives Claude Code a persistent SQLite memory, a
live code graph, a drift detector, a step ledger that survives compaction,
and **48 MCP tools** + **11 scanners** + **8 hooks** + **14 WebGL views**
backed by **22 sharded SQLite DBs**.

## Tool Usage Rules (always follow before Grep/Glob/Read)

When investigating code, **always** use mneme MCP tools first. They are
faster, cheaper (fewer tokens), and surface structural context (callers,
dependents, tests, decisions, drift findings) that file-scanning cannot.

| Question | Use this mneme tool first | Fallback |
|---|---|---|
| "Where is X used?" | `find_references(symbol)` | Grep |
| "What breaks if I change X?" | `blast_radius(target)` | manual trace |
| "What does this file do?" | `recall_file(path)` | Read |
| "What is the architecture?" | `god_nodes()` + `audit_corpus()` | exploring |
| "Did we already decide this?" | `recall_decision(query)` | re-derive |
| "Are there open TODOs?" | `recall_todo()` | grep TODOs |
| "What rules apply to this file?" | `recall_constraint(scope='file', file=path)` | read rules/ |
| "Who calls this function?" | `call_graph(function, direction='callers')` | grep |
| "What does this import?" | `dependency_chain(file, direction='forward')` | reading imports |
| "Are there cycles?" | `cyclic_deps()` | manual check |

Target: **<= 5 tool calls per task**, **<= 800 tokens of graph context**.
Fall back to Grep/Glob/Read **only** when mneme doesn't cover what you need.

## Step Ledger (compaction-resilient task tracking)

Multi-step tasks **must** be tracked in the step ledger. The ledger is a
SQLite-backed list of numbered steps with acceptance commands. It survives
context compaction - when context resets, the resumer rebuilds where you were.

Use this loop for any task with **3 or more** steps:

1. `step_plan_from(markdown_path)` - turn a plan into a ledger
   (or call `step_status` if a plan already exists).
2. For each step:
   - `step_show(step_id)` - read the step
   - Do the work
   - `step_verify(step_id)` - run the acceptance check
   - `step_complete(step_id)` - advance (only after verify passes)
3. After **every compaction or session restart**, call `step_resume()` first.

Never combine fixes. **One fix = one step.** This is non-negotiable.

## Drift Detection

Mneme continuously scans changed files for rule violations. Findings are
tagged red (critical) / yellow (should-fix) / green (info).

- Before commit: `audit(scope='diff')` - see what's wrong with your changes
- During work: `drift_findings(severity='critical')` - never let red sit
- Theme issues: `audit_theme(file=...)`
- Type issues: `audit_types(file=...)`
- Security: `audit_security(file=...)`
- Performance: `audit_perf(file=...)`
- Accessibility: `audit_a11y(file=...)`

Drift redirects: if your responses diverge from the active goal for **2+
consecutive turns**, mneme prepends a `<mneme-redirect>` block to your
next prompt. **Take it seriously** - re-anchor to the active step before
continuing.

## Compaction Resilience

When you sense context approaching its limit, the user's harness will compact
automatically. **You do not need to do anything special.** After compaction:

1. The hook auto-injects a `<mneme-resume>` block on the next turn.
2. It contains: original goal, completed steps with proofs, YOU ARE HERE,
   planned steps, active constraints, and the verification gate for the
   current step.
3. **Read the resume block. Continue from `Step <K>`. Do not restart.**

If for any reason the resume block is missing, call `step_resume()` to fetch
it manually.

## Performance Budgets (for your tool selection)

| Tool category | Target latency | When to use |
|---|---|---|
| `recall_file`, `recall_decision`, `recall_todo` | <= 5ms | always before Read |
| `blast_radius`, `call_graph`, `find_references` | <= 5ms | before Edit/Write |
| `recall_concept` (semantic search) | <= 50ms | open-ended exploration |
| `audit*`, `drift_findings` | 50-500ms | before commit, on demand |
| `graphify_corpus` | seconds-minutes | once per major change |

If a tool is slow (>1s), mneme's `health()` will be yellow/red. Tell the
user - do not silently retry.

## What mneme captures (privacy)

Everything stays local on this machine. Mneme captures:

- Tool calls, their parameters, and their results (history.db)
- File hashes + summaries (semantic.db)
- Decisions you make in conversation (decisions.db)
- Constraints sourced from CLAUDE.md / .claude/rules/
- Step ledger for every multi-step task (tasks.db)
- Drift findings (findings.db)

It does **not** make outbound network calls. No telemetry. No remote LLMs.
No cloud sync. See `<repo>/.claude/mneme.json` for per-project tuning.

## Quick Commands

```
/mn-view             open the 14-view live graph
/mn-step             view current step ledger
/mn-recall <query>   semantic search across decisions + concepts + history
/mn-blast <target>   blast radius for a file or function
/mn-audit            run all scanners on the working tree
/mn-doctor           full self-test
```

## Workflow Codewords

When the user starts a message with one of these single words, switch how you engage:

| Word | What it means |
|---|---|
| `coldstart` | Pause. Observe only. Read context, draft a plan, do not touch code. Wait for `hotstart` or `firestart` before doing anything. |
| `hotstart` | Resume with discipline. Numbered roadmap. Verify each step before moving to the next. |
| `firestart` | Maximum loadout. Load every fireworks skill that matches the task, prime the mneme graph (`god_nodes`, `audit_corpus`, `recall_decision`), then proceed with `hotstart` discipline. |
| `CHS` | "Check my screenshot" - read the latest file in the user's OS-native screenshot folder (Windows `Pictures/Screenshots`, macOS `Desktop`, Linux `Pictures/Screenshots`) and respond based on its contents. |

These are not casual conversation. Treat them as commands. Full protocol per codeword lives in `~/.mneme/plugin/skills/mneme-codewords/SKILL.md`.

<!-- mneme-end v1.0 -->
