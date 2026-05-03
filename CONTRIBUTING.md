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

# Bun MCP server
cd mcp && bun install && cd ..

# Bun vision app
cd vision && bun install && cd ..

# Python multimodal sidecar
cd workers/multimodal && pip install -e . && cd ../..

# Run the daemon for the first time
cargo run --bin mneme-supervisor -- start

# From another terminal
cargo run --bin mneme -- build .
cargo run --bin mneme -- status
```

Need more? See [`docs/dev-setup.md`](docs/dev-setup.md).

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
