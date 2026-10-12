"""Scenarios for scripts/pi-detect-regex and the Pi detection manifest.

The generator runs as a process: its `--verify` mode must pass, and every
regex it prints must equal the one runtime-packages/pi/detect.toml declares, so
the manifest stays the generated output it claims to be. Stdlib only.
"""

from pathlib import Path
import subprocess
import sys
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "pi-detect-regex"
MANIFEST = ROOT / "runtime-packages" / "pi" / "detect.toml"
# Upper bound for one run; --verify enumerates a few million short strings
# and takes well under a minute on a CI runner.
RUN_TIMEOUT_SECONDS = 300


def run(*args):
    return subprocess.run(
        [sys.executable, "-I", str(SCRIPT), *args],
        capture_output=True,
        text=True,
        timeout=RUN_TIMEOUT_SECONDS,
        check=False,
    )


class PiDetectRegexTest(unittest.TestCase):
    def test_verify_passes(self):
        result = run("--verify")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("ok dialog and overlay lines", result.stdout)

    def test_manifest_holds_the_generated_regexes(self):
        result = run()
        self.assertEqual(result.returncode, 0, result.stderr)
        generated = {}
        for line in result.stdout.splitlines():
            key, _, value = line.partition(" = ['")
            self.assertTrue(value.endswith("']"), line)
            generated[key] = value[: -len("']")]
        declared = {}
        for rule in tomllib.loads(MANIFEST.read_text(encoding="utf-8"))["rules"]:
            declared[f"{rule['id']} regex"] = rule["regex"][0]
            if "not" in rule:
                declared[f"{rule['id']} not.regex"] = rule["not"]["regex"][0]
        self.assertEqual(generated, declared)


if __name__ == "__main__":
    unittest.main()
