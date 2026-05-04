# Security Policy

Mneme takes security seriously. This document explains how to report
vulnerabilities and what to expect.

## Reporting a vulnerability

**Do NOT open a public GitHub issue for security problems.**

Instead, use GitHub's private vulnerability reporting:

1. Go to https://github.com/omanishay-cyber/mneme/security/advisories/new
2. Describe the vulnerability, how to reproduce it, and the impact you believe
   it has.
3. We will acknowledge receipt within 72 hours and aim to have an initial
   assessment within 7 days.

If GitHub's flow is not available to you, email the maintainer with the
subject line `[mneme-security]`. Contact address is listed on the
maintainer's GitHub profile.

## Scope

Mneme is a local daemon + local SQLite store + local MCP server. The threat
model is primarily:

- **Local process isolation.** Multiple users or programs on the same machine
  must not be able to read each other's mneme data.
- **Daemon authentication.** The IPC pipe must only accept requests from the
  user who started the daemon.
- **Supply chain.** A malicious dependency must not be able to exfiltrate
  graph data without local user consent.
- **Installer integrity.** The one-line installer is the largest attack
  surface; it downloads and executes code.

**Out of scope** (unless escalated by a chain of bugs):

- Denial of service against a local daemon running as the logged-in user.
- Information disclosure between processes running as the same OS user
  (standard OS sandbox applies).
- Attacks requiring physical access to the machine.

## Supported versions

Only the latest minor release receives security fixes. Older versions
(v0.1.x, v0.2.x) are no longer maintained; if you're still on those, upgrade
to the current v0.3.x or newer.

| Version | Supported |
|---|---|
| v0.3.x | YES |
| v0.2.x | NO |
| v0.1.x | NO |

## Disclosure timeline

Once we have a fix:

1. A patched release goes out.
2. A GitHub Security Advisory is published 7 days later, crediting the
   reporter (unless they request anonymity).
3. The CHANGELOG entry references the advisory.

We will not intentionally sit on a vulnerability. If a fix is complex and
takes longer than 30 days, we'll tell the reporter why and give an ETA.

## Known classes of sensitive data mneme stores

Because mneme indexes your source tree and conversation history, the
following kinds of data may end up in its SQLite files:

- Source code (obviously)
- Commit messages and git log excerpts
- Chat transcripts and tool calls (when hooks are enabled)
- Markdown files you point `mneme build` at, including `CLAUDE.md`,
  `AGENTS.md`, notes, PRDs
- Embeddings derived from all of the above

**Mneme never uploads any of this.** It has no network code in the hot path.
If you see mneme making unexpected outbound connections, that IS a
vulnerability - report it via the flow above.

### Network endpoints (installer + first-run only)

The bootstrap installer and first-run model setup touch a small, audited set
of HTTPS endpoints. None of these are reached during steady-state daemon
operation.

| Host | When | Why |
|---|---|---|
| `github.com` | install + self-update | Release zip, `bootstrap-install.ps1`, GitHub Releases fallback for model files |
| `huggingface.co` | first-run model fetch (primary) | BGE-small ONNX + Phi-3 model bundles via the `aaditya4u/mneme-models` mirror |
| `bun.sh` | install (only if Bun is missing) | Bun runtime installer for the MCP server |

All requests are HTTPS-only. There are no telemetry, analytics, or
phone-home endpoints. The daemon binds only to `127.0.0.1:7777` on the
loopback interface.

### Verification

Every artifact that crosses the network is integrity-checked locally before
it is executed or imported:

- **Release zip integrity (winget).** The published winget manifests
  (`winget/Anish/Mneme/0.3.2/Anish.Mneme.installer.yaml` and the parallel
  `Anish.Mnemeos` manifest) carry SHA-256 hashes for both the x64 and arm64
  Windows zips. winget refuses to install a zip whose hash does not match.
- **Bootstrap script + model files (sidecar manifest).** The bootstrap
  installer downloads a `release-checksums.json` sidecar from the same GitHub
  release and verifies every model file (BGE-small ONNX, Phi-3 GGUF parts,
  tokenizer JSON) against the SHA-256 listed there before unpacking. A hash
  mismatch aborts the install.
- **MCP dependency lock-in.** The MCP server's `bun install` step in CI and
  in the user installer uses `bun install --frozen-lockfile`. This refuses to
  resolve any dependency version not pinned by `mcp/bun.lock`, so a
  compromised registry mirror cannot inject a different version of
  `@modelcontextprotocol/sdk` or `zod` at install time.

## Hardening tips

- Put `~/.mneme/` on an encrypted volume if your code is sensitive.
- Use `mneme rollback` to undo installs cleanly if you need to redact history.
- Use `.mnemeignore` to keep secrets out of the index in the first place.
- Add Windows Defender exclusions via the installer rather than disabling
  Defender globally.

## Thank you

Responsible disclosure is how small OSS projects stay safe. If you report
something, you'll be credited on the advisory and in the CHANGELOG.
