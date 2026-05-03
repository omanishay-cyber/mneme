# Mneme Installation Guide

This guide is written to be readable by **humans AND by AI coding assistants**
(Claude Code, Cursor, Gemini, Codex, etc.). If a user asks any AI agent "install
mneme", the AI should follow the steps in this document.

Current version: **v0.3.2** (hotfix, 2026-05-02).

---

## TL;DR - one command per OS

Each one-liner downloads the v0.3.2 release bootstrap script, auto-detects your
CPU architecture, fetches the right binary archive, extracts to `~/.mneme`,
registers the MCP server with Claude Code, and starts the daemon.

### Windows (PowerShell, any user, no admin needed)

```powershell
iex (irm https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/bootstrap-install.ps1)
```

### macOS (Intel or Apple Silicon)

```bash
curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-mac.sh | bash
```

### Linux (x86_64 or aarch64)

```bash
curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-linux.sh | bash
```

> **Requirements:** 64-bit OS (x64 or ARM64) * CPU with AVX2 / BMI2 / FMA
> (Intel Haswell 2013+ or AMD Excavator 2015+ - virtually every PC sold since
> 2013) * 5 GB free disk * no admin required. 32-bit Windows is not supported
> (Bun runtime requirement). The binaries are built against the
> `x86-64-v3` baseline so older CPUs without AVX2 will fail at runtime - see
> [`docs/faq.md`](faq.md) for what to do on older hardware.
>
> **Restart Claude Code after install.** Verify with `mneme doctor` and
> `claude mcp list`.

### What the bootstrap actually does

The Windows / macOS / Linux installers all do the same thing - they wrap the
release-asset download for the host platform behind a single command:

1. Stop any running mneme daemon (so the fresh binary is not file-locked).
2. Detect host OS + CPU arch (`x64` or `arm64`) and pick the matching tarball.
3. Download the `mneme-v0.3.2-<os>-<arch>.zip` (or `.tar.gz`) release asset.
4. Verify SHA256, extract to `~/.mneme/`.
5. Download model assets from Hugging Face Hub (primary) - bge-small-en-v1.5
   (~33 MB), Qwen 2.5 Coder 0.5B (~340 MB), Qwen 2.5 Embed 0.5B (~340 MB),
   and Phi-3-mini-4k Q4_K_M (~2.28 GB). If HF is unreachable the bootstrap
   falls back to GitHub Releases (Phi-3 is split into `.part00` + `.part01`
   on the GitHub fallback because GitHub caps individual release assets at
   2 GB; the bootstrap concatenates them inline).
6. Add `~/.mneme/bin` to PATH and (Windows only) add Defender exclusions when
   admin rights are available.
7. Register MCP with Claude Code by writing `mcpServers.mneme` to
   `~/.claude.json` AND, by default, the 8 mneme hook entries under
   `~/.claude/settings.json::hooks`. Pass `--no-hooks` / `--skip-hooks` to
   opt out. Hook bodies are crash-safe (they read STDIN JSON and exit 0 on
   any internal error), so a mneme bug can never block a tool call.
8. Auto-register the plugin slash commands (`/mn-build`, `/mn-recall`,
   `/mn-doctor`, `/mn-resume`, etc.) so Claude Code surfaces them.
9. Start the mneme daemon in the background.

Total install time on a fresh machine: roughly 60-120 seconds depending on
download speed (the Phi-3 weights dominate).

The installers are **idempotent** - re-run them to upgrade or repair a broken
install.

---

## What gets written to disk

```
~/.mneme/
|- bin/                     pre-built binaries (mneme, mneme-supervisor,
|                            mneme-mcp + 9 worker exes; total ~250 MB)
|- mcp/                     Bun TS MCP server (48 tools)
|- models/                  bge-small + qwen-coder/embed + phi-3-mini-4k
|- plugin/
|  |- plugin.json
|  |- skills/               20 skills (19 fireworks + mneme-codewords)
|  |- agents/
|  |- commands/             /mn-build, /mn-recall, /mn-doctor, etc.
|  |- templates/
|- CLAUDE.md                user-scope mneme manifest (referenced via @include)
|- install-receipts/        rollback receipts with sha256 drift detection
|- run/                     PID file + IPC discovery (named pipe / unix socket)
|- logs/                    supervisor + worker logs

~/.claude.json              ADDS mcpServers.mneme (does not modify any other entry)
~/.claude/settings.json     ADDS the 8 mneme hook entries under hooks.* (default-on)
```

`~/.claude/settings.json` is touched ONLY for the 8 hook entries
under `hooks.{PreToolUse, PostToolUse, SessionStart, SessionEnd,
UserPromptSubmit, PreCompact, Notification, Stop}`. Every other key
in the file is left intact - backup snapshots are saved alongside
under `~/.claude/settings.json.mneme-<timestamp>.bak` for rollback.

---

## Clean reinstall (nuke + fresh)

If anything looks wrong, the path is always:

```powershell
# Windows
Get-Process | Where-Object { $_.ProcessName -match '^mneme' } | Stop-Process -Force
Remove-Item -Recurse -Force $env:USERPROFILE\.mneme
iex (irm https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/bootstrap-install.ps1)
```

```bash
# macOS / Linux
pkill -f mneme || true
rm -rf ~/.mneme
# macOS:
curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-mac.sh | bash
# Linux:
curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-linux.sh | bash
```

If MCP was registered and you want to unregister it:

```bash
mneme unregister-mcp --platform claude-code
```

Or roll the last install back from the receipts:

```bash
mneme rollback --list        # show every recorded install
mneme rollback               # undo the most recent one
mneme rollback <id>          # undo a specific receipt
```

---

## Register MCP with any of the 19 supported AI tools

After the one-liner completes, mneme is registered with Claude Code. For any of
the other 18 tools, run `mneme register-mcp --platform <name>` once.

| AI tool | Command | Writes to |
|---|---|---|
| Claude Code | `mneme register-mcp --platform claude-code` | `~/.claude.json` |
| Cursor | `mneme register-mcp --platform cursor` | `~/.cursor/mcp.json` |
| VS Code (Copilot + Claude ext) | `mneme register-mcp --platform vscode` | `%APPDATA%\Code\User\mcp.json` on Windows; platform-specific on macOS/Linux |
| Windsurf | `mneme register-mcp --platform windsurf` | `~/.codeium/windsurf/mcp_config.json` |
| Zed | `mneme register-mcp --platform zed` | `~/.config/zed/settings.json` |
| Continue | `mneme register-mcp --platform continue-dev` | `~/.continue/config.json` |
| Codex (OpenAI) | `mneme register-mcp --platform codex` | `~/.codex/config.toml` |
| Gemini CLI | `mneme register-mcp --platform gemini-cli` | `~/.gemini/settings.json` |
| Aider | `mneme register-mcp --platform aider` | `~/.aider.conf.yml` |
| GitHub Copilot CLI | `mneme register-mcp --platform copilot` | `~/.config/github-copilot/mcp.json` |
| OpenCode | `mneme register-mcp --platform opencode` | `~/.config/opencode/mcp.json` |
| Google Antigravity | `mneme register-mcp --platform antigravity` | platform-specific |
| Factory Droid | `mneme register-mcp --platform factory-droid` | platform-specific |
| Trae / Trae-CN | `mneme register-mcp --platform trae` | platform-specific |
| Kiro | `mneme register-mcp --platform kiro` | `.kiro/steering/mneme.md` + MCP entry |
| Qoder | `mneme register-mcp --platform qoder` | `QODER.md` + MCP entry |
| OpenClaw | `mneme register-mcp --platform openclaw` | platform-specific |
| Hermes | `mneme register-mcp --platform hermes` | platform-specific |
| Qwen Code | `mneme register-mcp --platform qwen` | `QWEN.md` + MCP entry |

All platforms share the same `mneme mcp stdio` command under the hood. Every
registration writes ONLY the mcpServers entry. No hooks, no settings.json, no
surprise files.

---

## Verification

After install, all three checks should pass:

```
mneme --version       # should print "mneme 0.3.2"
mneme daemon status   # should show "running" + worker list
mneme doctor          # prints a full health box including per-MCP-tool status
```

From inside Claude Code (after restarting it):

```
/mcp                  # should list mneme in the Connected row
/mn-recall "hello"    # should return zero or more hits, not an error
/mn-doctor            # should print the same doctor output
```

If any of these fail, run `mneme doctor --json` and attach the output to a
GitHub issue.

---

## Protocol for AI agents

If the user types "install mneme" or equivalent in any AI coding tool, the
agent should:

1. Detect the host OS:
   - Windows -> run the PowerShell one-liner (`iex (irm ...bootstrap-install.ps1)`).
   - macOS -> run the macOS one-liner (`curl -fsSL ...install-mac.sh | bash`).
   - Linux -> run the Linux one-liner (`curl -fsSL ...install-linux.sh | bash`).
2. Wait for the installer to exit. If exit is non-zero, read the tail of its
   output and report the error verbatim to the user; do NOT retry blindly.
3. After the installer succeeds, verify with `mneme doctor`.
4. Tell the user: "restart this AI tool once so the MCP connection comes up".
5. If the user has OTHER AI tools installed, mention the `mneme register-mcp
   --platform <name>` command for each detected tool.

If the user asks for a clean reinstall, run the nuke + fresh sequence above
instead of re-running the plain installer (locked binaries after a crash will
otherwise skip silently on re-extract).

If the user asks "what did you just install", hand them:

```
~/.mneme/            (binaries, models, MCP server, plugin)
~/.claude.json       (only the mcpServers.mneme key was added)
~/.claude/settings.json (only the 8 hook entries under hooks.* were added)
```

If the user asks "remove everything", run `mneme rollback` then delete
`~/.mneme/` (and remove the `mcpServers.mneme` block from `~/.claude.json`
plus the `hooks.*.mneme-*` entries from `~/.claude/settings.json` - the
rollback receipts handle this automatically).

---

## Troubleshooting

### "mneme: command not found" after install

Open a new terminal. The installer adds `~/.mneme/bin` to the user PATH, but
existing shells keep the old PATH until they restart. In PowerShell you can
force-refresh with:

```powershell
$env:PATH = [Environment]::GetEnvironmentVariable('PATH','User') + ';' + [Environment]::GetEnvironmentVariable('PATH','Machine')
```

### "MCP server failed to reconnect" in Claude Code

The daemon probably is not running. Try:

```bash
mneme daemon status            # is it up?
mneme daemon start             # start it
mneme daemon logs --lines 100  # see what happened
```

Or just run `mneme doctor` - it diagnoses all three (supervisor, MCP bridge,
and the 48 individual tools).

### Windows Defender flags mneme binaries

Mneme's log files and memory dumps contain agent-automation language
("pre-tool", "hook", "inject", "exec") that matches Defender's
`SAgent.HAG!MTB` ML heuristic. The installer adds `~/.mneme/` to Defender
exclusions when run with admin rights. If you installed without admin,
copy-paste this into an elevated PowerShell:

```powershell
Add-MpPreference -ExclusionPath "$env:USERPROFILE\.mneme"
Add-MpPreference -ExclusionPath "$env:USERPROFILE\.claude"
```

### "Illegal instruction" / `EXC_BAD_INSTRUCTION` on launch

Your CPU is older than Haswell (Intel 2013) / Excavator (AMD 2015) and lacks
AVX2 / BMI2 / FMA - the `x86-64-v3` baseline the binaries are compiled
against. Build from source on that machine to drop the baseline (see
[`docs/dev-setup.md`](dev-setup.md)).

### `mneme build` asks for confirmation on large paths

That is not a bug. If you point `mneme build` at a directory with more than
10,000 files (your home dir, a node_modules tree, etc.), it refuses to index
without `--yes`. Scope it to your project root and re-run.

### Audit hangs on a giant project

v0.3.2 replaced the wall-clock `MNEME_AUDIT_TIMEOUT_SEC` (now removed) with a
per-line stall detector (`MNEME_AUDIT_LINE_TIMEOUT_SEC`, default 30s). On a
multi-hour audit of a giant project the per-line guard alone keeps the audit
unstuck without binning legitimate long runs. Findings stream incrementally
into `findings.db`, so you can `mneme audit` again on a subset and pick up
where it left off. See [`docs/env-vars.md`](env-vars.md) for the full env
reference.

### Phi-3 download fails midway

The bootstrap downloads from Hugging Face Hub first and falls back to GitHub
Releases. On the GitHub path Phi-3 ships as two `.part00` + `.part01` files
that get concatenated locally (GitHub's 2 GB single-asset cap forces this).
If the bootstrap can reach neither, install Phi-3 by hand:

```bash
mneme models install --from-path /path/to/local/mirror
```

The local mirror just needs the four `.gguf` / `.onnx` files from the HF mirror
at https://huggingface.co/aaditya4u/mneme-models in any directory.

---

## For enterprise / air-gapped users

The release ZIP at
`https://github.com/omanishay-cyber/mneme/releases/tag/v0.3.2` is
self-contained for the binaries. Download it on a machine with network,
transfer to the target, extract to `~/.mneme/`, add the bin directory to
PATH. For models, download from the HF mirror
https://huggingface.co/aaditya4u/mneme-models on a connected machine and
copy them to the target. Then:

```bash
mneme models install --from-path /path/to/local/mirror
mneme daemon start
```

No network access is needed at runtime once models + binaries are in place.

If you must install Bun / Node offline, bundle them into `~/.bun/bin/bun.exe`
(Win) or `~/.bun/bin/bun` (Unix) yourself; mneme's doctor will find them.

---

## See also

- [`docs/faq.md`](faq.md) - common questions
- [`docs/dev-setup.md`](dev-setup.md) - build from source instructions
- [`docs/architecture.md`](architecture.md) - how mneme is built
- [`docs/mcp-tools.md`](mcp-tools.md) - reference for every MCP tool
- [`docs/env-vars.md`](env-vars.md) - all `MNEME_*` env vars

---

## License

Mneme is Apache-2.0. Use it, modify it, redistribute it. Just keep the
copyright and NOTICE file. Some commercial restrictions apply - see
[`LICENSE`](../LICENSE) and [`docs/faq.md`](faq.md#license--commercial-use).
