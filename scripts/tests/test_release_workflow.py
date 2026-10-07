"""Structural checks on release.yml: write-token jobs run no repository code.

A job holding `contents: write` may only download an artifact, check it, and
attach it. The one job holding `id-token: write` and `attestations: write`
(`attest`) may only download artifacts, check their checksums, and attest them,
and no other job may hold either scope. The checks compare every action and every `run:` script of such a
job against exact allowlists, so any other step, any other pinned action, and
any edit that smuggles a command into an allowed script (a substitution, a
pipe into a shell, a redirect) fails the test until the allowlist is updated
on purpose. The workflow is parsed with the standard library only, like the
other script tests.
"""

from pathlib import Path
import re
import unittest

WORKFLOW = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "release.yml"

# The only actions a write-token job may use, pinned to exact commits.
ALLOWED_WRITE_ACTIONS = {
    "actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093",
    "softprops/action-gh-release@3bb12739c298aeb8a4eeaf626c5b8d85266b0e65",
}

# The only actions the attest job may use: the artifact download and the
# attestation itself, both pinned to exact commits.
ATTEST_JOB = "attest"
ATTEST_ALLOWED_ACTIONS = {
    "actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093",
    "actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6",
}

# The only `run:` scripts a write-token job may contain, as normalized lines
# (stripped, without blank and comment lines). They verify the downloaded
# checksums and count the archives; nothing in them executes downloaded data.
ALLOWED_WRITE_SCRIPTS = {
    (
        "set -euo pipefail",
        'cd "$RUNNER_TEMP/sdk-download"',
        "sha256sum -c ./*.tgz.sha256",
        "for package in protocol sdk testkit; do",
        'test "$(ls ./pohunek-ts-"$package"-*.tgz | wc -l)" = 1',
        "done",
    ),
    (
        "set -euo pipefail",
        'cd "$RUNNER_TEMP/release-download"',
        "sha256sum -c ./*.tar.gz.sha256",
        'test "$(ls ./*.tar.gz | wc -l)" = 1',
    ),
    (
        "set -euo pipefail",
        'cd "$RUNNER_TEMP/signed-download"',
        "shasum -a 256 -c ./*.tar.gz.sha256",
        "test \"$(ls ./*.tar.gz | wc -l | tr -d ' ')\" = 1",
    ),
}

# The only `run:` script of the attest job. It checks the downloaded checksums
# and that every asset has exactly one checksum file; nothing in it executes
# downloaded data.
ATTEST_ALLOWED_SCRIPTS = {
    (
        "set -euo pipefail",
        'cd "$RUNNER_TEMP/attest-assets"',
        "sha256sum -c ./*.sha256",
        'test "$(ls ./*.tar.gz ./*.tgz | wc -l)" = "$(ls ./*.sha256 | wc -l)"',
    ),
}

JOB_HEADER = re.compile(r"^  ([a-z][a-z0-9-]*):\n", re.M)
USES = re.compile(r"^\s+(?:- )?uses:\s*(\S+)", re.M)
RUN = re.compile(r"^(\s+)(?:- )?run:\s*(.*)$", re.M)

# The parser is lexical, so it fails closed: a token block is recognized only
# in its canonical spelling, and every line of a write-token job must match
# one of these shapes. Anything else (whitespace before a colon, quoted keys,
# flow mappings, anchors, aliases, merge keys) is reported, not skipped.
PERMISSIONS_LINE = re.compile(r"^\s*['\"]?permissions['\"]?\s*:", re.M)
CANONICAL_JOB_PERMISSIONS = re.compile(
    r"^    permissions:\n      contents: (read|write)\n(?!      \S)", re.M
)
# The attest job's token block: read access plus the two OIDC scopes, nothing
# else. It is valid for the `attest` job only.
ATTEST_JOB_PERMISSIONS = re.compile(
    r"^    permissions:\n      contents: read\n      id-token: write\n      attestations: write\n(?!      \S)",
    re.M,
)
OIDC_SCOPE = re.compile(r"^\s*['\"]?(id-token|attestations)['\"]?\s*:", re.M)
# A plain scalar value: its first character may not open a flow collection,
# an anchor, an alias, a tag, a block scalar, or a quoted string.
_SCALAR = r"[^\s{}\[\]&*!|>'\"%@`][^\n]*"
CANONICAL_WRITE_JOB_LINES = [
    re.compile(pattern)
    for pattern in (
        rf"    (name|needs|runs-on|timeout-minutes): {_SCALAR}",
        r"    needs: \[[a-z0-9, -]+\]",
        r"    (permissions|strategy|steps):",
        r"      contents: write",
        r"      (fail-fast): (true|false)",
        r"      matrix:",
        r"        include:",
        r"        component: \[[a-z, ]+\]",
        r"          - target: [a-z0-9_-]+",
        r"            component: [a-z]+",
        rf"      - name: {_SCALAR}",
        r"        uses: [a-z0-9_.-]+/[a-z0-9_.-]+@[0-9a-f]{40}( # v[0-9.]+)?",
        r"        with:",
        rf"          (name|path): {_SCALAR}",
        r"          fail_on_unmatched_files: true",
        r"          files: \|",
        r"            \$\{\{ runner\.temp \}\}/[a-z-]+/[A-Za-z0-9_.*-]+",
        r"        shell: bash",
        r"        run: \|",
        r"\s*#[^\n]*",
    )
]


ATTEST_JOB_LINES = [
    re.compile(pattern)
    for pattern in (
        rf"    (name|needs|runs-on|timeout-minutes): {_SCALAR}",
        r"    needs: \[[a-z0-9, -]+\]",
        r"    (permissions|steps):",
        r"      contents: read",
        r"      id-token: write",
        r"      attestations: write",
        rf"      - name: {_SCALAR}",
        r"        uses: [a-z0-9_.-]+/[a-z0-9_.-]+@[0-9a-f]{40}( # v[0-9.]+)?",
        r"        with:",
        rf"          (name|path|pattern): {_SCALAR}",
        r"          merge-multiple: true",
        r"          subject-path: \|",
        r"            \$\{\{ runner\.temp \}\}/[a-z-]+/[A-Za-z0-9_.*-]+",
        r"        shell: bash",
        r"        run: \|",
        r"\s*#[^\n]*",
    )
]


def jobs(text: str) -> dict[str, str]:
    """Top-level job name -> its block of the `jobs:` section."""
    section = text.split("\njobs:\n", 1)[1]
    headers = list(JOB_HEADER.finditer(section))
    return {
        match.group(1): section[match.end() : headers[i + 1].start() if i + 1 < len(headers) else len(section)]
        for i, match in enumerate(headers)
    }


def holds_write_token(block: str) -> bool:
    return re.search(r"^    permissions:\n      contents: write$", block, re.M) is not None


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


def permission_violations(name: str, block: str) -> list[str]:
    """A job's token grant must be absent or spelled canonically.

    The OIDC scopes belong to the attest job alone, in exactly one spelling.
    """
    granted = len(PERMISSIONS_LINE.findall(block))
    canonical = len(CANONICAL_JOB_PERMISSIONS.findall(block + "\n"))
    attest_grants = len(ATTEST_JOB_PERMISSIONS.findall(block + "\n"))
    violations = []
    if name == ATTEST_JOB:
        if attest_grants != 1 or granted != 1 or len(OIDC_SCOPE.findall(block)) != 2:
            violations.append(
                f"{name} must declare exactly `contents: read`, `id-token: write`, `attestations: write`"
            )
        return violations
    if granted != canonical:
        violations.append(f"{name} declares permissions in a form the guard does not recognize")
    if OIDC_SCOPE.search(block):
        violations.append(f"{name} mentions id-token or attestations, which only {ATTEST_JOB} may hold")
    return violations


def lines_outside_run_bodies(block: str) -> list[str]:
    """Job lines minus the bodies of `run: |` steps.

    Run bodies are compared whole against `ALLOWED_WRITE_SCRIPTS`; every other
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


def attest_job_violations(name: str, block: str) -> list[str]:
    """Everything the attest job does beyond downloading, checking, and attesting.

    Fails closed like the write-token check: every line outside run bodies must
    match a canonical shape, every action must be allowlisted (so no checkout),
    and every script must be the allowlisted checksum check.
    """
    violations = []
    for line in lines_outside_run_bodies(block):
        if line.strip() and not any(shape.fullmatch(line) for shape in ATTEST_JOB_LINES):
            violations.append(f"{name} has a line outside the canonical shapes: {line!r}")
    for action in USES.findall(block):
        if action not in ATTEST_ALLOWED_ACTIONS:
            violations.append(f"{name} uses {action} with an OIDC token")
    for script in run_scripts(block):
        if script not in ATTEST_ALLOWED_SCRIPTS:
            violations.append(f"{name} runs a script outside the allowlist: {script!r}")
    return violations


def write_job_violations(text: str) -> list[str]:
    """Everything a write-token job does beyond the allowlisted steps.

    Fails closed: a job whose permissions are not spelled canonically, and any
    line of a write-token job outside the canonical shapes, is a violation.
    """
    violations = []
    header, _, _ = text.partition("\njobs:\n")
    if len(PERMISSIONS_LINE.findall(header)) != 1 or not re.search(
        r"(?m)^permissions:\n  contents: read\n(?!  \S)", header + "\n"
    ):
        violations.append("the workflow default token is not exactly `contents: read`")
    if OIDC_SCOPE.search(header):
        violations.append("the workflow default token mentions id-token or attestations")
    violations.extend(jobs_section_violations(text))
    for name, block in jobs(text).items():
        violations.extend(permission_violations(name, block))
        if name == ATTEST_JOB:
            violations.extend(attest_job_violations(name, block))
            continue
        if not holds_write_token(block):
            continue
        for line in lines_outside_run_bodies(block):
            if line.strip() and not any(shape.fullmatch(line) for shape in CANONICAL_WRITE_JOB_LINES):
                violations.append(f"{name} has a line outside the canonical shapes: {line!r}")
        for action in USES.findall(block):
            if action not in ALLOWED_WRITE_ACTIONS:
                violations.append(f"{name} uses {action} with a write token")
        for script in run_scripts(block):
            if script not in ALLOWED_WRITE_SCRIPTS:
                violations.append(f"{name} runs a script outside the allowlist: {script!r}")
    return violations


def matrix_pairs(block: str) -> list[tuple[str, str]]:
    """(target, component) of every matrix `include` entry, in order."""
    pairs = []
    for entry in re.split(r"^\s+- target: ", block, flags=re.M)[1:]:
        target = entry.split("\n", 1)[0].strip()
        component = re.search(r"^\s+component: (\S+)$", entry, re.M)
        pairs.append((target, component.group(1) if component else ""))
    return pairs


class WriteTokenJobTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text()
        cls.jobs = jobs(cls.text)

    def test_write_token_jobs_exist(self):
        self.assertEqual(
            sorted(name for name, block in self.jobs.items() if holds_write_token(block)),
            ["publish", "publish-macos", "sdk-publish"],
        )

    def test_only_the_attest_job_holds_oidc_scopes(self):
        holders = sorted(
            name for name, block in self.jobs.items() if OIDC_SCOPE.search(block)
        )
        self.assertEqual(holders, [ATTEST_JOB])
        self.assertRegex(self.jobs[ATTEST_JOB], ATTEST_JOB_PERMISSIONS)
        self.assertFalse(holds_write_token(self.jobs[ATTEST_JOB]))

    def test_attest_job_waits_for_every_asset_producer(self):
        self.assertRegex(
            self.jobs[ATTEST_JOB], r"(?m)^    needs: \[build, verify-macos, sdk-pack\]$"
        )

    def test_attest_subjects_cover_every_archive_kind(self):
        attest = self.jobs[ATTEST_JOB]
        for glob in ("*.tar.gz", "*.tgz", "*.sha256"):
            self.assertIn(f"${{{{ runner.temp }}}}/attest-assets/{glob}", attest)

    def test_attest_subjects_cover_every_published_asset_pattern(self):
        attest = self.jobs[ATTEST_JOB]
        subjects = re.findall(
            r"^            \$\{\{ runner\.temp \}\}/attest-assets/(\S+)$", attest, re.M
        )
        self.assertTrue(subjects)
        kinds = (".sha256", ".tar.gz", ".tgz")
        for name in ("publish", "publish-macos", "sdk-publish"):
            published = re.search(
                r"(?m)^          files: \|\n((?:            \$\{\{ .*\n)+)", self.jobs[name]
            )
            self.assertIsNotNone(published, name)
            for pattern in published.group(1).splitlines():
                pattern = pattern.strip()
                kind = next((kind for kind in kinds if pattern.endswith(kind)), None)
                with self.subTest(job=name, pattern=pattern):
                    self.assertIsNotNone(kind, f"unknown asset kind: {pattern}")
                    self.assertTrue(
                        any(subject.endswith(kind) for subject in subjects),
                        f"no attest subject covers {kind}",
                    )

    def test_every_publish_job_waits_for_the_attestation(self):
        for name in ("publish", "publish-macos", "sdk-publish"):
            with self.subTest(job=name):
                needs = re.search(r"(?m)^    needs: \[([^\]]*)\]$", self.jobs[name])
                self.assertIsNotNone(needs)
                self.assertIn(ATTEST_JOB, [item.strip() for item in needs.group(1).split(",")])

    def test_publish_macos_waits_for_verification_and_attestation(self):
        self.assertRegex(self.jobs["publish-macos"], r"(?m)^    needs: \[verify-macos, attest\]$")

    def test_write_token_jobs_only_download_check_and_attach(self):
        self.assertEqual(write_job_violations(self.text), [])

    def test_build_job_cannot_write(self):
        build = self.jobs["build"]
        self.assertRegex(build, r"(?m)^    permissions:\n      contents: read$")
        self.assertNotIn("action-gh-release", build)

    def test_publish_job_mirrors_the_build_matrix(self):
        self.assertEqual(matrix_pairs(self.jobs["publish"]), matrix_pairs(self.jobs["build"]))
        self.assertRegex(self.jobs["publish"], r"(?m)^    needs: \[build, attest\]$")


class LinkerSetupTests(unittest.TestCase):
    """A job whose RUSTFLAGS select mold installs mold on every matrix leg."""

    def test_mold_jobs_install_mold_unconditionally(self):
        for name, block in jobs(WORKFLOW.read_text()).items():
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
        for name, block in jobs(WORKFLOW.read_text()).items():
            test_run = re.search(r"(?m)^\s+(?:- )?run: cargo test\b", block)
            if test_run is None:
                continue
            fetch = re.search(r"(?m)^\s+(?:- )?run: cargo fetch --locked$", block)
            with self.subTest(job=name):
                self.assertIsNotNone(fetch, f"{name} runs cargo test without `cargo fetch --locked`")
                self.assertLess(fetch.start(), test_run.start(), f"{name} fetches after running cargo test")


class WriteJobGuardRejectionTests(unittest.TestCase):
    """The guard itself rejects the shapes it exists to stop."""

    PUBLISH = """name: Release
permissions:
  contents: read
jobs:
  publish:
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
    DOWNLOAD = "actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093"
    ALLOWED = (
        "          set -euo pipefail\n"
        '          cd "$RUNNER_TEMP/release-download"\n'
        "          sha256sum -c ./*.tar.gz.sha256\n"
        '          test "$(ls ./*.tar.gz | wc -l)" = 1'
    )

    def violations(self, action: str, script: str) -> list[str]:
        return write_job_violations(self.PUBLISH.format(action=action, script=script))

    def test_the_allowlisted_job_passes(self):
        self.assertEqual(self.violations(self.DOWNLOAD, self.ALLOWED), [])

    def test_a_command_substitution_in_an_allowed_command_is_rejected(self):
        script = self.ALLOWED.replace('test "$(ls ./*.tar.gz | wc -l)"', 'test "$(./payload)"')
        self.assertTrue(self.violations(self.DOWNLOAD, script))

    def test_a_pipe_into_a_shell_is_rejected(self):
        script = self.ALLOWED + "\n          sha256sum -c x.sha256 | sh"
        self.assertTrue(self.violations(self.DOWNLOAD, script))

    def test_a_redirect_is_rejected(self):
        script = self.ALLOWED + "\n          ls > ~/.bashrc"
        self.assertTrue(self.violations(self.DOWNLOAD, script))

    def test_an_unlisted_pinned_action_is_rejected(self):
        for action in (
            "evil/payload@" + "0" * 40,
            "actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683",
        ):
            with self.subTest(action=action):
                self.assertTrue(self.violations(action, self.ALLOWED))

    def test_whitespace_before_a_colon_is_rejected(self):
        base = self.PUBLISH.format(action=self.DOWNLOAD, script=self.ALLOWED)
        hidden_run = base.replace(
            "        run: |", "        run : |\n          ./untrusted-payload\n        x: |"
        )
        hidden_uses = base.replace(
            "        uses: " + self.DOWNLOAD, "        uses : ./untrusted-action"
        )
        for text in (hidden_run, hidden_uses):
            with self.subTest(text=text):
                self.assertNotEqual(text, base)
                self.assertTrue(write_job_violations(text))

    def test_quoted_keys_flow_mappings_and_aliases_are_rejected(self):
        base = self.PUBLISH.format(action=self.DOWNLOAD, script=self.ALLOWED)
        for extra in (
            '      - "run": ./untrusted-payload',
            "      - {run: ./untrusted-payload}",
            "      - <<: *untrusted",
            "      - name: &anchor x",
        ):
            with self.subTest(extra=extra):
                self.assertTrue(write_job_violations(base + extra + "\n"))

    def test_a_non_canonical_write_grant_is_rejected(self):
        for grant in (
            "    permissions: write-all\n",
            "    permissions:\n      contents : write\n",
            "    permissions: {contents: write}\n",
        ):
            text = self.PUBLISH.format(action=self.DOWNLOAD, script=self.ALLOWED).replace(
                "    permissions:\n      contents: write\n", grant
            )
            with self.subTest(grant=grant):
                self.assertTrue(write_job_violations(text))

    def test_an_extra_permission_scope_is_rejected(self):
        base = self.PUBLISH.format(action=self.DOWNLOAD, script=self.ALLOWED)
        for text in (
            base.replace("  contents: read\n", "  contents: read\n  actions: write\n", 1),
            base.replace("      contents: write\n", "      contents: write\n      issues: write\n"),
        ):
            with self.subTest(text=text):
                self.assertNotEqual(text, base)
                self.assertTrue(write_job_violations(text))

    def test_an_extra_permission_scope_on_a_read_job_is_rejected(self):
        text = self.PUBLISH.format(action=self.DOWNLOAD, script=self.ALLOWED) + (
            "  build:\n    permissions:\n      contents: read\n      actions: write\n"
            "    steps:\n      - run: cargo build\n"
        )
        self.assertTrue(write_job_violations(text))

    def test_a_flow_style_job_is_rejected(self):
        base = self.PUBLISH.format(action=self.DOWNLOAD, script=self.ALLOWED)
        hidden = "  hidden: {runs-on: ubuntu-latest, permissions: write-all, steps: [{run: ./x}]}\n"
        for text in (
            base.replace("jobs:\n", "jobs:\n" + hidden),
            base + hidden,
        ):
            with self.subTest(text=text):
                self.assertTrue(write_job_violations(text))

    def test_an_unlisted_action_input_is_rejected(self):
        release = "softprops/action-gh-release@3bb12739c298aeb8a4eeaf626c5b8d85266b0e65"
        base = self.PUBLISH.format(action=release, script=self.ALLOWED).replace(
            "        uses: " + release,
            "        uses: " + release + "\n        with:\n          fail_on_unmatched_files: true",
        )
        self.assertEqual(write_job_violations(base), [])
        self.assertTrue(write_job_violations(base.replace(
            "          fail_on_unmatched_files: true",
            "          fail_on_unmatched_files: true\n          tag_name: v0.0.1",
        )))


class AttestJobGuardRejectionTests(unittest.TestCase):
    """The OIDC scopes stay on the attest job, in its allowlisted shape."""

    WORKFLOW_TEXT = """name: Release
permissions:
  contents: read
jobs:
  build:
    permissions:
      contents: read
    steps:
      - run: cargo build
  attest:
    permissions:
      contents: read
      id-token: write
      attestations: write
    steps:
      - name: Download
        uses: actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093
        with:
          pattern: release-*
          merge-multiple: true
          path: ${{ runner.temp }}/attest-assets
      - name: Check
        shell: bash
        run: |
          set -euo pipefail
          cd "$RUNNER_TEMP/attest-assets"
          sha256sum -c ./*.sha256
          test "$(ls ./*.tar.gz ./*.tgz | wc -l)" = "$(ls ./*.sha256 | wc -l)"
      - name: Attest
        uses: actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6 # v4.2.2
        with:
          subject-path: |
            ${{ runner.temp }}/attest-assets/*.tar.gz
            ${{ runner.temp }}/attest-assets/*.tgz
            ${{ runner.temp }}/attest-assets/*.sha256
"""

    def test_the_allowlisted_attest_job_passes(self):
        self.assertEqual(write_job_violations(self.WORKFLOW_TEXT), [])

    def test_an_oidc_scope_on_another_job_is_rejected(self):
        for grant in (
            "      contents: read\n      id-token: write\n",
            "      contents: write\n      id-token: write\n",
            "      id-token: write\n",
            "      contents: read\n      attestations: write\n",
        ):
            text = self.WORKFLOW_TEXT.replace(
                "  build:\n    permissions:\n      contents: read\n",
                "  build:\n    permissions:\n" + grant,
                1,
            )
            with self.subTest(grant=grant):
                self.assertNotEqual(text, self.WORKFLOW_TEXT)
                self.assertTrue(write_job_violations(text))

    def test_a_flow_style_oidc_grant_is_rejected(self):
        text = self.WORKFLOW_TEXT.replace(
            "  build:\n    permissions:\n      contents: read\n",
            "  build:\n    permissions: {id-token: write}\n",
            1,
        )
        self.assertTrue(write_job_violations(text))

    def test_an_oidc_scope_in_the_workflow_default_is_rejected(self):
        text = self.WORKFLOW_TEXT.replace(
            "permissions:\n  contents: read\njobs:",
            "permissions:\n  contents: read\n  id-token: write\njobs:",
            1,
        )
        self.assertTrue(write_job_violations(text))

    def test_an_attest_job_with_an_extra_scope_is_rejected(self):
        for extra in ("      contents: write\n", "      packages: write\n"):
            text = self.WORKFLOW_TEXT.replace(
                "      attestations: write\n", "      attestations: write\n" + extra, 1
            )
            with self.subTest(extra=extra):
                self.assertTrue(write_job_violations(text))

    def test_an_attest_job_missing_a_scope_is_rejected(self):
        text = self.WORKFLOW_TEXT.replace("      attestations: write\n", "", 1)
        self.assertTrue(write_job_violations(text))

    def test_an_attest_job_that_checks_out_is_rejected(self):
        text = self.WORKFLOW_TEXT.replace(
            "    steps:\n      - name: Download\n",
            "    steps:\n      - uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683\n"
            "      - name: Download\n",
            1,
        )
        self.assertNotEqual(text, self.WORKFLOW_TEXT)
        self.assertTrue(write_job_violations(text))

    def test_an_attest_job_that_runs_a_downloaded_file_is_rejected(self):
        for command in (
            "./attest-assets/pohunek --version",
            "tar -xzf ./*.tar.gz && ./pohunek/pohunek",
        ):
            text = self.WORKFLOW_TEXT.replace(
                "          sha256sum -c ./*.sha256\n",
                "          sha256sum -c ./*.sha256\n          " + command + "\n",
                1,
            )
            with self.subTest(command=command):
                self.assertNotEqual(text, self.WORKFLOW_TEXT)
                self.assertTrue(write_job_violations(text))

    def test_an_attest_job_with_an_unlisted_action_is_rejected(self):
        text = self.WORKFLOW_TEXT.replace(
            "actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6", "evil/payload@" + "0" * 40, 1
        )
        self.assertTrue(write_job_violations(text))

    def test_an_attest_job_with_an_unlisted_step_key_is_rejected(self):
        for extra in ("        env:\n          X: y\n", "        if: always()\n"):
            text = self.WORKFLOW_TEXT.replace(
                "      - name: Check\n", "      - name: Check\n" + extra, 1
            )
            with self.subTest(extra=extra):
                self.assertTrue(write_job_violations(text))


if __name__ == "__main__":
    unittest.main()
