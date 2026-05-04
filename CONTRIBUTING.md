# Contributing to mneme

Thanks for the interest. Rules first, details below.

## Rules

1. Open an issue first for anything bigger than a typo. Avoids duplicated work.
2. One PR = one concern. Don't mix a bug fix, a refactor, and a new feature.
3. Match existing patterns. Mneme's crates have conventions (single-writer-per-shard in store, MPSC for cross-worker IPC, pure-Rust-where-possible for portability). Grep before you invent.
4. Run the checks before you push:
   ```bash
   cargo build --workspace
   cargo clippy --workspace -- -D warnings
   cargo test --workspace
   cd mcp && bun test
   ```
5. No new native DLL deps for the Rust side without discussion. Mneme stays Windows-friendly and statically linkable.
6. By submitting a PR you agree your contribution is licensed under [Apache-2.0](LICENSE).

## What we welcome

- Bug reports. Reproducible beats everything. Include OS, Rust version, Bun version, and the exact `mneme --verbose` output.
- New MCP tools. If you see a gap in the 48 tools, propose one. Pattern: add a new `.ts` file in `mcp/src/tools/`, add a helper in `mcp/src/store.ts` if you need a new DB query shape.
- New scanners. Theme / security / a11y drift rules live in `scanners/src/scanners/*.rs`. Each scanner is one file, one regex ruleset.
- New Tree-sitter language grammars. See `parsers/src/language.rs`. Add the crate dep to `parsers/Cargo.toml` behind a feature flag, register in `Language::ALL`, add queries to `query_cache.rs`.
- Vision views. The 14 view modes in `vision/src/views/` follow a pattern. Add a 15th by copying one and adjusting the rendering.
- Platform installers. Adding support for a new AI tool means one new file under `cli/src/platforms/` and one template under `plugin/templates/`.
- Documentation. README improvements, install guides, troubleshooting entries, FAQ additions all welcome.
- Benchmarks. If you index a large codebase and want to share numbers, PRs to the README's benchmark table are welcome.

## What to open an issue about first (don't just PR)

- Architectural changes (new crate, new IPC channel, new supervised worker)
- Breaking changes to the 22-layer schema
- Changes to the Step Ledger or Command Center behaviour
- Changes to the license or governance
- Swapping runtime deps (Bun -> Node, candle -> ort, etc.)
- Anything touching the install flow

## Local development

```bash
# Clone
git clone https://github.com/omanishay-cyber/mneme
cd mneme

# Rust workspace (the heavy parts)
cargo build --workspace

# Bun MCP server (must use --frozen-lockfile to mirror CI)
cd mcp && bun install --frozen-lockfile && cd ..

# Bun vision app
cd vision && bun install --frozen-lockfile && cd ..

# Run the daemon for the first time
cargo run --bin mneme-supervisor -- start

# From another terminal
cargo run --bin mneme -- build .
cargo run --bin mneme -- status
```

`--frozen-lockfile` is mandatory for `mcp/`. The MCP server is the
security boundary between AI clients and the mneme supervisor; CI
(`.github/workflows/ci.yml::mcp`) and the user installer
(`scripts/install.ps1` step 5b) both pin to `mcp/bun.lock`. Local dev
must do the same so what you ship is what CI tests. The Rust side is
already pinned by `Cargo.lock`.

The Python multimodal sidecar (`workers/multimodal/`) was retired in
v0.2; multimodal extraction is now pure Rust in the
`multimodal-bridge/` crate. Don't add it back.

Need more? See [`docs/dev-setup.md`](docs/dev-setup.md).

## Working on Mneme with Mneme

Mneme is a code-graph + memory tool, and the most efficient way to
work on it is to use it on itself. Once you have a local build, run
`mneme build .` against the mneme source tree and you get the same
recall / blast / audit / step surface that downstream users get on
their own projects.

Recommended starting moves before touching code:

```bash
# 1. Index the workspace
mneme build .

# 2. Find where something lives (better than grep — structural, not textual)
mneme recall "how is the supervisor IPC wired"

# 3. Before refactoring anything load-bearing, check the blast radius
mneme blast crates/store/src/builder.rs
mneme blast --symbol mneme_store::Builder::open

# 4. While developing, run audit to catch drift early
mneme audit

# 5. For multi-step features, plan against the Step Ledger so a
#    Claude Code session can resume across compactions.
mneme step plan-from "wire new MCP tool foo_bar"
mneme step show
```

In Claude Code specifically, the `firestart` codeword loads every
fireworks expert skill and the Step Ledger before you start a long
session. The four codewords (`coldstart`, `hotstart`, `firestart`,
`CHS`) are documented in `plugin/skills/mneme-codewords/SKILL.md`.

If you find a bug while doing this, that's a good bug to file — it
means a real user just hit it.

## Code style

- Rust: `cargo fmt` before every commit. Clippy warnings are errors. No `unwrap()` on user-input paths.
- TypeScript: strict mode. No `any`. Zod at the boundary. Named exports only.
- Python: strict type hints. Pydantic models at IPC boundaries. No sync `requests`; everything async.
- Markdown: wrap at 80 chars for docs; design docs can run long.

## Commit messages

We follow a loose [Conventional Commits](https://www.conventionalcommits.org/) style:

```
feat(mcp): wire recall_decision to history.db
fix(parsers): skip C# grammar at runtime when ABI mismatches
docs(readme): add Windows 11 install note
refactor(store): collapse builder + finder into DbRegistry
```

Not enforced strictly - clear natural prose is fine too.

## PR template

Your PR will get a template. Short version of what's in it:

- **What changes** (1–2 sentences)
- **Why** (what user problem or architectural gap)
- **How tested** (unit, integration, manual)
- **Screenshots** for any UI change
- **Benchmark numbers** for any perf-related change
- **Rollback plan** for any risky refactor

## Communication

- GitHub Issues: bug reports, feature requests, commercial licensing inquiries (public, search-indexed).
- GitHub Discussions: architecture questions, use cases, "is this a good idea?" conversations.

## Security

If you find a security issue - **please don't open a public issue**. Use GitHub's private security advisory feature:
**https://github.com/omanishay-cyber/mneme/security/advisories/new**

Include:

- A description of the vulnerability
- Steps to reproduce
- Proposed fix (if any)
- Whether you'd like credit in the advisory

We'll acknowledge within 72 hours.

## Code of conduct

See [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md). Be kind, be specific, be generous with reviews. That's it.

---

Thanks. Every contribution makes mneme better for everyone who uses it.

- Anish Trivedi & Kruti Trivedi
