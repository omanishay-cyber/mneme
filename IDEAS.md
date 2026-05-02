# IDEAS.md

Scratchpad for feature ideas that are **not yet committed** to any
release. Keeps scope creep out of the CHANGELOG and ROADMAP.

**Rule:** ideas stay here until they're either (a) promoted to
`docs/dev/v0.4-backlog.md` with an acceptance target, or (b) deleted.

---

## Idea inbox

### Time-travel replay
**Thought:** given the snapshot receipts in `~/.mneme/install-receipts/`,
mneme could replay any historical state of a project's graph. "Show me
what the codebase looked like 3 weeks ago, and tell me what changed
around auth."
**Why defer:** needs a UI, needs time-aware indexing, needs storage
budget planning.

### Mneme-to-mneme federation
**Thought:** one mneme daemon can query another's graph (e.g., two
developers working on the same repo share the same memory without
sharing source). Federated pattern matching is already a partial
primitive.
**Why defer:** privacy model has to be airtight first. Opt-in only.

### VS Code inline context hover
**Thought:** hover over a function name in VS Code, get a panel showing
`blast_radius`, recent history, related decisions, drift findings.
**Why defer:** needs a full VS Code extension (v0.4 stretch item).

### Built-in "why does this file exist" command
**Thought:** `mneme why <file>` runs the `why_chain` tool against the
ledger + git blame + concept graph and produces a 1-paragraph
explanation of why a file was created + what it's for.
**Why defer:** concept is live, needs a nicer CLI wrapper.

### Natural-language rule authoring
**Thought:** user types "never let a React component import from
electron/main", mneme parses it into a scanner rule that fires on drift.
Today rules are hand-authored .scm queries.
**Why defer:** rule parser is non-trivial; start with a DSL, not
free-text.

### Graph-aware commit messages
**Thought:** git commit hook asks mneme "what did this commit touch?"
and auto-drafts a commit body listing affected concepts, tests, and
dependents.
**Why defer:** git hook installation adds friction; make it opt-in
via a separate `mneme git-hook install` command later.

### Zed / Windsurf / Continue native panels
**Thought:** mirror the VS Code extension idea for Zed, Windsurf,
Continue. Each editor gets a mneme sidebar.
**Why defer:** wait for one platform to prove the pattern before
replicating to five.

### Agent-to-agent handoff protocol
**Thought:** when Claude Code finishes a session, mneme bundles the
step ledger + recent decisions into a JSON that Cursor or Codex can
load to resume exactly where Claude left off.
**Why defer:** each AI tool has different context-ingestion formats.
Solve for two before generalizing.

### WASM build for browser-side viewing
**Thought:** compile the query subset of the brain crate to WASM so a
static HTML page can show a read-only graph without a running daemon.
**Why defer:** Rust -> WASM for SQLite is painful (rusqlite needs
custom build); also the 14-view desktop app already covers this.

### Knowledge-base mode (non-code)
**Thought:** user points `mneme build` at their Obsidian vault or
research notes directory. Mneme indexes markdown + PDFs + audio-to-text
transcripts and provides semantic recall across them. The pipeline
already supports this; needs UX polish so non-coders can discover it.
**Why defer:** decide whether mneme's brand is "for developers" or
"for knowledge workers". Can't be both without positioning clarity.

### Multi-shard project support
**Thought:** one `mneme build` indexes a monorepo but treats each
top-level folder as a separate shard that can be queried individually
or unioned. Today shard == project_id == path hash.
**Why defer:** needs a `mneme project split` verb + migration plan for
existing shards.

### Rule-test fixtures
**Thought:** each scanner gets a fixture set - "this input must produce
this finding count". Caught drift in our own drift detector.
**Why defer:** first write 3 fixtures by hand, see if it catches
anything, then decide if a framework is needed.

### Graph-aware refactor preview
**Thought:** before `refactor_apply` runs, show the user a diff of what
the graph will look like after. Blast radius + new edges + removed
edges.
**Why defer:** UI work. Build the primitive first.

---

## How to add an idea

Just append to this file under "Idea inbox" with:

```markdown
### One-line title
**Thought:** what the idea is, in plain prose.
**Why defer:** what would need to happen first.
```

Don't write acceptance criteria here - that's for backlog promotion.
