"""Runs the claude-accounts scenario (stdlib only).

The scenario drives the official Claude package's setup program and its wrapper
through a pseudo-terminal, with a fake `claude` and `pohunek` on PATH. It needs
`script` (util-linux), so it runs on Linux only.
"""

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
PACKAGE = REPO / "runtime-packages" / "claude"
SCENARIO = Path(__file__).resolve().parent / "claude-accounts-scenario.sh"
# Work roots live under the directory the test environment names; the default
# temp location is used when it is unset.
WORK_PARENT = os.environ.get("TMPDIR") or None


@unittest.skipUnless(sys.platform.startswith("linux"), "the scenario uses util-linux script")
@unittest.skipUnless(shutil.which("script"), "script(1) is required")
class ClaudeAccountsScenario(unittest.TestCase):
    def test_scenario_passes(self) -> None:
        with tempfile.TemporaryDirectory(prefix="claude-accounts-", dir=WORK_PARENT) as root:
            result = subprocess.run(
                ["sh", str(SCENARIO), str(PACKAGE), root],
                capture_output=True,
                text=True,
                timeout=600,
                check=False,
            )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
