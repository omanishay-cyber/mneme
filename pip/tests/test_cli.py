"""Tests for the mneme-mcp CLI wrapper.

These tests deliberately avoid the network. Anything that would touch
the GitHub release or the local installer is mocked. Real end-to-end
testing of the install flow lives upstream in the bootstrap scripts'
own test suite.
"""

from __future__ import annotations

import hashlib
from pathlib import Path
from unittest.mock import MagicMock

import pytest

from mneme_mcp import bootstrap, cli


# ---------------------------------------------------------------------------
# detect_platform
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "system_value, expected",
    [
        ("Windows", "Windows"),
        ("Darwin", "Darwin"),
        ("Linux", "Linux"),
    ],
)
def test_detect_platform_returns_known_value(monkeypatch, system_value, expected):
    monkeypatch.setattr(bootstrap.platform, "system", lambda: system_value)
    assert bootstrap.detect_platform() == expected


def test_detect_platform_unsupported_raises(monkeypatch):
    monkeypatch.setattr(bootstrap.platform, "system", lambda: "FreeBSD")
    with pytest.raises(RuntimeError) as exc_info:
        bootstrap.detect_platform()
    assert "Unsupported platform" in str(exc_info.value)
    assert "FreeBSD" in str(exc_info.value)


def test_detect_platform_override_wins(monkeypatch):
    monkeypatch.setattr(bootstrap.platform, "system", lambda: "Linux")
    assert bootstrap.detect_platform("Windows") == "Windows"


def test_detect_platform_override_validated(monkeypatch):
    monkeypatch.setattr(bootstrap.platform, "system", lambda: "Linux")
    with pytest.raises(RuntimeError):
        bootstrap.detect_platform("Plan9")


# ---------------------------------------------------------------------------
# installer_url + expected_sha256
# ---------------------------------------------------------------------------


def test_installer_url_includes_release_and_filename():
    url = bootstrap.installer_url("Linux", "v0.3.2")
    assert url.endswith("/v0.3.2/install-linux.sh")
    assert "omanishay-cyber/mneme" in url


def test_expected_sha256_known_release_returns_hex():
    h = bootstrap.expected_sha256("Linux", "v0.3.2")
    assert h is not None
    assert len(h) == 64
    int(h, 16)  # hex sanity


def test_expected_sha256_unknown_release_returns_none():
    assert bootstrap.expected_sha256("Linux", "v9.9.9") is None


# ---------------------------------------------------------------------------
# verify_sha256
# ---------------------------------------------------------------------------


def test_verify_sha256_match(tmp_path: Path):
    target = tmp_path / "x.bin"
    target.write_bytes(b"hello world")
    expected = hashlib.sha256(b"hello world").hexdigest()
    assert bootstrap.verify_sha256(target, expected) is True


def test_verify_sha256_mismatch(tmp_path: Path):
    target = tmp_path / "x.bin"
    target.write_bytes(b"hello world")
    bogus = "0" * 64
    assert bootstrap.verify_sha256(target, bogus) is False


def test_verify_sha256_case_insensitive(tmp_path: Path):
    target = tmp_path / "x.bin"
    target.write_bytes(b"hello world")
    expected = hashlib.sha256(b"hello world").hexdigest().upper()
    assert bootstrap.verify_sha256(target, expected) is True


# ---------------------------------------------------------------------------
# CLI: --check mode never downloads
# ---------------------------------------------------------------------------


def test_check_mode_does_not_download(monkeypatch, capsys):
    called = MagicMock()
    monkeypatch.setattr(bootstrap, "download_with_progress", called)
    monkeypatch.setattr(
        bootstrap.platform, "system", lambda: "Linux"
    )
    rc = cli.main(["--check"])
    assert rc == 0
    called.assert_not_called()
    out = capsys.readouterr().out
    assert "--check" in out
    assert "Linux" in out


# ---------------------------------------------------------------------------
# CLI: SHA-256 mismatch aborts with exit 3
# ---------------------------------------------------------------------------


def test_sha256_mismatch_aborts(monkeypatch, tmp_path: Path):
    # Force the cache to a tmp dir so the test does not touch ~/.cache.
    monkeypatch.setattr(bootstrap, "CACHE_DIR", tmp_path)

    # Pretend we are on Linux so a known SHA pin exists.
    monkeypatch.setattr(bootstrap.platform, "system", lambda: "Linux")

    # Replace the downloader with a function that writes deterministic
    # bytes that will NOT match the pinned hash.
    def fake_download(url, dest, **kwargs):
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(b"this is not the real installer")
        return dest

    monkeypatch.setattr(bootstrap, "download_with_progress", fake_download)
    monkeypatch.setattr(cli, "download_with_progress", fake_download)

    # Make sure we never actually exec the downloaded "script".
    runner = MagicMock(return_value=0)
    monkeypatch.setattr(bootstrap, "run_bootstrap_script", runner)
    monkeypatch.setattr(cli, "run_bootstrap_script", runner)

    rc = cli.main([])
    assert rc == 3
    runner.assert_not_called()


def test_sha256_match_then_runs(monkeypatch, tmp_path: Path):
    """When the SHA matches, the wrapper should hand off to the script."""
    monkeypatch.setattr(bootstrap, "CACHE_DIR", tmp_path)
    monkeypatch.setattr(bootstrap.platform, "system", lambda: "Linux")

    pinned = bootstrap.expected_sha256("Linux", bootstrap.DEFAULT_VERSION)
    assert pinned is not None

    # Build payload bytes whose sha256 == the pinned hash. We can not do
    # that directly (one-way function), so instead we monkey-patch
    # verify_sha256 to True and confirm the runner is invoked.
    def fake_download(url, dest, **kwargs):
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(b"placeholder")
        return dest

    monkeypatch.setattr(bootstrap, "download_with_progress", fake_download)
    monkeypatch.setattr(cli, "download_with_progress", fake_download)

    monkeypatch.setattr(bootstrap, "verify_sha256", lambda p, e: True)
    monkeypatch.setattr(cli, "verify_sha256", lambda p, e: True)

    runner = MagicMock(return_value=0)
    monkeypatch.setattr(bootstrap, "run_bootstrap_script", runner)
    monkeypatch.setattr(cli, "run_bootstrap_script", runner)

    rc = cli.main([])
    assert rc == 0
    runner.assert_called_once()


def test_no_verify_flag_skips_hash_check(monkeypatch, tmp_path: Path):
    monkeypatch.setattr(bootstrap, "CACHE_DIR", tmp_path)
    monkeypatch.setattr(bootstrap.platform, "system", lambda: "Linux")

    def fake_download(url, dest, **kwargs):
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(b"unverified")
        return dest

    monkeypatch.setattr(bootstrap, "download_with_progress", fake_download)
    monkeypatch.setattr(cli, "download_with_progress", fake_download)

    verifier = MagicMock(return_value=False)
    monkeypatch.setattr(bootstrap, "verify_sha256", verifier)
    monkeypatch.setattr(cli, "verify_sha256", verifier)

    runner = MagicMock(return_value=0)
    monkeypatch.setattr(bootstrap, "run_bootstrap_script", runner)
    monkeypatch.setattr(cli, "run_bootstrap_script", runner)

    rc = cli.main(["--no-verify"])
    assert rc == 0
    verifier.assert_not_called()
    runner.assert_called_once()


def test_unsupported_platform_returns_2(monkeypatch):
    monkeypatch.setattr(bootstrap.platform, "system", lambda: "Plan9")
    rc = cli.main([])
    assert rc == 2


def test_strict_hash_with_unknown_release_returns_3(monkeypatch, tmp_path: Path):
    monkeypatch.setattr(bootstrap, "CACHE_DIR", tmp_path)
    monkeypatch.setattr(bootstrap.platform, "system", lambda: "Linux")

    def fake_download(url, dest, **kwargs):
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(b"x")
        return dest

    monkeypatch.setattr(bootstrap, "download_with_progress", fake_download)
    monkeypatch.setattr(cli, "download_with_progress", fake_download)

    rc = cli.main(["--release", "v9.9.9", "--strict-hash"])
    assert rc == 3


def test_force_redownloads(monkeypatch, tmp_path: Path):
    monkeypatch.setattr(bootstrap, "CACHE_DIR", tmp_path)
    monkeypatch.setattr(bootstrap.platform, "system", lambda: "Linux")

    # Pre-populate the cache.
    cached = tmp_path / bootstrap.DEFAULT_VERSION / "install-linux.sh"
    cached.parent.mkdir(parents=True, exist_ok=True)
    cached.write_bytes(b"old")

    download = MagicMock(side_effect=lambda url, dest, **kw: (dest.write_bytes(b"new"), dest)[1])
    monkeypatch.setattr(bootstrap, "download_with_progress", download)
    monkeypatch.setattr(cli, "download_with_progress", download)

    monkeypatch.setattr(bootstrap, "verify_sha256", lambda p, e: True)
    monkeypatch.setattr(cli, "verify_sha256", lambda p, e: True)

    runner = MagicMock(return_value=0)
    monkeypatch.setattr(bootstrap, "run_bootstrap_script", runner)
    monkeypatch.setattr(cli, "run_bootstrap_script", runner)

    rc = cli.main(["--force"])
    assert rc == 0
    download.assert_called_once()
