"""Every flaky network install goes through .github/scripts/ci-retry (#2201). Stdlib only."""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = sorted((ROOT / ".github" / "workflows").glob("*.yml"))
ACTIONS = sorted((ROOT / ".github" / "actions").glob("*/action.yml"))

# A comment line never counts as a call site.
CALL_PATTERNS = (
    "apt-get",
    "npm ci",
    "bun install",
    "cargo fetch",
    "uv python install",
    "uv pip install",
    "rustup toolchain install",
    "dtolnay/rust-toolchain",
)
RETRY = "ci-retry"
HELPER = ".github/scripts/ci-retry"


def steps_of(text: str) -> list[str]:
    """Split a workflow into step blocks (a `- ` item at six spaces of indent)."""
    return re.split(r"(?m)^(?=      - )", text)[1:]


def invocations_of(step: str) -> list[str]:
    """Join backslash-continued ci-retry invocations into single strings."""
    joined: list[str] = []
    current: list[str] = []

    def flush() -> None:
        if current:
            joined.append(" ".join(current))
            current.clear()

    for line in code_lines(step):
        stripped = line.strip()
        if RETRY in stripped and "usage" not in stripped:
            flush()
            current.append(stripped.rstrip("\\").strip())
        elif current:
            current.append(stripped.rstrip("\\").strip())
            if not stripped.endswith("\\"):
                flush()
    flush()
    return joined


def code_lines(step: str) -> list[str]:
    return [
        line
        for line in step.splitlines()
        if not line.lstrip().startswith("#")
    ]


def files_with(pattern: str) -> list[tuple[str, str]]:
    hits = []
    for path in [*WORKFLOWS, *ACTIONS]:
        for step in steps_of(path.read_text()):
            for line in code_lines(step):
                if pattern in line:
                    hits.append((str(path.relative_to(ROOT)), step))
                    break
    return hits


class NetworkRetryTests(unittest.TestCase):
    def test_every_network_install_goes_through_ci_retry(self):
        failures = []
        for pattern in CALL_PATTERNS:
            for path, step in files_with(pattern):
                name = step.splitlines()[0].strip()
                if pattern == "dtolnay/rust-toolchain":
                    failures.append(f"{path}: {name} still uses the third-party action")
                elif pattern == "rustup toolchain install" and "exec rustup toolchain install" in step:
                    # The install lives in the generated script file and reaches
                    # the network through the helper in the same step.
                    continue
                elif RETRY not in step:
                    failures.append(f"{path}: {name} runs `{pattern}` without {RETRY}")
        self.assertEqual(failures, [])

    def test_retry_invocations_carry_bounds_and_run_the_command(self):
        failures = []
        for path in [*WORKFLOWS, *ACTIONS]:
            for step in steps_of(path.read_text()):
                for invocation in invocations_of(step):
                    with self.subTest(invocation=invocation):
                        for flag in ("--label", "--attempts", "--attempt-timeout", "--backoff", "--"):
                            if flag not in invocation:
                                failures.append(f"{path}: missing {flag}: {invocation}")
        self.assertEqual(failures, [])

    def test_retry_uses_the_checkout_relative_helper_path(self):
        failures = []
        for path in [*WORKFLOWS, *ACTIONS]:
            text = path.read_text()
            lines = text.splitlines()
            job_starts = [
                i for i, line in enumerate(lines)
                if re.match(r"^  [A-Za-z0-9_-]+:\s*$", line)
            ]
            pos = 0
            for step in steps_of(text):
                start = text.index(step, pos)
                pos = start + len(step)
                start_line = text.count("\n", 0, start)
                n = len(lines)
                for invocation in invocations_of(step):
                    if "$GITHUB_ACTION_PATH" in invocation:
                        continue
                    job_index = max(i for i in job_starts if i <= start_line)
                    following = [i for i in job_starts if i > job_index]
                    job_end = min(following) if following else n
                    scope = "\n".join(lines[job_index:job_end])
                    relocated = "path: relocated" in scope or "working-directory: relocated" in scope
                    want = "$GITHUB_WORKSPACE/relocated/" + HELPER if relocated else "$GITHUB_WORKSPACE/" + HELPER
                    if want not in invocation:
                        failures.append(f"{path}: {invocation}")
        self.assertEqual(failures, [])

    def test_zig_transfers_carry_the_same_bounds_as_mold(self):
        zig = (ROOT / ".github" / "actions" / "setup-zig" / "action.yml").read_text()
        mold = (ROOT / ".github" / "actions" / "setup-mold" / "action.yml").read_text()
        for flag in (
            '--retry "$DOWNLOAD_RETRIES" --retry-all-errors',
            '--retry-max-time "$DOWNLOAD_RETRY_MAX_TIME_SECONDS"',
            '--connect-timeout "$DOWNLOAD_CONNECT_TIMEOUT_SECONDS"',
            '--speed-limit "$DOWNLOAD_STALL_BYTES_PER_SECOND"',
            '--speed-time "$DOWNLOAD_STALL_SECONDS"',
            '--max-time "$DOWNLOAD_MAX_TIME_SECONDS"',
        ):
            with self.subTest(flag=flag):
                self.assertIn(flag, mold)
                self.assertIn(flag, zig)


if __name__ == "__main__":
    unittest.main()
