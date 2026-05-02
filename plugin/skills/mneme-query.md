---
name: mneme-query
description: "Query the local mneme knowledge graph and project memory. Use when the user asks about files, decisions, blast radius, references, architecture, or anything 'what does X do' / 'where is Y used' / 'why did we pick Z'."
version: 1.0.0
author: Anish Trivedi & Kruti Trivedi
triggers: [query, query-the-graph, ask-mneme, /mn-recall, recall]
tags: [mneme, recall, knowledge-graph, memory]
---

# /mn-recall

Search the local mneme knowledge graph + persistent project memory for
the answer to a question — without re-reading any files.

## Usage

```
/mn-recall <free-form question>
/mn-recall "blast radius of src/auth/session.ts"
/mn-recall "decisions about state management"
/mn-recall "open todos tagged ipc"
/mn-recall "everything that calls validateToken"
```

## What this skill does

1. Picks the right mneme MCP tool for the question.
2. Calls it via the stdio MCP server (already running locally).
3. Returns a concise, structured answer.
4. Cites the source rows (with `source_location`) when quoting.

## When to invoke

Triggered by `/mn-recall`, but also use proactively when:

- The user asks "what does X do" — call `recall_file(path=X)`.
- The user asks "where is Y used" — call `find_references(symbol=Y)`.
- The user asks "what breaks if I change Z" — call `blast_radius(target=Z)`.
- The user asks "did we already decide …" — call `recall_decision(query=...)`.
- The user asks "what are the open TODOs" — call `recall_todo()`.
- The user asks "what rules apply to this file" — call
  `recall_constraint(scope='file', file=...)`.

## Procedure

### Step 1 — Pick the tool

Map the question to the matching mneme MCP tool:

| Question pattern | Tool |
|---|---|
| where / who calls / find usages of `X` | `find_references(symbol=X)` |
| what breaks if I change `X` | `blast_radius(target=X)` |
| summary of `<file>` | `recall_file(path=<file>)` |
| decisions about `Y` | `recall_decision(query=Y)` |
| open todos | `recall_todo()` |
| rules for `<file>` | `recall_constraint(scope='file', file=<file>)` |
| concept search across corpus | `recall_concept(query=…)` |
| architecture overview | `god_nodes()` then `audit_corpus()` |

### Step 2 — Call the tool

Invoke via the stdio MCP server (the harness has already auto-registered
mneme from `plugin.json`).

### Step 3 — Render the answer

- Quote the most relevant 1–3 results with their `source_location` (file:line).
- Keep the response under 800 tokens.
- Suggest the **single** most useful follow-up mneme tool call.

## Honesty Rules

- Never invent results. If mneme returns an empty array, say so.
- Always quote `source_location` when citing a specific fact.
- If a recall returns nothing useful, fall back to Grep/Glob — but only
  after mneme returned empty.
