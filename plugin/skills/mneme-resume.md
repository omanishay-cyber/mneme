---
name: mneme-resume
description: "Recover from a context compaction or session restart. Pulls the resumption bundle from the step ledger and continues from the current step. Use after any context reset, or when the user says 'where were we' / 'pick up where we left off'."
version: 1.0.0
author: Anish Trivedi & Kruti Trivedi
triggers: [resume, resume-step, continue, /mn-step, where-were-we]
tags: [mneme, resume, step-ledger, compaction]
---

# /mn-step (resume mode)

Recover from a context compaction or session restart by fetching the
mneme resumption bundle and continuing from the current step in the
step ledger.

## Usage

```
/mn-step               # show current step + resume bundle
/mn-step status        # explicit step_status call
/mn-step show <id>     # detail of one step
/mn-step verify <id>   # run acceptance check
/mn-step complete <id> # mark complete (only if verify passes)
/mn-step plan <md>     # ingest a roadmap into the ledger
/mn-step resume        # explicit resume bundle
```

## When to invoke

- **Always** at the start of any new session (the harness fires
  `SessionStart`, but if you somehow skip it, run `/mn-step` first).
- **Always** after the user mentions context was reset, compacted, or
  conversation was lost.
- When `<mneme-resume>` block appears in the prompt, read it carefully
  before doing anything else.

## Procedure

### Step 1 — Fetch the bundle

Call `step_resume()`. The result is a `<mneme-resume>` block with:

- Original goal (verbatim from session start)
- Goal stack (root → current leaf)
- Completed steps with proofs
- **YOU ARE HERE** — the current step
- Planned steps
- Active constraints
- Verification gate for the current step

### Step 2 — Read it

Do not skim. The bundle is the source of truth for what to do next.

### Step 3 — Continue from `Step <K>`

- Read the current step's description and acceptance command.
- Do the work for that step (and only that step).
- When done: `step_verify(step_id)`. If it passes:
  `step_complete(step_id)`.
- The resumer will surface the next step automatically.

### Step 4 — On compaction recurrence

If you compact again mid-step:
- The hook fires `step_resume()` automatically on the next prompt.
- Continue from the same `Step <K>` — do not restart it. The notes column
  has your in-progress reasoning.

## Honesty Rules

- Never restart from Step 1 if the bundle says you are at Step K.
- Never skip the acceptance command. `step_complete` will refuse to advance
  if `step_verify` hasn't passed (override only with `force=true`, and
  only with explicit user permission).
- If the bundle is empty (no plan exists), call `step_plan_from(<md>)`
  with whatever roadmap doc the user has, OR ask the user for a plan
  before starting work.
