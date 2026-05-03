# mneme-mcp

A small Python wrapper around the official mneme installer.

`mneme` is a local persistent code-graph and AI-superbrain MCP server. It
runs entirely on your machine, survives context compaction, and remembers
what your AI coding tools have already learned about your codebase.

This package gets you to a working install with one line of `pip`. It
does not reimplement anything -- once it has downloaded the right
bootstrap script for your platform and verified the SHA-256, it hands
off to the same code path you would hit running the published one-liner.

## Install

```bash
pip install mneme-mcp
mneme
```

That's it. Detected your platform, downloaded `bootstrap-install.ps1`
(Windows), `install-mac.sh` (macOS), or `install-linux.sh` (Linux),
verified its hash against a pinned value, and ran it.

## Common flags

```bash
mneme --check          # show what would happen, do nothing
mneme --force          # re-download even if cached
mneme --release v0.3.2 # pin a specific release
mneme --platform Linux # override platform detection
mneme --no-verify      # skip the SHA-256 check (beta only)
mneme -- --no-models   # pass arguments through to the installer
```

`mneme --help` lists everything.

## Where things live

- Downloaded scripts cache to `~/.cache/mneme-mcp/<release>/`.
- The installer drops mneme itself into `~/.mneme/`.
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

## License

Apache-2.0. Maintained by Anish Trivedi & Kruti Trivedi.
