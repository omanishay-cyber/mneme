---
name: mneme-codewords
version: 1.0.0
author: mneme
description: >
  Workflow codewords that give users a single-word way to switch how the AI
  engages with their task. `coldstart` pauses into observe-only mode,
  `hotstart` engages a disciplined numbered-roadmap execution pass, `firestart`
  is maximum-loadout: all fireworks skills + mneme graph priming + ledger.
  Activates whenever the user types one of these codewords as a message or at
  the start of a message. This is mneme's signature developer-ergonomic
  feature.
triggers:
  - coldstart
  - hotstart
  - firestart
  - CHS
  - "check my screenshot"
tags:
  - workflow
  - codewords
  - discipline
  - session-control
  - mneme
---

# Mneme Codewords — session-control verbs

Four short words that switch how the AI should engage with the user's task.
These are **mneme's signature developer-ergonomic feature**: instead of
retyping a long instruction ("pause, just observe, take notes, don't touch
code"), the user types one word and the AI knows exactly what to do.

## The four codewords

### `coldstart` — observe-only mode

When the user types `coldstart`:

1. STOP all code-modifying work immediately.
2. Read the current project context (git status, recent files, open PRs).
3. Call `step_plan_from_markdown` if a plan document exists, otherwise draft
   a plan in conversation.
4. Print the plan + open questions + what's ambiguous.
5. **Do NOT write, edit, commit, push, or run mutating commands.**
6. Wait for `hotstart` or `firestart` before doing any actual work.

Use when the user is thinking through a problem and needs the AI to observe
without acting. The default failure mode — jumping in to "help" prematurely —
is exactly what this mode prevents.

### `hotstart` — disciplined execution

When the user types `hotstart`:

1. Resume working on the active plan or ledger.
2. Call `step_resume()` first to rebuild where the previous session left off.
3. For each step in the ledger:
   - `step_show(step_id)` — read the step
   - Do the work
   - `step_verify(step_id)` — run the acceptance check
   - `step_complete(step_id)` — only after verify passes
4. Never skip verification.
5. Update the ledger after every meaningful change.
6. When all steps are `verified=true`, announce completion and stop.

The rule is **one step at a time, always verify, never skip**. If the user
has not yet run `coldstart`, hotstart implicitly builds a plan first using
the current conversation as the spec.

### `firestart` — maximum loadout

When the user types `firestart`:

1. Load ALL fireworks skills in addition to mneme's own (architect, debug,
   workflow, refactor, review, test, performance, security, design, research,
   patterns, config, devops, estimation, patterns, taskmaster, and any
   language-specific skills that match the active stack).
2. Prime the context with mneme graph queries:
   - `god_nodes()` to see the most-connected concepts
   - `audit_corpus()` to see drift findings
   - `recall_todo()` to see open TODOs
   - `recall_decision("<active-task-topic>")` to see prior decisions
3. Then proceed with `hotstart` — disciplined numbered-roadmap execution.

Use when the task is complex enough to need the full toolkit. Firestart is
heavy — it burns tokens on context priming. Reserve it for features that span
multiple files or for session starts where the AI needs full situational
awareness. For quick fixes, `hotstart` alone is cheaper and faster.

### `CHS` — check my screenshot

When the user types `CHS`:

1. Look in the OS-native screenshot directory:
   - Windows: `%USERPROFILE%\Pictures\Screenshots\`
   - macOS: `~/Desktop/` (default macOS screenshot destination)
   - Linux: `~/Pictures/Screenshots/` or `~/Pictures/`
2. Find the most recently modified file (by mtime).
3. Read it (screenshot file types: .png / .jpg / .jpeg / .webp / .heic).
4. Describe what you see and respond to the user based on the screenshot.

This is how the user communicates visual context without uploading or
pasting — they screenshot, type `CHS`, and the AI sees what they see.

## Why this matters

Every AI coding tool has tools. Nobody ships **workflow verbs** that change
how the AI engages. The codewords combined with mneme's 48 MCP tools + the
fireworks skills library + the Step Ledger = a complete workflow OS.

**Rule of thumb:** when a codeword is the first word (or only word) in a user
message, it takes precedence over any other interpretation. `firestart` does
not mean "write a paragraph about firestarting," it means activate the
firestart protocol above.

## What the AI must NOT do

- Never interpret these codewords as casual conversation. They are commands.
- Never proceed past `coldstart` without waiting for `hotstart` / `firestart`.
- Never skip `step_verify` during `hotstart` — that defeats the whole pattern.
- Never load fireworks skills outside of `firestart` — they're opt-in by
  keyword to avoid context bloat.

## Integration with mneme MCP

The codewords pair with these mneme MCP tools:

- `step_resume` — rebuilds session context after compaction (hotstart kicks it off)
- `step_plan_from_markdown` — turns a plan doc into a numbered ledger (coldstart uses it)
- `god_nodes` — central concepts in the graph (firestart primes with it)
- `audit_corpus` — drift findings (firestart shows them up front)
- `recall_decision` — prior decisions on the current topic (firestart surfaces them)
- `suggest_skill` — recommends which fireworks skill to load for a given task

## Authorship

These codewords are a gift from the mneme project to every developer using an
AI coding assistant. Use them. Remix them. Define your own. The pattern is
the real value — one word, one clear engagement mode.
