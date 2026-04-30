#!/usr/bin/env bash
# scripts/check-home-dir-discipline.sh
#
# Class HOME guardrail (audit 2026-04-29): the CLAUDE.md hard rule
# requires every mneme path to be constructed via
# `mneme_common::PathManager`. Direct `dirs::home_dir()` calls outside
# the allowlist below silently bypass `MNEME_HOME` and break operator
# overrides.
#
# This script greps the workspace for `dirs::home_dir()` *call sites*
# (not doc comments, not string literals) and fails if any are
# introduced outside the allowlisted files. The allowlist is for
# legitimate non-mneme paths (Claude Code settings, Bun install dir,
# external-tool platform adapters).
#
# Run from repo root:
#   bash scripts/check-home-dir-discipline.sh
#
# Exit codes:
#   0 — clean
#   1 — disallowed `dirs::home_dir()` call site found

set -euo pipefail

cd "$(dirname "$0")/.."

# Allowlisted files. Each entry is a path *relative to repo root*. A
# match in any of these is permitted.
#
#   * common/src/paths.rs           — the canonical resolver itself.
#   * cli/src/commands/doctor.rs    — `~/.claude/settings.json`
#                                      (Claude Code path, not mneme's).
#   * cli/src/commands/uninstall.rs — auxiliary cleanup of `~/.claude`
#                                      (Claude path) + `~/.bun` (Bun's
#                                      install cache).
#   * cli/src/main.rs               — `which_bun()` searches `~/.bun/bin`
#                                      (Bun's install, not mneme's).
#   * cli/src/platforms/mod.rs      — `AdapterContext.home` field used
#                                      by every external platform
#                                      adapter (Cursor, Codex, Zed, …)
#                                      to find each tool's own settings
#                                      dir. NOT a mneme path.
ALLOWLIST_REGEX='^(common/src/paths\.rs|cli/src/commands/doctor\.rs|cli/src/commands/uninstall\.rs|cli/src/commands/cache\.rs|cli/src/main\.rs|cli/src/platforms/mod\.rs)$'

# Stage 1 — raw grep: every `dirs::home_dir(` / `home::home_dir(`
# match in any tracked .rs file.
RAW=$(grep -rn -E 'dirs::home_dir\(|home::home_dir\(' \
  --include='*.rs' \
  --exclude-dir=target \
  --exclude-dir=vendor \
  --exclude-dir=third_party \
  . || true)

# Stage 2 — awk filter: drop doc-comments and string-literal hits.
# Stage 3 — sh filter: drop tests/ paths and allowlisted files.
violations=$(printf '%s\n' "$RAW" | awk -F: '
  NF >= 3 {
    line = $3
    for (i = 4; i <= NF; i++) line = line ":" $i
    body = line
    sub(/^[[:space:]]+/, "", body)
    if (substr(body, 1, 2) == "//") next
    match_idx = index(line, "dirs::home_dir(")
    if (match_idx == 0) match_idx = index(line, "home::home_dir(")
    quote_idx = index(line, "\"")
    if (quote_idx > 0 && quote_idx < match_idx) next
    print $0
  }
' | while IFS= read -r line; do
  file_part="${line#./}"
  file_path="${file_part%%:*}"
  case "$file_path" in
    */tests/*|*/tests.rs)
      continue
      ;;
  esac
  if echo "$file_path" | grep -qE "$ALLOWLIST_REGEX"; then
    continue
  fi
  printf '%s\n' "$line"
done)

if [ -n "$violations" ]; then
  echo "Class HOME violation: disallowed dirs::home_dir() call site(s) found." >&2
  echo "" >&2
  echo "All mneme paths must be constructed via mneme_common::PathManager." >&2
  echo "Add the file to the allowlist in this script ONLY for non-mneme" >&2
  echo "paths (e.g. ~/.claude or ~/.bun lookups)." >&2
  echo "" >&2
  echo "$violations" >&2
  exit 1
fi

echo "OK: no disallowed dirs::home_dir() call sites."
exit 0
