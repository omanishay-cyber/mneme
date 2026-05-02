#!/usr/bin/env bash
# stage-release-archive.sh - POSIX equivalent of stage-release-zip.ps1
#
# Assembles mneme-v<ver>-<os>-<arch>.tar.gz from target/<target>/release/*
# + mcp/ + vision/dist/ + scripts/ + plugin/ on macOS and Linux runners.
# Mirrors the layout install.sh expects to extract under ~/.mneme/.
#
# Layout produced:
#   bin/                      - all 9 mneme* binaries (+ libonnxruntime.{so,dylib})
#   mcp/                      - TS source + node_modules + dist (Bun runs from here)
#   static/vision/            - vision SPA dist (daemon serves via HTTP)
#   scripts/install.sh        - canonical POSIX installer (built by Wave A)
#   scripts/install.ps1       - kept for cross-OS users who unzip on Windows
#   plugin/                   - plugin.json + skills/ + agents/ + commands/
#   uninstall.sh              - dropped at archive root for visibility (if exists)
#   VERSION.txt               - "0.3.2" + git commit
#
# Usage:
#   ./stage-release-archive.sh \
#     --source-root . \
#     --target x86_64-apple-darwin \
#     --out ./mneme-v0.3.2-macos-x64.tar.gz \
#     --stage-dir ./stage-x86_64-apple-darwin \
#     --force
#
# Flags:
#   --source-root <path>   Repo root containing target/, mcp/, vision/, etc.
#   --target <triple>      Rust target triple (selects target/<triple>/release/)
#   --version <ver>        Version string for VERSION.txt (default: 0.3.2)
#   --out <path>           Output .tar.gz path
#   --stage-dir <path>     Intermediate staging directory (default: ./mneme-stage)
#   --force                Overwrite existing stage-dir / output without prompting
#
# Author: Anish Trivedi. Apache-2.0.

set -euo pipefail

# ---------------------------------------------------------------------------
# Args
# ---------------------------------------------------------------------------
SOURCE_ROOT="."
TARGET=""
VERSION="0.3.2"
OUT_ARCHIVE=""
STAGE_DIR="./mneme-stage"
FORCE=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --source-root) SOURCE_ROOT="$2"; shift 2 ;;
    --target) TARGET="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --out) OUT_ARCHIVE="$2"; shift 2 ;;
    --stage-dir) STAGE_DIR="$2"; shift 2 ;;
    --force) FORCE=1; shift ;;
    *) echo "ERROR: unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [[ -z "$TARGET" ]]; then
  echo "ERROR: --target is required (e.g. x86_64-apple-darwin)" >&2
  exit 2
fi
if [[ -z "$OUT_ARCHIVE" ]]; then
  echo "ERROR: --out is required (e.g. ./mneme-v0.3.2-linux-x64.tar.gz)" >&2
  exit 2
fi

# Resolve to absolute paths so our cd/tar dance below stays correct.
SOURCE_ROOT="$(cd "$SOURCE_ROOT" && pwd)"
# Stage + out can be relative to current dir; resolve their parents.
mkdir -p "$(dirname "$OUT_ARCHIVE")"
OUT_ARCHIVE="$(cd "$(dirname "$OUT_ARCHIVE")" && pwd)/$(basename "$OUT_ARCHIVE")"
mkdir -p "$(dirname "$STAGE_DIR")"
STAGE_DIR="$(cd "$(dirname "$STAGE_DIR")" && pwd)/$(basename "$STAGE_DIR")"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
section() { echo ""; echo "== $1 =="; }
step()    { echo "  -> $1"; }
ok()      { echo "     OK: $1"; }
fail()    { echo "     FAIL: $1" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Pre-flight
# ---------------------------------------------------------------------------
section "Pre-flight"
TARGET_DIR="$SOURCE_ROOT/target/$TARGET/release"
if [[ ! -d "$TARGET_DIR" ]]; then
  fail "target/$TARGET/release/ not found at $TARGET_DIR. Run 'cargo build --workspace --release --target $TARGET' first."
fi

MCP_DIR="$SOURCE_ROOT/mcp"
VISION_DIST="$SOURCE_ROOT/vision/dist"
SCRIPTS_DIR="$SOURCE_ROOT/scripts"
PLUGIN_DIR="$SOURCE_ROOT/plugin"

for d in "$MCP_DIR" "$VISION_DIST" "$SCRIPTS_DIR" "$PLUGIN_DIR"; do
  [[ -d "$d" ]] || fail "missing required dir: $d"
done

# Required binaries (workspace, no .exe suffix on POSIX).
REQUIRED_BINS=(
  "mneme"
  "mneme-daemon"
  "mneme-store"
  "mneme-parsers"
  "mneme-scanners"
  "mneme-brain"
  "mneme-livebus"
  "mneme-md-ingest"
)
# mneme-multimodal is feature-gated — checked separately as optional.

MISSING=()
for bin in "${REQUIRED_BINS[@]}"; do
  [[ -f "$TARGET_DIR/$bin" ]] || MISSING+=("$bin")
done
if [[ ${#MISSING[@]} -gt 0 ]]; then
  fail "missing release binaries: ${MISSING[*]}"
fi
ok "all ${#REQUIRED_BINS[@]} required release binaries present"

# Detect ONNX Runtime dynamic lib (matches whatever the workflow dropped).
ORT_LIB=""
for cand in "libonnxruntime.so" "libonnxruntime.dylib"; do
  if [[ -f "$TARGET_DIR/$cand" ]]; then
    ORT_LIB="$cand"
    break
  fi
done
if [[ -z "$ORT_LIB" ]]; then
  echo "  WARN: no libonnxruntime.{so,dylib} in $TARGET_DIR — BGE will fall back to hashing-trick" >&2
fi

# ---------------------------------------------------------------------------
# Stage dir setup
# ---------------------------------------------------------------------------
section "Stage dir"
if [[ -d "$STAGE_DIR" ]]; then
  if [[ $FORCE -ne 1 ]]; then
    fail "stage dir exists: $STAGE_DIR (pass --force to overwrite)"
  fi
  rm -rf "$STAGE_DIR"
fi
mkdir -p "$STAGE_DIR"
ok "fresh stage at $STAGE_DIR"

# ---------------------------------------------------------------------------
# Copy bin/
# ---------------------------------------------------------------------------
section "Copy bin/"
STAGE_BIN="$STAGE_DIR/bin"
mkdir -p "$STAGE_BIN"
for bin in "${REQUIRED_BINS[@]}"; do
  cp "$TARGET_DIR/$bin" "$STAGE_BIN/"
  step "+ $bin"
done

# Optional feature-gated binary.
if [[ -f "$TARGET_DIR/mneme-multimodal" ]]; then
  cp "$TARGET_DIR/mneme-multimodal" "$STAGE_BIN/"
  step "+ mneme-multimodal (feature-gated)"
else
  echo "     warning: mneme-multimodal missing (feature-gated, expected when sidecar disabled)"
fi

# B-011 equivalent: bundle libonnxruntime.{so,dylib} so brain's BGE
# embedder works without the user installing onnxruntime via the
# system package manager. The `ort` crate's load-dynamic feature
# searches the executable's dir first, then LD_LIBRARY_PATH /
# DYLD_LIBRARY_PATH, then the system loader cache. Bundling here
# makes the install self-contained.
if [[ -n "$ORT_LIB" ]]; then
  cp "$TARGET_DIR/$ORT_LIB" "$STAGE_BIN/"
  step "+ $ORT_LIB"
fi

BIN_COUNT=$(find "$STAGE_BIN" -type f | wc -l | tr -d ' ')
ok "bin/ complete: $BIN_COUNT files"

# ---------------------------------------------------------------------------
# Copy mcp/  (with B2 validation gate — same as PowerShell sibling)
# ---------------------------------------------------------------------------
section "Copy mcp/ (TS source + node_modules + dist)"
STAGE_MCP="$STAGE_DIR/mcp"

# B2 (2026-05-02): mcp/node_modules MUST contain zod and
# @modelcontextprotocol/sdk or the staged archive ships broken.
ZOD_PKG="$MCP_DIR/node_modules/zod/package.json"
SDK_PKG="$MCP_DIR/node_modules/@modelcontextprotocol/sdk/package.json"
if [[ ! -f "$ZOD_PKG" ]] || [[ ! -f "$SDK_PKG" ]]; then
  echo "  -> mcp/node_modules incomplete, running 'bun install --frozen-lockfile' first..."
  pushd "$MCP_DIR" > /dev/null
  bun install --frozen-lockfile
  popd > /dev/null
fi
[[ -f "$ZOD_PKG" ]] || fail "mcp/node_modules/zod/package.json STILL missing after bun install — refusing to stage broken archive (B2 / 2026-05-02 POS install bug)"
[[ -f "$SDK_PKG" ]] || fail "mcp/node_modules/@modelcontextprotocol/sdk/package.json STILL missing after bun install — refusing to stage broken archive"
ok "mcp/node_modules has zod + @modelcontextprotocol/sdk"

# Copy the entire mcp tree (including node_modules).
mkdir -p "$STAGE_MCP"
# Use `cp -R` with `.` glob to include hidden files; node_modules is large
# but no symlinks to worry about on Linux/macOS in the bun ecosystem.
cp -R "$MCP_DIR/." "$STAGE_MCP/"

# Post-stage assertion.
[[ -f "$STAGE_MCP/node_modules/zod/package.json" ]] || fail "post-stage: zod missing in $STAGE_MCP/node_modules — copy failed"
ok "mcp/ copied with node_modules intact"

# ---------------------------------------------------------------------------
# Copy vision/dist -> static/vision/
# ---------------------------------------------------------------------------
section "Copy vision/dist -> static/vision/"
STAGE_VISION="$STAGE_DIR/static/vision"
mkdir -p "$STAGE_VISION"
cp -R "$VISION_DIST/." "$STAGE_VISION/"
[[ -f "$STAGE_VISION/index.html" ]] || fail "static/vision/index.html missing after copy — vision SPA build incomplete"
INDEX_BYTES=$(wc -c < "$STAGE_VISION/index.html" | tr -d ' ')
ok "static/vision/ complete: index.html=${INDEX_BYTES} bytes"

# ---------------------------------------------------------------------------
# Copy scripts/  (drop scripts/test/ — VM scripts not needed at install time)
# ---------------------------------------------------------------------------
section "Copy scripts/"
STAGE_SCRIPTS="$STAGE_DIR/scripts"
mkdir -p "$STAGE_SCRIPTS"
# Use rsync if available (handles --exclude cleanly), fall back to cp + manual rm.
if command -v rsync > /dev/null 2>&1; then
  rsync -a --exclude='test/' "$SCRIPTS_DIR/" "$STAGE_SCRIPTS/"
else
  cp -R "$SCRIPTS_DIR/." "$STAGE_SCRIPTS/"
  rm -rf "$STAGE_SCRIPTS/test"
fi
ok "scripts/ complete (test/ excluded)"

# Drop standalone uninstall scripts at archive root for visibility.
for u in "uninstall.sh" "uninstall.ps1"; do
  if [[ -f "$STAGE_SCRIPTS/$u" ]]; then
    cp "$STAGE_SCRIPTS/$u" "$STAGE_DIR/$u"
    step "+ root $u"
  fi
done

# ---------------------------------------------------------------------------
# Copy plugin/
# ---------------------------------------------------------------------------
section "Copy plugin/"
STAGE_PLUGIN="$STAGE_DIR/plugin"
mkdir -p "$STAGE_PLUGIN"
cp -R "$PLUGIN_DIR/." "$STAGE_PLUGIN/"
ok "plugin/ complete"

# ---------------------------------------------------------------------------
# Copy release/  (install.sh + lib-common.sh + bootstrap-install.ps1 etc)
# ---------------------------------------------------------------------------
section "Copy release/"
RELEASE_SRC="$SOURCE_ROOT/release"
if [[ -d "$RELEASE_SRC" ]]; then
  STAGE_RELEASE="$STAGE_DIR/release"
  mkdir -p "$STAGE_RELEASE"
  cp -R "$RELEASE_SRC/." "$STAGE_RELEASE/"
  ok "release/ copied"
else
  echo "  WARN: $RELEASE_SRC missing — bootstrap installers won't ship in this archive"
fi

# ---------------------------------------------------------------------------
# Top-level metadata
# ---------------------------------------------------------------------------
section "Top-level metadata"
[[ -f "$SOURCE_ROOT/LICENSE" ]] && cp "$SOURCE_ROOT/LICENSE" "$STAGE_DIR/"
[[ -f "$SOURCE_ROOT/README.md" ]] && cp "$SOURCE_ROOT/README.md" "$STAGE_DIR/"
[[ -f "$SOURCE_ROOT/marketplace.json" ]] && cp "$SOURCE_ROOT/marketplace.json" "$STAGE_DIR/"
ok "LICENSE + README + marketplace.json (where present)"

# ---------------------------------------------------------------------------
# VERSION.txt
# ---------------------------------------------------------------------------
section "VERSION.txt"
GIT_COMMIT="unknown"
GIT_BRANCH="unknown"
if command -v git > /dev/null 2>&1 && [[ -d "$SOURCE_ROOT/.git" ]]; then
  GIT_COMMIT=$(cd "$SOURCE_ROOT" && git rev-parse HEAD 2>/dev/null || echo unknown)
  GIT_BRANCH=$(cd "$SOURCE_ROOT" && git branch --show-current 2>/dev/null || echo unknown)
fi
cat > "$STAGE_DIR/VERSION.txt" <<EOF
Mneme $VERSION
Built: $(date -u +%Y-%m-%dT%H:%M:%SZ)
Source: $SOURCE_ROOT
Target: $TARGET
Git commit: $GIT_COMMIT
Git branch: $GIT_BRANCH
EOF
ok "VERSION.txt written"

# ---------------------------------------------------------------------------
# Stage summary
# ---------------------------------------------------------------------------
section "Stage summary"
TOTAL_FILES=$(find "$STAGE_DIR" -type f | wc -l | tr -d ' ')
# `du -sh` is cross-platform between macOS BSD du and Linux GNU du.
TOTAL_SIZE=$(du -sh "$STAGE_DIR" | awk '{print $1}')
echo "  total: $TOTAL_SIZE across $TOTAL_FILES files"
for sub in "$STAGE_DIR"/*; do
  if [[ -e "$sub" ]]; then
    sub_size=$(du -sh "$sub" 2>/dev/null | awk '{print $1}')
    printf "    %-30s  %8s\n" "$(basename "$sub")" "$sub_size"
  fi
done

# ---------------------------------------------------------------------------
# Compress to tar.gz
# ---------------------------------------------------------------------------
section "Compress to tar.gz"
if [[ -f "$OUT_ARCHIVE" ]]; then
  if [[ $FORCE -ne 1 ]]; then
    fail "output archive exists: $OUT_ARCHIVE (pass --force to overwrite)"
  fi
  rm -f "$OUT_ARCHIVE"
fi

START=$(date +%s)
# `-C $STAGE_DIR .` packs contents-of-stage at archive root (matches
# `Compress-Archive -Path stage\*` semantics on the Windows sibling).
tar -czf "$OUT_ARCHIVE" -C "$STAGE_DIR" .
END=$(date +%s)
ELAPSED=$((END - START))

ARCHIVE_SIZE=$(du -sh "$OUT_ARCHIVE" | awk '{print $1}')
ok "archive created: $OUT_ARCHIVE ($ARCHIVE_SIZE) in ${ELAPSED}s"

# ---------------------------------------------------------------------------
# Verify archive (smoke: re-extract + sanity check)
# ---------------------------------------------------------------------------
section "Verify archive integrity"
SMOKE_DIR="${TMPDIR:-/tmp}/mneme-archive-smoke-$$"
mkdir -p "$SMOKE_DIR"
trap 'rm -rf "$SMOKE_DIR"' EXIT
tar -xzf "$OUT_ARCHIVE" -C "$SMOKE_DIR"

[[ -f "$SMOKE_DIR/bin/mneme" ]] || fail "post-extract: bin/mneme missing"
[[ -f "$SMOKE_DIR/mcp/src/index.ts" ]] || fail "post-extract: mcp/src/index.ts missing"
[[ -f "$SMOKE_DIR/mcp/node_modules/zod/package.json" ]] || fail "post-extract: mcp/node_modules/zod missing"
[[ -f "$SMOKE_DIR/static/vision/index.html" ]] || fail "post-extract: static/vision/index.html missing"
ok "archive integrity verified (bin/mneme, mcp/src + node_modules/zod, static/vision/index.html all present)"

section "DONE"
echo "  Archive: $OUT_ARCHIVE"
echo "  Size:    $ARCHIVE_SIZE"
echo ""
echo "Next: upload to GitHub Release via gh release upload"
