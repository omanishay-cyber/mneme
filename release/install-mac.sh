#!/bin/bash
# release/install-mac.sh
# ----------------------
# One-liner macOS installer for mneme -- TRULY one-command, all included.
#
# Usage (any user, no sudo needed):
#
#   curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-mac.sh | bash
#
# Or, equivalently (download first, then execute):
#
#   curl -fsSLo /tmp/install-mac.sh https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-mac.sh
#   bash /tmp/install-mac.sh
#
# What it does (mirrors bootstrap-install.ps1 design):
#   1. Auto-detects arch (Intel x86_64 -> x64, Apple Silicon arm64 -> arm64).
#   2. Picks a release version (default v0.3.2; override via $MNEME_VERSION).
#   3. Pre-flight: disk space, OS, arch, runtime tooling.
#   4. Downloads mneme-<ver>-macos-<arch>.tar.gz from the GitHub Release.
#   5. Extracts to ~/Library/Application Support/mneme/.
#   6. Symlinks ~/.mneme/ to the canonical location for compatibility with
#      hardcoded paths in the daemon + MCP layer.
#   7. Installs Bun if missing (curl -fsSL https://bun.sh/install | bash).
#   8. Registers daemon as launchd plist at
#      ~/Library/LaunchAgents/com.mneme.daemon.plist and loads it.
#   9. Downloads model assets (bge, qwen-embed, qwen-coder, phi-3) from
#      the Hugging Face mirror (HF Hub primary -- Cloudflare-backed,
#      ~5x faster than GitHub Releases, no 2 GB asset cap), with the
#      GitHub Release as a transparent fallback.
#  10. Runs `mneme models install --from-path` to register them.
#  11. Verifies daemon health via `mneme daemon status`.
#  12. Prints a boxed banner reminding the user to reopen Terminal so
#      ~/.mneme/bin lands on PATH.
#
# Opt-outs (env vars set BEFORE the curl|bash line):
#   MNEME_VERSION=v0.3.3        override the default release version
#   MNEME_NO_MODELS=1           skip the model download/install step
#   MNEME_KEEP_DOWNLOAD=1       keep the temp download dir for inspection
#   MNEME_SKIP_HASH_CHECK=1     skip SHA-256 verification (beta zips only)
#
# IMPORTANT: macOS Gatekeeper. The release binaries are NOT signed or
# notarized (out of scope for v0.3.2). On first launch, macOS may show
# "mneme cannot be opened because the developer cannot be verified". To
# unblock:
#   1. Right-click mneme in Finder -> Open -> Open
#   OR
#   2. xattr -dr com.apple.quarantine ~/Library/Application\ Support/mneme/
#
# Apache-2.0. (c) 2026 Anish Trivedi & Kruti Trivedi.

set -euo pipefail

# -----------------------------------------------------------------------------
# Source the shared helper library. When run via `curl ... | bash` the
# script body has no $0 location -- the helpers must be inlined or
# downloaded separately. We try the local-relative path first (works for
# `bash ./install-mac.sh`), then fall back to fetching lib-common.sh
# from the same release URL we'll pull binaries from.
# -----------------------------------------------------------------------------

# B-L01 (2026-05-03): renamed from VERSION to MNEME_REL_TAG for consistency
# with install-linux.sh and to avoid future clobbering by sourced env files.
MNEME_REL_TAG="${MNEME_VERSION:-v0.3.2}"
RELEASE_BASE="https://github.com/omanishay-cyber/mneme/releases/download/${MNEME_REL_TAG}"

# Locate lib-common.sh:
#   1. Same dir as this script (when invoked locally)
#   2. Ambient $MNEME_LIB_COMMON env override (for testing)
#   3. Download from the release page (when invoked via curl|bash)
_load_lib() {
    local script_dir
    if [ -n "${BASH_SOURCE[0]:-}" ]; then
        script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd || echo "")
        if [ -n "${script_dir}" ] && [ -f "${script_dir}/lib-common.sh" ]; then
            # shellcheck source=release/lib-common.sh
            . "${script_dir}/lib-common.sh"
            return 0
        fi
    fi
    if [ -n "${MNEME_LIB_COMMON:-}" ] && [ -f "${MNEME_LIB_COMMON}" ]; then
        # shellcheck source=/dev/null
        . "${MNEME_LIB_COMMON}"
        return 0
    fi
    # Fall back to fetching from the release URL.
    local tmp_lib
    tmp_lib=$(mktemp -t mneme-lib-common.XXXXXX) || tmp_lib="/tmp/mneme-lib-common.sh"
    if command -v curl >/dev/null 2>&1; then
        if curl --fail --location --silent --show-error \
                -o "${tmp_lib}" "${RELEASE_BASE}/lib-common.sh"; then
            # shellcheck source=/dev/null
            . "${tmp_lib}"
            rm -f "${tmp_lib}"
            return 0
        fi
    fi
    # shellcheck disable=SC2016  # `$MNEME_LIB_COMMON` is intentional literal text
    printf 'FAIL: could not load lib-common.sh (looked next to script, $MNEME_LIB_COMMON, and %s)\n' \
        "${RELEASE_BASE}/lib-common.sh" >&2
    exit 1
}
_load_lib

# A7-001 (2026-05-04): fetch the SHA-256 manifest before any download work.
# Non-fatal if the manifest is missing -- legacy unverified-download path
# still works and emits a single WARN. Once the manifest is published per
# release, every pinned asset becomes hard-fail on hash mismatch.
load_hash_manifest "${RELEASE_BASE}"

# -----------------------------------------------------------------------------
# Banner + detected-environment block (B11.5w: "user sees what the script sees")
# -----------------------------------------------------------------------------

OS=$(detect_os)
if [ "${OS}" != "mac" ]; then
    fail "this installer is for macOS only (detected: ${OS}). Use install-linux.sh on Linux."
fi
ARCH=$(detect_arch)

# A9-002 (2026-05-04): Intel Mac (x86_64) refusal.
# v0.3.2 ships only an aarch64-apple-darwin binary. The CI macos-13 leg was
# removed because GitHub-hosted Intel Mac runners are chronically queue-starved.
# Without an explicit guard here, an Intel Mac user gets ARCH=x64, then a 404
# downloading mneme-${MNEME_REL_TAG}-macos-x64.tar.gz -- confusing failure mode.
# Refuse early with an actionable message instead.
if [ "${ARCH}" = "x64" ]; then
    fail "Intel Mac (x86_64) is not supported in ${MNEME_REL_TAG}.\
\n  v0.3.2 ships only an Apple Silicon (arm64) binary.\
\n  Intel Mac users: build from source with\
\n    git clone https://github.com/omanishay-cyber/mneme.git\
\n    cd mneme && cargo build --release --workspace\
\n  Native Intel Mac binaries may return in a later release."
fi

# Read OS version + name for the diagnostic block.
MAC_PRODUCT_NAME=$(sw_vers -productName 2>/dev/null || echo "macOS")
MAC_PRODUCT_VERSION=$(sw_vers -productVersion 2>/dev/null || echo "?")
MAC_BUILD=$(sw_vers -buildVersion 2>/dev/null || echo "?")

step "mneme bootstrap installer (macOS)"
say "version    : ${MNEME_REL_TAG}"
say "user       : ${USER:-$(id -un)}"
say "os         : ${MAC_PRODUCT_NAME} ${MAC_PRODUCT_VERSION} (${MAC_BUILD})"
say "arch       : ${ARCH} (uname -m: $(uname -m))"
say "shell      : ${SHELL:-unknown}"
say "target     : ${HOME}/Library/Application Support/mneme"
if [ -n "${MNEME_NO_MODELS:-}" ]; then
    say "models     : SKIP (MNEME_NO_MODELS=1)"
else
    say "models     : AUTO-DOWNLOAD (HF Hub primary, GitHub fallback)"
fi
echo ""

# -----------------------------------------------------------------------------
# Pre-flight checks
# -----------------------------------------------------------------------------

step "pre-flight checks"

# B-L04 (2026-05-03): bumped from 5 GB to 8 GB. Real install peaks at
# ~7 GB because models stage to a temp dir then copy into ~/.mneme/models
# while the staging dir is still alive. 8 GB covers staging + final +
# binaries + working space + a small headroom.
pre_flight_disk_space 8

# bash 3.2 ships with macOS by default -- our script targets bash 3.2+
# semantics (no associative arrays, no [[ ${var,,} ]]). Confirm we have
# at least bash 3.x.
if [ -z "${BASH_VERSION:-}" ]; then
    fail "this script requires bash (running shell: ${SHELL:-unknown})"
fi
ok "bash ${BASH_VERSION}"

# curl is preinstalled on every supported macOS version.
require_cmd curl "should be preinstalled on macOS -- check /usr/bin/curl"
require_cmd tar  "should be preinstalled on macOS -- check /usr/bin/tar"
ok "curl + tar present"

# Optional: probe Homebrew for friendlier diagnostics later (not required).
if command -v brew >/dev/null 2>&1; then
    HOMEBREW_PREFIX=$(brew --prefix 2>/dev/null || echo "/opt/homebrew")
    ok "homebrew detected at ${HOMEBREW_PREFIX} (optional)"
else
    say "homebrew not detected (optional -- only used as a fallback path for jq/git)"
fi

# git: optional, only matters for `mneme build` commit-SHA metadata.
if command -v git >/dev/null 2>&1; then
    ok "git $(git --version | awk '{print $3}') present"
else
    warn "git not found -- mneme will still work but commit-SHA metadata is unavailable"
    say "to install: xcode-select --install   (or: brew install git)"
fi

# -----------------------------------------------------------------------------
# Bun runtime (required for MCP server)
# -----------------------------------------------------------------------------

step "bun runtime"

BUN_BIN=""
# Locate Bun: PATH first, then standard install paths on macOS.
if command -v bun >/dev/null 2>&1; then
    BUN_BIN=$(command -v bun)
elif [ -x "${HOME}/.bun/bin/bun" ]; then
    BUN_BIN="${HOME}/.bun/bin/bun"
elif [ -x "/opt/homebrew/bin/bun" ]; then
    BUN_BIN="/opt/homebrew/bin/bun"
elif [ -x "/usr/local/bin/bun" ]; then
    BUN_BIN="/usr/local/bin/bun"
fi

if [ -n "${BUN_BIN}" ]; then
    # A7-024 (2026-05-04): verify Bun binary architecture matches the
    # Mac architecture. On Apple Silicon (arm64), an x86_64 Bun
    # installed under Rosetta at /usr/local/bin/bun would still run
    # but adds ~30% startup latency for every MCP call. We don't
    # refuse the wrong-arch binary -- it works -- but we do warn and
    # point at the canonical install location so the user can fix it.
    bun_arch_warn=""
    if command -v file >/dev/null 2>&1; then
        bun_arch_info=$(file "${BUN_BIN}" 2>/dev/null || true)
        if [ "${ARCH}" = "arm64" ]; then
            case "${bun_arch_info}" in
                *arm64*) : ;;
                *x86_64*)
                    bun_arch_warn="bun at ${BUN_BIN} is x86_64 -- you are on arm64 Mac. Bun runs under Rosetta with ~30% startup penalty per MCP call. Reinstall: rm -rf ~/.bun && curl -fsSL https://bun.sh/install | bash"
                    ;;
            esac
        elif [ "${ARCH}" = "x64" ]; then
            case "${bun_arch_info}" in
                *x86_64*) : ;;
                *arm64*)
                    bun_arch_warn="bun at ${BUN_BIN} is arm64 -- you are on x86_64 Mac. Reinstall: rm -rf ~/.bun && curl -fsSL https://bun.sh/install | bash"
                    ;;
            esac
        fi
    fi
    ok "bun $("${BUN_BIN}" --version) present at ${BUN_BIN}"
    if [ -n "${bun_arch_warn}" ]; then
        warn "${bun_arch_warn}"
    fi
else
    say "bun not found -- installing from https://bun.sh/install"
    # Bun's installer writes to ~/.bun/bin and updates the user's shell rc.
    # We pipe through bash explicitly so the script doesn't assume the
    # caller's shell is bash-compatible (zsh is macOS default since Catalina).
    if curl --fail --location --silent --show-error https://bun.sh/install | bash; then
        BUN_BIN="${HOME}/.bun/bin/bun"
        if [ ! -x "${BUN_BIN}" ]; then
            fail "bun install completed but ${BUN_BIN} is not executable -- inspect /tmp for installer logs"
        fi
        ok "bun $("${BUN_BIN}" --version) installed at ${BUN_BIN}"
    else
        fail "bun install failed -- visit https://bun.sh/install and install manually, then re-run this script"
    fi
fi

# -----------------------------------------------------------------------------
# Stop any running mneme processes (upgrade safety)
# -----------------------------------------------------------------------------

step "stop existing mneme processes (if any)"

# launchctl bootout is the modern (macOS 11+) way to stop a launchd unit.
# We do best-effort cleanup -- not having it loaded is fine.
PLIST_LABEL="com.mneme.daemon"
PLIST_PATH="${HOME}/Library/LaunchAgents/${PLIST_LABEL}.plist"
if [ -f "${PLIST_PATH}" ]; then
    say "unloading existing launchd unit ${PLIST_LABEL}"
    launchctl bootout "gui/$(id -u)/${PLIST_LABEL}" 2>/dev/null || true
    launchctl unload "${PLIST_PATH}" 2>/dev/null || true
fi

# pkill anchored regex -- only matches argv[0] starting with mneme so
# we don't accidentally kill an editor with a mneme path open.
killed=0
if command -v pkill >/dev/null 2>&1; then
    if pkill -f '^mneme' >/dev/null 2>&1; then
        killed=1
        sleep 2
    fi
fi
if [ "${killed}" -eq 1 ]; then
    ok "stopped running mneme process(es)"
else
    ok "no mneme processes running -- safe to extract"
fi

# -----------------------------------------------------------------------------
# Download release tarball
# -----------------------------------------------------------------------------

step "download release tarball"

ASSET="mneme-${MNEME_REL_TAG}-macos-${ARCH}.tar.gz"
ASSET_URL="${RELEASE_BASE}/${ASSET}"

TMP_DIR=$(mktemp -d -t mneme-bootstrap)
# Cleanup on any exit path unless --keep-download equivalent set.
if [ -z "${MNEME_KEEP_DOWNLOAD:-}" ]; then
    trap 'rm -rf "${TMP_DIR}"' EXIT INT TERM
else
    say "MNEME_KEEP_DOWNLOAD=1 -- temp dir preserved at ${TMP_DIR}"
fi

LOCAL_TARBALL="${TMP_DIR}/${ASSET}"

# The macOS + Linux per-arch tarballs are produced by the cross-compile
# CI job (B11.55, separate agent's scope). Until that ships, the asset
# may 404. We give the user a clear, actionable error rather than a
# cryptic "download failed".
if ! download_with_retry "${ASSET_URL}" "${LOCAL_TARBALL}" 3; then
    fail "could not download ${ASSET}
       URL: ${ASSET_URL}
       This usually means the macOS-${ARCH} binary has not yet been
       uploaded to the v0.3.2 release page. Check the release at:
         https://github.com/omanishay-cyber/mneme/releases/tag/${MNEME_REL_TAG}
       If the asset is listed there, re-run this script. If not, the
       cross-compile workflow may still be building -- retry in ~15 min."
fi

# -----------------------------------------------------------------------------
# Extract tarball + symlink
# -----------------------------------------------------------------------------

step "extract to ~/Library/Application Support/mneme"

# macOS XDG-equivalent install path (Apple's recommended location for
# user-scoped non-sandboxed app data).
MNEME_HOME="${HOME}/Library/Application Support/mneme"
mkdir -p "${MNEME_HOME}"

# Extract. Use `tar -xzf - --strip-components=0` shape so we land the
# tarball's contents at the top of MNEME_HOME (the tarball is built
# without a wrapping top-level directory).
if ! tar -xzf "${LOCAL_TARBALL}" -C "${MNEME_HOME}"; then
    fail "extract failed -- archive may be corrupt"
fi

# Sanity: mneme binary should be at <MNEME_HOME>/bin/mneme.
MNEME_BIN="${MNEME_HOME}/bin/mneme"
if [ ! -f "${MNEME_BIN}" ]; then
    fail "post-extract sanity check failed: ${MNEME_BIN} missing"
fi
chmod +x "${MNEME_HOME}/bin"/* 2>/dev/null || true
ok "extracted (mneme present at ${MNEME_BIN})"

# Mneme OS branding alias: expose `mnemeos` alongside `mneme` so users
# on the new canonical brand name get the same binary. Idempotent.
MNEMEOS_BIN="${MNEME_HOME}/bin/mnemeos"
if [ -e "${MNEMEOS_BIN}" ] || [ -L "${MNEMEOS_BIN}" ]; then
    rm -f "${MNEMEOS_BIN}"
fi
if ln -sf mneme "${MNEMEOS_BIN}" 2>/dev/null; then
    ok "Mneme OS alias: mnemeos -> mneme (symlink)"
else
    say "warn: could not create mnemeos alias at ${MNEMEOS_BIN} (continuing)"
fi

# Compatibility symlink: many parts of the daemon + MCP code reference
# ~/.mneme/ as a hardcoded path. Symlink it to the canonical location.
SYMLINK_PATH="${HOME}/.mneme"
if [ -L "${SYMLINK_PATH}" ] || [ -e "${SYMLINK_PATH}" ]; then
    # If it's already a symlink to MNEME_HOME, leave it. Otherwise warn
    # but don't fail -- a previous install may have used a different layout
    # and replacing a real directory is destructive.
    if [ -L "${SYMLINK_PATH}" ]; then
        EXISTING_TARGET=$(readlink "${SYMLINK_PATH}")
        if [ "${EXISTING_TARGET}" = "${MNEME_HOME}" ]; then
            ok "compatibility symlink ~/.mneme -> ${MNEME_HOME} (already correct)"
        else
            say "updating compatibility symlink ~/.mneme (was -> ${EXISTING_TARGET})"
            rm -f "${SYMLINK_PATH}"
            ln -s "${MNEME_HOME}" "${SYMLINK_PATH}"
            ok "compatibility symlink ~/.mneme -> ${MNEME_HOME}"
        fi
    else
        # shellcheck disable=SC2088  # tilde is literal user-facing display text
        warn "~/.mneme exists as a real directory -- skipping symlink (may cause path-lookup confusion)"
    fi
else
    ln -s "${MNEME_HOME}" "${SYMLINK_PATH}"
    ok "compatibility symlink ~/.mneme -> ${MNEME_HOME}"
fi

# -----------------------------------------------------------------------------
# launchd plist registration
# -----------------------------------------------------------------------------

step "register launchd unit (${PLIST_LABEL})"

mkdir -p "${HOME}/Library/LaunchAgents"

# We intentionally write a fresh plist every install -- the binary path
# is stable but having the file regenerated guarantees no stale env vars
# from prior versions persist.
cat > "${PLIST_PATH}" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
                          "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${PLIST_LABEL}</string>

    <key>ProgramArguments</key>
    <array>
        <string>${MNEME_BIN}</string>
        <string>daemon</string>
        <string>start</string>
        <string>--foreground</string>
    </array>

    <key>EnvironmentVariables</key>
    <dict>
        <key>HOME</key>
        <string>${HOME}</string>
        <key>PATH</key>
        <string>${MNEME_HOME}/bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
    </dict>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
        <key>Crashed</key>
        <true/>
    </dict>

    <key>StandardOutPath</key>
    <string>${HOME}/Library/Logs/mneme-daemon.log</string>

    <key>StandardErrorPath</key>
    <string>${HOME}/Library/Logs/mneme-daemon.log</string>

    <key>ThrottleInterval</key>
    <integer>10</integer>
</dict>
</plist>
PLIST_EOF

ok "wrote ${PLIST_PATH}"

# Load via launchctl bootstrap (modern path). If the old `launchctl load`
# is the only thing available (pre-Catalina), fall back to it.
mkdir -p "${HOME}/Library/Logs"
if launchctl bootstrap "gui/$(id -u)" "${PLIST_PATH}" 2>/dev/null; then
    ok "loaded launchd unit (bootstrap)"
elif launchctl load "${PLIST_PATH}" 2>/dev/null; then
    ok "loaded launchd unit (legacy load)"
else
    warn "could not load launchd unit -- you can load it manually later with:"
    say "  launchctl bootstrap gui/$(id -u) ${PLIST_PATH}"
fi

# -----------------------------------------------------------------------------
# Run inner installer (registers MCP, hooks, etc.)
# -----------------------------------------------------------------------------

step "run inner installer (scripts/install.sh)"

INNER="${MNEME_HOME}/scripts/install.sh"
if [ -f "${INNER}" ]; then
    chmod +x "${INNER}" 2>/dev/null || true
    # The inner installer is shared with Linux. We invoke with --skip-download
    # because we already extracted; the inner script verifies layout only.
    if bash "${INNER}" --skip-download; then
        ok "inner installer completed"
    else
        warn "inner installer exited non-zero -- some MCP wiring may be incomplete"
        say "  manual retry: bash ${INNER} --skip-download"
    fi
else
    warn "inner installer not found at ${INNER} -- skipping MCP/hook wiring"
    say "  the release tarball may be missing scripts/ -- file an issue"
fi

# -----------------------------------------------------------------------------
# Model assets
# -----------------------------------------------------------------------------

if [ -n "${MNEME_NO_MODELS:-}" ]; then
    step "models -- SKIPPED (MNEME_NO_MODELS=1)"
    warn "smart-search will use the hashing-trick fallback (lower recall)"
    warn "local LLM summaries will fall back to signature-only text"
    say "to install later: re-run this script with MNEME_NO_MODELS unset"
else
    step "download + install model assets"

    MODELS_DIR="${TMP_DIR}/models"
    mkdir -p "${MODELS_DIR}"

    HF_BASE="https://huggingface.co/aaditya4u/mneme-models/resolve/main"

    # Asset list mirrors bootstrap-install.ps1 step 5. Names must match
    # what `mneme models install --from-path` expects on disk.
    #
    # tab-separated rows: name<TAB>required<TAB>primary_url<TAB>fallback_url
    MODEL_LIST=$(cat <<MODELS_EOF
bge-small-en-v1.5.onnx	1	${HF_BASE}/bge-small-en-v1.5.onnx	${RELEASE_BASE}/bge-small-en-v1.5.onnx
tokenizer.json	1	${HF_BASE}/tokenizer.json	${RELEASE_BASE}/tokenizer.json
qwen-embed-0.5b.gguf	0	${HF_BASE}/qwen-embed-0.5b.gguf	${RELEASE_BASE}/qwen-embed-0.5b.gguf
qwen-coder-0.5b.gguf	0	${HF_BASE}/qwen-coder-0.5b.gguf	${RELEASE_BASE}/qwen-coder-0.5b.gguf
phi-3-mini-4k.gguf	0	${HF_BASE}/phi-3-mini-4k.gguf
MODELS_EOF
)

    model_downloads=0
    model_failures=0
    OLDIFS="${IFS}"
    IFS=$'\n'
    for row in ${MODEL_LIST}; do
        IFS=$'\t' read -r m_name m_required m_primary m_fallback <<<"${row}"
        # A7-022 (2026-05-04): defensive default for `set -u` safety on
        # bash 3.2 (default macOS shell). The phi-3 row's trailing-tab
        # padding above guarantees a 4th field exists, but bash 3.2
        # `read` can still leave m_fallback unset on a literally-empty
        # trailing field. Force-default so download_dual_source doesn't
        # see an unbound variable when phi-3's GitHub fallback is empty.
        m_fallback="${m_fallback:-}"
        IFS=$'\n'
        dest="${MODELS_DIR}/${m_name}"
        # Run in subshell so a fail() inside download_dual_source doesn't
        # kill the whole loop -- we tolerate optional-asset failures.
        if ( download_dual_source "${m_name}" "${m_primary}" "${m_fallback}" "${dest}" ); then
            model_downloads=$(( model_downloads + 1 ))
        else
            model_failures=$(( model_failures + 1 ))
            if [ "${m_required}" = "1" ]; then
                warn "REQUIRED asset ${m_name} failed -- smart embeddings will be unavailable"
            else
                warn "optional asset ${m_name} failed -- corresponding capability disabled"
            fi
        fi
    done
    IFS="${OLDIFS}"

    ok "downloaded ${model_downloads} model asset(s) (${model_failures} failed)"

    if [ "${model_downloads}" -gt 0 ]; then
        say "registering models via mneme models install --from-path"
        if "${MNEME_BIN}" models install --from-path "${MODELS_DIR}"; then
            ok "models installed under ${MNEME_HOME}/models"
        else
            fail "mneme models install exited non-zero -- bootstrap aborted (models are required for smart recall)"
        fi
    fi
fi

# -----------------------------------------------------------------------------
# Daemon health probe
# -----------------------------------------------------------------------------

step "verify daemon health"

# After load, give launchd a moment to spawn and the daemon a moment to
# bind its IPC socket. Poll up to 15s.
healthy=0
waited=0
while [ "${waited}" -lt 15 ]; do
    sleep 1
    waited=$(( waited + 1 ))
    status_out=$("${MNEME_BIN}" daemon status 2>&1 || true)
    case "${status_out}" in
        *running*|*healthy*|*'"pid"'*)
            healthy=1
            break
            ;;
    esac
done

if [ "${healthy}" -eq 1 ]; then
    ok "daemon healthy (${waited}s)"
else
    warn "daemon did not report healthy within 15s -- it may still be coming up"
    say "  inspect: tail -n 50 ${HOME}/Library/Logs/mneme-daemon.log"
    say "  manual: ${MNEME_BIN} doctor"
fi

# -----------------------------------------------------------------------------
# Done
# -----------------------------------------------------------------------------

step "DONE"
say "mneme ${MNEME_REL_TAG} installed for macOS-${ARCH}."
echo ""
say "Verify (in a NEW terminal so PATH picks up ~/.mneme/bin):"
say "  mneme --version"
say "  mneme doctor"
say "  claude mcp list           # should show: mneme: Connected"
echo ""

# Final boxed banner. Mirrors the Windows installer's end-of-script
# block so the PATH-just-applied note can't be skimmed past.
echo ""
printf '%b  +-----------------------------------------------------------+%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |  IMPORTANT: open a NEW Terminal window before running     |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |  "mneme doctor" or "mneme build" -- the PATH change just  |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |  applied is not visible in this shell session.            |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |                                                           |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |  Gatekeeper note: binaries are not yet signed/notarized.  |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |  If macOS blocks first launch, right-click mneme in       |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |  Finder -> Open -> Open, OR run:                          |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |    xattr -dr com.apple.quarantine ~/.mneme/               |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  +-----------------------------------------------------------+%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
echo ""
