# Publishing mneme-mcp to PyPI

One-page reference for the maintainer. Run from the repo root.

## Distribution name

`mneme-mcp`. The bare name `mneme` was already taken on PyPI by an
unrelated notes app, so we ship under `mneme-mcp`. The console script is
still `mneme` -- end users do not see the qualifier.

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

That writes `dist/mneme_mcp-0.3.2-py3-none-any.whl` and
`dist/mneme_mcp-0.3.2.tar.gz`.

## Smoke test the wheel

In a throwaway venv:

```bash
python -m venv /tmp/check-mneme
/tmp/check-mneme/bin/python -m pip install dist/mneme_mcp-0.3.2-py3-none-any.whl
/tmp/check-mneme/bin/mneme --check
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
<https://pypi.org/project/mneme-mcp/>.

## After upload

1. Pin a CHANGELOG entry under the upstream project's `CHANGELOG.md`
   noting the new wrapper version.
2. If a new mneme release ships, regenerate the SHA-256 pins in
   `src/mneme_mcp/bootstrap.py`, bump the package version, and rebuild.
3. The bare command is `mneme`; the alias `mneme-bootstrap` is also
   wired up so power users can keep both around without conflict.

## Trusted Publishing (future task)

GitHub Actions can mint short-lived OIDC tokens that PyPI accepts in
place of `TWINE_PASSWORD`. The setup is:

1. On PyPI: `Manage project -> Publishing -> Add a new publisher`,
   point it at `omanishay-cyber/mneme`, workflow `pip-publish.yml`,
   environment `pypi`.
2. Add `.github/workflows/pip-publish.yml` that builds and uploads.

Document only; do not implement here.
