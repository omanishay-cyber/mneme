#!/usr/bin/env sh
# mneme - INNER installer for macOS / Linux (v0.3.1+)
#
# THIS IS THE INNER INSTALLER. For a one-command end-user install,
# use the bootstrap entry points in `release/` instead:
#   macOS:  curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-mac.sh   | bash
#   Linux:  curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-linux.sh | bash
# Those wrappers handle download + extract + daemon registration, then
# invoke this script with --skip-download to wire MCP / hooks / PATH.
#
# Direct usage (advanced -- assumes ~/.mneme is already populated, or
# you're running from inside an extracted tarball):
#   curl -fsSL https://raw.githubusercontent.com/omanishay-cyber/mneme/main/scripts/install.sh | sh
#
# Flags (POSIX-sh argument parsing):
#   --local-zip <path>     skip GitHub fetch in step 2/8, use the
#                          caller-supplied tarball directly in step 3/8.
#                          Same use cases as install.ps1's -LocalZip:
#                          air-gapped installs, locally-built betas,
#                          test ships where the archive has been scp'd
#                          onto the target machine. The path must exist;
#                          tag_name is reported as 'local-zip'.
#   --skip-download        skip BOTH step 2/8 (release metadata) AND
#                          step 3/8 (download + extract). Assumes
#                          ~/.mneme/ is already populated. install.sh
#                          verifies ~/.mneme/bin/mneme exists before
#                          continuing; if missing, errors with a clear
#                          remediation hint.
#
# Note: when piped via `curl ... | sh`, sh has no way to receive these
# flags. Use them only when invoking the script directly:
#   sh ./install.sh --local-zip /path/to/mneme.tar.gz
#   sh ./install.sh --skip-download
#
# (WIDE-005: banner repo path matches the REPO= line below. Earlier
# revisions said `omanishay-cyber/codex/...` which 404'd for anyone who
# copied the URL out of the comment.)
#
# What it does, in order (mirrors scripts/install.ps1):
#   0. Stops any running mneme processes (upgrade safety, prevents
#      partial-extract over locked binaries).
#   1. Detects required runtimes (bun / node / git). Prints a clear
#      manual-install hint for each missing tool. Does NOT auto-invoke
#      sudo - a piped-curl installer should never run sudo on Unix.
#   2. Resolves the latest GitHub release asset for the host OS+arch.
#      Skipped under --local-zip / --skip-download.
#   3. Downloads + extracts to ~/.mneme/ (bin/, mcp/, plugin/).
#      Under --local-zip: extracts the supplied tarball.
#      Under --skip-download: verifies layout only, no extract.
#   4. (No Windows Defender on Unix - prints a SELinux note for
#      Fedora/Rocky users instead.)
#   5. Adds ~/.mneme/bin to the user's PATH via ~/.profile (Linux)
#      or ~/.zprofile (macOS).
#   6. Starts the mneme daemon in the background and polls daemon
#      status until healthy or 15s timeout.
#   7. Registers the mneme MCP server with Claude Code.
#   8. Prints next steps and verification commands.
#
# Safe to re-run. Every step is idempotent; a step that fails prints a
# clear warning and does not abort the remaining steps (except when
# download / extract themselves fail, which is unrecoverable).
#
# POSIX sh-compatible. ASCII-only output. No emoji.

set -eu

REPO="omanishay-cyber/mneme"
HOME_DIR="${HOME:?HOME not set}"
MNEME_HOME="${HOME_DIR}/.mneme"
BIN_DIR="${MNEME_HOME}/bin"
MNEME_BIN="${BIN_DIR}/mneme"

# ----------------------------------------------------------------------------
# Argument parsing for --local-zip / --skip-download.
#
# We deliberately handle these BEFORE any side effects so the banner can
# report the active source. Mutually-exclusive: passing both is almost
# certainly a mistake (local-zip implies extract; skip-download implies
# skip extract). Fail fast.
# ----------------------------------------------------------------------------

LOCAL_ZIP=""
SKIP_DOWNLOAD=0

while [ $# -gt 0 ]; do
    case "$1" in
        --local-zip)
            if [ $# -lt 2 ]; then
                printf '    error: --local-zip requires a path argument\n' >&2
                exit 1
            fi
            LOCAL_ZIP="$2"
            shift 2
            ;;
        --local-zip=*)
            LOCAL_ZIP="${1#--local-zip=}"
            shift
            ;;
        --skip-download)
            SKIP_DOWNLOAD=1
            shift
            ;;
        --help|-h)
            sed -n '2,30p' "$0"
            exit 0
            ;;
        *)
            printf '    warning: unknown flag %s (continuing)\n' "$1" >&2
            shift
            ;;
    esac
done

if [ -n "${LOCAL_ZIP}" ] && [ "${SKIP_DOWNLOAD}" -eq 1 ]; then
    printf '==> mneme - one-line installer\n'
    printf '    error: --local-zip and --skip-download are mutually exclusive\n' >&2
    printf '           --local-zip <path>  : extract from a local tarball (skip GitHub fetch)\n' >&2
    printf '           --skip-download     : skip BOTH fetch + extract (assume ~/.mneme already populated)\n' >&2
    exit 1
fi

if [ -n "${LOCAL_ZIP}" ]; then
    if [ ! -f "${LOCAL_ZIP}" ]; then
        printf '==> mneme - one-line installer\n'
        printf '    error: --local-zip path does not exist: %s\n' "${LOCAL_ZIP}" >&2
        exit 1
    fi
    # Canonicalise so logs show the resolved path. `realpath` is not
    # POSIX, but we can fake it with cd+pwd which is portable.
    LOCAL_ZIP_DIR=$(cd "$(dirname "${LOCAL_ZIP}")" && pwd)
    LOCAL_ZIP_BASE=$(basename "${LOCAL_ZIP}")
    LOCAL_ZIP="${LOCAL_ZIP_DIR}/${LOCAL_ZIP_BASE}"
fi

# ----------------------------------------------------------------------------
# Output helpers (ASCII-only; intentionally plain so it pipes cleanly into
# CI logs and is screen-reader friendly).
# ----------------------------------------------------------------------------

step() { printf '==> %s\n' "$1"; }
info() { printf '    %s\n' "$1"; }
warn() { printf '    warning: %s\n' "$1" >&2; }
ok()   { printf '    ok: %s\n' "$1"; }
fail() { printf '    error: %s\n' "$1" >&2; }

# B-006 follow-on parity helper: generic native-exe probe.
#
# install.sh runs under `set -eu`, which is sh's analogue of PowerShell's
# `$ErrorActionPreference = 'Stop'`. A bare `"$exe" --version` would abort
# the script the moment the exe exits non-zero. The install.ps1 helper
# `Invoke-NativeProbe` returns a structured result instead of throwing;
# `invoke_probe` is the sh equivalent and returns the exit code without
# tripping `set -e` (the trailing `return $?` makes the call a `||`-able
# statement so `set -e` does not abort).
#
# Usage:
#   if invoke_probe "/path/to/exe" --version; then ...; fi
#
# Returns the exe's exit code (0 = success). Returns 1 if the exe path
# is unset or not executable (without trying to run it).
invoke_probe() {
    local exe="$1"; shift
    [ -n "${exe}" ] || return 1
    [ -x "${exe}" ] || return 1
    "${exe}" "$@" >/dev/null 2>&1
    return $?
}

# ----------------------------------------------------------------------------
# Step 0 - Stop any running mneme processes (upgrade safety)
# ----------------------------------------------------------------------------
#
# If a previous mneme daemon / worker is alive, tar will happily overwrite
# the executable inode on Linux/macOS (unlike Windows), but the *running*
# process keeps the old code in memory. The user then thinks they upgraded
# but is still talking to the old binary until the daemon is restarted.
# Easier: just stop everything first. If nothing's running, this is a
# silent no-op.

step "step 0/8 - stop any existing mneme daemon + workers"

# pkill with anchored regex so we don't accidentally kill processes whose
# command line CONTAINS "mneme" but aren't ours (e.g. an editor with a
# mneme path open). `^mneme` only matches argv[0] starting with mneme.
tries=0
while [ "${tries}" -lt 5 ]; do
    if command -v pkill >/dev/null 2>&1; then
        if pkill -f '^mneme' >/dev/null 2>&1; then
            info "sent SIGTERM to mneme process(es); waiting"
            sleep 2
        else
            break
        fi
    else
        # No pkill (rare on minimal containers). Best-effort via the CLI.
        if [ -x "${MNEME_BIN}" ]; then
            "${MNEME_BIN}" daemon stop >/dev/null 2>&1 || true
            sleep 1
        fi
        break
    fi
    tries=$((tries + 1))
done

if command -v pgrep >/dev/null 2>&1 && pgrep -f '^mneme' >/dev/null 2>&1; then
    warn "could not stop all mneme processes - close any 'mneme' shell and rerun"
    warn "tar will still extract on Unix, but running processes keep the old binary in memory"
else
    ok "no mneme processes running - safe to extract"
fi

# ----------------------------------------------------------------------------
# Step 1 - Check runtime prerequisites
# ----------------------------------------------------------------------------
#
# Three tools matter for a full mneme + Claude-Code experience:
#   bun  - mneme's MCP server runtime (`mneme mcp stdio` runs Bun TS).
#   node - only needed for the Claude Code CLI itself (`@anthropic-ai/claude-code`).
#   git  - optional, gives `mneme build` commit SHA metadata.
#
# Adaptation from install.ps1: on Windows we silently install missing
# tools because winget / direct MSI is safe at user scope. On Unix we
# REFUSE to invoke sudo from a piped-curl install. We detect, then print
# the platform-correct manual-install command. The user has full control.

# Detect a tool: PATH first, then platform-standard fallback locations.
# Echoes the resolved path (or empty string if not found).
find_tool() {
    name="$1"
    if command -v "${name}" >/dev/null 2>&1; then
        command -v "${name}"
        return 0
    fi
    # Fallback locations vary by OS.
    case "${UNAME_S}" in
        Darwin)
            for p in \
                "/opt/homebrew/bin/${name}" \
                "/usr/local/bin/${name}" \
                "${HOME_DIR}/.bun/bin/${name}"; do
                if [ -x "${p}" ]; then
                    echo "${p}"
                    return 0
                fi
            done
            ;;
        Linux)
            for p in \
                "/usr/bin/${name}" \
                "/usr/local/bin/${name}" \
                "${HOME_DIR}/.bun/bin/${name}"; do
                if [ -x "${p}" ]; then
                    echo "${p}"
                    return 0
                fi
            done
            ;;
    esac
    echo ""
}

# Detect package manager (used for clearer install hints).
# Echoes one of: brew | apt | dnf | yum | pacman | apk | unknown
detect_pkg_mgr() {
    case "${UNAME_S}" in
        Darwin)
            if command -v brew >/dev/null 2>&1; then echo brew
            else echo unknown; fi
            ;;
        Linux)
            if command -v apt-get >/dev/null 2>&1; then echo apt
            elif command -v dnf      >/dev/null 2>&1; then echo dnf
            elif command -v yum      >/dev/null 2>&1; then echo yum
            elif command -v pacman   >/dev/null 2>&1; then echo pacman
            elif command -v apk      >/dev/null 2>&1; then echo apk
            else echo unknown; fi
            ;;
        *) echo unknown ;;
    esac
}

# Print the manual install hint for a missing tool.
hint_install() {
    tool="$1"
    case "${PKG_MGR}-${tool}" in
        brew-bun)     info "to install: brew install oven-sh/bun/bun" ;;
        brew-node)    info "to install: brew install node" ;;
        brew-git)     info "to install: brew install git" ;;
        apt-bun)      info "to install: curl -fsSL https://bun.sh/install | bash" ;;
        apt-node)     info "to install: sudo apt install nodejs npm" ;;
        apt-git)      info "to install: sudo apt install git" ;;
        dnf-bun)      info "to install: curl -fsSL https://bun.sh/install | bash" ;;
        dnf-node)     info "to install: sudo dnf install nodejs npm" ;;
        dnf-git)      info "to install: sudo dnf install git" ;;
        yum-bun)      info "to install: curl -fsSL https://bun.sh/install | bash" ;;
        yum-node)     info "to install: sudo yum install nodejs npm" ;;
        yum-git)      info "to install: sudo yum install git" ;;
        pacman-bun)   info "to install: sudo pacman -S bun  (or: curl -fsSL https://bun.sh/install | bash)" ;;
        pacman-node)  info "to install: sudo pacman -S nodejs npm" ;;
        pacman-git)   info "to install: sudo pacman -S git" ;;
        apk-bun)      info "to install: curl -fsSL https://bun.sh/install | bash" ;;
        apk-node)     info "to install: sudo apk add nodejs npm" ;;
        apk-git)      info "to install: sudo apk add git" ;;
        *-bun)        info "to install: curl -fsSL https://bun.sh/install | bash" ;;
        *-node)       info "to install: see https://nodejs.org/" ;;
        *-git)        info "to install: see https://git-scm.com/" ;;
    esac
}

# OS detection runs early so Step 0 / Step 1 can use UNAME_S.
UNAME_S=$(uname -s 2>/dev/null || echo unknown)
UNAME_M=$(uname -m 2>/dev/null || echo unknown)
PKG_MGR=$(detect_pkg_mgr)

if [ "${SKIP_DOWNLOAD}" -eq 1 ]; then
    INSTALL_SOURCE="pre-extracted (--skip-download, no fetch + no extract)"
elif [ -n "${LOCAL_ZIP}" ]; then
    INSTALL_SOURCE="local zip ${LOCAL_ZIP}"
else
    INSTALL_SOURCE="github releases (${REPO}/releases/latest)"
fi

step "mneme - one-line installer"
info "target   : ${MNEME_HOME}"
info "bin      : ${BIN_DIR}"
info "os       : ${UNAME_S} ${UNAME_M}"
info "pkg mgr  : ${PKG_MGR}"
info "source   : ${INSTALL_SOURCE}"
echo ""

step "step 1/8 - runtime prerequisites (bun / node / git)"

# --- 1a. bun (required for MCP server) -------------------------------------
BUN_PATH=$(find_tool bun)
if [ -n "${BUN_PATH}" ]; then
    BUN_VER=$("${BUN_PATH}" --version 2>/dev/null || echo "?")
    ok "bun ${BUN_VER} present at ${BUN_PATH}"
else
    warn "bun not found - mneme CLI will work, but MCP tools in Claude Code will not"
    hint_install bun
fi

# --- 1b. node (for Claude Code CLI) ----------------------------------------
NODE_PATH=$(find_tool node)
if [ -n "${NODE_PATH}" ]; then
    NODE_VER=$("${NODE_PATH}" --version 2>/dev/null || echo "?")
    ok "node ${NODE_VER} present at ${NODE_PATH}"
else
    warn "node not found - Claude Code CLI cannot be installed until node is present"
    hint_install node
fi

# --- 1c. git (optional) ----------------------------------------------------
GIT_PATH=$(find_tool git)
if [ -n "${GIT_PATH}" ]; then
    GIT_VER=$("${GIT_PATH}" --version 2>/dev/null || echo "?")
    ok "${GIT_VER} present at ${GIT_PATH}"
else
    warn "git not found - mneme will still work, but no commit-SHA metadata in the graph"
    hint_install git
fi

# --- 1d. Optional dev-toolchain probes (G1-G10 from phase-a-issues.md) -----
#
# Beyond bun/node/git (already probed above), mneme integrates with a
# longer dev-toolchain list per our project directive: rust, tauri-cli,
# python, sqlite3, java, tesseract, magick. We DO NOT auto-install any
# of these from a piped-curl shell installer - that path should never
# touch sudo. We probe, surface what's missing with one concrete fix
# command per tool, and let the user decide.
#
# Canonical list (and same logic) lives in
# `cli/src/commands/doctor.rs::KNOWN_TOOLCHAIN` and surfaces via
# `mneme doctor --strict`.

step "step 1d/8 - optional dev-toolchain probes (detect-only, no auto-install)"

# Per-tool platform-specific install hint (only the unix flavours).
toolchain_hint() {
    tool="$1"
    case "${PKG_MGR}-${tool}" in
        brew-rust)         echo "brew install rustup-init  &&  rustup-init -y" ;;
        brew-tauri)        echo "cargo install tauri-cli --version \"^2.0\"" ;;
        brew-python)       echo "brew install python@3.11" ;;
        brew-sqlite3)      echo "brew install sqlite" ;;
        brew-java)         echo "brew install openjdk@21" ;;
        brew-tesseract)    echo "brew install tesseract" ;;
        brew-magick)       echo "brew install imagemagick" ;;
        apt-rust)          echo "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" ;;
        apt-tauri)         echo "cargo install tauri-cli --version \"^2.0\"" ;;
        apt-python)        echo "sudo apt install python3 python3-pip" ;;
        apt-sqlite3)       echo "sudo apt install sqlite3" ;;
        apt-java)          echo "sudo apt install openjdk-21-jdk" ;;
        apt-tesseract)     echo "sudo apt install tesseract-ocr" ;;
        apt-magick)        echo "sudo apt install imagemagick" ;;
        dnf-rust|yum-rust) echo "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" ;;
        dnf-python|yum-python) echo "sudo dnf install python3 python3-pip" ;;
        dnf-sqlite3|yum-sqlite3) echo "sudo dnf install sqlite" ;;
        dnf-java|yum-java) echo "sudo dnf install java-21-openjdk-devel" ;;
        dnf-tesseract|yum-tesseract) echo "sudo dnf install tesseract" ;;
        dnf-magick|yum-magick) echo "sudo dnf install ImageMagick" ;;
        pacman-rust)       echo "sudo pacman -S rustup  &&  rustup default stable" ;;
        pacman-python)     echo "sudo pacman -S python python-pip" ;;
        pacman-sqlite3)    echo "sudo pacman -S sqlite" ;;
        pacman-java)       echo "sudo pacman -S jdk-openjdk" ;;
        pacman-tesseract)  echo "sudo pacman -S tesseract" ;;
        pacman-magick)     echo "sudo pacman -S imagemagick" ;;
        apk-rust)          echo "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" ;;
        apk-python)        echo "sudo apk add python3 py3-pip" ;;
        apk-sqlite3)       echo "sudo apk add sqlite" ;;
        apk-java)          echo "sudo apk add openjdk21" ;;
        apk-tesseract)     echo "sudo apk add tesseract-ocr" ;;
        apk-magick)        echo "sudo apk add imagemagick" ;;
        # Fallbacks if pkg manager unknown.
        *-rust)            echo "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" ;;
        *-tauri)           echo "cargo install tauri-cli --version \"^2.0\"" ;;
        *-python)          echo "see https://www.python.org/downloads/" ;;
        *-sqlite3)         echo "see https://www.sqlite.org/download.html" ;;
        *-java)            echo "see https://adoptium.net/" ;;
        *-tesseract)       echo "see https://github.com/tesseract-ocr/tesseract" ;;
        *-magick)          echo "see https://imagemagick.org/script/download.php" ;;
        *)                 echo "(no install hint available)" ;;
    esac
}

# (id, severity, display-name, binary, purpose)
# Severity tier: HIGH = blocks `mneme doctor --strict`; MED/LOW are warnings.
# Tab-separated to keep the loop trivial in POSIX sh.
TOOLCHAIN_LIST="$(cat <<'EOF'
G1	HIGH	Rust toolchain	cargo	rust	vision/tauri/ build, future Rust-port work
G4	MED	Tauri CLI	tauri	tauri	ergonomic Tauri builds (tauri build / tauri dev)
G6	MED	Python	python3	python	PNG->ICO icon conversion (PIL), multimodal sidecar
G7	LOW	SQLite CLI	sqlite3	sqlite3	manual shard inspection (sqlite3 graph.db .schema)
G8	LOW	Java JDK	java	java	optional - only if a future feature needs the JVM
G9	MED	Tesseract OCR	tesseract	tesseract	image OCR via multimodal sidecar (binary feature-gated)
G10	LOW	ImageMagick	magick	magick	PNG->ICO conversion fallback when Python+PIL unavailable
EOF
)"

TOOLCHAIN_MISSING=0
# IFS surgery so we read tab-separated rows.
OLDIFS="${IFS}"
IFS='
'
for row in ${TOOLCHAIN_LIST}; do
    IFS='	' read -r tc_id tc_sev tc_name tc_bin tc_hint_key tc_purpose <<EOF
${row}
EOF
    IFS="${OLDIFS}"
    found=""
    if command -v "${tc_bin}" >/dev/null 2>&1; then
        found=$(command -v "${tc_bin}")
    fi
    # Python falls back to `python` if `python3` not present.
    if [ -z "${found}" ] && [ "${tc_bin}" = "python3" ]; then
        if command -v python >/dev/null 2>&1; then
            found=$(command -v python)
        fi
    fi
    # B-006 parity: on macOS/Linux there is no equivalent of the
    # Microsoft-Store python.exe stub (Windows-only path under
    # `*\WindowsApps\*`), but if found python lives under
    # `/mnt/c/Users/*/AppData/Local/Microsoft/WindowsApps/` (WSL bridge
    # to host stub) treat it as missing. install.sh stays detect-only,
    # so this is just a cleaner "missing" vs "broken" classification.
    if [ -n "${found}" ] && [ "${tc_bin}" = "python3" ]; then
        case "${found}" in
            */WindowsApps/*|*/windowsapps/*)
                warn "[${tc_id}] python at ${found} is the WSL-bridged Microsoft-Store stub - treating as missing"
                found=""
                ;;
        esac
    fi
    # Tauri CLI also reachable via `cargo tauri --version`.
    # Use invoke_probe so a `cargo: no such command: tauri` exit-101 case
    # cannot trip `set -e` even if the surrounding context loses the
    # protective `if` (parity with install.ps1's Invoke-NativeProbe helper).
    if [ -z "${found}" ] && [ "${tc_bin}" = "tauri" ]; then
        cargo_path=$(command -v cargo 2>/dev/null || true)
        if [ -n "${cargo_path}" ] && invoke_probe "${cargo_path}" tauri --version; then
            ok "[${tc_id}] ${tc_name} present (cargo subcommand)"
            IFS='
'
            continue
        fi
    fi
    if [ -n "${found}" ]; then
        ver=$("${found}" --version 2>&1 | head -n1 | sed 's/[[:space:]]*$//')
        ok "[${tc_id}] ${tc_name} present (${ver})"
    else
        TOOLCHAIN_MISSING=$((TOOLCHAIN_MISSING + 1))
        warn "[${tc_id}] [${tc_sev}] ${tc_name} NOT detected - ${tc_purpose}"
        info "    Install: $(toolchain_hint "${tc_hint_key}")"
    fi
    IFS='
'
done
IFS="${OLDIFS}"

if [ "${TOOLCHAIN_MISSING}" -eq 0 ]; then
    ok "every optional dev-tool detected"
else
    info "${TOOLCHAIN_MISSING} optional tool(s) missing - install above is advisory; mneme will run at reduced capability"
    info "Run \`mneme doctor --strict\` after install for the same probe set + per-tool fix instructions"
fi

# ----------------------------------------------------------------------------
# Step 2 - Resolve OS+arch -> release asset name and fetch metadata
#          (skipped under --local-zip / --skip-download)
# ----------------------------------------------------------------------------
#
# Three sources are supported (mirrors install.ps1 step 2):
#   default          : fetch latest release metadata from GitHub.
#   --local-zip      : skip the GitHub API call. Use the supplied tarball
#                      in step 3/8. tag_name is reported as 'local-zip'.
#   --skip-download  : skip BOTH step 2 and step 3 entirely. Assume the
#                      user has already extracted the tarball into
#                      ~/.mneme/. We just verify mneme exists in step 3.

if [ "${SKIP_DOWNLOAD}" -eq 1 ]; then
    step "step 2/8 - SKIPPED (--skip-download set; using existing ~/.mneme contents)"
    info "no GitHub API call, no download. install.sh will verify ~/.mneme/bin/mneme in step 3/8."
    RELEASE_TAG="pre-extracted"
    ASSET_URL=""
    ASSET=""
elif [ -n "${LOCAL_ZIP}" ]; then
    step "step 2/8 - SKIPPED (--local-zip set; using local archive)"
    if zip_size=$(wc -c < "${LOCAL_ZIP}" 2>/dev/null); then
        # Format MB without bc/awk requirements.
        mb=$(( zip_size / 1024 / 1024 ))
        ok "using local archive ${LOCAL_ZIP} (${mb} MB)"
    else
        ok "using local archive ${LOCAL_ZIP}"
    fi
    RELEASE_TAG="local-zip"
    ASSET_URL=""
    ASSET=""
else
    step "step 2/8 - fetching latest release metadata"

    case "${UNAME_S}" in
        Linux)
            case "${UNAME_M}" in
                x86_64|amd64) ASSET="mneme-linux-x64.tar.gz" ;;
                aarch64|arm64)
                    fail "no prebuilt binary for linux/${UNAME_M} yet"
                    fail "build from source: https://github.com/${REPO}"
                    exit 1
                    ;;
                *)
                    fail "no prebuilt binary for linux/${UNAME_M}"
                    fail "build from source: https://github.com/${REPO}"
                    exit 1
                    ;;
            esac
            ;;
        Darwin)
            case "${UNAME_M}" in
                arm64) ASSET="mneme-macos-arm64.tar.gz" ;;
                x86_64)
                    # Apple Silicon Macs run arm64 natively; Intel Macs can run
                    # arm64 binaries under Rosetta 2. We fall back rather than
                    # refuse, because the universal experience is nicer than
                    # "sorry, Intel Mac unsupported".
                    warn "no native x86_64 mac build yet; falling back to arm64 (runs under Rosetta 2)"
                    warn "if you don't have Rosetta installed: softwareupdate --install-rosetta --agree-to-license"
                    ASSET="mneme-macos-arm64.tar.gz"
                    ;;
                *)
                    fail "unsupported mac arch: ${UNAME_M}"
                    exit 1
                    ;;
            esac
            ;;
        MINGW*|MSYS*|CYGWIN*)
            fail "on Windows, use install.ps1 instead:"
            fail "  iwr -useb https://raw.githubusercontent.com/${REPO}/main/scripts/install.ps1 | iex"
            exit 1
            ;;
        *)
            fail "unsupported OS: ${UNAME_S}"
            exit 1
            ;;
    esac

    info "asset    : ${ASSET}"

    # Pick a fetcher. curl preferred; wget fallback for minimal images.
    if command -v curl >/dev/null 2>&1; then
        HAVE_CURL=1
    elif command -v wget >/dev/null 2>&1; then
        HAVE_CURL=0
    else
        fail "neither curl nor wget available - install one and retry"
        exit 1
    fi

    API_URL="https://api.github.com/repos/${REPO}/releases/latest"

    if [ "${HAVE_CURL}" -eq 1 ]; then
        RELEASE_JSON=$(curl -fsSL --retry 3 "${API_URL}") || {
            fail "GitHub API unreachable (curl exit $?)"
            exit 1
        }
    else
        RELEASE_JSON=$(wget -qO- "${API_URL}") || {
            fail "GitHub API unreachable (wget failed)"
            exit 1
        }
    fi

    # Resolve the asset's browser_download_url without requiring jq. Splits on
    # commas, then picks the line containing the asset name.
    ASSET_URL=$(printf '%s' "${RELEASE_JSON}" \
        | tr ',' '\n' \
        | grep "browser_download_url.*${ASSET}" \
        | head -n1 \
        | sed 's/.*"\(https:[^"]*\)".*/\1/')

    if [ -z "${ASSET_URL}" ]; then
        fail "${ASSET} not yet attached to the latest release"
        fail "the release workflow may still be building; retry in ~15 min"
        fail "see: https://github.com/${REPO}/releases"
        exit 1
    fi

    # Pull the tag for the final summary line. Best-effort; missing tag is fine.
    RELEASE_TAG=$(printf '%s' "${RELEASE_JSON}" \
        | tr ',' '\n' \
        | grep '"tag_name"' \
        | head -n1 \
        | sed 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')
    [ -z "${RELEASE_TAG}" ] && RELEASE_TAG="unknown"

    ok "release ${RELEASE_TAG} - asset URL resolved"
fi

# ----------------------------------------------------------------------------
# Step 3 - Download + extract (or verify pre-extracted layout)
# ----------------------------------------------------------------------------

if [ "${SKIP_DOWNLOAD}" -eq 1 ]; then
    step "step 3/8 - SKIPPED (--skip-download set; verifying existing extraction)"

    # The whole point of --skip-download is that the user has already put a
    # tarball's contents into ~/.mneme. If mneme is missing the rest of
    # the install is doomed (step 6 daemon start, step 7 register-mcp both
    # call $MNEME_BIN). Bail loudly with a remediation hint rather than
    # produce a half-installed silent failure later.
    if [ ! -x "${MNEME_BIN}" ] && [ ! -f "${MNEME_BIN}" ]; then
        fail "--skip-download was set but ${MNEME_BIN} does not exist"
        fail "       expected layout under ${MNEME_HOME}:"
        fail "         bin/mneme"
        fail "         bin/mneme-daemon (and 6 worker exes)"
        fail "         mcp/, plugin/, static/vision/..."
        fail "       remediation:"
        fail "         1. extract the mneme tarball into ~/.mneme  (tar -xzf ...)"
        fail "         2. re-run install.sh --skip-download"
        fail "       OR drop --skip-download to fetch from GitHub Releases."
        fail "       OR pass --local-zip <path> to extract a local tarball."
        exit 1
    fi
    ok "mneme present at ${MNEME_BIN}; extraction skipped"
else
    step "step 3/8 - downloading + extracting"

    TMP=$(mktemp -d 2>/dev/null || mktemp -d -t mneme-install)
    # Cleanup on any exit path - success, failure, ^C.
    trap 'rm -rf "${TMP}"' EXIT INT TERM

    if [ -n "${LOCAL_ZIP}" ]; then
        # Local-zip mode: reference the caller-supplied path directly.
        # The source is the user's file, not ours; nothing to clean up.
        ARCHIVE="${LOCAL_ZIP}"
        info "source: ${ARCHIVE} (local, no download)"
    else
        ARCHIVE="${TMP}/${ASSET}"
        info "downloading ${ASSET_URL}"
        if [ "${HAVE_CURL}" -eq 1 ]; then
            curl -fsSL --retry 3 -o "${ARCHIVE}" "${ASSET_URL}" || {
                fail "download failed (curl exit $?)"
                exit 1
            }
        else
            wget -qO "${ARCHIVE}" "${ASSET_URL}" || {
                fail "download failed (wget)"
                exit 1
            }
        fi
    fi

    mkdir -p "${MNEME_HOME}"

    info "extracting to ${MNEME_HOME}"
    if ! tar -xzf "${ARCHIVE}" -C "${MNEME_HOME}"; then
        fail "extract failed - archive may be corrupt"
        exit 1
    fi

    # tar preserves modes inside the archive, but be defensive in case the
    # release was packed without +x on bin/.
    if [ -d "${BIN_DIR}" ]; then
        chmod +x "${BIN_DIR}"/* 2>/dev/null || true
    fi

    ok "extracted to ${MNEME_HOME}"
fi

# F1 D1: verify the Vision SPA static bundle landed at the canonical
# production layout the daemon expects (~/.mneme/static/vision/index.html).
# The daemon's tower-http ServeDir mount in supervisor/src/health.rs
# resolves `<MNEME_HOME>/static/vision/` before falling back to the
# in-repo dev path. Missing static dir is non-fatal (daemon logs a
# warning and continues with API-only endpoints), but `mneme view` /
# the browser fallback at http://127.0.0.1:7777/ would 404. We surface
# the gap loudly so the user knows the visual layer is unavailable.
VISION_STATIC_DIR="${MNEME_HOME}/static/vision"
VISION_INDEX_FILE="${VISION_STATIC_DIR}/index.html"
VISION_ASSETS_DIR="${VISION_STATIC_DIR}/assets"
if [ ! -f "${VISION_INDEX_FILE}" ]; then
    warn "vision SPA missing: ${VISION_INDEX_FILE} not found"
    warn "  the daemon will start API-only; http://127.0.0.1:7777/ will 404."
    warn "  this means the release tarball was built without the vision/dist payload."
    warn "  open an issue at https://github.com/${REPO}/issues citing 'A12 / vision/dist missing'."
elif [ ! -d "${VISION_ASSETS_DIR}" ]; then
    warn "vision SPA index.html present but assets/ missing at ${VISION_ASSETS_DIR}"
    warn "  the SPA will load index.html but every chunk will 404."
else
    asset_count=$(find "${VISION_ASSETS_DIR}" -maxdepth 1 -type f 2>/dev/null | wc -l | tr -d ' ')
    ok "vision SPA staged at ${VISION_STATIC_DIR} (${asset_count} asset file(s))"
fi

# ----------------------------------------------------------------------------
# Step 4 - SELinux note (Unix equivalent of the Windows Defender step)
# ----------------------------------------------------------------------------
#
# Adaptation: there is no Defender on Unix. The closest analogue that
# actually bites users is SELinux on Fedora / Rocky / RHEL in Enforcing
# mode, which can label mneme's per-project sqlite files in a way that
# blocks writes from the user's home directory if it sits on an unusual
# fs (e.g. an SELinux-confined NFS mount). We don't try to fix it from
# this installer - sealert / chcon would need elevated privileges. We
# print a one-line warning so the user knows where to look if writes
# fail later.

step "step 4/8 - sandbox / mandatory access control check"

if command -v getenforce >/dev/null 2>&1; then
    SE_STATE=$(getenforce 2>/dev/null || echo "")
    if [ "${SE_STATE}" = "Enforcing" ]; then
        warn "SELinux is Enforcing. If mneme database writes fail later,"
        warn "see: https://github.com/${REPO}#selinux-faq"
    else
        ok "SELinux not enforcing (${SE_STATE:-not present})"
    fi
else
    ok "no SELinux on this host (skip)"
fi

# ----------------------------------------------------------------------------
# Step 5 - Add bin dir to PATH
# ----------------------------------------------------------------------------
#
# Adaptation: Windows updates the User-scope PATH registry key. On Unix
# we append a one-line `export PATH=...` to the right shell rc file. We
# pick rc files conservatively:
#   - Linux: ~/.profile (sourced by all login shells regardless of bash/zsh/dash)
#   - macOS: ~/.zprofile (macOS defaults to zsh since Catalina)
# If the user runs a non-default shell, they'll see the printed export
# line and can add it themselves.

step "step 5/8 - updating PATH"

case "${UNAME_S}" in
    Darwin) PROFILE_FILE="${HOME_DIR}/.zprofile" ;;
    *)      PROFILE_FILE="${HOME_DIR}/.profile" ;;
esac

# Already on PATH for THIS process? Then nothing to do for this session.
ALREADY_ON_PATH=0
case ":${PATH}:" in
    *":${BIN_DIR}:"*) ALREADY_ON_PATH=1 ;;
esac

# Already in the rc file? Then idempotent on disk.
ALREADY_IN_RC=0
if [ -f "${PROFILE_FILE}" ] && grep -q '\.mneme/bin' "${PROFILE_FILE}" 2>/dev/null; then
    ALREADY_IN_RC=1
fi

if [ "${ALREADY_IN_RC}" -eq 1 ]; then
    ok "${PROFILE_FILE} already exports ~/.mneme/bin"
else
    # Append, preserving an existing newline if present.
    # The literal `$PATH` is intentional - it must be expanded by the
    # user's shell each time the rc is sourced, NOT at install time.
    {
        printf '\n# Added by mneme installer\n'
        # shellcheck disable=SC2016
        printf 'export PATH="%s:$PATH"\n' "${BIN_DIR}"
    } >> "${PROFILE_FILE}"
    ok "appended PATH entry to ${PROFILE_FILE}"
fi

if [ "${ALREADY_ON_PATH}" -eq 0 ]; then
    info "open a new shell to pick up the PATH change, or run:"
    info "  export PATH=\"${BIN_DIR}:\$PATH\""
fi

# ----------------------------------------------------------------------------
# Step 6 - Start the mneme daemon (background, with liveness poll)
# ----------------------------------------------------------------------------
#
# Mirror of install.ps1 step 6. We launch via nohup so the daemon
# survives the shell that ran the installer. stdin <- /dev/null and
# stdout/stderr -> /tmp/mneme-daemon.log so the daemon does not inherit
# our pipe handles (which would hang the curl-piped install if the
# daemon is slow to fork its own process group).
#
# After spawn, we poll `mneme daemon status` every 0.5s for up to 15s.
# Healthy = exit 0 AND output mentions "running" / "healthy" / a pid.
# Unhealthy after 15s -> warn (not fail) so the install completes and
# the user can investigate with `mneme doctor`.

step "step 6/8 - starting mneme daemon"

if [ ! -x "${MNEME_BIN}" ]; then
    warn "mneme binary not found at ${MNEME_BIN} - did extraction succeed?"
    warn "skipping daemon start. run manually later: mneme daemon start"
else
    DAEMON_LOG="/tmp/mneme-daemon.log"
    # nohup + & + redirected fds = fully detached. The shell continues
    # immediately. The daemon log is at /tmp/mneme-daemon.log.
    nohup "${MNEME_BIN}" daemon start </dev/null >"${DAEMON_LOG}" 2>&1 &
    DAEMON_PID=$!
    info "spawned daemon (parent pid ${DAEMON_PID}); polling status..."

    # Poll status. 0.5s intervals up to 15s = 30 attempts.
    waited_ms=0
    healthy=0
    while [ "${waited_ms}" -lt 15000 ]; do
        sleep 1
        # We sleep 1s instead of 0.5s because POSIX sh's sleep doesn't
        # universally accept fractional seconds. This still gives us
        # 15 polls inside the 15s budget.
        waited_ms=$((waited_ms + 1000))
        STATUS_OUT=$("${MNEME_BIN}" daemon status 2>&1 || true)
        case "${STATUS_OUT}" in
            *running*|*healthy*|*'"pid"'*)
                healthy=1
                break
                ;;
        esac
    done

    if [ "${healthy}" -eq 1 ]; then
        ok "daemon started"
    else
        warn "daemon did not report healthy within 15s - it may still be coming up"
        warn "check later: mneme doctor; daemon log at ${DAEMON_LOG}"
    fi
fi

# ----------------------------------------------------------------------------
# Step 7 - Register MCP with Claude Code
# ----------------------------------------------------------------------------
#
# Same v0.3.1 hard rule as the Windows installer: only writes
# mcpServers.mneme into ~/.claude.json. Does NOT touch
# ~/.claude/settings.json. Does NOT inject hooks. Does NOT write a
# CLAUDE.md manifest.

step "step 7/8 - registering MCP with Claude Code"

if [ ! -x "${MNEME_BIN}" ]; then
    warn "mneme binary not present - skipping MCP registration"
else
    if "${MNEME_BIN}" register-mcp --platform claude-code 2>&1 | while IFS= read -r line; do
        info "${line}"
    done; then
        ok "Claude Code MCP registration complete"
    else
        warn "register-mcp exited non-zero - MCP may not be registered"
        warn "run manually later: mneme register-mcp --platform claude-code"
    fi
fi

# ----------------------------------------------------------------------------
# Step 8 - Done
# ----------------------------------------------------------------------------

step "step 8/8 - complete"
echo ""
echo "================================================================"
echo "  mneme installed - ${RELEASE_TAG}"
echo "================================================================"
echo ""
echo "  Next steps:"
echo "    1. Restart Claude Code so it picks up the new MCP server"
echo "    2. Open a project directory and run: mneme build ."
echo "    3. Inside Claude Code, try:  /mn-recall \"what does auth do\""
echo ""
echo "  Verify:"
echo "    mneme daemon status"
echo "    mneme --version"
echo "    mneme doctor --strict       # full pre-flight: G1-G10 toolchain + binary self-test"
echo ""
echo "  Uninstall:"
echo "    mneme unregister-mcp --platform claude-code"
echo "    sh ${MNEME_HOME}/scripts/uninstall.sh   (or rm -rf ${MNEME_HOME})"
echo ""
if [ "${ALREADY_ON_PATH}" -eq 0 ]; then
    echo "  Open a NEW shell to pick up the PATH change."
    echo ""
fi

exit 0
