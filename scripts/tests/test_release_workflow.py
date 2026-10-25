"""Structural checks on the release workflows: one writer, no repository code in it.

The release is built by four workflow files: release.yml (the tag trigger,
the gate, the macOS jobs, and the single `publish` job), release-build.yml
(reusable producers), release-evidence.yml (reusable evidence, assembly and
smoke) and release-smoke-scenarios.yml (the synthetic-archive smoke scenarios,
which ci.yml starts before the rehearsal build and release-evidence.yml calls
in a release; issue #1259). The checks pin the security model (issue #150, D2):

- exactly one job in the four files holds `contents: write`, the job `publish`,
  and it needs every producer and the evidence workflow;
- `publish` and the two jobs holding the OIDC scopes (`attest`,
  `attest-bundle`) are compared against exact allowlists of actions, of
  `run:` scripts and of line shapes, so any other step, any other pinned action
  and any edit that smuggles a command into an allowed script fails the test
  until the allowlist is updated on purpose;
- the signing secret appears once, in the signing step of the `assemble` job,
  which alone has the environment `release` and runs only in release mode;
- every action is pinned to a commit and every asset name agrees with
  packaging/release-policy.json.

The workflows are parsed with the standard library only, like the other script
tests; the parser is lexical, so it fails closed: a token block is recognized
only in its canonical spelling and anything else (whitespace before a colon,
quoted keys, flow mappings, anchors, aliases, merge keys) is reported.
"""

import fnmatch
import json
import os
from pathlib import Path
import re
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"
RELEASE = WORKFLOWS / "release.yml"
BUILD = WORKFLOWS / "release-build.yml"
EVIDENCE = WORKFLOWS / "release-evidence.yml"
SCENARIOS = WORKFLOWS / "release-smoke-scenarios.yml"
CI = WORKFLOWS / "ci.yml"
POLICY = ROOT / "packaging" / "release-policy.json"
SCRIPTS = ROOT / "scripts" / "release-workflow"
SCCACHE_ACTION = ROOT / ".github" / "actions" / "setup-sccache" / "action.yml"

DOWNLOAD = "actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093"
ATTEST_ACTION = "actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6"

WRITER = "publish"
OIDC_JOBS = ("attest", "attest-bundle")
# The caller jobs that must grant the OIDC scopes so that a called workflow
# may hold them. They run nothing themselves.
RELEASE_CALLER = "evidence"
CI_CALLER = "release-rehearsal"
SIGNING_JOB = "assemble"
SIGNING_SECRET = "CATALOG_SIGNING_KEY_CI"
# The terms that start the CI rehearsal: the nightly tier (schedule and a
# `nightly` dispatch) and a pull request whose `release_rehearsal` filter
# matched. Push to main and a `main` dispatch never start it.
REHEARSAL_RUN_TERMS = (
    "github.event_name == 'schedule'",
    "(github.event_name == 'workflow_dispatch' && inputs.tier == 'nightly')",
    "(github.event_name == 'pull_request' && needs.changes.outputs.release_rehearsal == 'true')",
)
REHEARSAL_INPUTS = {
    ".github/workflows/ci.yml",
    ".github/workflows/release.yml",
    ".github/workflows/release-build.yml",
    ".github/workflows/release-evidence.yml",
    ".github/workflows/release-smoke-scenarios.yml",
    "packaging/**",
    "scripts/release-workflow/**",
    "scripts/tests/test_release_workflow.py",
    "scripts/tests/test_release_workflow_scripts.py",
    "crates/xtask/**",
    "crates/package/**",
    "compat/**",
    "runtime-packages/**",
    "crates/cli/tests/release_consumer.rs",
    "crates/cli/tests/support/**",
    "sdk/ts/scripts/**",
    "Cargo.toml",
    "Cargo.lock",
}

# Every producer and evidence job the writer waits for, spelled out so that
# transitivity is never relied on.
PUBLISH_NEEDS = [
    "tier-evidence",
    "gate",
    "docs-gate",
    "sdk-gate",
    "upgrade-test",
    "build",
    "stage-macos",
    "package-macos",
    "verify-macos",
    "evidence",
    "release-notes",
]

JOB_HEADER = re.compile(r"^  ([a-z][a-z0-9-]*):\n", re.M)
USES = re.compile(r"^\s+(?:- )?uses:\s*(\S+)", re.M)
RUN = re.compile(r"^(\s+)(?:- )?run:\s*(.*)$", re.M)
PINNED = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}$")
# A local composite action runs from the same commit as the workflow; every
# action it uses is pinned to a commit in turn.
LOCAL_ACTION = re.compile(r"^\./\.github/actions/[a-z-]+$")

PERMISSIONS_LINE = re.compile(r"^\s*['\"]?permissions['\"]?\s*:", re.M)
OIDC_SCOPE = re.compile(r"^\s*['\"]?(id-token|attestations)['\"]?\s*:", re.M)
WRITE_BLOCK = re.compile(r"^    permissions:\n      contents: write$", re.M)
READ_BLOCK = r"    permissions:\n      contents: read\n(?!      \S)"
WRITE_BLOCK_FULL = r"    permissions:\n      contents: write\n(?!      \S)"
EMPTY_BLOCK = r"    permissions: \{\}\n"
OIDC_BLOCK = (
    r"    permissions:\n      contents: read\n      id-token: write\n"
    r"      attestations: write\n(?!      \S)"
)
TIER_READ_BLOCK = (
    r"    permissions:\n      contents: read\n      actions: read\n(?!      \S)"
)
CANONICAL_PERMISSIONS = re.compile(
    "^(?:" + "|".join((READ_BLOCK, WRITE_BLOCK_FULL, EMPTY_BLOCK, OIDC_BLOCK, TIER_READ_BLOCK)) + ")", re.M
)

# A plain scalar value: its first character may not open a flow collection,
# an anchor, an alias, a tag, a block scalar, or a quoted string.
_SCALAR = r"[^\s{}\[\]&*!|>'\"%@`][^\n]*"
_COMMENT = r"\s*#[^\n]*"
_PINNED_USES = r"        uses: [A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}( # v[0-9.]+)?"

WRITER_LINES = [
    re.compile(pattern)
    for pattern in (
        rf"    (name|runs-on|timeout-minutes): {_SCALAR}",
        r"    needs: \[[a-z0-9, -]+\]",
        r"    (permissions|env|steps):",
        r"      contents: write",
        r"      (GH_TOKEN|GH_REPO|TAG|COMMIT): \$\{\{ github\.(token|repository|ref_name|sha) \}\}",
        rf"      - name: {_SCALAR}",
        _PINNED_USES,
        r"        with:",
        rf"          (name|path): {_SCALAR}",
        r"        id: draft",
        r"        if: \$\{\{ failure\(\) && steps\.draft\.outcome == 'success' \}\}",
        r"        shell: bash",
        r"        run: \|",
        _COMMENT,
    )
]

OIDC_LINES = [
    re.compile(pattern)
    for pattern in (
        rf"    (name|runs-on|timeout-minutes): {_SCALAR}",
        r"    if: \$\{\{ inputs\.mode == 'release' \}\}",
        r"    needs: \[[a-z0-9, -]+\]",
        r"    (permissions|steps):",
        r"      contents: read",
        r"      id-token: write",
        r"      attestations: write",
        rf"      - name: {_SCALAR}",
        _PINNED_USES,
        r"        with:",
        rf"          (name|path|pattern): {_SCALAR}",
        r"          merge-multiple: true",
        r"          subject-path: \|",
        r"            \$\{\{ runner\.temp \}\}/[a-z-]+/[A-Za-z0-9_.*-]+",
        r"        shell: bash",
        r"        run: \|",
        _COMMENT,
    )
]

# The caller job grants the OIDC ceiling and passes one signing secret.
CALLER_LINES = [
    re.compile(pattern)
    for pattern in (
        rf"    (name|needs): {_SCALAR}",
        r"    needs: \[[a-z0-9, -]+\]",
        r"    uses: \./\.github/workflows/release-evidence\.yml",
        r"    (permissions|with|secrets):",
        r"      contents: read",
        r"      id-token: write",
        r"      attestations: write",
        r"      version: \$\{\{ needs\.build\.outputs\.version \}\}",
        r"      commit: \$\{\{ github\.sha \}\}",
        r"      mode: release",
        r"      CATALOG_SIGNING_KEY_CI: \$\{\{ secrets\.CATALOG_SIGNING_KEY_CI \}\}",
        _COMMENT,
    )
]

TIER_EVIDENCE_LINES = [
    re.compile(pattern)
    for pattern in (
        rf"    (name|runs-on): {_SCALAR}",
        r"    (permissions|env|steps):",
        r"      (contents|actions): read",
        r"      (GH_TOKEN|GH_REPO|COMMIT): \$\{\{ github\.(token|repository|sha) \}\}",
        rf"      - name: {_SCALAR}",
        r"      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262(?: # v[0-9.]+)?",
        r"        with:",
        r"          persist-credentials: false",
        r"        run: python3 scripts/ci-evidence --repo \"\$GH_REPO\" --sha \"\$COMMIT\" --tier (main|nightly)",
        _COMMENT,
    )
]

EVIDENCE_CALL = "./.github/workflows/release-evidence.yml"
TIER_EVIDENCE_JOB = "tier-evidence"
CHECKOUT = "actions/checkout@11d5960a326750d5838078e36cf38b85af677262"
UPLOAD_ARTIFACT = "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02"
PUBLISH_ACTIONS = {DOWNLOAD}
OIDC_ACTIONS = {DOWNLOAD, ATTEST_ACTION}

OIDC_CHECK_SCRIPT = (
    "set -euo pipefail",
    'cd "$RUNNER_TEMP/attest-assets"',
    "sha256sum -c ./*.sha256",
    "archives=\"$(find . -maxdepth 1 -type f \\( -name '*.tar.gz' -o -name '*.tgz' -o -name '*.tar.zst' \\) | wc -l)\"",
    "checksums=\"$(find . -maxdepth 1 -type f -name '*.sha256' | wc -l)\"",
    'test "$archives" = "$checksums"',
)
BUNDLE_CHECK_SCRIPT = (
    "set -euo pipefail",
    'cd "$RUNNER_TEMP/bundle"',
    "sha256sum -c release-inventory.sha256",
)
OIDC_SCRIPTS = {OIDC_CHECK_SCRIPT, BUNDLE_CHECK_SCRIPT}

PUBLISH_SCRIPTS = {
    (
        'set -euo pipefail',
        '[[ "$TAG" =~ ^v[0-9]+\\.[0-9]+\\.[0-9]+$ ]]',
        'version="${TAG#v}"',
        'cd "$RUNNER_TEMP/bundle"',
        'test -z "$(find . -mindepth 1 ! -type f -print)"',
        'test -z "$(find . -mindepth 2 -print)"',
        'test -z "$(grep -Ev \'^[0-9a-f]{64}  [A-Za-z0-9][A-Za-z0-9._-]*$\' release-inventory.sha256 || true)"',
        'sha256sum -c release-inventory.sha256',
        '{ cut -c67- release-inventory.sha256; echo release-inventory.sha256; } | LC_ALL=C sort > "$RUNNER_TEMP/listed.txt"',
        'find . -maxdepth 1 -type f -printf \'%f\\n\' | LC_ALL=C sort > "$RUNNER_TEMP/present.txt"',
        'diff "$RUNNER_TEMP/listed.txt" "$RUNNER_TEMP/present.txt"',
        'while IFS= read -r name; do',
        'case "$name" in',
        'runtime-catalog.json | release-inventory.sha256 | attestation-*.json) ;;',
        '*"-$version-"* | *"-$version."*) ;;',
        '*) echo "::error::$name does not belong to release $version"; exit 1 ;;',
        'esac',
        'done < "$RUNNER_TEMP/present.txt"',
    ),
    (
        'set -euo pipefail',
        'cd "$RUNNER_TEMP/bundle"',
        'for file in *; do',
        'gh attestation verify "$file" --repo "$GH_REPO" \\',
        '--signer-workflow "$GH_REPO/.github/workflows/release-evidence.yml" \\',
        '--signer-digest "$COMMIT" --source-ref "$GITHUB_REF" --source-digest "$COMMIT" \\',
        '--deny-self-hosted-runners > /dev/null',
        'done',
    ),
    (
        'set -euo pipefail',
        'test "$(gh api "repos/$GH_REPO/commits/$TAG" --jq .sha)" = "$COMMIT"',
        'existing="$(gh api --paginate "repos/$GH_REPO/releases" --jq \'.[] | select(.tag_name == env.TAG) | .id\')"',
        'test -z "$existing"',
    ),
    (
        'set -euo pipefail',
        'test -s "$RUNNER_TEMP/release-notes/release-notes.md"',
        'gh release create "$TAG" --draft --verify-tag --title "$TAG" --notes-file "$RUNNER_TEMP/release-notes/release-notes.md"',
    ),
    (
        'set -euo pipefail',
        'gh api --paginate "repos/$GH_REPO/releases" --jq \'.[] | select(.tag_name == env.TAG and .draft) | .body\' > "$RUNNER_TEMP/draft-body.txt"',
        'cmp "$RUNNER_TEMP/draft-body.txt" <(cat "$RUNNER_TEMP/release-notes/release-notes.md"; echo)',
    ),
    (
        'set -euo pipefail',
        'cd "$RUNNER_TEMP/bundle"',
        'gh release upload "$TAG" ./*',
    ),
    (
        'set -euo pipefail',
        'cd "$RUNNER_TEMP/bundle"',
        'want="$(for file in *; do printf \'%s sha256:%s %s\\n\' "$file" "$(sha256sum "$file" | cut -d\' \' -f1)" "$(stat -c %s "$file")"; done | LC_ALL=C sort)"',
        'have="$(gh api --paginate "repos/$GH_REPO/releases" --jq \'.[] | select(.tag_name == env.TAG and .draft) | .assets[] | select(.state == "uploaded") | "\\(.name) \\(.digest) \\(.size)"\' | LC_ALL=C sort)"',
        'test -n "$have"',
        'test "$want" = "$have"',
    ),
    (
        'set -euo pipefail',
        'test "$(gh api "repos/$GH_REPO/commits/$TAG" --jq .sha)" = "$COMMIT"',
        'gh release edit "$TAG" --draft=false',
    ),
    (
        'set -euo pipefail',
        'cd "$RUNNER_TEMP/bundle"',
        'want="$(for file in *; do printf \'%s sha256:%s %s\\n\' "$file" "$(sha256sum "$file" | cut -d\' \' -f1)" "$(stat -c %s "$file")"; done | LC_ALL=C sort)"',
        'have="$(gh api --paginate "repos/$GH_REPO/releases" --jq \'.[] | select(.tag_name == env.TAG and (.draft | not)) | .assets[] | select(.state == "uploaded") | "\\(.name) \\(.digest) \\(.size)"\' | LC_ALL=C sort)"',
        'test -n "$have"',
        'test "$want" = "$have"',
    ),
    (
        'set -euo pipefail',
        'for id in $(gh api --paginate "repos/$GH_REPO/releases" --jq \'.[] | select(.tag_name == env.TAG and .draft) | .id\'); do',
        'gh api --method DELETE "repos/$GH_REPO/releases/$id"',
        'done',
    ),
}
NOTES_SCRIPTS = {
    (
        'set -euo pipefail',
        '[[ "$TAG" =~ ^v[0-9]+\\.[0-9]+\\.[0-9]+$ ]]',
        'version="${TAG#v}"',
        'mkdir -p "$RUNNER_TEMP/release-notes"',
        'python3 scripts/changelog notes "$version" > "$RUNNER_TEMP/release-notes/release-notes.md"',
    ),
}


def jobs(text: str) -> dict[str, str]:
    """Top-level job name -> its block of the `jobs:` section."""
    section = text.split("\njobs:\n", 1)[1]
    headers = list(JOB_HEADER.finditer(section))
    return {
        match.group(1): section[match.end() : headers[i + 1].start() if i + 1 < len(headers) else len(section)]
        for i, match in enumerate(headers)
    }


def filter_paths(text: str, name: str) -> set[str]:
    match = re.search(
        rf"^            {re.escape(name)}:\n((?:              - '[^\n]*'\n)+)", text, re.M
    )
    if match is None:
        raise AssertionError(f"{name} paths filter not found")
    return set(re.findall(r"- '([^']+)'", match.group(1)))


def holds_write_token(block: str) -> bool:
    return WRITE_BLOCK.search(block) is not None


def needs_of(block: str) -> list[str]:
    match = re.search(r"(?m)^    needs: \[([^\]]*)\]$", block)
    return [item.strip() for item in match.group(1).split(",")] if match else []


def run_scripts(block: str) -> list[tuple[str, ...]]:
    """Every `run:` script of a job as normalized lines."""
    lines = block.splitlines()
    scripts = []
    for index, line in enumerate(lines):
        match = RUN.match(line)
        if match is None:
            continue
        indent, inline = match.groups()
        body = [] if inline in ("|", ">", "|-", ">-") else [inline]
        if not body:
            for following in lines[index + 1 :]:
                if following.strip() and len(following) - len(following.lstrip()) <= len(indent):
                    break
                body.append(following)
        scripts.append(
            tuple(
                stripped
                for stripped in (raw.strip() for raw in body)
                if stripped and not stripped.startswith("#")
            )
        )
    return scripts


def lines_outside_run_bodies(block: str) -> list[str]:
    """Job lines minus the bodies of `run: |` steps.

    Run bodies are compared whole against the script allowlists; every other
    line must match a canonical shape on its own.
    """
    kept = []
    body_indent = None
    for line in block.splitlines():
        indent = len(line) - len(line.lstrip())
        if body_indent is not None:
            if not line.strip() or indent > body_indent:
                continue
            body_indent = None
        kept.append(line)
        if re.fullmatch(r"\s+run: \|", line):
            body_indent = indent
    return kept


def jobs_section_violations(text: str) -> list[str]:
    """Every top-level entry of `jobs:` must be a canonical block header.

    A flow-style job (`  name: {...}`), a quoted job key, or content before the
    first header would otherwise escape the per-job checks.
    """
    section = text.split("\njobs:\n", 1)[1]
    violations = []
    for line in section.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        indent = len(line) - len(line.lstrip())
        if indent < 2 or (indent == 2 and not re.fullmatch(r"  [a-z][a-z0-9-]*:", line)):
            violations.append(f"the jobs section has an entry the guard does not recognize: {line!r}")
    first = JOB_HEADER.search(section)
    preamble = section[: first.start()] if first else section
    if first is None or any(
        line.strip() and not line.lstrip().startswith("#") for line in preamble.splitlines()
    ):
        violations.append("the jobs section has content before its first job")
    return violations


def header_violations(text: str) -> list[str]:
    """The workflow default token is empty and mentions no OIDC scope."""
    header = text.partition("\njobs:\n")[0]
    violations = []
    if len(PERMISSIONS_LINE.findall(header)) != 1 or not re.search(
        r"(?m)^permissions: \{\}$", header
    ):
        violations.append("the workflow default token is not exactly `permissions: {}`")
    if OIDC_SCOPE.search(header):
        violations.append("the workflow default token mentions id-token or attestations")
    return violations


def permission_violations(name: str, block: str, oidc: set[str]) -> list[str]:
    """A job's token grant must be absent or spelled canonically.

    The OIDC scopes belong to the `oidc` jobs alone, in exactly one spelling,
    and `contents: write` to the writer.
    """
    violations = []
    granted = len(PERMISSIONS_LINE.findall(block))
    canonical = len(CANONICAL_PERMISSIONS.findall(block + "\n"))
    if granted != canonical:
        violations.append(f"{name} declares permissions in a form the guard does not recognize")
    has_oidc = OIDC_SCOPE.search(block) is not None
    if name in oidc:
        if re.search(OIDC_BLOCK, block + "\n", re.M) is None or len(OIDC_SCOPE.findall(block)) != 2:
            violations.append(
                f"{name} must declare exactly `contents: read`, `id-token: write`, `attestations: write`"
            )
    elif has_oidc:
        violations.append(f"{name} mentions id-token or attestations, which only {sorted(oidc)} may hold")
    return violations


def job_shape_violations(name: str, block: str, shapes, actions: set[str], scripts) -> list[str]:
    """Fail closed: every line, action and script of the job must be allowlisted."""
    violations = []
    for line in lines_outside_run_bodies(block):
        if line.strip() and not any(shape.fullmatch(line) for shape in shapes):
            violations.append(f"{name} has a line outside the canonical shapes: {line!r}")
    for action in USES.findall(block):
        if action not in actions:
            violations.append(f"{name} uses {action}, which is not allowlisted for it")
    for script in run_scripts(block):
        if script not in scripts:
            violations.append(f"{name} runs a script outside the allowlist: {script!r}")
    return violations


def workflow_violations(
    text: str,
    *,
    writer: str | None,
    oidc: set[str],
    callers: set[str] = frozenset(),
    publish_scripts=PUBLISH_SCRIPTS,
) -> list[str]:
    """Everything the guard rejects in one workflow file's text."""
    violations = header_violations(text)
    violations.extend(jobs_section_violations(text))
    for name, block in jobs(text).items():
        violations.extend(permission_violations(name, block, oidc | set(callers)))
        if name == TIER_EVIDENCE_JOB:
            violations.extend(job_shape_violations(name, block, TIER_EVIDENCE_LINES, {CHECKOUT}, {
                ("python3 scripts/ci-evidence --repo \"$GH_REPO\" --sha \"$COMMIT\" --tier main",),
                ("python3 scripts/ci-evidence --repo \"$GH_REPO\" --sha \"$COMMIT\" --tier nightly",),
            }))
        elif name == writer:
            if not holds_write_token(block):
                violations.append(f"{name} must hold the write token")
            violations.extend(
                job_shape_violations(name, block, WRITER_LINES, PUBLISH_ACTIONS, publish_scripts)
            )
        elif holds_write_token(block):
            violations.append(f"{name} holds a write token, which only {writer} may")
        elif name in oidc:
            violations.extend(job_shape_violations(name, block, OIDC_LINES, OIDC_ACTIONS, OIDC_SCRIPTS))
        elif name in callers:
            violations.extend(job_shape_violations(name, block, CALLER_LINES, {EVIDENCE_CALL}, set()))
            if "steps:" in block:
                violations.append(f"{name} must only call a workflow")
    return violations


def all_uses(text: str) -> list[str]:
    return USES.findall(text)


class ReleaseWorkflowFiles(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.release = RELEASE.read_text()
        cls.build = BUILD.read_text()
        cls.evidence = EVIDENCE.read_text()
        cls.scenarios = SCENARIOS.read_text()
        cls.ci = CI.read_text()
        cls.release_jobs = jobs(cls.release)
        cls.build_jobs = jobs(cls.build)
        cls.evidence_jobs = jobs(cls.evidence)
        cls.scenario_jobs = jobs(cls.scenarios)
        cls.texts = {
            "release.yml": cls.release,
            "release-build.yml": cls.build,
            "release-evidence.yml": cls.evidence,
            "release-smoke-scenarios.yml": cls.scenarios,
        }


class WriterTests(ReleaseWorkflowFiles):
    def test_exact_commit_tier_evidence_gates_every_producer_and_writer(self):
        gate = self.release_jobs[TIER_EVIDENCE_JOB]
        self.assertIn("--tier main", gate)
        self.assertIn("--tier nightly", gate)
        self.assertNotIn("--dispatch-ref", gate)
        for name in (
            "gate", "upgrade-test", "docs-gate", "sdk-gate", "build",
            "stage-macos", "package-macos", "verify-macos", "evidence",
            "release-notes", WRITER,
        ):
            with self.subTest(job=name):
                self.assertIn(TIER_EVIDENCE_JOB, needs_of(self.release_jobs[name]))
        self.assertEqual(
            workflow_violations(self.release, writer=WRITER, oidc=set(), callers={RELEASE_CALLER}),
            [],
        )

    def test_exactly_one_job_holds_the_write_token(self):
        holders = sorted(
            (file, name)
            for file, text in self.texts.items()
            for name, block in jobs(text).items()
            if holds_write_token(block) or "contents: write" in block
        )
        self.assertEqual(holders, [("release.yml", WRITER)])

    def test_the_writer_needs_every_producer_and_the_evidence_workflow(self):
        self.assertEqual(needs_of(self.release_jobs[WRITER]), PUBLISH_NEEDS)
        for name in PUBLISH_NEEDS:
            self.assertIn(name, self.release_jobs)

    def test_the_evidence_job_waits_for_every_artifact_it_downloads(self):
        self.assertEqual(needs_of(self.release_jobs[RELEASE_CALLER]), ["tier-evidence", "build", "verify-macos"])
        self.assertIn("uses: ./.github/workflows/release-build.yml", self.release_jobs["build"])
        self.assertEqual(
            needs_of(self.release_jobs["build"]), ["tier-evidence", "gate", "docs-gate", "sdk-gate", "upgrade-test"]
        )

    def test_the_writer_runs_nothing_from_the_repository_or_the_bundle(self):
        block = self.release_jobs[WRITER]
        self.assertNotIn("checkout", block)
        self.assertNotRegex(block, r"uses: \./")
        self.assertEqual(set(USES.findall(block)), {DOWNLOAD})
        self.assertEqual(workflow_violations(self.release, writer=WRITER, oidc=set(), callers={RELEASE_CALLER}), [])

    def test_the_writer_creates_a_draft_and_publishes_only_after_verifying_it(self):
        block = self.release_jobs[WRITER]
        order = [
            "sha256sum -c release-inventory.sha256",
            "gh attestation verify",
            "gh release create",
            "--draft --verify-tag",
            "gh release upload",
            "Verify the draft against the inventory",
            "gh release edit",
            "--draft=false",
            "Verify the published release",
        ]
        positions = [block.index(item) for item in order]
        self.assertEqual(positions, sorted(positions))
        self.assertIn(".draft)", block)
        self.assertIn("test -z \"$existing\"", block)
        self.assertIn("steps.draft.outcome == 'success'", block)
        self.assertEqual(block.count("gh release create"), 1)

    def test_the_writer_binds_the_tag_to_the_commit_twice(self):
        block = self.release_jobs[WRITER]
        self.assertEqual(block.count('gh api "repos/$GH_REPO/commits/$TAG" --jq .sha'), 2)

    def test_no_other_job_touches_a_release(self):
        for file, text in self.texts.items():
            for name, block in jobs(text).items():
                if (file, name) == ("release.yml", WRITER):
                    continue
                with self.subTest(job=f"{file}:{name}"):
                    self.assertNotRegex(block, r"gh release|gh api|softprops|action-gh-release")
        for file, text in self.texts.items():
            self.assertNotIn("softprops", text, file)
            self.assertNotIn("action-gh-release", text, file)

    def test_producers_hold_read_only_tokens(self):
        for file, text in self.texts.items():
            for name, block in jobs(text).items():
                if (file, name) == ("release.yml", WRITER):
                    continue
                with self.subTest(job=f"{file}:{name}"):
                    self.assertNotIn("contents: write", block)
                    self.assertNotIn("GH_TOKEN: ${{ secrets", block)


class ReleaseNotesTests(ReleaseWorkflowFiles):
    """The release body is data rendered by a read-only job from the tagged commit."""

    def test_the_notes_job_reads_only_the_tagged_commit(self):
        block = self.release_jobs["release-notes"]
        self.assertEqual(needs_of(block), [TIER_EVIDENCE_JOB])
        self.assertEqual(
            re.findall(r"(?m)^    permissions:\n      contents: read$", block), ["    permissions:\n      contents: read"]
        )
        self.assertEqual(set(USES.findall(block)), {CHECKOUT, UPLOAD_ARTIFACT})
        self.assertRegex(block, r"(?m)^          persist-credentials: false$")
        self.assertNotRegex(block, r"\bgh (release|api)\b")
        self.assertEqual(
            workflow_violations(self.release, writer=WRITER, oidc=set(), callers={RELEASE_CALLER}),
            [],
        )
        self.assertEqual(run_scripts(block), list(NOTES_SCRIPTS))

    def test_the_notes_are_uploaded_as_their_own_artifact(self):
        block = self.release_jobs["release-notes"]
        self.assertRegex(block, r"(?m)^          name: release-notes$")
        self.assertRegex(block, r"(?m)^          if-no-files-found: error$")

    def test_the_draft_takes_its_body_from_the_notes_artifact(self):
        block = self.release_jobs[WRITER]
        self.assertRegex(
            block,
            r"(?m)^          name: release-notes\n          path: \$\{\{ runner\.temp \}\}/release-notes$",
        )
        self.assertEqual(block.count("--notes-file"), 1)
        self.assertNotIn('--notes ""', block)
        self.assertLess(block.index("Download the release notes"), block.index("Create the draft release"))

    def test_the_draft_body_is_verified_before_publication(self):
        block = self.release_jobs[WRITER]
        self.assertIn("Verify the draft body", block)
        self.assertLess(block.index("Verify the draft against the inventory"), block.index("Verify the draft body"))
        self.assertLess(block.index("Verify the draft body"), block.index("Publish the draft"))
        self.assertNotIn("Verify the published release body", block)
        self.assertNotIn("releases/tags/$TAG", block)
        self.assertIn('select(.tag_name == env.TAG and .draft) | .body', block)
        self.assertIn('cmp "$RUNNER_TEMP/draft-body.txt"', block)

    def test_the_notes_never_enter_the_bundle_or_its_inventory(self):
        self.assertNotIn("release-notes", self.build + self.evidence + self.scenarios)
        for name, block in jobs(self.release).items():
            if name in (WRITER, "release-notes"):
                continue
            with self.subTest(job=name):
                self.assertNotIn("release-notes", block)
        for script in run_scripts(self.release_jobs[WRITER]):
            if any("bundle" in line for line in script):
                self.assertFalse(any("release-notes" in line for line in script))


class OidcTests(ReleaseWorkflowFiles):
    def test_only_the_attest_jobs_hold_the_oidc_scopes(self):
        holders = sorted(name for name, block in self.evidence_jobs.items() if OIDC_SCOPE.search(block))
        self.assertEqual(holders, sorted(OIDC_JOBS))
        self.assertEqual(sorted(name for name, block in self.build_jobs.items() if OIDC_SCOPE.search(block)), [])
        self.assertEqual(
            sorted(name for name, block in self.release_jobs.items() if OIDC_SCOPE.search(block)),
            [RELEASE_CALLER],
        )
        self.assertEqual(
            sorted(name for name, block in jobs(self.ci).items() if OIDC_SCOPE.search(block)),
            [CI_CALLER],
        )
        for text in (self.release, self.build, self.evidence, self.ci):
            self.assertFalse(OIDC_SCOPE.search(text.partition("\njobs:\n")[0]))

    def test_the_attest_jobs_run_in_release_mode_only_and_check_out_nothing(self):
        for name in OIDC_JOBS:
            block = self.evidence_jobs[name]
            with self.subTest(job=name):
                self.assertIn("    if: ${{ inputs.mode == 'release' }}\n", block)
                self.assertNotIn("checkout", block)
                self.assertNotRegex(block, r"uses: \./")
                self.assertEqual(set(USES.findall(block)) - OIDC_ACTIONS, set())

    def test_the_attest_jobs_cover_the_producers_and_the_bundle(self):
        attest = self.evidence_jobs["attest"]
        for glob in ("*.tar.gz", "*.tgz", "*.tar.zst", "*.sha256", "*.json"):
            self.assertIn(f"${{{{ runner.temp }}}}/attest-assets/{glob}", attest)
        for pattern in ("build-*", "macos-signed-*", "attestation-*"):
            self.assertIn(f"pattern: {pattern}", attest)
        for name in ("package-archives", "sdk-release-assets"):
            self.assertIn(f"name: {name}", attest)
        self.assertIn("${{ runner.temp }}/bundle/*", self.evidence_jobs["attest-bundle"])
        self.assertEqual(needs_of(self.evidence_jobs["attest-bundle"]), ["assemble"])

    def test_the_guard_accepts_the_evidence_workflow_and_the_callers(self):
        self.assertEqual(
            workflow_violations(self.evidence, writer=None, oidc=set(OIDC_JOBS)), []
        )
        self.assertEqual(workflow_violations(self.build, writer=None, oidc=set()), [])
        self.assertEqual(workflow_violations(self.scenarios, writer=None, oidc=set()), [])

    def test_the_ci_caller_runs_in_rehearsal_mode_and_never_for_forks(self):
        ci_jobs = jobs(self.ci)
        build = ci_jobs["release-rehearsal-build"]
        evidence = ci_jobs[CI_CALLER]
        for name, block in (("release-rehearsal-build", build), (CI_CALLER, evidence)):
            with self.subTest(job=name):
                condition = re.search(r"(?m)^    if: (.*)$", block)
                self.assertIsNotNone(condition)
                for term in REHEARSAL_RUN_TERMS:
                    self.assertIn(term, condition.group(1))
                self.assertNotIn("github.event_name == 'push'", condition.group(1))
                self.assertIn("github.event.pull_request.head.repo.full_name == github.repository", block)
                self.assertIn("dependabot[bot]", block)
                self.assertIn("mode: rehearsal", block)
                self.assertNotIn("secrets", block)
                self.assertNotIn("environment", block)
        self.assertIn("uses: ./.github/workflows/release-build.yml", build)
        self.assertIn("uses: ./.github/workflows/release-evidence.yml", evidence)
        # The evidence caller also waits for the scenario job, so that its
        # result can travel into the verdict as `scenarios_result`.
        self.assertEqual(needs_of(evidence), ["changes", "release-rehearsal-build", "release-rehearsal-scenarios"])
        self.assertIn("needs.release-rehearsal-build.outputs.version", evidence)
        self.assertIn("scenarios_result: ${{ needs.release-rehearsal-scenarios.result }}", evidence)
        # Scenario failure must reach the verdict, but missing producer
        # artifacts and a cancelled run must never start evidence jobs.
        self.assertIn(
            "    if: ${{ !cancelled() && needs.release-rehearsal-build.result == 'success' && ",
            evidence,
        )
        self.assertNotIn("always()", evidence)
        self.assertNotRegex(self.ci, r"(?m)^  pull_request_target:")

    def test_the_ci_scenario_caller_starts_before_the_build_and_the_bundle(self):
        # D2 of #1259: the synthetic-archive scenarios read only the checkout,
        # so their caller waits for the paths filter alone — never for the
        # rehearsal build, the evidence workflow, or any producer job.
        ci_jobs = jobs(self.ci)
        caller = ci_jobs["release-rehearsal-scenarios"]
        self.assertEqual(needs_of(caller), ["changes"])
        self.assertIn("uses: ./.github/workflows/release-smoke-scenarios.yml", caller)
        self.assertNotIn("release-rehearsal-build", caller)
        self.assertNotIn("release-rehearsal:", caller)
        self.assertNotIn("secrets", caller)
        self.assertNotIn("environment", caller)
        condition = re.search(r"(?m)^    if: (.*)$", caller)
        self.assertIsNotNone(condition)
        for term in REHEARSAL_RUN_TERMS:
            self.assertIn(term, condition.group(1))
        self.assertNotIn("github.event_name == 'push'", condition.group(1))
        self.assertIn("github.event.pull_request.head.repo.full_name == github.repository", caller)
        self.assertIn("dependabot[bot]", caller)
        # The scenarios workflow runs the suite on macOS and holds no secret.
        scenarios = self.scenario_jobs["smoke-scenarios"]
        self.assertIn("    runs-on: macos-15\n", scenarios)
        self.assertNotIn("secrets", self.scenarios)
        self.assertNotIn("cargo", self.scenarios)

    def test_ci_rehearsal_filter_covers_its_inputs(self):
        self.assertEqual(REHEARSAL_INPUTS - filter_paths(self.ci, "release_rehearsal"), set())
        self.assertIn(
            "release_rehearsal: ${{ steps.filter.outputs.release_rehearsal }}",
            jobs(self.ci)["changes"],
        )

    def test_release_workflow_change_triggers_the_release_build(self):
        self.assertIn(".github/workflows/release.yml", filter_paths(self.ci, "release"))


class SigningTests(ReleaseWorkflowFiles):
    def test_the_signing_secret_is_forwarded_once_and_read_only_by_assemble(self):
        for file, text in self.texts.items():
            expected = 1 if file in ("release.yml", "release-evidence.yml") else 0
            self.assertEqual(text.count(f"secrets.{SIGNING_SECRET}"), expected, file)
            self.assertEqual(len(re.findall(r"\bsecrets\.", text)), expected, file)
            self.assertNotIn("secrets: inherit", text)
        self.assertNotIn(SIGNING_SECRET, self.ci)
        caller = self.release_jobs["evidence"]
        self.assertIn(f"    secrets:\n      {SIGNING_SECRET}: ${{{{ secrets.{SIGNING_SECRET} }}}}\n", caller)
        declaration = self.evidence.split("\npermissions:", 1)[0]
        self.assertIn(f"    secrets:\n      {SIGNING_SECRET}:\n", declaration)
        self.assertIn("        required: false\n", declaration.split(f"      {SIGNING_SECRET}:\n", 1)[1])
        assemble = self.evidence_jobs[SIGNING_JOB]
        self.assertEqual(assemble.count(f"secrets.{SIGNING_SECRET}"), 1)
        step = assemble.split("      - name: Sign and assemble the bundle\n", 1)[1].split("\n      - name:", 1)[0]
        self.assertIn(f"          {SIGNING_SECRET}: ${{{{ secrets.{SIGNING_SECRET} }}}}\n", step)
        self.assertIn("scripts/release-workflow/assemble", step)

    def test_only_assemble_has_the_release_environment_and_only_in_release_mode(self):
        for file, text in self.texts.items():
            holders = [name for name, block in jobs(text).items() if re.search(r"(?m)^    environment:", block)]
            self.assertEqual(holders, [SIGNING_JOB] if file == "release-evidence.yml" else [], file)
        assemble = self.evidence_jobs[SIGNING_JOB]
        self.assertIn("    environment: release\n", assemble)
        self.assertIn("    if: ${{ inputs.mode == 'release' }}\n", assemble)
        self.assertNotIn("environment:", self.ci)

    def test_assemble_restores_no_cache_and_its_key_handling_lives_in_one_script(self):
        assemble = self.evidence_jobs[SIGNING_JOB]
        self.assertNotRegex(assemble, r"rust-cache|sccache|actions/cache")
        self.assertIn("persist-credentials: false", assemble)
        script = (SCRIPTS / "assemble").read_text()
        for expected in ("/dev/shm", "chmod 700", "shred -u", "unset CATALOG_SIGNING_KEY_CI", "umask 077"):
            self.assertIn(expected, script)
        self.assertNotRegex(script, r"echo[^\n]*CATALOG_SIGNING_KEY_CI")
        self.assertIn("--anchor", script)
        self.assertIn("packaging/runtime-catalog-anchor.json", script)

    def test_the_rehearsal_assembly_sees_no_secret_and_no_environment(self):
        block = self.evidence_jobs["assemble-rehearsal"]
        self.assertNotIn("secrets.", block)
        self.assertNotIn("environment:", block)
        self.assertIn("    if: ${{ inputs.mode == 'rehearsal' }}\n", block)
        self.assertNotIn("id-token", block)
        script = (SCRIPTS / "assemble").read_text()
        self.assertIn("a rehearsal must not see the release signing key", script)

    def test_the_rehearsal_key_is_derived_from_public_values_and_refuses_tags(self):
        script = (SCRIPTS / "rehearsal-trust").read_text()
        self.assertIn("rehearsal %s %s", script)
        self.assertIn('[ "${GITHUB_REF_TYPE:-}" != tag ]', script)
        build = self.build_jobs["binaries"]
        self.assertIn("inputs.mode == 'rehearsal' && contains(matrix.components, 'daemon')", build)
        rehearsal_macos = self.build_jobs["rehearsal-macos"]
        self.assertIn("    if: ${{ inputs.mode == 'rehearsal' }}\n", rehearsal_macos)
        self.assertIn("runs-on: macos-15\n", rehearsal_macos)
        self.assertIn("permissions:\n      contents: read\n", rehearsal_macos)
        self.assertNotIn("matrix", rehearsal_macos)
        self.assertIn("test \"$(uname -m)\" = \"arm64\"", rehearsal_macos)
        self.assertIn("scripts/release-workflow/rehearsal-trust", rehearsal_macos)
        self.assertIn("packaging/macos/package --adhoc-release", rehearsal_macos)
        for component in ("cli", "daemon"):
            self.assertIn(f"name: macos-signed-{component}\n", rehearsal_macos)
        self.assertIn("mkdir -p dist", rehearsal_macos)
        self.assertNotIn("rehearsal-trust", self.release)

    def test_provenance_of_every_input_is_verified_before_signing(self):
        assemble = self.evidence_jobs[SIGNING_JOB]
        self.assertLess(
            assemble.index("scripts/release-workflow/verify-provenance"),
            assemble.index("scripts/release-workflow/assemble"),
        )
        script = (SCRIPTS / "verify-provenance").read_text()
        for flag in ("--repo", "--signer-workflow", "--signer-digest", "--source-ref", "--source-digest", "--deny-self-hosted-runners"):
            self.assertIn(flag, script)
        self.assertIn(".github/workflows/release-evidence.yml", script)


class EvidenceWorkflowTests(ReleaseWorkflowFiles):
    def test_the_workflows_are_reusable_and_nothing_else(self):
        for text, inputs in (
            (self.evidence, ["version", "commit", "mode", "targets", "scenarios_result"]),
            (self.build, ["mode", "version"]),
            (self.scenarios, []),
        ):
            on_block = text.split("\non:\n", 1)[1].split("\npermissions:", 1)[0]
            self.assertEqual(re.findall(r"(?m)^  ([a-z_]+):$", on_block), ["workflow_call"])
            self.assertEqual(re.findall(r"(?m)^      ([a-z_]+):\n        description:", on_block), inputs)

    def test_every_job_declares_its_permissions(self):
        for file, text in self.texts.items():
            for name, block in jobs(text).items():
                with self.subTest(job=f"{file}:{name}"):
                    self.assertRegex(block, r"(?m)^    permissions:")

    def test_the_jobs_run_in_the_documented_order(self):
        needs = {name: needs_of(block) for name, block in self.evidence_jobs.items()}
        self.assertEqual(needs["plan"], [])
        self.assertEqual(needs["rows"], ["plan"])
        self.assertEqual(needs["attest"], ["rows"])
        self.assertEqual(needs["assemble"], ["rows", "attest"])
        self.assertEqual(needs["assemble-rehearsal"], ["rows"])
        self.assertEqual(needs["smoke"], ["plan", "rows", "assemble", "assemble-rehearsal"])
        # The scenarios read no artifact of the build, the bundle or the rows,
        # so they start as soon as the workflow does (issue #1259, D2).
        self.assertEqual(needs["smoke-scenarios"], [])

    def test_the_verdict_requires_every_other_job_of_the_mode(self):
        verdict = self.evidence_jobs["verdict"]
        self.assertEqual(
            sorted(needs_of(verdict)), sorted(name for name in self.evidence_jobs if name != "verdict")
        )
        self.assertIn("    if: ${{ always() }}\n", verdict)
        for name in ("plan", "rows", "attest", "assemble", "assemble-rehearsal", "attest-bundle", "smoke", "smoke-scenarios"):
            self.assertIn(f'"{name}"', verdict)
        self.assertIn('"skipped"', verdict)

    def test_the_verdict_names_every_planned_row_that_did_not_pass(self):
        verdict = self.evidence_jobs["verdict"]
        self.assertIn("          pattern: row-results-*\n", verdict)
        self.assertIn("          PLAN: ${{ needs.plan.outputs.rows }}\n", verdict)
        self.assertIn('no result from Rows on', verdict)
        self.assertIn('elif $result.status == "pass" then empty', verdict)
        self.assertIn("    permissions: {}\n", verdict)
        # Both checks run whatever the download did; neither replaces the other.
        self.assertEqual(verdict.count("        if: ${{ !cancelled() }}\n"), 2)
        self.assertLess(verdict.index("Name every row that did not pass"), verdict.index("Require the jobs of the mode"))

    def test_the_release_never_consults_a_baseline_or_a_selection(self):
        # The tag builds, tests and attests every official package from its
        # own commit; the per-pull-request selection and the release baseline
        # it verifies are CI cost decisions only.
        scripts = {f"scripts/release-workflow/{path.name}": path.read_text() for path in sorted(SCRIPTS.iterdir()) if path.is_file()}
        for file, text in {**self.texts, **scripts}.items():
            for forbidden in ("package baseline", "package affected", "package-selection", "gh release download", "_baseline_digest"):
                with self.subTest(file=file, forbidden=forbidden):
                    self.assertNotIn(forbidden, text)

    def test_rows_are_data_driven_and_run_the_archive_binaries(self):
        rows = self.evidence_jobs["rows"]
        self.assertIn("matrix: ${{ fromJSON(needs.plan.outputs.rows) }}", rows)
        self.assertIn("pattern: ${{ matrix.daemon_pattern }}", rows)
        self.assertIn("    timeout-minutes: ${{ matrix.timeout_minutes }}\n", rows)
        self.assertIn("ROW_LIMIT_SECONDS: ${{ matrix.row_limit_seconds }}", rows)
        self.assertIn('XTASK="$PWD/target/debug/xtask" scripts/release-workflow/run-rows', rows)
        for runtime in ("pi", "codex", "claude"):
            self.assertNotRegex(self.evidence + self.build, rf"\b{runtime}\b(?!-)", runtime)
        script = (SCRIPTS / "run-row").read_text()
        self.assertIn("tar -xzf", script)
        self.assertIn("compat attest", script)
        self.assertIn("compat stage-upstream", script)
        self.assertNotIn("cargo build", script)

    def step(self, block: str, name: str) -> str:
        start = block.index(f"      - name: {name}\n")
        end = block.find("\n      - ", start)
        return block[start:] if end < 0 else block[start : end + 1]

    def test_rows_upload_their_reports_and_results_whatever_the_result(self):
        rows = self.evidence_jobs["rows"]
        run_step = rows.index("      - name: Run the rows and write their attestations\n")
        self.assertIn("          OUT_DIR: ${{ runner.temp }}/rows\n", rows)
        for name, artifact, path in (
            ("Upload the row test reports", "matrix-junit-${{ matrix.os }}", "junit-*.xml"),
            ("Upload the row results", "row-results-${{ matrix.os }}", "row-results.jsonl"),
        ):
            with self.subTest(step=name):
                upload = self.step(rows, name)
                self.assertLess(run_step, rows.index(upload))
                self.assertIn("        if: ${{ always() && matrix.rows != '' }}\n", upload)
                self.assertIn("uses: actions/upload-artifact@", upload)
                self.assertIn(f"          name: {artifact}\n", upload)
                self.assertIn(f"          path: ${{{{ runner.temp }}}}/rows/{path}\n", upload)
        self.assertIn("junit=$OUT_DIR/junit-$RUNTIME-$TARGET.xml", (SCRIPTS / "run-row").read_text())
        # Reports and results are no release input: no release download
        # pattern may pick them up.
        for pattern in re.findall(r"(?m)^\s+pattern: (\S+)$", self.release + self.build + self.evidence + self.ci):
            with self.subTest(pattern=pattern):
                self.assertFalse(fnmatch.fnmatchcase("matrix-junit-linux", pattern))
                if pattern != "row-results-*":
                    self.assertFalse(fnmatch.fnmatchcase("row-results-linux", pattern))

    def test_rows_upload_only_passing_attestations_under_the_attestation_pattern(self):
        rows = self.evidence_jobs["rows"]
        upload = self.step(rows, "Upload the attestations")
        self.assertIn("        if: ${{ matrix.rows != '' }}\n", upload)
        self.assertIn("          name: attestation-${{ matrix.os }}\n", upload)
        self.assertIn("          path: ${{ runner.temp }}/rows/attestation-*.json\n", upload)
        self.assertIn("          if-no-files-found: error\n", upload)
        for os_name in ("linux", "macos"):
            self.assertTrue(fnmatch.fnmatchcase(f"attestation-{os_name}", "attestation-*"))
        self.assertIn('(out_dir / f"attestation-{runtime}-{target}.json").unlink(missing_ok=True)', (SCRIPTS / "run-rows").read_text())

    def test_every_upload_of_the_row_job_survives_a_rerun_of_the_job(self):
        # "Re-run failed jobs" uploads again under the same names.
        rows = self.evidence_jobs["rows"]
        uploads = rows.count("uses: actions/upload-artifact@")
        self.assertEqual(uploads, 4)
        self.assertEqual(rows.count("          overwrite: true\n"), uploads)

    def test_the_consumer_is_uploaded_before_any_upstream_runs(self):
        rows = self.evidence_jobs["rows"]
        upload = rows.index("      - name: Upload the consumer executable\n")
        self.assertLess(rows.index("      - name: Build the consumer executable\n"), upload)
        for later in (
            "      - name: Download the daemon archives of the runner\n",
            "      - name: Install Node\n",
            "      - name: Run the rows and write their attestations\n",
        ):
            with self.subTest(step=later):
                self.assertLess(upload, rows.index(later))
        self.assertIn("          name: release-consumer-${{ runner.os }}\n", self.step(rows, "Upload the consumer executable"))
        self.assertNotIn("matrix.rows", self.step(rows, "Upload the consumer executable"))

    def test_the_row_guards_name_their_scopes_like_the_test_support_crate(self):
        script = (SCRIPTS / "run-rows").read_text()
        env = (ROOT / "crates" / "test-support" / "src" / "env.rs").read_text()
        lib = (ROOT / "crates" / "test-support" / "src" / "lib.rs").read_text()
        prefix = re.search(r'const ROOT_PREFIX: &str = "([^"]+)";', env).group(1)
        base = re.search(r'const MACOS_TEMP_ROOT: &str = "([^"]+)";', lib).group(1)
        self.assertIn(f'TEST_ROOT_PREFIX = "{prefix}"', script)
        self.assertIn(f'MACOS_TEMP_ROOT = "{base}"', script)
        # The row root is the base of the consumer's TestEnv roots. The
        # deepest socket below it, raw or resolved (/tmp is /private/tmp on
        # macOS), must fit the budget test-support checks with bytes to spare.
        template = re.search(r'ROW_ROOT_TEMPLATE = "([^"]+)"', script).group(1)
        random_bytes = int(re.search(r"const ROOT_RANDOM_BYTES: usize = (\d+);", env).group(1))
        suffix = re.search(r'const NESTED_SOCKET_SUFFIX: &str =\s*"([^"]+)";', env).group(1)
        capacity = int(re.search(r"pub const STRICTEST_SOCKET_PATH_CAPACITY: usize = (\d+);", env).group(1))
        for base in (template, "/private" + template):
            with self.subTest(base=base):
                deepest = len(base) + 1 + len(prefix) + random_bytes + len(suffix)
                self.assertLessEqual(deepest + 1, capacity - 3)
        self.assertIn('POHUNEK_TEST_TEMP_ROOT="$work"', (SCRIPTS / "run-row").read_text())
        # C2: the keychain is read without -d, which would print secret data.
        self.assertIn('["security", "dump-keychain", login.strip().strip(\'"\')]', script)
        self.assertEqual(script.count('"dump-keychain"'), 1)
        self.assertNotRegex(script, r'"dump-keychain"[^\]]*"-d"')

    def test_rows_and_smoke_run_on_the_node_the_locks_require(self):
        for name, script in (("rows", "run-rows"), ("smoke", "run-smoke")):
            block = self.evidence_jobs[name]
            with self.subTest(job=name):
                self.assertIn("node-version: ${{ needs.plan.outputs.node }}", block)
                self.assertLess(block.index("actions/setup-node@"), block.index(f"scripts/release-workflow/{script}"))
        self.assertIn("scripts/release-workflow/node-version compat/matrix.json", self.evidence_jobs["plan"])

    def test_the_smoke_probes_isolation_before_running(self):
        smoke = self.evidence_jobs["smoke"]
        self.assertLess(
            smoke.index("scripts/release-workflow/probe-isolation"),
            smoke.index("scripts/release-workflow/run-smoke"),
        )
        self.assertIn("ISOLATION: ${{ steps.isolation.outputs.strategy }}", smoke)
        run_smoke_text = (SCRIPTS / "run-smoke").read_text()
        self.assertIn("--isolation", run_smoke_text)
        # On macOS, run-smoke stages under a root-owned base and hands it to the throwaway user
        self.assertIn('[ "$(uname -s)" = Darwin ]', run_smoke_text)
        self.assertIn("/usr/bin/sudo -n /usr/bin/mktemp -d /private/var/pks-stage.XXXXXX", run_smoke_text)
        self.assertIn("trap 'cleanup_base' EXIT INT TERM", run_smoke_text)
        self.assertIn("/usr/bin/sudo -n /usr/sbin/chown", run_smoke_text)
        self.assertNotIn("chmod -R go+rX", run_smoke_text)
        # Smoke is a matrix job with ubuntu-latest and macos-15
        self.assertIn("    strategy:\n      fail-fast: false\n      matrix:\n        include:", smoke)
        self.assertIn("          - os: ubuntu-latest\n            smoke_target: \"\"", smoke)
        self.assertIn("          - os: macos-15\n            smoke_target: aarch64-apple-darwin", smoke)
        self.assertIn("SMOKE_TARGET: ${{ matrix.smoke_target }}", smoke)

    def test_smoke_runs_archive_scenarios_on_macos(self):
        # D1/D2 of #1259: the synthetic-archive scenarios are their own job in
        # their own reusable workflow, parallel to the real-archive smoke and
        # independent of the bundle. The scenario job still runs the isolated
        # scenarios fail-closed, and the bundle smoke no longer runs them.
        scenarios = self.scenario_jobs["smoke-scenarios"]
        self.assertIn("    runs-on: macos-15\n", scenarios)
        self.assertIn("python3 -m unittest -v scripts.tests.test_smoke_archive", scenarios)
        self.assertIn('POHUNEK_SMOKE_TESTS_REQUIRE_ISOLATED_RUN: "1"', scenarios)
        scenario_step = scenarios.split("      - name: Run the archive smoke scenarios on macOS\n", 1)[1]
        self.assertIn("timeout-minutes: 20", scenario_step)
        self.assertNotIn("test_smoke_archive", self.evidence_jobs["smoke"])
        # A release runs the scenarios from this workflow; a rehearsal runs
        # them in ci.yml, which passes the result for the verdict to require.
        caller = self.evidence_jobs["smoke-scenarios"]
        self.assertIn("    if: ${{ inputs.mode == 'release' }}\n", caller)
        self.assertIn("uses: ./.github/workflows/release-smoke-scenarios.yml", caller)
        self.assertIn('SCENARIOS: ${{ inputs.scenarios_result }}', self.evidence_jobs["verdict"])

    def verdict_mode_script(self):
        for script in run_scripts(self.evidence_jobs["verdict"]):
            if any("jq -e --arg mode" in line for line in script):
                return "\n".join(script)
        self.fail("the verdict has no jobs-of-the-mode check")

    def run_verdict_mode_script(self, results, mode, scenarios=""):
        needs = json.dumps({name: {"result": result} for name, result in results.items()})
        return subprocess.run(
            ["bash", "-c", self.verdict_mode_script()],
            env={**os.environ, "NEEDS": needs, "MODE": mode, "SCENARIOS": scenarios},
            capture_output=True,
            text=True,
        )

    def test_the_verdict_fails_a_phase_that_failed_cancelled_or_was_skipped(self):
        success = {name: "success" for name in ("plan", "rows", "assemble-rehearsal", "smoke")}
        skipped = {name: "skipped" for name in ("attest", "assemble", "attest-bundle", "smoke-scenarios")}
        passing_rehearsal = {**success, **skipped}
        with self.subTest(case="a passing rehearsal"):
            self.assertEqual(self.run_verdict_mode_script(passing_rehearsal, "rehearsal", "success").returncode, 0)
        # Any result other than success of a required phase fails, whatever
        # the phase: the scenarios and the bundle smoke are both required.
        for result in ("failure", "cancelled", "skipped"):
            for phase in ("rows", "smoke"):
                with self.subTest(case=f"rehearsal {phase} {result}"):
                    verdict = self.run_verdict_mode_script({**passing_rehearsal, phase: result}, "rehearsal", "success")
                    self.assertNotEqual(verdict.returncode, 0, verdict.stdout)
        # The external scenario result is required in a rehearsal: a caller
        # that skipped, cancelled or failed it, or never passed it, fails.
        for scenarios in ("failure", "cancelled", "skipped", ""):
            with self.subTest(case=f"external scenarios {scenarios or 'unset'}"):
                verdict = self.run_verdict_mode_script(passing_rehearsal, "rehearsal", scenarios)
                self.assertNotEqual(verdict.returncode, 0, verdict.stdout)

    def test_the_verdict_fails_a_release_phase_that_did_not_pass(self):
        release_success = {name: "success" for name in ("plan", "rows", "attest", "assemble", "attest-bundle", "smoke", "smoke-scenarios")}
        release_success["assemble-rehearsal"] = "skipped"
        with self.subTest(case="a passing release"):
            self.assertEqual(self.run_verdict_mode_script(release_success, "release").returncode, 0)
        # In a release the scenarios run here, so a skipped scenario job fails
        # the verdict even though the caller passed no external result.
        for result in ("failure", "cancelled", "skipped"):
            with self.subTest(case=f"release smoke-scenarios {result}"):
                verdict = self.run_verdict_mode_script({**release_success, "smoke-scenarios": result}, "release")
                self.assertNotEqual(verdict.returncode, 0, verdict.stdout)

    def test_the_consumer_test_name_matches_the_smoke(self):
        pattern = r"consumer_test=(\S+)"
        smoke = re.search(pattern, (ROOT / "packaging" / "smoke-archive").read_text()).group(1)
        row = re.search(pattern, (SCRIPTS / "run-row").read_text()).group(1)
        self.assertEqual(row, smoke)


class PinningTests(ReleaseWorkflowFiles):
    def test_every_action_is_pinned_to_a_commit_or_local(self):
        for file, text in self.texts.items():
            for action in all_uses(text):
                with self.subTest(file=file, action=action):
                    if LOCAL_ACTION.match(action):
                        nested = (ROOT / action / "action.yml").read_text()
                        for inner in all_uses(nested):
                            self.assertRegex(inner, PINNED)
                    elif action.startswith("./"):
                        self.assertRegex(action, r"^\./\.github/workflows/[a-z-]+\.yml$")
                    else:
                        self.assertRegex(action, PINNED)

    def test_the_trigger_is_a_tag_push_only(self):
        header = self.release.partition("\njobs:\n")[0]
        self.assertIn('on:\n  push:\n    tags:\n      - "v[0-9]+.[0-9]+.[0-9]+"\n', header)
        self.assertNotIn("pull_request", header)
        self.assertNotIn("workflow_dispatch", header)


class AssetNameTests(ReleaseWorkflowFiles):
    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        cls.policy = json.loads(POLICY.read_text())

    def matrix_entries(self, block: str) -> set[tuple[str, str]]:
        entries = set()
        for entry in re.split(r"^\s+- target: ", block, flags=re.M)[1:]:
            target = entry.split("\n", 1)[0].strip()
            components = re.search(r"^\s+components: (.+)$", entry, re.M).group(1).split()
            entries.update((component, target) for component in components)
        return entries

    def test_the_linux_matrix_equals_the_policy_archives(self):
        linux = {(slot["component"], slot["target"]) for slot in self.policy["archives"] if "-linux-" in slot["target"]}
        binaries = self.build_jobs["binaries"]
        self.assertEqual(self.matrix_entries(binaries), linux)
        # One upload per component, under the name the evidence workflow reads.
        for component in {component for component, _target in linux}:
            with self.subTest(component=component):
                upload = binaries.split(f"- name: Upload the {component} archive\n", 1)[1].split("\n\n", 1)[0]
                self.assertIn(f"if: ${{{{ contains(matrix.components, '{component}') }}}}", upload)
                self.assertIn(f"name: build-{component}-${{{{ matrix.target }}}}\n", upload)
                self.assertIn(f"dist/pohunek-{component}-${{{{ needs.meta.outputs.version }}}}-${{{{ matrix.target }}}}.tar.gz\n", upload)
                self.assertIn("overwrite: true", upload)

    def test_every_upload_of_the_release_build_survives_a_rerun_of_its_job(self):
        # A job that fails after an upload is rerun with the same artifact
        # names, which upload-artifact v4 refuses unless it may overwrite.
        for name, block in self.build_jobs.items():
            with self.subTest(job=name):
                uploads = block.split("uses: actions/upload-artifact@")[1:]
                for upload in uploads:
                    self.assertIn("overwrite: true", upload.split("\n\n", 1)[0])

    def test_each_linux_component_is_packaged_before_the_next_one_builds(self):
        # The daemon build relinks `pohunek` in the shared target dir, so the
        # cli archive must be staged from the cli build alone.
        step = self.build_jobs["binaries"].split("- name: Build and package each component\n", 1)[1].split("\n\n", 1)[0]
        self.assertEqual(step.count("for component in $COMPONENTS; do"), 1)
        body = step.split("for component in $COMPONENTS; do", 1)[1].split("\n          done\n", 1)[0]
        self.assertLess(body.index("cargo build"), body.index("packaging/stage-archive"))
        self.assertLess(body.index("packaging/stage-archive"), body.index("sh packaging/archive"))
        self.assertIn('if [ "$name" != "pohunek-$component-$VERSION-$TARGET" ]; then', body)

    def test_the_rehearsal_builds_each_macos_component_as_stage_macos_does(self):
        darwin = [slot["component"] for slot in self.policy["archives"] if slot["target"].endswith("-apple-darwin")]
        stage = self.release_jobs["stage-macos"]
        rehearsal = self.build_jobs["rehearsal-macos"]
        release_build = stage.split("- name: Build release binaries\n", 1)[1].split("\n\n", 1)[0]
        release_run = release_build.split("run: |\n", 1)[1]
        self.assertIn('packaging/macos/build-release "${{ matrix.component }}"', release_run)
        # Both jobs build with the same environment, and nothing in the
        # rehearsal job changes the target dir, the flags or the lock flag.
        self.assertIn("    env:\n      RUSTFLAGS: -D warnings\n", stage)
        self.assertIn("    env:\n      RUSTFLAGS: -D warnings\n", rehearsal)
        self.assertNotRegex(rehearsal, r"CARGO_TARGET_DIR|CARGO_LOCKED_FLAG|CARGO_ENCODED_RUSTFLAGS")
        self.assertEqual(rehearsal.count("RUSTFLAGS"), 1)
        positions = []
        for component in sorted(darwin):
            with self.subTest(component=component):
                build = rehearsal.split(f"- name: Build {component} release binaries\n", 1)[1].split("\n\n", 1)[0]
                self.assertIn(f"env:\n          COMPONENT: {component}\n", build)
                run = build.split("run: |\n", 1)[1]
                self.assertEqual(run, release_run.replace('"${{ matrix.component }}"', '"$COMPONENT"'))
                self.assertEqual(rehearsal.count("packaging/macos/build-release"), len(darwin))
                positions.append(
                    (
                        rehearsal.index(f"- name: Build {component} release binaries\n"),
                        rehearsal.index(f"- name: Stage and package the {component} archive\n"),
                    )
                )
        # Each component is staged before the next one builds.
        for (_build, staged), (next_build, _next_staged) in zip(positions, positions[1:]):
            self.assertLess(staged, next_build)
        for build, staged in positions:
            self.assertLess(build, staged)

    def test_the_macos_jobs_equal_the_policy_archives(self):
        darwin = {slot["component"] for slot in self.policy["archives"] if slot["target"].endswith("-apple-darwin")}
        targets = {slot["target"] for slot in self.policy["archives"] if slot["target"].endswith("-apple-darwin")}
        self.assertEqual(targets, {"aarch64-apple-darwin"})
        for name in ("stage-macos", "package-macos", "verify-macos"):
            block = self.release_jobs[name]
            listed = re.search(r"(?m)^        component: \[([^\]]*)\]$", block).group(1)
            self.assertEqual({item.strip() for item in listed.split(",")}, darwin, name)
            self.assertIn("aarch64-apple-darwin", block)
        self.assertIn("name: macos-signed-${{ matrix.component }}", self.release_jobs["package-macos"])

    def test_the_sdk_packages_equal_the_policy(self):
        pack = (ROOT / "sdk" / "ts" / "scripts" / "pack-release.ts").read_text()
        directories = re.search(r"PACKAGE_DIRECTORIES = \[([^\]]*)\]", pack).group(1)
        self.assertEqual(set(re.findall(r'"([a-z]+)"', directories)), set(self.policy["sdk_packages"]))
        packages = self.build_jobs["packages"]
        self.assertIn("name: sdk-release-assets", packages)
        self.assertNotIn("sdk", self.build_jobs)
        # npm dependency code runs only after the package archives and their
        # evidence have left the job.
        install = packages.index("bun install --frozen-lockfile")
        for upload in ("name: package-archives\n", "name: package-evidence\n"):
            self.assertLess(packages.index(upload), install, upload)

    def test_package_and_attestation_names_match_the_assembler(self):
        policy_source = (ROOT / "crates" / "xtask" / "src" / "release_policy.rs").read_text()
        self.assertIn('format!("pohunek-runtime-{runtime}-{version}{PACKAGE_SUFFIX}")', policy_source)
        self.assertIn('format!("attestation-{runtime}-{target}{ATTESTATION_SUFFIX}")', policy_source)
        packages = (SCRIPTS / "build-packages").read_text()
        self.assertIn('archive="pohunek-runtime-$runtime-$version.tar.zst"', packages)
        self.assertIn("runtime-packages/*/", packages)
        row = (SCRIPTS / "run-row").read_text()
        self.assertIn('attestation-$RUNTIME-$TARGET.json', row)
        self.assertIn("pohunek-runtime-$RUNTIME-$VERSION.tar.zst", row)

    def test_artifact_names_agree_between_producers_and_consumers(self):
        produced = {
            "build-": self.build,
            "package-archives": self.build,
            "sdk-release-assets": self.build,
            "macos-signed-": self.release,
            "attestation-": self.evidence,
            "release-consumer-": self.evidence,
            "release-bundle": self.evidence,
        }
        consumed = "\n".join(
            block
            for text in (self.release, self.evidence)
            for block in jobs(text).values()
            if "download-artifact" in block
        )
        for name, producer in produced.items():
            with self.subTest(artifact=name):
                self.assertRegex(producer, rf"(?m)^\s+name: {re.escape(name)}")
                self.assertIn(name, consumed)
        # `release-bundle` must stay outside every `release-*` download pattern.
        self.assertNotRegex(self.release + self.evidence + self.build, r"pattern: release-")
        # Consumer artifacts must include the OS suffix; smoke runs on ubuntu and
        # macos and must download release-consumer-${{ runner.os }} specifically.
        smoke = self.evidence_jobs["smoke"]
        self.assertIn("name: release-consumer-${{ runner.os }}", smoke)

    def test_the_package_evidence_is_uploaded_and_never_consumed(self):
        # Builder evidence is in-house JSON outside the release policy: it is
        # a workflow artifact of the package job, kept apart from the archives
        # the assembler reads, and no release job downloads it.
        packages = self.build_jobs["packages"]
        self.assertIn(
            'scripts/release-workflow/build-packages \\\n'
            '            "$VERSION" "$RUNNER_TEMP/package-archives" "$RUNNER_TEMP/package-evidence"\n',
            packages,
        )
        self.assertIn("          name: package-evidence\n", packages)
        self.assertIn("          path: ${{ runner.temp }}/package-evidence\n", packages)
        for text in (self.release, self.evidence):
            for name, block in jobs(text).items():
                with self.subTest(job=name):
                    self.assertNotIn("package-evidence", block)
                    for pattern in re.findall(r"(?m)^\s+pattern: (\S+)$", block):
                        self.assertFalse(fnmatch.fnmatchcase("package-evidence", pattern), pattern)

    def test_the_bundle_is_the_only_published_set(self):
        publish = self.release_jobs[WRITER]
        self.assertEqual(publish.count("download-artifact"), 2)
        self.assertIn("name: release-bundle", publish)
        self.assertIn("name: release-notes", publish)
        for name in ("sdk-release-assets", "macos-signed", "package-archives"):
            self.assertNotIn(name, publish)


class GuardRejectionTests(unittest.TestCase):
    """The guard itself rejects the shapes it exists to stop."""

    SCRIPT = (
        "          set -euo pipefail\n"
        '          cd "$RUNNER_TEMP/bundle"\n'
        "          sha256sum -c release-inventory.sha256"
    )
    ALLOWED = {("set -euo pipefail", 'cd "$RUNNER_TEMP/bundle"', "sha256sum -c release-inventory.sha256")}
    TEXT = """name: Release
permissions: {{}}
jobs:
  publish:
    name: Publish
    permissions:
      contents: write
    steps:
      - name: Download
        uses: {action}
      - name: Check
        shell: bash
        run: |
{script}
"""

    def violations(self, action: str = DOWNLOAD, script: str | None = None, **replace: str) -> list[str]:
        text = self.TEXT.format(action=action, script=script or self.SCRIPT)
        for old, new in replace.items():
            text = text.replace(old.replace("__", " "), new)
        return workflow_violations(text, writer="publish", oidc=set(), publish_scripts=self.ALLOWED)

    def test_the_allowlisted_writer_passes(self):
        self.assertEqual(self.violations(), [])

    def test_a_command_substitution_in_an_allowed_command_is_rejected(self):
        self.assertTrue(self.violations(script=self.SCRIPT.replace("sha256sum -c release-inventory.sha256", 'sha256sum -c "$(./payload)"')))

    def test_a_pipe_into_a_shell_or_a_redirect_is_rejected(self):
        for extra in ("sha256sum -c x.sha256 | sh", "ls > ~/.bashrc"):
            with self.subTest(extra=extra):
                self.assertTrue(self.violations(script=self.SCRIPT + "\n          " + extra))

    def test_an_unlisted_action_is_rejected(self):
        for action in (
            "evil/payload@" + "0" * 40,
            "actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683",
            "softprops/action-gh-release@3bb12739c298aeb8a4eeaf626c5b8d85266b0e65",
        ):
            with self.subTest(action=action):
                self.assertTrue(self.violations(action=action))

    def test_a_second_writer_is_rejected(self):
        text = self.TEXT.format(action=DOWNLOAD, script=self.SCRIPT) + (
            "  other:\n    permissions:\n      contents: write\n    steps:\n      - run: echo\n"
        )
        self.assertTrue(workflow_violations(text, writer="publish", oidc=set(), publish_scripts=self.ALLOWED))

    def test_whitespace_before_a_colon_is_rejected(self):
        base = self.TEXT.format(action=DOWNLOAD, script=self.SCRIPT)
        hidden_run = base.replace("        run: |", "        run : |\n          ./untrusted-payload\n        x: |")
        hidden_uses = base.replace("        uses: " + DOWNLOAD, "        uses : ./untrusted-action")
        for text in (hidden_run, hidden_uses):
            with self.subTest(text=text):
                self.assertNotEqual(text, base)
                self.assertTrue(workflow_violations(text, writer="publish", oidc=set(), publish_scripts=self.ALLOWED))

    def test_quoted_keys_flow_mappings_and_aliases_are_rejected(self):
        base = self.TEXT.format(action=DOWNLOAD, script=self.SCRIPT)
        for extra in (
            '      - "run": ./untrusted-payload',
            "      - {run: ./untrusted-payload}",
            "      - <<: *untrusted",
            "      - name: &anchor x",
        ):
            with self.subTest(extra=extra):
                self.assertTrue(workflow_violations(base + extra + "\n", writer="publish", oidc=set(), publish_scripts=self.ALLOWED))

    def test_a_non_canonical_write_grant_is_rejected(self):
        for grant in (
            "    permissions: write-all\n",
            "    permissions:\n      contents : write\n",
            "    permissions: {contents: write}\n",
            "    permissions:\n      contents: write\n      issues: write\n",
        ):
            text = self.TEXT.format(action=DOWNLOAD, script=self.SCRIPT).replace(
                "    permissions:\n      contents: write\n", grant
            )
            with self.subTest(grant=grant):
                self.assertTrue(workflow_violations(text, writer="publish", oidc=set(), publish_scripts=self.ALLOWED))

    def test_a_workflow_default_other_than_empty_is_rejected(self):
        for default in ("permissions:\n  contents: read\n", "permissions:\n  contents: read\n  id-token: write\n"):
            text = self.TEXT.format(action=DOWNLOAD, script=self.SCRIPT).replace("permissions: {}\n", default, 1)
            with self.subTest(default=default):
                self.assertTrue(workflow_violations(text, writer="publish", oidc=set(), publish_scripts=self.ALLOWED))

    def test_a_flow_style_job_is_rejected(self):
        base = self.TEXT.format(action=DOWNLOAD, script=self.SCRIPT)
        hidden = "  hidden: {runs-on: ubuntu-latest, permissions: write-all, steps: [{run: ./x}]}\n"
        for text in (base.replace("jobs:\n", "jobs:\n" + hidden), base + hidden):
            with self.subTest(text=text):
                self.assertTrue(workflow_violations(text, writer="publish", oidc=set(), publish_scripts=self.ALLOWED))


class OidcGuardRejectionTests(unittest.TestCase):
    """The OIDC scopes stay on the attest jobs, in their allowlisted shape."""

    TEXT = """name: Evidence
permissions: {}
jobs:
  build:
    permissions:
      contents: read
    steps:
      - run: cargo build
  attest:
    if: ${{ inputs.mode == 'release' }}
    permissions:
      contents: read
      id-token: write
      attestations: write
    steps:
      - name: Download
        uses: actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093
        with:
          pattern: build-*
          merge-multiple: true
          path: ${{ runner.temp }}/attest-assets
      - name: Check
        shell: bash
        run: |
          set -euo pipefail
          cd "$RUNNER_TEMP/attest-assets"
          sha256sum -c ./*.sha256
          archives="$(find . -maxdepth 1 -type f \\( -name '*.tar.gz' -o -name '*.tgz' -o -name '*.tar.zst' \\) | wc -l)"
          checksums="$(find . -maxdepth 1 -type f -name '*.sha256' | wc -l)"
          test "$archives" = "$checksums"
      - name: Attest
        uses: actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6 # v4.2.2
        with:
          subject-path: |
            ${{ runner.temp }}/attest-assets/*.tar.gz
"""

    def violations(self, text: str) -> list[str]:
        return workflow_violations(text, writer=None, oidc={"attest"})

    def test_the_allowlisted_attest_job_passes(self):
        self.assertEqual(self.violations(self.TEXT), [])

    def test_an_oidc_scope_on_another_job_is_rejected(self):
        for grant in (
            "      contents: read\n      id-token: write\n",
            "      contents: write\n      id-token: write\n",
            "      id-token: write\n",
            "      contents: read\n      attestations: write\n",
        ):
            text = self.TEXT.replace(
                "  build:\n    permissions:\n      contents: read\n",
                "  build:\n    permissions:\n" + grant,
                1,
            )
            with self.subTest(grant=grant):
                self.assertNotEqual(text, self.TEXT)
                self.assertTrue(self.violations(text))

    def test_a_flow_style_oidc_grant_is_rejected(self):
        text = self.TEXT.replace(
            "  build:\n    permissions:\n      contents: read\n",
            "  build:\n    permissions: {id-token: write}\n",
            1,
        )
        self.assertTrue(self.violations(text))

    def test_an_oidc_scope_in_the_workflow_default_is_rejected(self):
        text = self.TEXT.replace("permissions: {}\n", "permissions:\n  id-token: write\n", 1)
        self.assertTrue(self.violations(text))

    def test_an_attest_job_with_an_extra_or_missing_scope_is_rejected(self):
        for old, new in (
            ("      attestations: write\n", "      attestations: write\n      contents: write\n"),
            ("      attestations: write\n", "      attestations: write\n      packages: write\n"),
            ("      attestations: write\n", ""),
        ):
            with self.subTest(new=new):
                self.assertTrue(self.violations(self.TEXT.replace(old, new, 1)))

    def test_an_attest_job_that_checks_out_or_runs_downloaded_code_is_rejected(self):
        checkout = self.TEXT.replace(
            "    steps:\n      - name: Download\n",
            "    steps:\n      - uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683\n      - name: Download\n",
            1,
        )
        self.assertTrue(self.violations(checkout))
        for command in ("./attest-assets/pohunek --version", "tar -xzf ./*.tar.gz && ./pohunek/pohunek"):
            text = self.TEXT.replace(
                "          sha256sum -c ./*.sha256\n",
                "          sha256sum -c ./*.sha256\n          " + command + "\n",
                1,
            )
            with self.subTest(command=command):
                self.assertTrue(self.violations(text))

    def test_an_attest_job_with_an_unlisted_action_or_step_key_is_rejected(self):
        self.assertTrue(
            self.violations(
                self.TEXT.replace("actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6", "evil/payload@" + "0" * 40, 1)
            )
        )
        for extra in ("        env:\n          X: y\n", "        if: always()\n"):
            with self.subTest(extra=extra):
                self.assertTrue(self.violations(self.TEXT.replace("      - name: Check\n", "      - name: Check\n" + extra, 1)))


class LinkerSetupTests(unittest.TestCase):
    """A job whose RUSTFLAGS select mold installs mold on every matrix leg."""

    def test_mold_jobs_install_mold_unconditionally(self):
        for path in (RELEASE, BUILD, EVIDENCE):
            for name, block in jobs(path.read_text()).items():
                if not re.search(r"(?m)^      RUSTFLAGS: [^\n]*-fuse-ld=mold", block):
                    continue
                step = re.search(r"(?m)^      - name: Install mold linker\n((?:        [^\n]*\n)*)", block)
                with self.subTest(job=name):
                    self.assertIsNotNone(step, f"{name} uses mold without installing it")
                    self.assertNotRegex(step.group(1), r"(?m)^        if:", f"{name} installs mold conditionally")


class OfflineDependencySourcesTests(unittest.TestCase):
    """A job running `cargo test` fetches the locked sources before the tests.

    The xtask dependency-policy tests resolve the locked graph with
    `--offline`, which fails when a locked package source is missing from the
    cargo registry.
    """

    def test_cargo_test_jobs_fetch_locked_sources_first(self):
        for path in (RELEASE, BUILD, EVIDENCE):
            for name, block in jobs(path.read_text()).items():
                test_run = re.search(r"(?m)^\s+(?:- )?run: cargo test\b", block)
                if test_run is None:
                    continue
                fetch = re.search(r"(?m)^\s+(?:- )?run: cargo fetch --locked$", block)
                with self.subTest(job=name):
                    self.assertIsNotNone(fetch, f"{name} runs cargo test without `cargo fetch --locked`")
                    self.assertLess(fetch.start(), test_run.start(), f"{name} fetches after running cargo test")


class SccacheSetupTests(unittest.TestCase):
    """The Rust jobs reach sccache only through .github/actions/setup-sccache."""

    SETUP_STEP = (
        r"(?m)^      - name: Enable sccache\n"
        r"        uses: \./\.github/actions/setup-sccache\n"
        r"        with:\n"
        r"          version: \$\{\{ env\.SCCACHE_VERSION \}\}\n"
        r"          sha256: \$\{\{ env\.SCCACHE_SHA256 \}\}\n"
    )

    @classmethod
    def setUpClass(cls):
        cls.action = SCCACHE_ACTION.read_text()
        cls.texts = {
            name: (WORKFLOWS / name).read_text()
            for name in ("release.yml", "release-build.yml", "release-evidence.yml", "ci.yml")
        }

    def test_no_job_sets_the_wrapper_or_the_sccache_flag(self):
        for name, text in self.texts.items():
            header = text.partition("\njobs:\n")[0]
            with self.subTest(file=name):
                self.assertNotRegex(header, r"(?m)^  (RUSTC_WRAPPER|SCCACHE_GHA_ENABLED):")
                for job, block in jobs(text).items():
                    self.assertNotRegex(block, r"(?m)^\s+(RUSTC_WRAPPER|SCCACHE_GHA_ENABLED):", job)

    def test_sccache_is_set_up_only_through_the_composite(self):
        for name, text in self.texts.items():
            with self.subTest(file=name):
                self.assertNotIn("mozilla-actions/sccache-action", text)
        self.assertEqual(len(re.findall(self.SETUP_STEP, self.texts["release.yml"])), 3)
        self.assertEqual(len(re.findall(self.SETUP_STEP, self.texts["release-build.yml"])), 1)
        self.assertEqual(self.texts["release.yml"].count("- name: Enable sccache"), 3)
        self.assertEqual(self.texts["release-build.yml"].count("- name: Enable sccache"), 1)

    def test_the_composite_pins_the_action_and_passes_the_version_explicitly(self):
        steps = re.split(r"(?m)^    - ", self.action)
        action_steps = [step for step in steps if "uses: mozilla-actions/sccache-action@" in step]
        self.assertEqual(len(action_steps), 1)
        step = action_steps[0]
        self.assertRegex(step, r"uses: mozilla-actions/sccache-action@[0-9a-f]{40} # v0\.0\.11\n")
        self.assertIn("        version: v${{ inputs.version }}\n", step)
        self.assertIn("      if: ${{ steps.seed.outputs.ok == 'true' }}\n", step)

    def test_the_wrapper_is_exported_only_after_the_verified_binary_is_resolved(self):
        steps = re.split(r"(?m)^    - ", self.action)
        export = [step for step in steps if "RUSTC_WRAPPER=sccache" in step]
        self.assertEqual(len(export), 1)
        self.assertIn("      if: ${{ steps.seed.outputs.ok == 'true' }}\n", export[0])
        self.assertNotIn("continue-on-error", self.action)

    def test_the_pinned_release_agrees_between_the_two_workflows(self):
        pins = {}
        for name in ("release.yml", "release-build.yml"):
            header = self.texts[name].partition("\njobs:\n")[0]
            version = re.search(r'(?m)^  SCCACHE_VERSION: "([0-9.]+)"$', header)
            digest = re.search(r"(?m)^  SCCACHE_SHA256: ([0-9a-f]{64})$", header)
            self.assertIsNotNone(version, name)
            self.assertIsNotNone(digest, name)
            pins[name] = (version.group(1), digest.group(1))
        self.assertEqual(pins["release.yml"], pins["release-build.yml"])


if __name__ == "__main__":
    unittest.main()
