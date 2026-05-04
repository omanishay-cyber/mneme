# shellcheck shell=bash
# release/lib-common.sh
# ----------------------
# Shared POSIX helpers sourced by release/install-mac.sh and
# release/install-linux.sh. Pure helper library -- contains no top-level
# side effects, no exit calls outside of fail(), and never assumes a
# particular working directory. Sourced via:
#
#   # shellcheck source=release/lib-common.sh
#   . "$(dirname "${BASH_SOURCE[0]}")/lib-common.sh"
#
# Callers must already have set:
#   set -euo pipefail        (strict mode -- helpers assume nounset)
#
# Helpers exported:
#   ANSI / output     : say, ok, warn, fail, step
#   OS / arch         : detect_os, detect_arch
#   Tooling           : require_cmd
#   Resource probes   : pre_flight_disk_space
#   Network downloads : download_with_retry, download_dual_source
#   Integrity         : verify_sha256
#
# Apache-2.0. (c) 2026 Anish Trivedi & Kruti Trivedi.

# Guard against double-sourcing -- the file is idempotent if sourced
# twice but the colour-init block would re-detect a TTY each time.
if [ -n "${MNEME_LIB_COMMON_LOADED:-}" ]; then
    # shellcheck disable=SC2317  # `true` IS reachable when `return` fails (script-mode invocation)
    return 0 2>/dev/null || true
fi
MNEME_LIB_COMMON_LOADED=1

# ---------------------------------------------------------------------------
# ANSI colour init
# ---------------------------------------------------------------------------
# Only emit colour if stdout is a TTY AND the terminal advertises colour
# support. Piped invocations (curl ... | bash) get plain ASCII so logs
# stay clean in CI / file redirections.
if [ -t 1 ] && [ "${TERM:-dumb}" != "dumb" ]; then
    _MNEME_C_RESET='\033[0m'
    _MNEME_C_RED='\033[31m'
    _MNEME_C_GREEN='\033[32m'
    _MNEME_C_YELLOW='\033[33m'
    _MNEME_C_CYAN='\033[36m'
else
    _MNEME_C_RESET=''
    _MNEME_C_RED=''
    _MNEME_C_GREEN=''
    _MNEME_C_YELLOW=''
    _MNEME_C_CYAN=''
fi

# ---------------------------------------------------------------------------
# Output helpers
# ---------------------------------------------------------------------------
# All output goes to stderr for warn/fail (so the caller can pipe stdout
# to a structured destination if desired). step/ok/say go to stdout.

step() { printf '%b==> %s%b\n' "${_MNEME_C_CYAN}" "$1" "${_MNEME_C_RESET}"; }
say()  { printf '    %s\n' "$1"; }
ok()   { printf '    %bOK:%b %s\n' "${_MNEME_C_GREEN}" "${_MNEME_C_RESET}" "$1"; }
warn() { printf '    %bWARN:%b %s\n' "${_MNEME_C_YELLOW}" "${_MNEME_C_RESET}" "$1" >&2; }
fail() {
    printf '    %bFAIL:%b %s\n' "${_MNEME_C_RED}" "${_MNEME_C_RESET}" "$1" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# detect_os
# ---------------------------------------------------------------------------
# Echoes one of: mac | linux
# Exits with fail() on anything else (we don't ship a generic POSIX
# binary -- the daemon mechanism + install paths differ per OS).
detect_os() {
    local uname_s
    uname_s=$(uname -s 2>/dev/null || echo unknown)
    case "${uname_s}" in
        Darwin)  echo "mac" ;;
        Linux)   echo "linux" ;;
        MINGW*|MSYS*|CYGWIN*)
            fail "this is a Windows host (uname=${uname_s}); use bootstrap-install.ps1 instead"
            ;;
        *)
            fail "unsupported OS: ${uname_s} (mneme ships binaries for macOS + Linux + Windows only)"
            ;;
    esac
}

# ---------------------------------------------------------------------------
# detect_arch
# ---------------------------------------------------------------------------
# Echoes one of: x64 | arm64
# Refuses 32-bit hosts (i686, i386, armv7l, armhf) because mneme's
# Bun-based MCP layer requires a 64-bit runtime (see B11.55).
detect_arch() {
    local uname_m
    uname_m=$(uname -m 2>/dev/null || echo unknown)
    case "${uname_m}" in
        x86_64|amd64)
            echo "x64"
            ;;
        arm64|aarch64)
            echo "arm64"
            ;;
        i386|i686)
            fail "32-bit x86 is not supported (uname=${uname_m}). Bun runtime requires x64 or arm64. Upgrade to a 64-bit OS."
            ;;
        armv6l|armv7l|armhf)
            fail "32-bit ARM is not supported (uname=${uname_m}). Need aarch64 (64-bit ARM). Use a Raspberry Pi 4/5 with arm64 OS image, not the 32-bit Raspbian."
            ;;
        *)
            fail "unsupported CPU architecture: ${uname_m} (mneme ships x64 + arm64 only)"
            ;;
    esac
}

# ---------------------------------------------------------------------------
# require_cmd <cmd> [<install-hint>]
# ---------------------------------------------------------------------------
# Verifies <cmd> resolves on PATH. Exits with a clear message if not.
# Optional second argument is a one-line install hint shown to the user.
require_cmd() {
    local cmd="$1"
    local hint="${2:-}"
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        if [ -n "${hint}" ]; then
            fail "required command not found: ${cmd} (install: ${hint})"
        else
            fail "required command not found: ${cmd}"
        fi
    fi
}

# ---------------------------------------------------------------------------
# pre_flight_disk_space <gb>
# ---------------------------------------------------------------------------
# Verifies $HOME has at least <gb> gigabytes free. Refuses install early
# if not -- the model bundle alone is 3.4 GB, plus binaries + ONNX runtime.
# Per B11.5w: "<5 GB disk free -> refuse early".
pre_flight_disk_space() {
    local needed_gb="$1"
    local home_dir="${HOME:-/tmp}"
    local avail_kb
    # `df -Pk` is the POSIX-portable form (-P for portable output, -k for
    # 1024-byte blocks). The 4th field is "Available" in 1K blocks.
    avail_kb=$(df -Pk "${home_dir}" 2>/dev/null | awk 'NR==2 {print $4}')
    if [ -z "${avail_kb}" ] || ! [ "${avail_kb}" -ge 0 ] 2>/dev/null; then
        warn "could not determine free disk space at ${home_dir} (df returned no usable data)"
        return 0
    fi
    local avail_gb=$(( avail_kb / 1024 / 1024 ))
    if [ "${avail_gb}" -lt "${needed_gb}" ]; then
        fail "insufficient disk space at ${home_dir}: ${avail_gb} GB free, need ${needed_gb} GB (binaries + 3.4 GB models + working space)"
    fi
    ok "disk space at ${home_dir}: ${avail_gb} GB free (need ${needed_gb} GB)"
}

# ---------------------------------------------------------------------------
# download_with_retry <url> <dest> [<retries>]
# ---------------------------------------------------------------------------
# Downloads <url> to <dest> with up to <retries> attempts (default 3).
# Uses curl preferentially, wget as fallback. Exits with fail() on the
# final attempt's failure.
#
# Treats HTTP non-2xx as a hard error (curl --fail / wget default), so a
# 404 doesn't silently land an HTML error page on disk.
download_with_retry() {
    local url="$1"
    local dest="$2"
    local retries="${3:-3}"

    local fetcher=""
    if command -v curl >/dev/null 2>&1; then
        fetcher="curl"
    elif command -v wget >/dev/null 2>&1; then
        fetcher="wget"
    else
        fail "neither curl nor wget available -- install one of them and retry"
    fi

    local attempt=1
    while [ "${attempt}" -le "${retries}" ]; do
        say "downloading (attempt ${attempt}/${retries}): ${url}"
        if [ "${fetcher}" = "curl" ]; then
            # --fail        : non-2xx responses become a hard error
            # --location    : follow redirects (GitHub release URLs redirect)
            # --silent      : no progress bar (we print our own status)
            # --show-error  : but DO show the error message on failure
            # --retry 0     : we manage retries ourselves (so backoff fits this loop)
            # --max-time 0  : no overall timeout (model downloads can be 2-3 GB)
            if curl --fail --location --silent --show-error \
                    --retry 0 --max-time 0 \
                    -o "${dest}" "${url}"; then
                # Sanity: tiny files (<256 bytes) are almost certainly an
                # HTML error page that curl --fail didn't catch (e.g. a
                # CDN-level soft-404 that returns 200 with empty body).
                local sz
                sz=$(wc -c < "${dest}" 2>/dev/null || echo 0)
                if [ "${sz}" -lt 256 ]; then
                    warn "downloaded file is only ${sz} bytes -- probably a soft-404 page; treating as failure"
                    rm -f "${dest}"
                else
                    ok "downloaded $(basename "${dest}") (${sz} bytes)"
                    return 0
                fi
            fi
        else
            # wget options:
            # --quiet                       : suppress its own progress bar
            # --tries=1                     : we manage retries
            # --output-document=<dest>      : explicit destination
            # --server-response             : we don't actually read these,
            #                                 but a 4xx exits non-zero by default
            if wget --quiet --tries=1 --output-document="${dest}" "${url}"; then
                local sz
                sz=$(wc -c < "${dest}" 2>/dev/null || echo 0)
                if [ "${sz}" -lt 256 ]; then
                    warn "downloaded file is only ${sz} bytes -- probably a soft-404 page; treating as failure"
                    rm -f "${dest}"
                else
                    ok "downloaded $(basename "${dest}") (${sz} bytes)"
                    return 0
                fi
            fi
        fi

        warn "attempt ${attempt} failed for ${url}"
        rm -f "${dest}"
        if [ "${attempt}" -eq "${retries}" ]; then
            fail "download failed after ${retries} attempts: ${url}"
        fi
        # Linear backoff: 2s, 4s, 6s. Keeps the loop snappy for transient
        # blips while still backing off enough not to hammer the CDN on a
        # genuine outage.
        sleep $(( attempt * 2 ))
        attempt=$(( attempt + 1 ))
    done
}

# ---------------------------------------------------------------------------
# download_dual_source <name> <primary_url> <fallback_url> <dest>
# ---------------------------------------------------------------------------
# Tries <primary_url> first; if it fails (404, timeout, all retries
# exhausted), automatically falls back to <fallback_url>. Mirrors the
# Wave 6 dual-source pattern in bootstrap-install.ps1 (HF Hub primary,
# GitHub Releases fallback for model assets).
#
# A blank fallback URL is allowed (single-source mode) -- in that case
# this is just a thin wrapper around download_with_retry.
download_dual_source() {
    local name="$1"
    local primary_url="$2"
    local fallback_url="$3"
    local dest="$4"

    # A7-001 (2026-05-04): on success, verify SHA-256 against the manifest.
    # If the user passed MNEME_SKIP_HASH_CHECK=1, skip integrity verification
    # entirely (used for hand-cut beta zips before the manifest is regenerated).
    # If the manifest isn't loaded OR the name isn't pinned, verify_sha256
    # logs a single WARN and returns 0 (legacy unverified path).
    _maybe_verify() {
        if [ -n "${MNEME_SKIP_HASH_CHECK:-}" ]; then
            return 0
        fi
        local expected
        expected=$(lookup_expected_hash "${name}" 2>/dev/null || echo "")
        verify_sha256 "${dest}" "${expected}"
    }

    say "fetching ${name} from primary source"
    # Run primary in a subshell so its fail() (called by download_with_retry
    # on exhaustion) doesn't kill the whole installer -- we want to fall
    # back. Capture exit code via the standard `if` form which `set -e`
    # explicitly excuses from immediate exit.
    if ( download_with_retry "${primary_url}" "${dest}" 3 ); then
        _maybe_verify
        return 0
    fi

    if [ -z "${fallback_url}" ] || [ "${fallback_url}" = "${primary_url}" ]; then
        fail "primary source exhausted for ${name} and no distinct fallback configured"
    fi

    warn "primary source exhausted for ${name} -- trying fallback"
    say "fetching ${name} from fallback source"
    # Fallback is allowed to fail() for real -- if both sources are dead,
    # there's nothing more we can do.
    download_with_retry "${fallback_url}" "${dest}" 3
    _maybe_verify
}

# ---------------------------------------------------------------------------
# verify_sha256 <file> <expected_hash>
# ---------------------------------------------------------------------------
# Computes SHA-256 of <file> and compares (case-insensitive) to
# <expected_hash>. Mismatch -> fail loud and remove the file (so a retry
# doesn't trust the cached copy).
#
# Picks the available hashing tool: sha256sum (Linux) / shasum (macOS).
# On platforms missing both, prints a WARN and returns 0 (don't block
# install on a tooling gap).
verify_sha256() {
    local file="$1"
    local expected="$2"

    if [ ! -f "${file}" ]; then
        fail "verify_sha256: file does not exist: ${file}"
    fi
    if [ -z "${expected}" ]; then
        warn "verify_sha256: no expected hash provided for ${file} -- skipping integrity check"
        return 0
    fi

    local actual=""
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "${file}" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "${file}" | awk '{print $1}')
    elif command -v openssl >/dev/null 2>&1; then
        actual=$(openssl dgst -sha256 "${file}" | awk '{print $NF}')
    else
        warn "no SHA-256 tool found (sha256sum/shasum/openssl) -- skipping integrity check for ${file}"
        return 0
    fi

    # Lowercase both sides so case differences don't trigger a false mismatch.
    local actual_lc expected_lc
    actual_lc=$(printf '%s' "${actual}" | tr '[:upper:]' '[:lower:]')
    expected_lc=$(printf '%s' "${expected}" | tr '[:upper:]' '[:lower:]')

    if [ "${actual_lc}" != "${expected_lc}" ]; then
        rm -f "${file}"
        fail "SHA-256 mismatch for $(basename "${file}")
       expected: ${expected_lc}
       actual:   ${actual_lc}
       (file removed; refusing to install possibly-tampered or partial download)"
    fi
    ok "SHA-256 verified for $(basename "${file}")"
}

# ---------------------------------------------------------------------------
# A7-001 (2026-05-04): release-checksums.json manifest support
# ---------------------------------------------------------------------------
# load_hash_manifest <release_base_url>
#
# Fetches release-checksums.json from the GH Release alongside the binary
# zips/tarballs and parses it into two parallel arrays
# (_MNEME_HASH_NAMES / _MNEME_HASH_VALUES) keyed by file name. Bash 3.2
# (macOS default) lacks associative arrays, hence the parallel-array
# pattern. Naive JSON parse via grep+sed avoids a jq dependency.
#
# Manifest format (sidecar file):
#   {
#     "version": "v0.3.2",
#     "generated": "2026-05-04T05:00:00Z",
#     "files": {
#       "mneme-v0.3.2-linux-x64.tar.gz": "0123ABCD...",
#       "bge-small-en-v1.5.onnx":        "...",
#       ...
#     }
#   }
#
# A missing manifest is non-fatal: this function emits a single WARN and
# returns 0, leaving the parallel arrays empty. Each download then falls
# through to the legacy unverified path. Once a release ships a manifest,
# downloads of files listed there become hard-fail on hash mismatch.
_MNEME_HASH_NAMES=()
_MNEME_HASH_VALUES=()
load_hash_manifest() {
    local release_base="$1"
    if [ -z "${release_base}" ]; then
        warn "load_hash_manifest called without release_base; skipping"
        return 0
    fi
    local tmp
    tmp=$(mktemp -t mneme-hashes.XXXXXX) || tmp="/tmp/mneme-hashes.json"
    if command -v curl >/dev/null 2>&1; then
        if ! curl --fail --silent --location --show-error --max-time 10 \
                -o "${tmp}" "${release_base}/release-checksums.json" 2>/dev/null; then
            warn "release-checksums.json not available at ${release_base} -- continuing with unverified downloads"
            rm -f "${tmp}"
            return 0
        fi
    elif command -v wget >/dev/null 2>&1; then
        if ! wget --quiet --tries=1 --timeout=10 \
                --output-document="${tmp}" "${release_base}/release-checksums.json"; then
            warn "release-checksums.json not available at ${release_base} -- continuing with unverified downloads"
            rm -f "${tmp}"
            return 0
        fi
    else
        warn "neither curl nor wget available; cannot load checksum manifest"
        return 0
    fi

    # Naive JSON parse: pick out lines matching `"name": "HASH",?` inside the
    # `files` object. The hash regex is hex-only so it can't accidentally
    # eat the "version"/"generated" string fields.
    local line name value
    while IFS= read -r line; do
        name=$(printf '%s' "${line}" | sed -E 's/^[[:space:]]*"([^"]+)":[[:space:]]*"[a-fA-F0-9]+",?$/\1/')
        value=$(printf '%s' "${line}" | sed -E 's/^[[:space:]]*"[^"]+":[[:space:]]*"([a-fA-F0-9]+)",?$/\1/')
        if [ "${name}" != "${line}" ] && [ -n "${value}" ]; then
            _MNEME_HASH_NAMES+=("${name}")
            _MNEME_HASH_VALUES+=("${value}")
        fi
    done < <(grep -E '^\s*"[^"]+":\s*"[a-fA-F0-9]+",?\s*$' "${tmp}")
    rm -f "${tmp}"
    ok "loaded SHA-256 manifest: ${#_MNEME_HASH_NAMES[@]} pinned files"
    return 0
}

# lookup_expected_hash <name>
#
# Echoes the expected hex hash for <name>, or empty string if not in the
# manifest. Linear scan because Bash 3.2 lacks associative arrays.
lookup_expected_hash() {
    local needle="$1"
    local i=0
    while [ "${i}" -lt "${#_MNEME_HASH_NAMES[@]}" ]; do
        if [ "${_MNEME_HASH_NAMES[${i}]}" = "${needle}" ]; then
            echo "${_MNEME_HASH_VALUES[${i}]}"
            return 0
        fi
        i=$((i + 1))
    done
    return 1
}
