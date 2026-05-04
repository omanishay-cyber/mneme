# mnemeos

A small Python wrapper around the official **Mneme OS** installer.

`mnemeos` is a local persistent code-graph and AI-superbrain MCP server.
It runs entirely on your machine, survives context compaction, and
remembers what your AI coding tools have already learned about your
codebase.

This package gets you to a working install with one line of `pip`. It
does not reimplement anything — once it has downloaded the right
bootstrap script for your platform and verified the SHA-256, it hands
off to the same code path you would hit running the published one-liner.

## Install

```bash
pip install mnemeos
mnemeos
```

That's it. The wrapper detects your platform automatically, downloads
the right installer, verifies its SHA-256, and runs it:

| OS | Auto-detected arch | Installer downloaded |
|---|---|---|
| Windows | x64 / arm64 (via `PROCESSOR_ARCHITECTURE`) | `bootstrap-install.ps1` |
| macOS   | x86_64 / arm64 (via `uname -m`) | `install-mac.sh` |
| Linux   | x86_64 / aarch64 (via `uname -m`) | `install-linux.sh` |

x86 (32-bit) Windows is refused at install time — the Bun runtime
required by the MCP layer doesn't ship 32-bit Windows builds.

## Backward-compatible CLI names

The console scripts `mneme` and `mneme-bootstrap` are still wired up for
backward compatibility, so existing scripts and docs continue to work.
`mnemeos` is the new canonical name going forward.

```bash
mnemeos          # canonical (recommended)
mneme            # legacy alias — still works
mneme-bootstrap  # legacy alias — still works
```

## Common flags

```bash
mnemeos --check          # show what would happen, do nothing
mnemeos --force          # re-download even if cached
mnemeos --release v0.3.2 # pin a specific release
mnemeos --platform Linux # override platform detection
mnemeos --no-verify      # skip the SHA-256 check (beta only)
mnemeos -- --no-models   # pass arguments through to the installer
```

`mnemeos --help` lists everything.

## Where things live

- Downloaded scripts cache to `~/.cache/mnemeos/<release>/`
  (legacy: `~/.cache/mneme-mcp/<release>/` still readable for upgrades).
- The installer drops Mneme OS itself into `~/.mneme/`.
- Logs from the installer print to your terminal in real time.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Bootstrap completed (or `--check` succeeded). |
| 1 | The bootstrap script ran but returned non-zero. |
| 2 | Platform unsupported, or required argument missing. |
| 3 | SHA-256 mismatch on the downloaded installer. |
| 4 | Network or filesystem error during download. |

## Why a wrapper?

Python developers tend to reach for `pip` first. We did not want to
force them through `curl | bash` or PowerShell `iex` if the rest of
their toolchain ships through PyPI. The wrapper is intentionally thin:
all the install logic stays in the bootstrap scripts that the rest of
the project already maintains, so there is one source of truth for
what an install actually does.

## Full project

Source, docs, and the canonical installer scripts live at
[github.com/omanishay-cyber/mneme](https://github.com/omanishay-cyber/mneme).

Project site: [omanishay-cyber.github.io/mneme](https://omanishay-cyber.github.io/mneme/)

## Why "mnemeos" not "mneme"?

The bare name `mneme` was claimed on PyPI in 2014 by an unrelated
Flask-based note-taking app. We chose `mnemeos` (short for "Mneme OS",
the project's brand) to publish cleanly without name fight, and it
doubles as a clearer signal of what the project is — a persistent
memory layer for AI coding work, not just a memo viewer.

## License

Apache-2.0. Maintained by Anish Trivedi & Kruti Trivedi.
