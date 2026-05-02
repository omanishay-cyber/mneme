---
name: mneme-audit
description: "Run mneme's drift + quality scanners (theme, types, security, a11y, perf) over the working tree, the diff, or one file. Use before commits, after refactors, or when the user asks 'what's wrong with this'."
version: 1.0.0
author: Anish Trivedi & Kruti Trivedi
triggers: [audit, auditing, audit-codebase, /mn-audit]
tags: [mneme, audit, drift, scanners, quality]
---

# /mn-audit

Run mneme's full scanner suite (theme, types, security, accessibility,
performance, IPC contracts, etc.) and surface the findings ranked by
severity.

## Usage

```
/mn-audit                    # all scanners on the project
/mn-audit --scope diff       # only files in `git status`
/mn-audit --file <path>      # single file
/mn-audit --scanner theme    # one scanner
```

## When to invoke

- Before every commit.
- After a large refactor.
- When the user says "what's wrong with this", "lint this", "review this".
- When `<mneme-redirect>` appears at the top of a prompt — drift was
  detected and a fresh audit is the fastest way to see what.

## Procedure

### Step 1 — Pick the scope

| User intent | Scope arg |
|---|---|
| "before commit" / "what changed" | `scope='diff'` |
| "review file `<path>`" | `scope='file', file=<path>` |
| "full audit" / "everything" | `scope='project'` |

### Step 2 — Pick the scanner(s)

If the user asked for one scanner ("theme audit", "type audit"), call the
matching `audit_<scanner>` tool. Otherwise call `audit(scope=...)` to run
the full suite in one round-trip.

### Step 3 — Render the findings

- Group by severity (critical → high → medium → low → info).
- For each finding: `[severity] file:line — rule — message — suggestion`.
- Cap at 30 findings in the response; if more, summarize counts and offer
  to drill into a specific severity or scanner.

### Step 4 — Suggest next move

- If any critical findings: refuse to suggest commit; suggest fixing first.
- If only low/info: surface them, offer to skip.
- Always remind the user: `audit` is idempotent — re-run after fixes to
  confirm.

## Honesty Rules

- Never lower a severity to make the result look cleaner.
- Never silently drop findings that don't fit the response budget.
  Mention the count and offer to drill in.
- If a scanner errored (e.g. missing parser), report it explicitly.
