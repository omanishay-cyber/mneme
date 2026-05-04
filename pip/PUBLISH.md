# Publishing mnemeos to PyPI

One-page reference for the maintainer. Run from the repo root.

## Distribution name

**`mnemeos`** — short for "Mneme OS", the project's brand.

The bare name `mneme` was claimed on PyPI in 2014 by an unrelated
Flask-based note-taking package by Risto Stevcev (
<https://github.com/Risto-Stevcev/flask-mneme>), so we ship under
`mnemeos` instead. The console scripts include both `mnemeos` (canonical)
and `mneme` / `mneme-bootstrap` (legacy aliases) — end users invoking
either name work.

Earlier internal builds were named `mneme-mcp`. That distribution name is
deprecated and not published. Anyone who runs `pip install mneme-mcp`
gets nothing because we never uploaded under that name.

## One-time PyPI setup

1. Create a PyPI account at <https://pypi.org/account/register/>.
2. Verify the email tied to the account.
3. Generate a project-scoped API token at
   <https://pypi.org/manage/account/token/>. Save it somewhere safe.
4. (Recommended) Configure GitHub Actions Trusted Publishing so future
   releases do not need a long-lived token. Out of scope for this doc.

## Build

From `pip/`:

```bash
python -m pip install --upgrade build twine
python -m build
```

That writes `dist/mnemeos-0.3.2-py3-none-any.whl` and
`dist/mnemeos-0.3.2.tar.gz`.

## Smoke test the wheel

In a throwaway venv:

```bash
python -m venv /tmp/check-mnemeos
/tmp/check-mnemeos/bin/python -m pip install dist/mnemeos-0.3.2-py3-none-any.whl
/tmp/check-mnemeos/bin/mnemeos --check
```

If `--check` prints the expected platform / URL / SHA-256, the wheel is
shippable.

## Upload

```bash
TWINE_USERNAME=__token__ \
TWINE_PASSWORD=pypi-AgEIcH... \
python -m twine upload dist/*
```

Twine prints the project URL on success. Verify the listing at
<https://pypi.org/project/mnemeos/>.

## After upload

1. Pin a CHANGELOG entry under the upstream project's `CHANGELOG.md`
   noting the new wrapper version.
2. If a new Mneme OS release ships, regenerate the SHA-256 pins in
   `src/mneme_mcp/bootstrap.py`, bump the package version, and rebuild.
3. Three console scripts are wired up: `mnemeos` (canonical), `mneme`
   (legacy), `mneme-bootstrap` (legacy). All three call the same entry
   point so users on either name work.

## Trusted Publishing (future task)

GitHub Actions can mint short-lived OIDC tokens that PyPI accepts in
place of `TWINE_PASSWORD`. The setup is:

1. On PyPI: `Manage project -> Publishing -> Add a new publisher`,
   point it at `omanishay-cyber/mneme`, workflow `pip-publish.yml`,
   environment `pypi`.
2. Add `.github/workflows/pip-publish.yml` that builds and uploads.

Document only; do not implement here.
