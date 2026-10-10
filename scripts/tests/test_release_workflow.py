"""Structural checks on the release workflows: one writer, no repository code in it.

The release is built by three workflow files: release.yml (the tag trigger,
the gate, the macOS jobs, and the single `publish` job), release-build.yml
(reusable producers) and release-evidence.yml (reusable evidence, assembly and
smoke). The checks pin the security model (issue #150, D2):

- exactly one job in the three files holds `contents: write`, the job `publish`,
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

import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"
RELEASE = WORKFLOWS / "release.yml"
BUILD = WORKFLOWS / "release-build.yml"
EVIDENCE = WORKFLOWS / "release-evidence.yml"
CI = WORKFLOWS / "ci.yml"
POLICY = ROOT / "packaging" / "release-policy.json"
SCRIPTS = ROOT / "scripts" / "release-workflow"

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
FULL_RUN_EVENTS = ("schedule", "push", "workflow_dispatch")
REHEARSAL_INPUTS = {
    ".github/workflows/ci.yml",
    ".github/workflows/release.yml",
    ".github/workflows/release-build.yml",
    ".github/workflows/release-evidence.yml",
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
    "gate",
    "docs-gate",
    "sdk-gate",
    "upgrade-test",
    "build",
    "stage-macos",
    "package-macos",
    "verify-macos",
    "evidence",
]

JOB_HEADER = re.compile(r"^  ([a-z][a-z0-9-]*):\n", re.M)
USES = re.compile(r"^\s+(?:- )?uses:\s*(\S+)", re.M)
RUN = re.compile(r"^(\s+)(?:- )?run:\s*(.*)$", re.M)
PINNED = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}$")

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
CANONICAL_PERMISSIONS = re.compile(
    "^(?:" + "|".join((READ_BLOCK, WRITE_BLOCK_FULL, EMPTY_BLOCK, OIDC_BLOCK)) + ")", re.M
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

EVIDENCE_CALL = "./.github/workflows/release-evidence.yml"
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
        'gh release create "$TAG" --draft --verify-tag --title "$TAG" --notes ""',
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
        if name == writer:
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
        cls.ci = CI.read_text()
        cls.release_jobs = jobs(cls.release)
        cls.build_jobs = jobs(cls.build)
        cls.evidence_jobs = jobs(cls.evidence)
        cls.texts = {"release.yml": cls.release, "release-build.yml": cls.build, "release-evidence.yml": cls.evidence}


class WriterTests(ReleaseWorkflowFiles):
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
        self.assertEqual(needs_of(self.release_jobs[RELEASE_CALLER]), ["build", "verify-macos"])
        self.assertIn("uses: ./.github/workflows/release-build.yml", self.release_jobs["build"])
        self.assertEqual(
            needs_of(self.release_jobs["build"]), ["gate", "docs-gate", "sdk-gate", "upgrade-test"]
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

    def test_the_ci_caller_runs_in_rehearsal_mode_and_never_for_forks(self):
        ci_jobs = jobs(self.ci)
        build = ci_jobs["release-rehearsal-build"]
        evidence = ci_jobs[CI_CALLER]
        for name, block in (("release-rehearsal-build", build), (CI_CALLER, evidence)):
            with self.subTest(job=name):
                condition = re.search(r"(?m)^    if: (.*)$", block)
                self.assertIsNotNone(condition)
                for event in FULL_RUN_EVENTS:
                    self.assertIn(f"github.event_name == '{event}'", condition.group(1))
                self.assertIn("needs.changes.outputs.release_rehearsal == 'true'", condition.group(1))
                self.assertIn("github.event.pull_request.head.repo.full_name == github.repository", block)
                self.assertIn("dependabot[bot]", block)
                self.assertIn("mode: rehearsal", block)
                self.assertNotIn("secrets", block)
                self.assertNotIn("environment", block)
        self.assertIn("uses: ./.github/workflows/release-build.yml", build)
        self.assertIn("uses: ./.github/workflows/release-evidence.yml", evidence)
        self.assertEqual(needs_of(evidence), ["changes", "release-rehearsal-build"])
        self.assertIn("needs.release-rehearsal-build.outputs.version", evidence)
        self.assertNotRegex(self.ci, r"(?m)^  pull_request_target:")

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
        self.assertIn("inputs.mode == 'rehearsal' && matrix.component == 'daemon'", build)
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
        for text, inputs in ((self.evidence, ["version", "commit", "mode", "targets"]), (self.build, ["mode", "version"])):
            on_block = text.split("\non:\n", 1)[1].split("\npermissions:", 1)[0]
            self.assertEqual(re.findall(r"(?m)^  ([a-z_]+):$", on_block), ["workflow_call"])
            self.assertEqual(re.findall(r"(?m)^      ([a-z]+):\n        description:", on_block), inputs)

    def test_every_job_declares_its_permissions(self):
        for file, text in self.texts.items():
            for name, block in jobs(text).items():
                with self.subTest(job=f"{file}:{name}"):
                    self.assertRegex(block, r"(?m)^    permissions:")

    def test_the_jobs_run_in_the_documented_order(self):
        needs = {name: needs_of(block) for name, block in self.evidence_jobs.items()}
        self.assertEqual(needs["plan"], [])
        self.assertEqual(needs["rows"], ["plan", "consumer"])
        self.assertEqual(needs["attest"], ["rows"])
        self.assertEqual(needs["assemble"], ["rows", "attest"])
        self.assertEqual(needs["assemble-rehearsal"], ["rows"])
        self.assertEqual(needs["smoke"], ["plan", "assemble", "assemble-rehearsal", "consumer"])

    def test_the_verdict_requires_every_other_job_of_the_mode(self):
        verdict = self.evidence_jobs["verdict"]
        self.assertEqual(
            sorted(needs_of(verdict)), sorted(name for name in self.evidence_jobs if name != "verdict")
        )
        self.assertIn("    if: ${{ always() }}\n", verdict)
        for name in ("plan", "consumer", "rows", "attest", "assemble", "assemble-rehearsal", "attest-bundle", "smoke"):
            self.assertIn(f'"{name}"', verdict)
        self.assertIn('"skipped"', verdict)

    def test_rows_are_data_driven_and_run_the_archive_binaries(self):
        rows = self.evidence_jobs["rows"]
        self.assertIn("matrix: ${{ fromJSON(needs.plan.outputs.rows) }}", rows)
        self.assertIn("build-daemon-${{ matrix.target }}", rows)
        for runtime in ("pi", "codex", "claude"):
            self.assertNotRegex(self.evidence + self.build, rf"\b{runtime}\b(?!-)", runtime)
        script = (SCRIPTS / "run-row").read_text()
        self.assertIn("tar -xzf", script)
        self.assertIn("compat attest", script)
        self.assertIn("compat stage-upstream", script)
        self.assertNotIn("cargo build", script)

    def test_rows_and_smoke_run_on_the_node_the_locks_require(self):
        for name in ("rows", "smoke"):
            block = self.evidence_jobs[name]
            with self.subTest(job=name):
                self.assertIn("node-version: ${{ needs.plan.outputs.node }}", block)
                self.assertLess(block.index("actions/setup-node@"), block.index("      - name: Build xtask"))
        self.assertIn("scripts/release-workflow/node-version compat/matrix.json", self.evidence_jobs["plan"])

    def test_the_smoke_probes_isolation_before_running(self):
        smoke = self.evidence_jobs["smoke"]
        self.assertLess(
            smoke.index("scripts/release-workflow/probe-isolation"),
            smoke.index("scripts/release-workflow/run-smoke"),
        )
        self.assertIn("ISOLATION: ${{ steps.isolation.outputs.strategy }}", smoke)
        self.assertIn("--isolation", (SCRIPTS / "run-smoke").read_text())

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
                    if action.startswith("./"):
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

    def matrix_entries(self, block: str) -> set[tuple[str, str, str]]:
        entries = set()
        for entry in re.split(r"^\s+- target: ", block, flags=re.M)[1:]:
            target = entry.split("\n", 1)[0].strip()
            component = re.search(r"^\s+component: (\S+)$", entry, re.M).group(1)
            prefix = re.search(r"^\s+archive_prefix: (\S+)$", entry, re.M).group(1)
            entries.add((component, target, prefix))
        return entries

    def test_the_linux_matrix_equals_the_policy_archives(self):
        linux = {
            (slot["component"], slot["target"], f"pohunek-{slot['component']}")
            for slot in self.policy["archives"]
            if "-linux-" in slot["target"]
        }
        self.assertEqual(self.matrix_entries(self.build_jobs["binaries"]), linux)

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
        self.assertIn("name: sdk-release-assets", self.build_jobs["sdk"])

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
            "release-consumer": self.evidence,
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

    def test_the_bundle_is_the_only_published_set(self):
        publish = self.release_jobs[WRITER]
        self.assertEqual(publish.count("download-artifact"), 1)
        self.assertIn("name: release-bundle", publish)
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


if __name__ == "__main__":
    unittest.main()
