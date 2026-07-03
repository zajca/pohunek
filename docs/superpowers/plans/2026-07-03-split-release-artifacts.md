# Split Release Artifacts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish separate CLI, daemon, and GUI release archives from the existing single `vX.Y.Z` release.

**Architecture:** Keep the shared workspace version and single GitHub Release. Extend the release workflow build matrix with component metadata so each matrix entry packages exactly one binary plus common release extras. Update install/update documentation to describe component archives instead of one combined archive.

**Tech Stack:** GitHub Actions YAML, POSIX/Bash packaging script, Cargo release build, Pohunek knowledge docs.

---

## File Structure

- Modify `.github/workflows/release.yml`: add a component matrix dimension and use component metadata in the package step.
- Modify `README.md`: describe component-specific release archives and keep the source build command for all three binaries.
- Modify `docs/knowledge/guides/gui.md`: state that GUI installs come from the GUI archive, not a combined archive.
- Modify `docs/knowledge/runbooks/update-after-release.md`: tell operators to download the archive for the binary they are updating.

### Task 1: Component Release Workflow

**Files:**
- Modify: `.github/workflows/release.yml`

- [ ] **Step 1: Inspect the current build and package steps**

Run:

```bash
rtk sed -n '70,150p' .github/workflows/release.yml
```

Expected: the build step builds all three binaries and the package step copies
`pohunek`, `pohunekd`, and `pohunek-gui` into one archive.

- [ ] **Step 2: Add component metadata to the build matrix**

Replace the target-only matrix with entries that include `component`,
`archive_prefix`, and `binary`. Keep the two supported targets and create three
entries per target:

```yaml
- target: x86_64-unknown-linux-gnu
  runner: ubuntu-latest
  component: cli
  archive_prefix: pohunek-cli
  binary: pohunek
- target: x86_64-unknown-linux-gnu
  runner: ubuntu-latest
  component: daemon
  archive_prefix: pohunek-daemon
  binary: pohunekd
- target: x86_64-unknown-linux-gnu
  runner: ubuntu-latest
  component: gui
  archive_prefix: pohunek-gui
  binary: pohunek-gui
- target: x86_64-unknown-linux-musl
  runner: ubuntu-latest
  component: cli
  archive_prefix: pohunek-cli
  binary: pohunek
- target: x86_64-unknown-linux-musl
  runner: ubuntu-latest
  component: daemon
  archive_prefix: pohunek-daemon
  binary: pohunekd
- target: x86_64-unknown-linux-musl
  runner: ubuntu-latest
  component: gui
  archive_prefix: pohunek-gui
  binary: pohunek-gui
```

- [ ] **Step 3: Update job and cache names**

Use component-aware names so GitHub UI and cache keys are unambiguous:

```yaml
name: Build ${{ matrix.component }} for ${{ matrix.target }}
```

and:

```yaml
key: ${{ matrix.target }}-${{ matrix.component }}
```

- [ ] **Step 4: Change package staging to copy one binary**

Update the package shell step so it computes:

```bash
version="${GITHUB_REF_NAME#v}"
name="${{ matrix.archive_prefix }}-${version}-${{ matrix.target }}"
staging="dist/${name}"
bindir="target/${{ matrix.target }}/release"
cp "${bindir}/${{ matrix.binary }}" "${staging}/"
```

Keep the docs and root extras copy logic unchanged.

- [ ] **Step 5: Verify workflow syntax locally**

Run a YAML parser if available:

```bash
rtk ruby -e 'require "yaml"; YAML.load_file(".github/workflows/release.yml"); puts "ok"'
```

Expected: `ok`.

If Ruby is unavailable, run:

```bash
rtk python3 -c 'import yaml; yaml.safe_load(open(".github/workflows/release.yml")); print("ok")'
```

Expected: `ok`.

### Task 2: User-Facing Documentation

**Files:**
- Modify: `README.md`
- Modify: `docs/knowledge/guides/gui.md`
- Modify: `docs/knowledge/runbooks/update-after-release.md`

- [ ] **Step 1: Update README install text**

Change the install section to say release downloads are component archives:

```markdown
Download the component release archive for the binary you want to install, or
build the three binaries from a checked-out repository:
```

Change the archive sentence to:

```markdown
Each component archive includes one binary, the root README, MIT license, and the
offline documentation bundle.
```

- [ ] **Step 2: Update GUI guide release text**

Change:

```markdown
Release archives include `pohunek-gui` alongside the CLI and daemon binaries.
```

to:

```markdown
Install `pohunek-gui` from the GUI component release archive.
```

- [ ] **Step 3: Update update-after-release runbook**

Change the opening sentence to:

```markdown
Use this runbook after replacing an installed Pohunek binary from a component
release archive or rebuilding it from source.
```

Add a first checklist item:

```markdown
1. Download the component archive for the binary being updated: CLI (`pohunek`),
   daemon (`pohunekd`), or GUI (`pohunek-gui`).
```

Renumber the existing checklist.

### Task 3: Verification

**Files:**
- Read: `.github/workflows/release.yml`
- Read: `README.md`
- Read: `docs/knowledge/guides/gui.md`
- Read: `docs/knowledge/runbooks/update-after-release.md`

- [ ] **Step 1: Confirm component archive names**

Run:

```bash
rtk rg -n "archive_prefix|matrix.binary|pohunek-cli|pohunek-daemon|pohunek-gui" .github/workflows/release.yml
```

Expected: workflow contains all three archive prefixes and package step copies
`${{ matrix.binary }}`.

- [ ] **Step 2: Confirm stale combined-archive wording is gone**

Run:

```bash
rtk rg -n "alongside the CLI and daemon binaries|includes the CLI, daemon, native GUI|three binaries" README.md docs/knowledge/guides/gui.md docs/knowledge/runbooks/update-after-release.md
```

Expected: no stale combined-archive wording remains. The README source build
sentence may still mention building the three binaries.

- [ ] **Step 3: Run docs check**

Run:

```bash
rtk cargo xtask docs check
```

Expected: all checks pass.

- [ ] **Step 4: Review final diff**

Run:

```bash
rtk git diff -- .github/workflows/release.yml README.md docs/knowledge/guides/gui.md docs/knowledge/runbooks/update-after-release.md docs/superpowers/specs/2026-07-03-split-release-artifacts-design.md docs/superpowers/plans/2026-07-03-split-release-artifacts.md
```

Expected: diff is scoped to release workflow, release docs, and the spec/plan.
