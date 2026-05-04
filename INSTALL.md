# INSTALL.md - mneme v0.3.2 (hotfix 2026-05-02)

Three ways to use mneme:

- Install from the public bootstrap (one command per OS, recommended)
- Install from the home zip (offline-friendly, exact build artifact)
- Work on the source (edit + build)

All three are documented below.

---

## Public bootstrap (one command per OS)

Each script auto-detects your architecture (x64 / ARM64), downloads the matching binary archive, pulls 5 model files (~3.4 GB) from the [Hugging Face Hub mirror](https://huggingface.co/aaditya4u/mneme-models) with GitHub Releases as automatic fallback, registers the MCP server + plugin commands + hooks, and starts the daemon.

### Windows (x64 / ARM64)

```powershell
# PowerShell * no admin needed * auto-detects PROCESSOR_ARCHITECTURE
iex (irm https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/bootstrap-install.ps1)
```

### macOS (Apple Silicon arm64)

```bash
# auto-detects via uname -m
curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-mac.sh | bash
```

> **Intel Macs (x86_64) are not supported in v0.3.2.** v0.3.2 ships only an `aarch64-apple-darwin` binary. Intel Mac users must build from source: `git clone` + `cargo build --release --workspace`. Native Intel Mac binaries may return in a later release if GitHub-hosted runner capacity recovers.

### Linux (x64 / ARM64)

```bash
# auto-detects via uname -m
curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-linux.sh | bash
```

### Python wrapper (any OS, pip-friendly)

```bash
pip install mnemeos
mnemeos                    # canonical command
# or
mneme                      # legacy alias, same binary
```

The Python wrapper detects your platform (`sys.platform`) and architecture, downloads the matching bootstrap script (Windows / macOS / Linux), verifies its SHA-256, and runs it. Same install path as the OS-native commands above - just gets you there from `pip` if that's where your toolchain lives.

The PyPI distribution name is `mnemeos` (the bare `mneme` was claimed on PyPI in 2014 by an unrelated Flask-based note-taking app). The CLI command can be invoked as `mnemeos` (canonical) or `mneme` (legacy alias) - both refer to the same binary.

### Requirements

| Requirement | Detail |
|---|---|
| **OS** | 64-bit Windows 10/11 (x64 live, arm64 in CI build), macOS 14+ Apple Silicon (arm64 live; Intel x86_64 build-from-source only in v0.3.2), Ubuntu 22.04+ / Debian / Fedora (x64 live, arm64 in CI build) |
| **CPU baseline** | x86-64-v3 - AVX2 / BMI2 / FMA. Intel Haswell (2013+) or AMD Excavator (2015+). Almost every PC sold since 2013 qualifies. The bootstrap refuses pre-Haswell hardware with a clear error. |
| **Disk** | 5 GB free (binaries ~250 MB, models ~3.4 GB, room for first project's shards) |
| **Privileges** | No admin needed. Defender exclusions are added best-effort if elevated; install proceeds without them otherwise. |
| **32-bit Windows** | **Not supported.** Bun runtime requires x64 or ARM64. The bootstrap detects and refuses with `Fail "32-bit Windows is not supported (Bun runtime requires x64 or ARM64)..."`. |

---

## Offline / home-zip install

Use this when you have the `mneme final` home zip and want to install the binary artifact from disk (no network call to GitHub Releases). Same outcome as the public bootstrap, sourced from the local zip.

```powershell
# 1. Open PowerShell (any user, no admin needed)
# 2. Extract the home zip wherever you want, then:
cd "<extracted-path>\mneme final 2026-04-29\release"
Expand-Archive -Path mneme-v0.3.2-windows-x64.zip -DestinationPath "$env:USERPROFILE\.mneme" -Force
cd "$env:USERPROFILE\.mneme"
.\scripts\install.ps1 -LocalZip "<extracted-path>\mneme final 2026-04-29\release\mneme-v0.3.2-windows-x64.zip"
```

Either path - the installer:

1. Detects OS + architecture (x64 / ARM64) and refuses early if the CPU lacks AVX2 / BMI2 / FMA (pre-Haswell)
2. Stops any running mneme processes (3-pass kill ladder - graceful then taskkill then hard abort if locks remain)
3. Verifies Bun is installed (installs it if missing)
4. **Runs `bun install --frozen-lockfile`** in `~/.mneme/mcp/` to populate `node_modules` (B1 hotfix 2026-05-02 - without this the MCP server crashed on first start with `ENOENT while resolving package 'zod'`)
5. Adds `~/.mneme/bin` to user PATH
6. Adds Defender exclusions for `~/.mneme` and `~/.claude` (best-effort if not elevated)
7. Pulls 5 model files from the [Hugging Face Hub mirror](https://huggingface.co/aaditya4u/mneme-models) (`bge-small-en-v1.5.onnx` + `tokenizer.json` + `qwen-embed-0.5b.gguf` + `qwen-coder-0.5b.gguf` + `phi-3-mini-4k.gguf` as a single 2.23 GB file - no part-merge anymore), with GitHub Releases as automatic fallback
8. Starts the mneme daemon in the background
9. Registers the mneme MCP server with Claude Code: writes the `mcpServers.mneme` entry to `~/.claude.json` AND, by default (K1 fix in v0.3.2), writes the 8 mneme hook entries under `~/.claude/settings.json::hooks` so the persistent-memory pipeline (history.db, tasks.db, tool_cache.db, livestate.db) actually fills. Pass `--no-hooks` / `--skip-hooks` to opt out. Hook bodies are crash-safe: every hook binary reads STDIN JSON and exits 0 on any internal error, so a mneme bug can never block your tool calls.
10. Registers the mneme **plugin slash commands** (`/mn-build`, `/mn-recall`, `/mn-why`, `/mn-resume`, `/mn-blast`, `/mn-doctor`, ...) with Claude Code so they show up in autocomplete (B1.5 hotfix 2026-05-02)
11. Verifies post-install: every required binary present, daemon responding, MCP probe green

**To verify it worked:**

```powershell
mneme --version           # should print 0.3.2
mneme doctor              # full diagnostic boxes (Bun, Node, Git, MSVC, ~/.mneme, ~/.claude)
mneme cache du            # show what mneme is using on disk
claude mcp list           # should show: mneme: ✓ Connected
```

If `claude mcp list` shows mneme connected, the headline test passes.

---

## First-run sanity (after install)

```powershell
# index this very project (or any project you want)
cd C:\path\to\some\project
mneme build .

# basic ops
mneme status             # what's indexed, last build time
mneme cache du           # disk usage breakdown by directory + per project
mneme cache du --json    # same data as JSON for scripting
mneme daemon status      # is the supervisor up? per-worker pid/uptime
mneme daemon logs        # tail the last 200 lines of daemon log
```

If any of these print sensible output, you're set.

---

## Operational (run + abort + self-update)

### Aborting an in-flight job

Long-running operations (`mneme build`, `mneme audit`, `mneme graphify`,
`mneme update`) can be aborted cleanly. The supervisor catches the abort
signal, flushes any partial work to the on-disk shards, and stops the
worker pool without orphaning subprocesses or leaving WAL files in a torn
state.

```powershell
mneme abort                       # ask the active session to stop (SIGINT-like)
mneme abort --force               # immediate stop (SIGKILL-like); used only if the soft
                                  #   path stalls past --timeout-secs
mneme abort --all                 # abort every active job across every project for this user
mneme abort --timeout-secs 30     # how long the soft path waits before escalating
                                  #   (default: 10s)
```

The graceful path is always tried first. `--force` is a last-resort
escape hatch and is documented because it has to exist, not because you
should reach for it.

### Self-updating the binaries

`mneme self-update` is a cross-OS in-place upgrade of the `~/.mneme/bin/`
binaries. It performs the daemon-stop / download / SHA-256 verify /
atomic-rename / daemon-restart sequence the install scripts would run on
a fresh box, but without re-running the full installer.

```powershell
mneme self-update                 # stop daemon, fetch latest release, verify, swap, restart
mneme self-update --dry-run       # show what would happen, change nothing
mneme self-update --pin 0.3.2     # pin to a specific version instead of "latest"
mneme self-update --allow-unsigned
                                  # skip signature verification (DEV use only — prefer SHA-256)
```

What it actually does, in order:

1. Calls `mneme daemon stop` and waits for the supervisor + worker pool
   to exit cleanly.
2. Resolves the target version (default: latest GitHub release tag).
3. Downloads the platform-correct release zip + `release-checksums.json`
   sidecar from the GitHub release.
4. Verifies SHA-256 of the zip against the sidecar manifest. Aborts on
   mismatch.
5. Extracts to a temp dir, then atomic-renames into `~/.mneme/`.
6. Restarts the daemon. Final `mneme doctor` confirms the upgraded
   binaries are live.

If a step fails, the previous installation is left in place. There is no
torn state where some binaries upgraded and others didn't.

`--allow-unsigned` exists for development builds where the signature
sidecar isn't published. SHA-256 still runs; the flag only skips the
`minisign` / `cosign` signature step. Production users should not pass
this flag.

---

## Cache management (since v0.3.0)

The `~/.mneme/projects/<id>/` shards grow with each indexed project. To reclaim space:

```powershell
mneme cache du                                  # see what's using disk
mneme cache prune --older-than 30d --dry-run    # preview which snapshots would be deleted
mneme cache prune --older-than 30d              # delete snapshots older than 30 days
mneme cache gc --dry-run                        # preview which DBs would be VACUUMed
mneme cache gc                                  # VACUUM all shard DBs (+ wal_checkpoint truncate)
mneme cache drop <project-path> --yes           # nuke a single project's cache (destructive)
```

Without these, first-launch users would have no recovery path on small drives. Now they do.

---

## Working on the source

```powershell
# 1. Extract the home zip
# 2. Open a shell in mneme-home-package\source\
cd "<extracted-path>\mneme-home-package\source"

# 3. Build everything
cargo build --workspace --release

# 4. Run individual crate tests
cargo test --workspace

# 5. MCP server (TypeScript)
cd mcp
bun install      # only if node_modules wasn't included in zip
bunx tsc --noEmit
bun test
```

The home zip ships with `mcp/node_modules` (~100 MB, needed for MCP to run pre-built) but NOT `vision/node_modules` (~576 MB, regenerable from `bun install`).

---

## 6. Vision app (Tauri)

> **STATUS:** shipped (vision SPA at `static/vision/`; `mneme-vision.exe`
> in bin payload; SPA fallback via explicit-route handler). All 17
> `/api/graph/*` endpoints respond with real shard data. Cycle-3 EC2
> verified the round-trip (`GET /` and SPA-router URLs) via Wave 3
> Agent M's cached-`Arc<[u8]>` handler at `supervisor/src/health.rs:317-411`.

The CLI command `mneme view` launches `mneme-vision.exe` from
`~/.mneme/bin/`. The browser fallback at `http://127.0.0.1:7777/` serves
the dashboard from `~/.mneme/static/vision/index.html` - see the
"v0.3 Known Limitations" table below for the full status matrix.

### Prerequisites for vision dev

To run `tauri dev` (or build the vision app from source), the following
must be on PATH:

| Prerequisite | Why required | Install |
|---|---|---|
| **Bun 1.3+** | `tauri.conf.json` declares `"beforeDevCommand": "bun server.ts"` - `tauri dev` invokes Bun to start the dev API server. Production builds also need Bun to run `vite build`. | Windows: `irm bun.sh/install.ps1 \| iex` * macOS/Linux: `curl -fsSL https://bun.sh/install \| bash` |
| **Rust 1.78+ + cargo** | Tauri shell compiles with `cargo build --release` inside `vision/tauri/` | Standard `rustup` install |
| **Platform Tauri deps** | Windows: WebView2 (preinstalled on Win 11) + MSVC Build Tools * macOS: Xcode CLT * Linux: `webkit2gtk-4.1`, `libsoup-3.0`, `libgtk-3` | Per-platform - see [tauri.app/start/prerequisites](https://tauri.app/start/prerequisites/) |

Without Bun on PATH, `tauri dev` fails cryptically (the `beforeDevCommand`
errors before Tauri reports a useful diagnostic). Production `tauri build`
shipped binaries do **not** need Bun at runtime - Bun is a dev-only tool.

### Bun install (vision dev)

The Tauri config (`vision/tauri/tauri.conf.json`) has
`"beforeDevCommand": "bun server.ts"` - `tauri dev` will fail cryptically
without Bun on PATH. Install Bun 1.3+ first:

```powershell
# Windows
irm bun.sh/install.ps1 | iex
```

```bash
# macOS / Linux
curl -fsSL https://bun.sh/install | bash
```

### What's missing in source (you must add these to build)

| Missing file | Why required | Symptom if absent |
|---|---|---|
| `vision/tauri/build.rs` | `tauri::generate_context!()` requires `OUT_DIR` set by `tauri-build` | `error: OUT_DIR env var is not set, do you have a build script?` |
| `vision/tauri/icons/icon.png` | Referenced as `bundle.icon` in `tauri.conf.json` | Bundle stage fails |
| `vision/tauri/icons/icon.ico` | Tauri-build embeds .ico into Windows .exe via Windows Resource | `'icons/icon.ico' not found; required for generating a Windows Resource file during tauri-build` |

Minimum viable `build.rs`:

```rust
// vision/tauri/build.rs
fn main() {
    tauri_build::build()
}
```

Generate `icons/icon.ico` from any PNG via `magick convert`, Python PIL,
or an online ICO generator (multi-size: 16/32/48/64/128/256).

### Known gotchas (read before you build)

1. **Workspace mismatch.** `vision/tauri/` is neither in `[workspace.members]`
   nor `[workspace.exclude]` in the root `Cargo.toml`. Running `cargo build
   --release` inside `vision/tauri/` errors with `current package believes
   it's in a workspace when it's not`. Fix: add an empty `[workspace]`
   table to `vision/tauri/Cargo.toml` OR add the path to
   `workspace.exclude` at the root.

2. **Hardcoded `"url": "http://127.0.0.1:7777"` in `tauri.conf.json`
   window config.** When Tauri opens, the window loads the daemon root -
   which 404s. The bundled `frontendDist: "../dist"` is never used. Fix:
   remove the `url` field; Tauri 2.0 will then load `index.html` from
   `frontendDist` via the `tauri://` custom protocol.

3. **Frontend uses relative URLs that don't resolve in production
   Tauri.** `vision/src/api.ts`, `vision/src/api/graph.ts`,
   `vision/src/components/SidePanel.tsx`, and
   `vision/src/components/TimelineScrubber.tsx` all `fetch("/api/graph/...")`.
   When the page is loaded via `tauri://localhost/index.html`, those
   relative fetches resolve to `tauri://localhost/api/graph/*`, which
   Tauri's custom-protocol handler answers with the bundled `index.html`
   (SPA fallback). Result: `Unexpected token '<', "<!DOCTYPE "... is not
   valid JSON` - empty dashboard. The frontend was designed to talk to
   the Bun server in `vision/server.ts`; in production nothing spawns it.

4. **Vision Bun server defaults to port 7777 - collides with the mneme
   daemon.** `vision/server.ts` reads `process.env.VISION_PORT ?? 7777`.
   The daemon's HTTP `/health` is also on 7777. To run the dev server
   alongside the daemon, set `VISION_PORT=7782` (or anything not 7777).

5. **No production data layer.** Every `/api/graph/*` endpoint is in
   TypeScript-Bun, none in Rust. Even after building the Tauri binary,
   the views are empty because the production Tauri shell has no API to
   talk to. v0.4 plan: either spawn `bun server.ts` from Tauri's
   `main.rs`, or reimplement the 17 endpoints as `#[tauri::command]`
   invocations.

For now, **prefer the CLI + MCP surface**. The 48 MCP tools cover the same
data the views would render.

```powershell
# 6. (skip - vision is not shippable in v0.3)
# When v0.4 lands, this section will become:
#   cd vision
#   bun install
#   bun run build
#   cd tauri
#   cargo build --release
```

`.git/` is included so you can `git log`, branch, commit, and push (you have 63 commits ahead of `origin/main` - `git push origin main` from this checkout pushes all of v0.3.0).

---

## v0.3 Known Limitations

Mirrors the canonical table in [`CLAUDE.md`](CLAUDE.md) §"Known limitations in v0.3" (lines 55-78). v0.3.2 changes are reflected below.

| Surface | Status | Notes |
|---|---|---|
| `mneme view` (Tauri vision app) | shipped (vision SPA at static/vision/; mneme-vision.exe in bin payload; SPA fallback via explicit-route handler) | F1 D2-D4 wired all 17 daemon JSON endpoints + frontend `API_BASE`; 14/14 view components in `vision/src/views/*.tsx` consume real shard data. Browser fallback at `http://127.0.0.1:7777/` serves the dashboard via the cached-`Arc<[u8]>` explicit-route handler at `supervisor/src/health.rs:317-411` (Wave 3 Agent M, cycle-3 EC2 verified). |
| WebSocket livebus relay (`/ws`) | dev-only, partial | `livebus/` crate compiles + SSE/WebSocket schema defined, but production daemon does not host the `/ws` endpoint. Used only in dev when both Bun server and Tauri are local. |
| Voice navigation (`/api/voice`) | stub | Endpoint returns `{enabled: false, phase: "stub"}`. No voice recognition wired. |
| Per-worker `rss_mb` on Windows | resolved (C1 in v0.3.2) | Supervisor SLA snapshot now reports real `rss_mb` values on Windows via `GetProcessMemoryInfo`. Previously always `0`. |
| Tesseract OCR (image text) | **on by default at runtime in v0.3.2 (B-1 fix)** | install.ps1 auto-installs `UB-Mannheim.TesseractOCR` via winget on Windows (and the OS package on macOS/Linux). multimodal-bridge probes both `PATH` and `C:\Program Files\Tesseract-OCR\tesseract.exe` at runtime and shells out. No rebuild needed. Falls back gracefully (logs + skips) if Tesseract isn't found. Whisper / ffmpeg remain compile-time opt-in - planned for v0.5. |
| Real BGE-small ONNX embeddings | **on by default in v0.3.2** | The bootstrap pulls 5 model files (~3.4 GB) from the HF Hub mirror at install time. ONNX Runtime 1.24.4 is bundled in `~/.mneme/bin/onnxruntime.dll`; `brain` auto-pins `ORT_DYLIB_PATH` to it on first BGE call (defeats Win11 24H2 System32 hijack). Set `MNEME_FORCE_HASH_EMBED=1` to bypass BGE if you need the pure-Rust hashing-trick fallback for any reason. |
| Claude Code hooks | default-on (K1 fix in v0.3.2) | `mneme install` now writes the 8 hook entries under `~/.claude/settings.json::hooks` by default. Without hooks the persistent-memory pipeline (history.db, tasks.db, tool_cache.db, livestate.db) stays empty. To skip, pass `--no-hooks` / `--skip-hooks`. Every hook binary reads STDIN JSON and exits 0 on internal error - a mneme bug can never block the user's tool calls. |

For the full list of what shipped, see `docs-and-memory/V0.3.0-WHATS-IN.md`. For phase-A categorisation of remaining issues, see `docs-and-memory/phase-a-issues.md`.

---

## Uninstall

```powershell
# Remove platform configs (Claude Code MCP entry, etc.)
mneme uninstall --platform claude-code

# Full removal: stop daemon, drop PATH entries, remove Defender exclusions, delete state
mneme uninstall --all --purge-state
```

`--all` runs the full nuclear path: taskkill the daemon + workers, drop `~/.mneme/bin` from user PATH, remove Defender exclusions for `~/.mneme` and `~/.claude`, then with `--purge-state` deletes `~/.mneme/` entirely. Without `--purge-state`, project shards survive so you can reinstall later without re-indexing.

---

## Troubleshooting

**`claude mcp list` shows mneme as not connected:**
- Run `mneme doctor` to see what's missing
- Verify `mneme.exe` is on PATH (`where mneme` in PowerShell)
- Verify `~/.claude.json` has `mcpServers.mneme.command` pointing at `mneme.exe` (or full path)
- The installer (since v0.3.0) writes the absolute path via `which::which("mneme")` (closes I-1 from VMware audit)

**Install fails with "FATAL: N mneme process(es) still running":**
- Close any open VS Code with mneme MCP active, or any other Claude session
- Run `mneme daemon stop`
- If still failing: `taskkill /F /T /IM mneme-daemon.exe` then rerun installer

**Install fails because Bun missing:**
- The installer auto-installs Bun via the official Bun installer (bun.sh)
- If that fails: `irm bun.sh/install.ps1 | iex` manually, then rerun mneme installer

**Daemon won't start:**
- `mneme daemon logs --lines 500` - check for panic / config error
- `mneme doctor` - Windows: ensure MSVC Build Tools probe doesn't show MISSING
- Check `~/.mneme/run/daemon.pid` exists; if stale, `mneme daemon stop` then `mneme daemon start`

**`mneme build .` is slow:**
- First run on a large repo (>50K LOC) takes minutes - parsing every file via tree-sitter
- Watch progress: `mneme daemon logs --lines 100` (look for `worker=parsers status=running`)
- Subsequent runs use the incremental cache (~10× faster)

**Disk filling up:**
- `mneme cache du` - see breakdown
- `mneme cache prune --older-than 30d` - drop old snapshots
- `mneme cache gc` - VACUUM shards (typical 20-40% reduction)
- `mneme cache drop <project>` - nuke a project entirely

---

## Where things live on your machine after install

```
%USERPROFILE%\
├── .mneme\
│   ├── bin\                    # 9 mneme binaries (~250 MB)
│   ├── mcp\                    # MCP server (TypeScript + node_modules)
│   ├── projects\<id>\          # per-project shards (graph.db + 25 others)
│   ├── snapshots\<id>\         # point-in-time DB copies
│   ├── cache\                  # docs/embed/multimodal cache (LRU bounded)
│   ├── models\                 # LLM weights (only if --features=llm)
│   ├── install-receipts\       # what install wrote (for clean uninstall)
│   ├── run\daemon.pid          # supervisor PID (for `mneme daemon status`)
│   ├── meta.db                 # global metadata
│   └── supervisor.pipe         # IPC socket name
└── .claude\
    └── (existing Claude config - mneme adds only mcpServers.mneme entry)
```

---

## Related docs in this package

- `docs-and-memory/SESSION-2026-04-25-FINAL.md` - what got built today
- `docs-and-memory/V0.3.0-WHATS-IN.md` - full v0.3.0 feature catalog
- `docs-and-memory/V0.3.1-PLUS-ROADMAP.md` - what's deferred / next-up
- `docs-and-memory/issues.md` - issue tracker (closed + remaining)
- `docs-and-memory/memory/` - maintainer memory files (preserve these on local machine)

---

## License

Apache-2.0. Copyright 2026 Anish Trivedi & Kruti Trivedi.

Sole copyright holder. Permissive: use, modify, distribute, sublicense, including commercially. Requires attribution + NOTICE preservation.
