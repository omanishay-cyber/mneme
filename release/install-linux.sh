#!/bin/bash
# release/install-linux.sh
# ------------------------
# One-liner Linux installer for mneme -- TRULY one-command, all included.
#
# Usage (any user, no sudo needed):
#
#   curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-linux.sh | bash
#
# Or, equivalently (download first, then execute):
#
#   curl -fsSLo /tmp/install-linux.sh https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-linux.sh
#   bash /tmp/install-linux.sh
#
# What it does (mirrors bootstrap-install.ps1 design):
#   1. Auto-detects arch (x86_64 -> x64, aarch64 -> arm64).
#   2. Detects WSL -- if running under WSL, points the user to the
#      Windows installer to avoid double-install across the OS boundary.
#   3. Picks a release version (default v0.3.2; override via $MNEME_VERSION).
#   4. Pre-flight: disk space, OS, arch, runtime tooling, distro family.
#   5. Downloads mneme-<ver>-linux-<arch>.tar.gz from the GitHub Release.
#   6. Extracts to ~/.local/share/mneme/ (XDG Base Directory compliant).
#   7. Symlinks ~/.mneme/ to the canonical location for compatibility
#      with hardcoded paths in the daemon + MCP layer.
#   8. Installs Bun if missing (curl -fsSL https://bun.sh/install | bash).
#   9. Registers daemon as systemd user unit at
#      ~/.config/systemd/user/mneme-daemon.service and enables + starts it.
#  10. Downloads model assets (bge, qwen-embed, qwen-coder, phi-3) from
#      the Hugging Face mirror (HF Hub primary -- Cloudflare-backed,
#      ~5x faster than GitHub Releases, no 2 GB asset cap), with the
#      GitHub Release as a transparent fallback.
#  11. Runs `mneme models install --from-path` to register them.
#  12. Verifies daemon health via `mneme daemon status`.
#  13. Prints a boxed banner reminding the user to reopen the shell so
#      ~/.mneme/bin lands on PATH.
#
# Opt-outs (env vars set BEFORE the curl|bash line):
#   MNEME_VERSION=v0.3.3        override the default release version
#   MNEME_NO_MODELS=1           skip the model download/install step
#   MNEME_KEEP_DOWNLOAD=1       keep the temp download dir for inspection
#   MNEME_SKIP_HASH_CHECK=1     skip SHA-256 verification (beta zips only)
#   MNEME_NO_SYSTEMD=1          skip systemd unit setup (containers, no-systemd hosts)
#
# Apache-2.0. (c) 2026 Anish Trivedi & Kruti Trivedi.

set -euo pipefail

# -----------------------------------------------------------------------------
# Source the shared helper library. When run via `curl ... | bash` the
# script body has no $0 location -- the helpers must be inlined or
# downloaded separately. We try the local-relative path first (works for
# `bash ./install-linux.sh`), then fall back to fetching lib-common.sh
# from the same release URL we'll pull binaries from.
# -----------------------------------------------------------------------------

# B-L01 (2026-05-03): renamed from VERSION to MNEME_REL_TAG to avoid being
# clobbered by `. /etc/os-release` later (which sets VERSION=22.04.5 LTS
# on Ubuntu, mangling our download URL into spaces + parens). Internal-only
# var; the user-facing env override MNEME_VERSION is unchanged.
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
# WSL detection (point user to Windows installer)
# -----------------------------------------------------------------------------

# WSL2 sets WSL_DISTRO_NAME; WSL1 leaves a Microsoft fingerprint in
# /proc/version. Check both.
detect_wsl() {
    if [ -n "${WSL_DISTRO_NAME:-}" ]; then
        echo "wsl"
        return 0
    fi
    if [ -r /proc/version ] && grep -qi 'microsoft\|wsl' /proc/version 2>/dev/null; then
        echo "wsl"
        return 0
    fi
    echo ""
}

if [ -n "$(detect_wsl)" ]; then
    step "WSL detected"
    warn "you are running under WSL (Windows Subsystem for Linux)"
    say ""
    say "Mneme is meant to run as a NATIVE Windows install when used from WSL,"
    say "so the daemon + MCP server are visible to Claude Code on the Windows host."
    say "Installing into the WSL filesystem will work but creates a second mneme"
    say "instance that the Windows-side Claude Code cannot reach."
    say ""
    say "RECOMMENDED -- run the Windows installer instead, from PowerShell:"
    say "  iex (irm https://github.com/omanishay-cyber/mneme/releases/download/${MNEME_REL_TAG}/bootstrap-install.ps1)"
    say ""
    say "If you genuinely want a Linux-side install (e.g. you're using Claude Code"
    say "from inside WSL), set MNEME_FORCE_WSL=1 and re-run this script."
    say ""
    if [ -z "${MNEME_FORCE_WSL:-}" ]; then
        fail "aborting WSL install (set MNEME_FORCE_WSL=1 to override)"
    else
        say "MNEME_FORCE_WSL=1 set -- continuing with Linux install inside WSL"
    fi
fi

# -----------------------------------------------------------------------------
# Banner + detected-environment block (B11.5w: "user sees what the script sees")
# -----------------------------------------------------------------------------

OS=$(detect_os)
if [ "${OS}" != "linux" ]; then
    fail "this installer is for Linux only (detected: ${OS}). Use install-mac.sh on macOS."
fi
ARCH=$(detect_arch)

# Detect distro family for clearer install hints in error paths.
DISTRO_FAMILY="unknown"
DISTRO_NAME="linux"
if [ -r /etc/os-release ]; then
    # Source carefully -- /etc/os-release is shell-syntax key=value.
    # shellcheck source=/dev/null
    . /etc/os-release
    DISTRO_NAME="${PRETTY_NAME:-${NAME:-linux}}"
    case "${ID:-}:${ID_LIKE:-}" in
        debian:*|*:*debian*|ubuntu:*|*:*ubuntu*) DISTRO_FAMILY="debian" ;;
        fedora:*|rhel:*|centos:*|rocky:*|*:*rhel*|*:*fedora*) DISTRO_FAMILY="redhat" ;;
        arch:*|*:*arch*|manjaro:*) DISTRO_FAMILY="arch" ;;
        alpine:*) DISTRO_FAMILY="alpine" ;;
        opensuse*:*|*:*suse*) DISTRO_FAMILY="suse" ;;
        *) DISTRO_FAMILY="other" ;;
    esac
fi

# Detect package manager string for install hints.
PKG_MGR="unknown"
if   command -v apt-get >/dev/null 2>&1; then PKG_MGR="apt"
elif command -v dnf     >/dev/null 2>&1; then PKG_MGR="dnf"
elif command -v yum     >/dev/null 2>&1; then PKG_MGR="yum"
elif command -v pacman  >/dev/null 2>&1; then PKG_MGR="pacman"
elif command -v apk     >/dev/null 2>&1; then PKG_MGR="apk"
elif command -v zypper  >/dev/null 2>&1; then PKG_MGR="zypper"
fi

step "mneme bootstrap installer (Linux)"
say "version    : ${MNEME_REL_TAG}"
say "user       : ${USER:-$(id -un)}"
say "distro     : ${DISTRO_NAME} (family: ${DISTRO_FAMILY})"
say "arch       : ${ARCH} (uname -m: $(uname -m))"
say "kernel     : $(uname -r)"
say "shell      : ${SHELL:-unknown}"
say "pkg mgr    : ${PKG_MGR}"
say "target     : ${HOME}/.local/share/mneme"
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
# ~7 GB because models stage at /tmp/mneme-bootstrap.XXX/models (3.6 GB)
# THEN copy into ~/.mneme/models (another 3.6 GB) while the staging dir
# is still alive. 5 GB passes pre-flight then fails halfway through the
# phi-3-mini-4k copy with ENOSPC. 8 GB covers staging + final + binaries
# + working space + a small headroom.
pre_flight_disk_space 8

# bash version (we target bash 3.2+ semantics for portability).
if [ -z "${BASH_VERSION:-}" ]; then
    fail "this script requires bash (running shell: ${SHELL:-unknown})"
fi
ok "bash ${BASH_VERSION}"

# curl + tar should be present on every modern Linux distro. We require
# them outright -- distro-specific install hints below if missing.
if ! command -v curl >/dev/null 2>&1; then
    case "${DISTRO_FAMILY}" in
        debian) fail "curl missing -- install: sudo apt-get install -y curl" ;;
        redhat) fail "curl missing -- install: sudo dnf install -y curl" ;;
        arch)   fail "curl missing -- install: sudo pacman -S --noconfirm curl" ;;
        alpine) fail "curl missing -- install: sudo apk add --no-cache curl" ;;
        suse)   fail "curl missing -- install: sudo zypper install -y curl" ;;
        *)      fail "curl missing -- install via your distro's package manager" ;;
    esac
fi
require_cmd tar "should be preinstalled on Linux -- check /usr/bin/tar"

# B-L03 (2026-05-03): unzip is required by the `bun` upstream installer
# (bun.sh/install hard-fails with "unzip is required to install bun" on
# fresh Ubuntu 22.04 and other minimal images). Probe + offer install hint.
if ! command -v unzip >/dev/null 2>&1; then
    case "${DISTRO_FAMILY}" in
        debian) fail "unzip missing (required by Bun installer) -- install: sudo apt-get install -y unzip" ;;
        redhat) fail "unzip missing (required by Bun installer) -- install: sudo dnf install -y unzip" ;;
        arch)   fail "unzip missing (required by Bun installer) -- install: sudo pacman -S --noconfirm unzip" ;;
        alpine) fail "unzip missing (required by Bun installer) -- install: sudo apk add --no-cache unzip" ;;
        suse)   fail "unzip missing (required by Bun installer) -- install: sudo zypper install -y unzip" ;;
        *)      fail "unzip missing (required by Bun installer) -- install via your distro's package manager" ;;
    esac
fi

ok "curl + tar + unzip present"

# git: optional, only matters for `mneme build` commit-SHA metadata.
if command -v git >/dev/null 2>&1; then
    ok "git $(git --version | awk '{print $3}') present"
else
    warn "git not found -- mneme will still work but commit-SHA metadata is unavailable"
    case "${DISTRO_FAMILY}" in
        debian) say "to install: sudo apt-get install -y git" ;;
        redhat) say "to install: sudo dnf install -y git" ;;
        arch)   say "to install: sudo pacman -S --noconfirm git" ;;
        alpine) say "to install: sudo apk add --no-cache git" ;;
        suse)   say "to install: sudo zypper install -y git" ;;
        *)      say "to install: see https://git-scm.com/" ;;
    esac
fi

# systemd availability check (we use systemd --user; many containers
# don't ship it). Failing gracefully rather than aborting.
SYSTEMD_AVAILABLE=0
if [ -z "${MNEME_NO_SYSTEMD:-}" ] && command -v systemctl >/dev/null 2>&1; then
    # Even when systemctl is on PATH, the user-mode bus may not be set up
    # (rootless containers, Docker default, minimal images). Probe.
    if systemctl --user list-units --no-legend >/dev/null 2>&1; then
        SYSTEMD_AVAILABLE=1
        ok "systemd --user is available"
    else
        warn "systemctl present but systemd --user is not running for this user"
        say "  daemon will be started directly (no systemd unit) -- see step 9"
    fi
else
    if [ -n "${MNEME_NO_SYSTEMD:-}" ]; then
        say "MNEME_NO_SYSTEMD=1 -- systemd unit setup will be skipped"
    else
        warn "systemctl not found -- systemd unit setup will be skipped"
    fi
fi

# -----------------------------------------------------------------------------
# Bun runtime (required for MCP server)
# -----------------------------------------------------------------------------

step "bun runtime"

BUN_BIN=""
if command -v bun >/dev/null 2>&1; then
    BUN_BIN=$(command -v bun)
elif [ -x "${HOME}/.bun/bin/bun" ]; then
    BUN_BIN="${HOME}/.bun/bin/bun"
elif [ -x "/usr/local/bin/bun" ]; then
    BUN_BIN="/usr/local/bin/bun"
fi

if [ -n "${BUN_BIN}" ]; then
    ok "bun $("${BUN_BIN}" --version) present at ${BUN_BIN}"
else
    say "bun not found -- installing from https://bun.sh/install"
    # Bun's installer writes to ~/.bun/bin and updates the user's shell rc.
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

UNIT_NAME="mneme-daemon.service"
UNIT_PATH="${HOME}/.config/systemd/user/${UNIT_NAME}"

# If a systemd unit is present, stop + disable it cleanly first.
if [ -f "${UNIT_PATH}" ] && [ "${SYSTEMD_AVAILABLE}" -eq 1 ]; then
    say "stopping existing systemd unit ${UNIT_NAME}"
    systemctl --user stop "${UNIT_NAME}" 2>/dev/null || true
    systemctl --user disable "${UNIT_NAME}" 2>/dev/null || true
fi

# pkill anchored regex -- only matches argv[0] starting with mneme.
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

ASSET="mneme-${MNEME_REL_TAG}-linux-${ARCH}.tar.gz"
ASSET_URL="${RELEASE_BASE}/${ASSET}"

TMP_DIR=$(mktemp -d -t mneme-bootstrap.XXXXXX)
if [ -z "${MNEME_KEEP_DOWNLOAD:-}" ]; then
    trap 'rm -rf "${TMP_DIR}"' EXIT INT TERM
else
    say "MNEME_KEEP_DOWNLOAD=1 -- temp dir preserved at ${TMP_DIR}"
fi

LOCAL_TARBALL="${TMP_DIR}/${ASSET}"

# The Linux per-arch tarballs are produced by the cross-compile CI job
# (B11.55, separate agent's scope). Until that ships, the asset may
# 404. We give the user a clear, actionable error rather than a cryptic
# "download failed".
if ! download_with_retry "${ASSET_URL}" "${LOCAL_TARBALL}" 3; then
    fail "could not download ${ASSET}
       URL: ${ASSET_URL}
       This usually means the linux-${ARCH} binary has not yet been
       uploaded to the v0.3.2 release page. Check the release at:
         https://github.com/omanishay-cyber/mneme/releases/tag/${MNEME_REL_TAG}
       If the asset is listed there, re-run this script. If not, the
       cross-compile workflow may still be building -- retry in ~15 min."
fi

# -----------------------------------------------------------------------------
# Extract tarball + symlink
# -----------------------------------------------------------------------------

step "extract to ~/.local/share/mneme"

# XDG-compliant install path for user-scoped applications. Falls under
# the user's $HOME so no sudo is needed.
MNEME_HOME="${HOME}/.local/share/mneme"
mkdir -p "${MNEME_HOME}"

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
# systemd user unit registration
# -----------------------------------------------------------------------------

step "register systemd user unit (${UNIT_NAME})"

if [ "${SYSTEMD_AVAILABLE}" -eq 1 ]; then
    mkdir -p "${HOME}/.config/systemd/user"
    cat > "${UNIT_PATH}" <<UNIT_EOF
[Unit]
Description=mneme daemon (code-graph supervisor)
Documentation=https://github.com/omanishay-cyber/mneme
After=network.target

[Service]
# B-L05 (2026-05-03): invoke the supervisor binary directly. The CLI's
# 'mneme daemon start' wrapper spawns supervisor detached and EXITS,
# which under Type=simple makes systemd think the unit failed (and the
# unsupported --foreground flag also tripped clap). The supervisor
# binary mneme-daemon is itself the long-running process — Type=simple
# owns it directly, no double-spawn, no --foreground needed.
Type=simple
ExecStart=${MNEME_HOME}/bin/mneme-daemon start
Restart=on-failure
RestartSec=10
TimeoutStartSec=30
TimeoutStopSec=15

# Logging
StandardOutput=append:${HOME}/.local/state/mneme/daemon.log
StandardError=append:${HOME}/.local/state/mneme/daemon.log

# Environment
Environment="HOME=${HOME}"
Environment="PATH=${MNEME_HOME}/bin:${HOME}/.bun/bin:/usr/local/bin:/usr/bin:/bin"

[Install]
WantedBy=default.target
UNIT_EOF

    # Make sure the log directory exists; systemd refuses to start
    # otherwise (cannot open append: target).
    mkdir -p "${HOME}/.local/state/mneme"

    ok "wrote ${UNIT_PATH}"

    # Reload + enable + start. enable --now is a single atomic op.
    if systemctl --user daemon-reload 2>/dev/null \
        && systemctl --user enable --now "${UNIT_NAME}" 2>/dev/null; then
        ok "systemd unit enabled + started"
    else
        warn "could not enable+start the systemd unit -- you can do it manually with:"
        say "  systemctl --user daemon-reload"
        say "  systemctl --user enable --now ${UNIT_NAME}"
    fi

    # Linger so the daemon survives logout. Best-effort -- requires
    # `loginctl` and write access to /var/lib/systemd/linger which is
    # often allowed for the user themselves.
    if command -v loginctl >/dev/null 2>&1; then
        if loginctl enable-linger "$(id -un)" 2>/dev/null; then
            ok "enabled linger -- daemon will keep running after logout"
        else
            say "could not enable linger (the daemon will stop on logout)"
            say "  to enable manually: sudo loginctl enable-linger $(id -un)"
        fi
    fi
else
    warn "systemd unavailable -- starting daemon directly (will not survive reboot)"
    DAEMON_LOG="/tmp/mneme-daemon-$(id -u).log"
    nohup "${MNEME_BIN}" daemon start </dev/null >"${DAEMON_LOG}" 2>&1 &
    say "spawned ${MNEME_BIN} daemon start (parent pid $!), log at ${DAEMON_LOG}"
fi

# -----------------------------------------------------------------------------
# Run inner installer (registers MCP, hooks, etc.)
# -----------------------------------------------------------------------------

step "run inner installer (scripts/install.sh)"

INNER="${MNEME_HOME}/scripts/install.sh"
if [ -f "${INNER}" ]; then
    chmod +x "${INNER}" 2>/dev/null || true
    # The inner installer is shared with macOS. We invoke with --skip-download
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
    # A7-022 (2026-05-04): every row now has 4 tab-separated fields. The
    # phi-3 row previously had 3 fields (no GitHub fallback because the
    # 2.28 GB single file exceeds GitHub's 2 GB asset cap), which under
    # `set -u` would leave m_fallback unbound on that row. Trailing tab
    # + empty 4th column makes every row uniformly shaped; the
    # download_dual_source helper accepts an empty fallback URL and
    # treats it as "primary-only" (no GitHub fallback for phi-3).
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
        # A7-022 (2026-05-04): defensive default. With every row now
        # 4-field (per the trailing-tab fix above) m_fallback is always
        # set to either a URL or empty, but `read` on bash 3.2 (some
        # macOS hosts) can leave a trailing-tab field unset. Force-
        # default so `set -u` doesn't trip later when m_fallback is
        # passed to download_dual_source.
        m_fallback="${m_fallback:-}"
        IFS=$'\n'
        dest="${MODELS_DIR}/${m_name}"
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
    if [ "${SYSTEMD_AVAILABLE}" -eq 1 ]; then
        say "  inspect: systemctl --user status ${UNIT_NAME}"
        say "  logs:    journalctl --user -u ${UNIT_NAME} -n 50"
    else
        say "  inspect: tail -n 50 /tmp/mneme-daemon-$(id -u).log"
    fi
    say "  manual: ${MNEME_BIN} doctor"
fi

# -----------------------------------------------------------------------------
# Done
# -----------------------------------------------------------------------------

step "DONE"
say "mneme ${MNEME_REL_TAG} installed for linux-${ARCH}."
echo ""
say "Verify (in a NEW shell so PATH picks up ~/.mneme/bin):"
say "  mneme --version"
say "  mneme doctor"
say "  claude mcp list           # should show: mneme: Connected"
echo ""

# Final boxed banner. Mirrors the Windows + macOS installers' end-of-script
# block so the PATH-just-applied note can't be skimmed past.
echo ""
printf '%b  +-----------------------------------------------------------+%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |  IMPORTANT: open a NEW shell session before running       |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |  "mneme doctor" or "mneme build" -- the PATH change just  |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |  applied is not visible in this shell session.            |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |                                                           |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
if [ "${SYSTEMD_AVAILABLE}" -eq 1 ]; then
printf '%b  |  Daemon is registered as a systemd --user unit. Manage:   |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |    systemctl --user status mneme-daemon                   |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |    journalctl --user -u mneme-daemon -f                   |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
else
printf '%b  |  systemd unavailable -- daemon was started directly and   |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |  will NOT survive reboot. Use a process supervisor or     |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
printf '%b  |  cron @reboot to keep it alive.                           |%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
fi
printf '%b  +-----------------------------------------------------------+%b\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}"
echo ""
