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


def write_job_violations(text: str) -> list[str]:
    """Everything a write-token job does beyond the allowlisted steps."""
    violations = []
    for name, block in jobs(text).items():
        if not holds_write_token(block):
            continue
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
jobs:
  publish:
    permissions:
      contents: write
    steps:
      - uses: {action}
      - shell: bash
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

    def test_a_checkout_is_rejected(self):
        self.assertTrue(
            self.violations("actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683", self.ALLOWED)
        )


if __name__ == "__main__":
    unittest.main()
