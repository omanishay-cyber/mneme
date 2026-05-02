# Mneme Installation Guide

This guide is written to be readable by **humans AND by AI coding assistants**
(Claude Code, Cursor, Gemini, Codex, etc.). If a user asks any AI agent "install
mneme", the AI should follow the steps in this document.

---

## TL;DR - one-line install

### Windows (PowerShell, any user)

```powershell
iwr -useb https://raw.githubusercontent.com/omanishay-cyber/mneme/main/scripts/install.ps1 | iex
```

### macOS / Linux (bash, zsh, sh)

```bash
curl -fsSL https://raw.githubusercontent.com/omanishay-cyber/mneme/main/scripts/install.sh | sh
```

### Native package managers (alternative to the one-liners)

These read from the same GitHub release asset; pick whichever feels natural.

**Homebrew** (macOS arm64 + Linux x86_64):
```bash
brew tap omanishay-cyber/mneme https://github.com/omanishay-cyber/homebrew-mneme
brew install mneme
```
Formula source: `dist/homebrew/Formula/mneme.rb`. The tap repo is created by the maintainer when the formula is ready to publish; until then, use the one-liner.

**Scoop** (Windows):
```powershell
scoop bucket add mneme https://github.com/omanishay-cyber/scoop-mneme
scoop install mneme
```
Manifest source: `dist/scoop/mneme.json`. Same publishing caveat.

**Winget** (Windows):
```powershell
winget install Omanishay-Cyber.Mneme
```
Manifest source: `dist/winget/manifests/o/Omanishay-Cyber/Mneme/0.3.0/`. Submitted to `microsoft/winget-pkgs` separately.

**VS Code extension** (Marketplace):
```
ext install Omanishay-Cyber.mneme
```
Source: `vscode/`. The extension auto-runs `mneme register-mcp --platform vscode` on activation, adds a status bar indicator, and exposes 6 commands (Build, Doctor, Recall, Open Vision, Start/Stop daemon).

Both scripts are **idempotent**. Re-run them to upgrade. They will:

1. Stop any running mneme daemon (so the fresh binary is not file-locked).
2. Install any missing runtime deps (Bun, Node, git). Rust is NOT required;
   mneme ships pre-built binaries.
3. Download the latest GitHub release asset for the host platform.
4. Extract to `~/.mneme/`.
5. Add Defender / PATH entries.
6. Start the mneme daemon in the background.
7. Register the MCP server with Claude Code by writing the
   `mcpServers.mneme` entry to `~/.claude.json` AND, by default
   (K1 fix in v0.3.2), the 8 mneme hook entries under
   `~/.claude/settings.json::hooks`. Pass `--no-hooks` /
   `--skip-hooks` to opt out. Hook bodies are crash-safe (read STDIN
   JSON, exit 0 on any internal error) so a mneme bug can never
   block a tool call.

Total install time on a fresh machine: roughly 60-90 seconds.

---

## What gets written to disk

```
~/.mneme/
|- bin/                     pre-built binaries (mneme, mneme-daemon, plus N workers where N = 1 (store) + num_cpus (parsers) + num_cpus/2 (scanners) + 1 (md-ingest) + 1 (brain) + 1 (livebus); ~16 on an 8-core machine)
|- mcp/                     Bun TS MCP server (48 tools)
|- plugin/
|  |- plugin.json
|  |- skills/               20 skills (19 fireworks + mneme-codewords + 3 mneme-*)
|  |- agents/
|  |- commands/
|  |- templates/
|- CLAUDE.md                user-scope mneme manifest (referenced via @include)
|- install-receipts/        rollback receipts with sha256 drift detection

~/.claude.json              ADDS mcpServers.mneme (does not modify any other entry)
~/.claude/settings.json     ADDS the 8 mneme hook entries under hooks.* (default-on; pass --no-hooks to opt out)
```

`~/.claude/settings.json` is touched ONLY for the 8 hook entries
under `hooks.{PreToolUse, PostToolUse, SessionStart, SessionEnd,
UserPromptSubmit, PreCompact, Notification, Stop}`. Every other key
in the file is left intact - backup snapshots are saved alongside
under `~/.claude/settings.json.mneme-<timestamp>.bak` for rollback.
The v0.3.0 install incident (see `mneme-install-report/`) was a
hook-binary bug that blocked tool calls; v0.3.1+ rebuilt every hook
binary to read STDIN JSON and exit 0 on any internal error, so the
incident class is architecturally impossible now.

---

## Clean reinstall (nuke + fresh)

If anything looks wrong, the path is always:

```powershell
# Windows
Get-Process | Where-Object { $_.ProcessName -match '^mneme' } | Stop-Process -Force
Remove-Item -Recurse -Force $env:USERPROFILE\.mneme
iwr -useb https://raw.githubusercontent.com/omanishay-cyber/mneme/main/scripts/install.ps1 | iex
```

```bash
# macOS / Linux
pkill -f mneme || true
rm -rf ~/.mneme
curl -fsSL https://raw.githubusercontent.com/omanishay-cyber/mneme/main/scripts/install.sh | sh
```

If MCP was registered and you want to unregister it:

```bash
mneme unregister-mcp --platform claude-code
```

Or roll the last install back byte-for-byte from the receipts:

```bash
mneme rollback --list        # show every recorded install
mneme rollback               # undo the most recent one
mneme rollback <id>          # undo a specific receipt
```

---

## Register MCP with any of the 18 supported AI tools

After the one-liner completes, mneme is registered with Claude Code. For any of
the other 17 tools, run `mneme register-mcp --platform <name>` once.

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
mneme --version       # should print a version string
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
   - Windows -> run the PowerShell one-liner.
   - macOS / Linux -> run the curl | sh one-liner.
2. Wait for the installer to exit. If exit is non-zero, read the tail of its
   output and report the error verbatim to the user; do NOT retry blindly.
3. After the installer succeeds, verify with `mneme doctor`.
4. Tell the user: "restart this AI tool once so the MCP connection comes up".
5. If the user has OTHER AI tools installed, mention the `mneme register-mcp
   --platform <name>` command for each detected tool.

If the user asks for a clean reinstall, run the nuke + fresh sequence above
instead of the plain installer (locked binaries after a crash will otherwise
skip silently on re-extract).

If the user asks "what did you just install", hand them:

```
~/.mneme/
~/.claude.json       (only the mcpServers.mneme key was added)
```

If the user asks "remove everything", run `mneme rollback` then delete
`~/.mneme/`.

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
copy-paste the printed one-liner into an elevated PowerShell:

```powershell
Add-MpPreference -ExclusionPath "$env:USERPROFILE\.mneme"
Add-MpPreference -ExclusionPath "$env:USERPROFILE\.claude"
```

### `mneme build` asks for confirmation on large paths

That is not a bug. If you point `mneme build` at a directory with more than
10,000 files (your home dir, a node_modules tree, etc.), it refuses to index
without `--yes`. Scope it to your project root and re-run.

---

## For enterprise / air-gapped users

The release ZIP at `https://github.com/omanishay-cyber/mneme/releases/latest`
is self-contained. Download it on a machine with network, transfer to the
target, extract to `~/.mneme/`, add the bin directory to PATH, and run
`mneme daemon start`. No network access is needed at runtime.

If you must install Bun / Node offline, bundle them into `~/.bun/bin/bun.exe`
and `C:\Program Files\nodejs\` yourself; mneme's doctor will find them.

---

## License

Mneme is Apache-2.0. Use it, modify it, redistribute it. Just keep the
copyright and NOTICE file.
