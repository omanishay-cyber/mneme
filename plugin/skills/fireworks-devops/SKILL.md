---
name: fireworks-devops
description: Git, CI/CD, and deployment superbrain — conventional commits, PR workflows, Electron packaging, release management
version: 1.0.0
author: mneme
tags: [git, ci-cd, devops, packaging, releases, electron-builder, github-actions]
triggers: [git, commit, PR, pull request, release, deploy, package, build, CI, CD, pipeline, changelog]
---

# Fireworks DevOps — Git, CI/CD & Deployment Superbrain

> Conventional commits. PR workflows. Electron packaging. Release management. CI/CD pipeline design. Documentation sync. Everything from commit to customer.

---

## 1. Git Workflow Protocol

### Branch Naming Convention

```
Feature:    feat/short-description
Bug fix:    fix/short-description
Refactor:   refactor/short-description
Docs:       docs/short-description
Test:       test/short-description
Chore:      chore/short-description
Hotfix:     hotfix/short-description
Release:    release/vX.Y.Z
```

Rules:
- All lowercase, hyphens to separate words
- Keep it short but descriptive (max 50 characters)
- Include ticket/issue number if applicable: `feat/123-add-invoice-export`
- Never work directly on `main` — always branch

### Conventional Commits

```
Format: <type>(<scope>): <description>

Types:
  feat:     New feature (triggers MINOR version bump)
  fix:      Bug fix (triggers PATCH version bump)
  refactor: Code change that neither fixes a bug nor adds a feature
  docs:     Documentation only changes
  test:     Adding or correcting tests
  chore:    Build process, auxiliary tool changes
  perf:     Performance improvement
  style:    Code style (formatting, semicolons, etc.)
  ci:       CI/CD configuration changes
  build:    Build system or external dependency changes

Scope (optional): The area of the codebase affected
  feat(inventory): add barcode scanner support
  fix(reports): correct profit margin calculation
  refactor(ipc): simplify channel registration

Description:
  - Imperative mood: "add" not "added" or "adds"
  - Lowercase first letter
  - No period at the end
  - Maximum 72 characters for the subject line
```

### Commit Message Structure

```
<type>(<scope>): <subject line — max 72 chars>
                                          ← blank line
<body — wrap at 72 chars>                 ← explain WHY, not WHAT
<body — the diff shows WHAT changed>      ← focus on motivation and context
                                          ← blank line
<footer>                                  ← breaking changes, issue refs
```

### Example Commit Messages

```
feat(invoice): add PDF export with customizable templates

Customers requested the ability to export invoices as PDFs
with their own branding. This adds a template system using
jsPDF with support for custom logos, colors, and footer text.

Closes #234
```

```
fix(sync): resolve race condition in concurrent GitHub sync

Two sync operations could overlap when the user triggered a
manual sync while auto-sync was in progress. Added a mutex
lock to prevent concurrent sync operations.

```

### Safety Rules (Non-Negotiable)

- **NEVER** force-push to `main` or `master`
- **NEVER** skip hooks with `--no-verify`
- **NEVER** `git reset --hard` without explicit user permission
- **NEVER** `git clean -fd` without explicit user permission
- **NEVER** `taskkill //F //IM node.exe` — this kills Claude Code
- **ALWAYS** create NEW commits rather than amending (unless user explicitly requests amend)
- **ALWAYS** use HEREDOC for commit messages to preserve formatting
- **ALWAYS** stage specific files, not `git add -A` (avoid committing secrets)

> See `references/git-workflow.md` for merge vs rebase, conflict resolution, stash, cherry-pick, bisect.

---

## 2. PR Creation Workflow

### Pre-PR Checklist

```bash
# 1. Check current state
git status                          # See all changes
git diff                            # See unstaged changes
git diff --staged                   # See staged changes
git log --oneline main..HEAD        # See all commits on this branch

# 2. Verify branch is up to date
git fetch origin
git log --oneline HEAD..origin/main # See if main has moved ahead

# 3. Run checks locally
tsc --noEmit                        # TypeScript check
npm test                            # Run tests (if applicable)
npm run lint                        # Lint check (if applicable)

# 4. Review the full diff against main
git diff main...HEAD                # Everything that will be in the PR
```

### PR Creation

```bash
# Push branch with tracking
git push -u origin HEAD

# Create PR with structured body
gh pr create --title "feat(inventory): add barcode scanner support" --body "$(cat <<'EOF'
## Summary
- Added barcode scanner integration using `quagga2` library
- Scanner auto-detects UPC-A, EAN-13, and Code-128 formats
- Results populate the product lookup field automatically

## Test plan
- [ ] Test with physical barcode scanner (USB HID mode)
- [ ] Test with webcam scanner (mobile devices)
- [ ] Test all three barcode formats
- [ ] Verify fallback when no scanner detected
- [ ] Test in both light and dark themes

EOF
)"
```

### PR Title Guidelines

- Under 70 characters
- Use conventional commit format: `type(scope): description`
- Imperative mood: "add" not "added"
- Specific: "add barcode scanner" not "update inventory"

### PR Body Structure

```markdown
## Summary
<1-3 bullet points explaining WHAT changed and WHY>

## Changes
<Detailed list of changes, grouped by area>

## Test plan
<Bulleted checklist of things to test>

## Screenshots (if UI changes)
<Before/after screenshots>

## Breaking changes (if any)
<What breaks, migration steps>
```

---

---

## Full Reference

For complete patterns, examples, and advanced usage, see [`references/full-guide.md`](./references/full-guide.md).
Read that file when you need deeper context than the summary above.
