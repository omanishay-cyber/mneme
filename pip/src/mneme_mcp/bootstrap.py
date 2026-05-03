"""Bootstrap helpers for the mneme installer wrapper.

Each helper does one job:

* :func:`detect_platform` -- normalise ``platform.system()`` to one of the
  three values we know how to install for.
* :func:`installer_url` -- build the GitHub release URL for the platform.
* :func:`download_with_progress` -- stream a file to disk and show a
  byte-level counter so the user can see something is happening.
* :func:`verify_sha256` -- hash a local file and compare to an expected
  hex string (case-insensitive).
* :func:`run_bootstrap_script` -- shell out to the downloaded installer
  with the right interpreter and pass extra arguments through.

The wrapper deliberately does not reimplement any install logic. Once the
installer script is on disk we hand control to it; that is the same code
path users get when they run the published one-liner.
"""

from __future__ import annotations

import hashlib
import platform
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Iterable

# Default release the wrapper targets. Override at the CLI with --version.
DEFAULT_VERSION = "v0.3.2"

# Filename per platform on the GitHub release.
INSTALLER_FILENAME = {
    "Windows": "bootstrap-install.ps1",
    "Darwin": "install-mac.sh",
    "Linux": "install-linux.sh",
}

# SHA-256 of each installer at v0.3.2. Computed against the published
# release assets at github.com/omanishay-cyber/mneme/releases/tag/v0.3.2.
# When a new release ships, regenerate these and bump DEFAULT_VERSION.
INSTALLER_SHA256: dict[str, dict[str, str]] = {
    "v0.3.2": {
        "Windows": "05fa149936416b14257a5f0af484cc842e20bcf67ca86c2c07010eb918b33e95",
        "Darwin": "d2cc2c300761120c71657e51a0b14fccbd89c841697eaf404113d491849af62b",
        "Linux": "bbbd287c4b7bf473e13418d68642ef7a8d07f8b2ae096310061736804a651f89",
    },
}

# Where downloaded scripts get cached between runs. Reusing avoids
# re-downloading on transient failures and lets users inspect what was
# fetched.
CACHE_DIR = Path.home() / ".cache" / "mneme-mcp"

GITHUB_RELEASE_BASE = (
    "https://github.com/omanishay-cyber/mneme/releases/download"
)


def detect_platform(override: str | None = None) -> str:
    """Return one of ``"Windows"``, ``"Darwin"``, ``"Linux"``.

    If ``override`` is provided it is validated against the supported set
    and returned as-is. Otherwise we read ``platform.system()``.

    Raises:
        RuntimeError: if the host (or override) is not one of the three
            platforms the installer supports.
    """
    name = override or platform.system()
    if name not in INSTALLER_FILENAME:
        raise RuntimeError(
            f"Unsupported platform: {name}. "
            f"mneme installs on Windows, macOS, or Linux."
        )
    return name


def installer_url(plat: str, version: str = DEFAULT_VERSION) -> str:
    """Return the canonical download URL for ``plat`` at ``version``."""
    filename = INSTALLER_FILENAME[plat]
    return f"{GITHUB_RELEASE_BASE}/{version}/{filename}"


def expected_sha256(plat: str, version: str = DEFAULT_VERSION) -> str | None:
    """Return the pinned SHA-256 for ``plat`` at ``version``, or ``None``.

    A return of ``None`` means the wrapper does not have a pin for that
    combination -- typically because a new version was released between
    the wrapper's pinning and the user's upgrade. Callers should warn
    rather than fail in that case unless ``--strict-hash`` is set.
    """
    return INSTALLER_SHA256.get(version, {}).get(plat)


def download_with_progress(
    url: str,
    dest: Path,
    *,
    chunk_size: int = 64 * 1024,
    out=sys.stderr,
) -> Path:
    """Stream ``url`` into ``dest`` while printing a byte counter.

    The destination's parent directory is created as needed. Returns the
    written path.
    """
    dest.parent.mkdir(parents=True, exist_ok=True)
    written = 0
    try:
        with urllib.request.urlopen(url) as resp, dest.open("wb") as fh:
            total_header = resp.headers.get("Content-Length")
            total = int(total_header) if total_header else None
            while True:
                chunk = resp.read(chunk_size)
                if not chunk:
                    break
                fh.write(chunk)
                written += len(chunk)
                if total:
                    pct = (written / total) * 100
                    out.write(
                        f"\r  download: {written:>10,} / {total:,} bytes "
                        f"({pct:5.1f}%)"
                    )
                else:
                    out.write(f"\r  download: {written:>10,} bytes")
                out.flush()
        out.write("\n")
        out.flush()
    except urllib.error.URLError as exc:
        # Wipe a partial file so the next run does not see a half-written
        # script and try to execute it.
        if dest.exists():
            dest.unlink()
        raise RuntimeError(f"download failed: {url} ({exc})") from exc
    return dest


def verify_sha256(path: Path, expected: str) -> bool:
    """Hash ``path`` with SHA-256 and compare to ``expected`` hex.

    Comparison is case-insensitive. Returns ``True`` on match.
    """
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for block in iter(lambda: fh.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest().lower() == expected.strip().lower()


def run_bootstrap_script(
    script: Path,
    plat: str,
    extra_args: Iterable[str] | None = None,
) -> int:
    """Execute ``script`` for ``plat`` and return its exit code.

    On Windows we invoke PowerShell with execution policy bypassed for
    this single call -- mirroring how the published one-liner is meant
    to be run. On macOS and Linux we hand the file to ``bash``.

    Output is not captured; the child process inherits stdout/stderr so
    the user sees real-time install progress.
    """
    args = list(extra_args or [])
    if plat == "Windows":
        cmd = [
            "powershell",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            str(script),
            *args,
        ]
    else:
        cmd = ["bash", str(script), *args]

    try:
        result = subprocess.run(cmd, check=False)
    except FileNotFoundError as exc:
        # PowerShell missing on Windows or bash missing on POSIX is rare
        # enough that we explain the fix rather than dump a traceback.
        interpreter = "powershell" if plat == "Windows" else "bash"
        raise RuntimeError(
            f"could not find '{interpreter}' on PATH. "
            f"Install it and retry, or run the script directly: {script}"
        ) from exc
    return result.returncode
