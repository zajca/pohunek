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

# Directories the runners check out into instead of the workspace root.
RELOCATED_MARKERS = ("relocated/",)

# Workflows that reach setup-rust or hermes-test-python, keyed by workflow
# name: every CI_RETRY_* env name those composite actions read must be
# defined by the workflow, or the action's retry input must carry its own
# default (a called workflow never inherits the caller's env).
SETUP_RUST_REQUIRED_ENV = {
    "CI_RETRY_ATTEMPTS": "retry-attempts",
    "CI_RETRY_BACKOFF_SECONDS": "retry-backoff-seconds",
    "CI_RETRY_ATTEMPT_TIMEOUT_RUSTUP_SECONDS": "retry-attempt-timeout-seconds",
}
HERMES_REQUIRED_ENV = {
    "CI_RETRY_ATTEMPTS",
    "CI_RETRY_BACKOFF_SECONDS",
    "CI_RETRY_ATTEMPT_TIMEOUT_UV_SECONDS",
}


def code_lines_of(text: str) -> list[tuple[int, str]]:
    """(1-based line number, line) for every non-comment, non-empty line."""
    lines = []
    for number, line in enumerate(text.splitlines(), start=1):
        if line.lstrip().startswith("#") or not line.strip():
            continue
        lines.append((number, line))
    return lines


def steps_of(text: str) -> list[tuple[int, str]]:
    """Step blocks as (1-based start line, block text).

    Workflow steps are `- ` items at six spaces of indent; composite action
    steps sit at four spaces. A step item is a `- name:`/`- id:`/`- uses:`/
    `- run:`/`- shell:`/`- if:`/`- working-directory:`/`- env:`/`- with:`
    entry: a `- ` item without one of those keys is a script list inside
    the step body (indented to the step indent for readability), never a
    new step. A `key:` mapping value on the same line (e.g. `- uses: x`)
    still counts: the key is what matters, not the shape of its value.
    """
    lines = text.splitlines()
    starts = []
    for number, line in enumerate(lines, start=1):
        stripped = line.lstrip()
        if not stripped.startswith("- "):
            continue
        indent = len(line) - len(stripped)
        if indent not in (4, 6):
            continue
        if not re.match(
            r"^- (name|id|uses|run|shell|if|working-directory|env|with):(\s|$)", stripped
        ):
            continue
        starts.append(number)
    blocks = []
    for i, start in enumerate(starts):
        end = starts[i + 1] if i + 1 < len(starts) else len(lines) + 1
        blocks.append((start, "\n".join(lines[start - 1:end - 1])))
    return blocks


def invocations_of(step: str) -> list[str]:
    """Join backslash-continued ci-retry invocations into single strings."""
    joined: list[str] = []
    current: list[str] = []

    def flush() -> None:
        if current:
            joined.append(" ".join(current))
            current.clear()

    for line in step.splitlines():
        if line.lstrip().startswith("#"):
            continue
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


def step_calls_retry(step: str) -> bool:
    """Whether the step itself invokes the retry helper (not a comment)."""
    return any(RETRY in line for line in step.splitlines() if not line.lstrip().startswith("#"))


def invocation_line_numbers(numbered: list[tuple[int | None, str]]) -> set[int]:
    """1-based line numbers a ci-retry invocation covers, with continuations.

    `numbered` pairs each line with its file line number (`None` for
    synthetic fixture lines, which fall back to the entry offset).
    """
    covered: set[int] = set()
    current: list[int] = []

    def flush() -> None:
        covered.update(current)
        current.clear()

    for offset, entry in enumerate(numbered):
        number, line = entry
        if line.lstrip().startswith("#"):
            continue
        stripped = line.strip()
        key = number if number is not None else offset
        if RETRY in stripped and "usage" not in stripped:
            flush()
            current.append(key)
            if not stripped.endswith("\\"):
                flush()
        elif current:
            current.append(key)
            if not stripped.endswith("\\"):
                flush()
    flush()
    return covered


def define_action_inputs(text: str) -> dict[str, str]:
    """Defaulted composite action inputs: `{name: default}` for `inputs:` entries."""
    inputs: dict[str, str] = {}
    match = re.search(r"(?m)^inputs:\s*$", text)
    if match is None:
        return inputs
    rest = text[match.end():]
    end = re.search(r"(?m)^(runs|outputs):\s*$", rest)
    block = rest[: end.start()] if end else rest
    current = None
    for line in block.splitlines():
        name = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line)
        if name:
            current = name.group(1)
            continue
        default = re.match(r'^    default: "(.*)"\s*$', line)
        if default is not None and current is not None:
            inputs[current] = default.group(1)
            current = None
    return inputs


def workflow_top_env(text: str) -> set[str]:
    """Names of the workflow's top-level `env:` block (before `jobs:`)."""
    head = text.split("\njobs:", 1)[0]
    names = set()
    in_env = False
    for line in head.splitlines():
        if re.match(r"^env:\s*$", line):
            in_env = True
            continue
        if in_env and re.match(r"^  [A-Z][A-Z0-9_]*:\s*", line):
            names.add(line.split(":")[0].strip())
        elif in_env and line and not line.startswith(" "):
            break
    return names


class NetworkRetryTests(unittest.TestCase):
    def check_lines(self, path: Path, start: int, step: str) -> list[str]:
        """Per-line failures naming `path:line` for unwrapped call sites.

        Each network line must sit on a helper invocation, or on a line the
        helper invocation continues onto: a bare `apt-get` on its own line
        fails even when another line of the same step is wrapped.
        """
        numbered = [(start + offset, line) for offset, line in enumerate(step.splitlines())]
        invocation_lines = invocation_line_numbers(numbered)
        failures = []
        for number, line in numbered:
            if line.lstrip().startswith("#") or not line.strip():
                continue
            for pattern in CALL_PATTERNS:
                if pattern not in line:
                    continue
                if pattern == "dtolnay/rust-toolchain":
                    failures.append(f"{path}:{number}: still uses the third-party action")
                elif RETRY in line or number in invocation_lines:
                    continue
                else:
                    failures.append(f"{path}:{number}: runs `{pattern}` without {RETRY}")
        return failures

    def test_every_network_install_goes_through_ci_retry(self):
        failures = []
        for path in [*WORKFLOWS, *ACTIONS]:
            text = path.read_text()
            rel = path.relative_to(ROOT)
            for start, step in steps_of(text):
                failures.extend(str(message).replace(str(path), str(rel), 1) for message in self.check_lines(path, start, step))
        self.assertEqual(failures, [])

    def test_guard_catches_an_unwrapped_call_in_an_action(self):
        path = ROOT / ".github" / "actions" / "setup-mold" / "action.yml"
        with self.subTest("clean step"):
            self.assertEqual(self.check_lines(path, 40, "    - name: Get mold\n      run: curl https://example.invalid/tool.tar.gz"), [])
        with self.subTest("names the unwrapped line, not the step"):
            failures = self.check_lines(path, 40, "    - name: Install\n      run: echo wraps-nothing\n      run: apt-get install -y mold")
            self.assertEqual(len(failures), 1)
            self.assertIn("setup-mold/action.yml:42", failures[0])
        with self.subTest("one wrapped call does not exempt an unwrapped one"):
            step = (
                "    - name: Install\n"
                '      run: "$GITHUB_ACTION_PATH/../../scripts/ci-retry" --label x -- \\\n'
                "        apt-get install -y wrapped\n"
                "      run: apt-get install -y bare"
            )
            failures = self.check_lines(path, 40, step)
            self.assertEqual(len(failures), 1)
            self.assertIn("setup-mold/action.yml:43", failures[0])
        with self.subTest("a wrapped line on a continuation passes"):
            wrapped = (
                "    - name: Install\n"
                '      run: "$GITHUB_ACTION_PATH/../../scripts/ci-retry" --label x \\\n'
                "        --attempts 3 -- apt-get install -y wrapped"
            )
            self.assertEqual(self.check_lines(path, 40, wrapped), [])
        with self.subTest("a sh -c payload after a line without backslash fails"):
            # Regression shape of setup-rust H1 (r2): the helper line ends
            # in `-- sh -c` with no trailing `\`, so the shell runs the
            # helper without a command string and the payload line below
            # never goes through the helper. The payload must fail even
            # though it reads like a helper argument.
            broken = (
                "    - name: Install Rust\n"
                '      run: "$GITHUB_ACTION_PATH/../../scripts/ci-retry" --label rustup \\\n'
                '        --attempts 3 -- sh -c\n'
                '          \'exec rustup toolchain install "$0" $1\' "$TOOLCHAIN" "$args"'
            )
            failures = self.check_lines(path, 40, broken)
            self.assertEqual(len(failures), 1)
            self.assertIn("setup-mold/action.yml:43", failures[0])

    def test_setup_rust_needs_no_caller_env(self):
        """The action installs with its own retry input defaults (#2201 H1)."""
        action = (ROOT / ".github" / "actions" / "setup-rust" / "action.yml").read_text()
        code = "\n".join(
            line for line in action.splitlines()
            if not line.lstrip().startswith("#")
        )
        inputs = define_action_inputs(action)
        for env_name, input_name in SETUP_RUST_REQUIRED_ENV.items():
            with self.subTest(env=env_name):
                self.assertNotIn(f"env.{env_name}", code, f"setup-rust reads caller env {env_name}")
                self.assertNotIn(f"${env_name}", code, f"setup-rust reads caller env {env_name}")
                self.assertIn(input_name, inputs, f"setup-rust has no {input_name} input")
                self.assertTrue(inputs[input_name], f"setup-rust input {input_name} has no default")

    def test_hermes_and_every_caller_cover_their_retry_env(self):
        """Each workflow a retried action reaches defines the env it reads."""
        hermes = (ROOT / ".github" / "actions" / "hermes-test-python" / "action.yml").read_text()
        for name in HERMES_REQUIRED_ENV:
            self.assertIn(name, hermes, f"hermes-test-python forgot {name}")
        setup_rust = (ROOT / ".github" / "actions" / "setup-rust" / "action.yml").read_text()
        setup_rust_code = "\n".join(
            line for line in setup_rust.splitlines()
            if not line.lstrip().startswith("#")
        )
        self.assertNotIn("env.CI_RETRY_", setup_rust_code)
        self.assertNotIn("$CI_RETRY_", setup_rust_code)
        failures = []
        for path in WORKFLOWS:
            text = path.read_text()
            rel = str(path.relative_to(ROOT))
            if "actions/setup-rust" not in text and "hermes-test-python" not in text:
                continue
            defined = workflow_top_env(text)
            needed: set[str] = set()
            if "hermes-test-python" in text:
                needed |= HERMES_REQUIRED_ENV
            if "actions/setup-rust" in text:
                # setup-rust carries its own retry input defaults, so a
                # workflow without CI_RETRY_* still installs; a workflow that
                # passes `retry-*` inputs through must define the matching env.
                for env_name in SETUP_RUST_REQUIRED_ENV:
                    if env_name in text:
                        needed.add(env_name)
            for name in sorted(needed):
                if name not in defined:
                    failures.append(f"{rel}: uses a retried action reading {name} without defining it")
        self.assertEqual(failures, [])

    def test_retry_invocations_carry_bounds_and_run_the_command(self):
        failures = []
        for path in [*WORKFLOWS, *ACTIONS]:
            text = path.read_text()
            rel = str(path.relative_to(ROOT))
            for start, step in steps_of(text):
                for invocation in invocations_of(step):
                    with self.subTest(invocation=invocation):
                        for flag in ("--label", "--attempts", "--attempt-timeout", "--backoff", "--"):
                            if flag not in invocation:
                                failures.append(f"{rel}:{start}: missing {flag}: {invocation}")
        self.assertEqual(failures, [])

    def test_retry_uses_the_checkout_relative_helper_path(self):
        failures = []
        for path in [*WORKFLOWS, *ACTIONS]:
            text = path.read_text()
            rel = str(path.relative_to(ROOT))
            lines = text.splitlines()
            job_starts = [
                i for i, line in enumerate(lines)
                if re.match(r"^  [A-Za-z0-9_-]+:\s*$", line)
            ]
            pos = 0
            for start, step in steps_of(text):
                offset = text.index(step, pos)
                pos = offset + len(step)
                start_line = text.count("\n", 0, offset)
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
                        failures.append(f"{rel}:{start}: {invocation}")
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
