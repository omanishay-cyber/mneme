"""Command-line entry point for the mneme installer wrapper.

Exit codes (kept stable so CI scripts can branch on them):

* ``0`` -- bootstrap completed (or ``--check`` returned cleanly).
* ``1`` -- bootstrap script ran but returned non-zero.
* ``2`` -- platform unsupported, or required argument missing.
* ``3`` -- SHA-256 mismatch on the downloaded installer.
* ``4`` -- network or filesystem error during download.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Sequence

from . import __version__
from .bootstrap import (
    CACHE_DIR,
    DEFAULT_VERSION,
    INSTALLER_FILENAME,
    detect_platform,
    download_with_progress,
    expected_sha256,
    installer_url,
    run_bootstrap_script,
    verify_sha256,
)


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="mneme",
        description=(
            "Install mneme by downloading the official bootstrap script "
            "for your platform and running it. This wrapper does not "
            "reimplement install logic -- it just gets you to the same "
            "code path as the curl/iex one-liner."
        ),
        epilog=(
            "Examples:\n"
            "  mneme                  # detect platform and install\n"
            "  mneme --check          # show what would happen, do nothing\n"
            "  mneme --force          # re-download even if cached\n"
            "  mneme --platform Linux # override platform detection\n"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--version",
        action="version",
        version=f"mneme-mcp {__version__}",
    )
    parser.add_argument(
        "--release",
        default=DEFAULT_VERSION,
        metavar="TAG",
        help=f"release tag to install (default: {DEFAULT_VERSION})",
    )
    parser.add_argument(
        "--platform",
        dest="platform_override",
        choices=sorted(INSTALLER_FILENAME),
        default=None,
        help="override platform detection (Windows, Darwin, or Linux)",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="print what would be downloaded and run, then exit",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="re-download the installer even if a cached copy exists",
    )
    parser.add_argument(
        "--prerelease",
        action="store_true",
        help="resolve the latest prerelease tag instead of the pinned default",
    )
    parser.add_argument(
        "--no-verify",
        action="store_true",
        help=(
            "skip SHA-256 verification of the downloaded installer "
            "(only use for hand-cut beta scripts not yet pinned)"
        ),
    )
    parser.add_argument(
        "--strict-hash",
        action="store_true",
        help=(
            "fail if no SHA-256 pin is known for this release/platform "
            "(default: warn and continue)"
        ),
    )
    parser.add_argument(
        "extra_args",
        nargs=argparse.REMAINDER,
        help="arguments after `--` are passed through to the installer",
    )
    return parser


def _resolve_release(tag: str, prerelease: bool) -> str:
    """Return the release tag to install.

    For now ``--prerelease`` is documented but defers to the pinned
    default. Resolving 'latest-prerelease' would require a network call
    to the GitHub API which is out of scope for this wrapper -- users
    who want a specific prerelease can pass ``--release vX.Y.Z`` directly.
    """
    if prerelease and tag == DEFAULT_VERSION:
        # No silent magic: tell the user what we did so they can pin
        # explicitly if needed.
        sys.stderr.write(
            "  note: --prerelease has no pinned default; "
            "pass --release vX.Y.Z to install a specific prerelease.\n"
        )
    return tag


def main(argv: Sequence[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)

    try:
        plat = detect_platform(args.platform_override)
    except RuntimeError as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 2

    release = _resolve_release(args.release, args.prerelease)
    url = installer_url(plat, release)
    script_name = INSTALLER_FILENAME[plat]
    cached = CACHE_DIR / release / script_name
    pinned_hash = expected_sha256(plat, release)

    print(f"mneme installer wrapper {__version__}")
    print(f"  platform : {plat}")
    print(f"  release  : {release}")
    print(f"  source   : {url}")
    print(f"  cache    : {cached}")
    if pinned_hash:
        print(f"  sha256   : {pinned_hash}")
    else:
        print("  sha256   : <not pinned for this release>")

    if args.check:
        # Strip the `--` separator if argparse left it in the remainder.
        passthrough = [a for a in args.extra_args if a != "--"]
        if passthrough:
            print(f"  passthru : {' '.join(passthrough)}")
        print("\n  --check: no download or execution performed.")
        return 0

    # Download (or reuse cached copy).
    if cached.exists() and not args.force:
        print(f"  using cached script: {cached}")
    else:
        if cached.exists():
            cached.unlink()
        try:
            download_with_progress(url, cached)
        except RuntimeError as exc:
            sys.stderr.write(f"error: {exc}\n")
            return 4

    # Verify SHA-256 unless explicitly skipped.
    if args.no_verify:
        sys.stderr.write(
            "  warning: SHA-256 verification skipped via --no-verify.\n"
        )
    elif pinned_hash is None:
        msg = (
            f"  no SHA-256 pin for release {release} on {plat}. "
            f"This wrapper was built for {DEFAULT_VERSION}; "
            f"newer releases may not be covered."
        )
        if args.strict_hash:
            sys.stderr.write(f"error:{msg}\n")
            return 3
        sys.stderr.write(f"  warning:{msg}\n")
    else:
        if not verify_sha256(cached, pinned_hash):
            sys.stderr.write(
                f"error: SHA-256 mismatch for {cached.name}.\n"
                f"  expected: {pinned_hash}\n"
                f"  Refusing to execute. Re-run with --force, or with "
                f"--no-verify if you know what you are doing.\n"
            )
            try:
                cached.unlink()
            except OSError:
                pass
            return 3
        print(f"  sha256 verified: {cached.name}")

    # Hand off to the installer. Strip the argparse `--` sentinel.
    passthrough = [a for a in args.extra_args if a != "--"]
    print(f"\n-- handing off to {script_name} --")
    try:
        rc = run_bootstrap_script(cached, plat, passthrough)
    except RuntimeError as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 4

    if rc == 0:
        print(f"\nmneme installed. Logs (if any) live under {Path.home() / '.mneme'}.")
        return 0

    sys.stderr.write(
        f"\nbootstrap script exited {rc}. "
        f"Re-run with the same flags to retry, or inspect {cached}.\n"
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
