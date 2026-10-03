"""Structural checks on release.yml: write-token jobs run no repository code.

A job holding `contents: write` may only download an artifact, check it, and
attach it. The checks compare every action and every `run:` script of such a
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
    """A job's token grant must be absent or spelled canonically."""
    granted = len(PERMISSIONS_LINE.findall(block))
    canonical = len(CANONICAL_JOB_PERMISSIONS.findall(block + "\n"))
    if granted != canonical:
        return [f"{name} declares permissions in a form the guard does not recognize"]
    return []


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
    violations.extend(jobs_section_violations(text))
    for name, block in jobs(text).items():
        violations.extend(permission_violations(name, block))
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

    def test_workflow_default_token_is_read_only(self):
        self.assertRegex(self.text, r"(?m)^permissions:\n  contents: read$")

    def test_write_token_jobs_only_download_check_and_attach(self):
        self.assertEqual(write_job_violations(self.text), [])

    def test_build_job_cannot_write(self):
        build = self.jobs["build"]
        self.assertRegex(build, r"(?m)^    permissions:\n      contents: read$")
        self.assertNotIn("action-gh-release", build)

    def test_publish_job_mirrors_the_build_matrix(self):
        self.assertEqual(matrix_pairs(self.jobs["publish"]), matrix_pairs(self.jobs["build"]))
        self.assertRegex(self.jobs["publish"], r"(?m)^    needs: \[build\]$")


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
        self.assertTrue(self.violations("evil/payload@" + "0" * 40, self.ALLOWED))

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

    def test_a_checkout_is_rejected(self):
        self.assertTrue(
            self.violations("actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683", self.ALLOWED)
        )


if __name__ == "__main__":
    unittest.main()
