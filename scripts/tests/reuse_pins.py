"""Checked-in pins for the allowlisted jobs and the called workflow.

If a job body or `upgrade.yml` changes, the guard fails with the job
name: re-check reuse eligibility for the change, then update the pin
and the evaluation table below in the same commit. This file is the
source data - it is never recomputed from the workflow under test.
"""
import hashlib



def pinned_hash(text):
    return hashlib.sha256(text.encode()).hexdigest()

def pinned_bodies():
    # sha256 of each allowlisted job body, taken from ci.yml at pin time.
    return dict(PINNED_BODY_HASHES)

def pinned_callee():
    return dict(PINNED_CALLEE)

# Populated by the pin-update procedure; see docs/development.md (Delta dispatch).
PINNED_BODY_HASHES = {
  "build-bins": "ad27b9dd000e779bdcab273e02b60cd3f82b511e9e6579728b65648f5899098a",
  "build-tests": "55351e1992f8d274a6a1c762887e7cd8c0f0d30790c125c5a494bccccd61804c",
  "darwin-check": "20741efc376fff1eef708b2a50d8baaa9d53adcc4b3ca74e1f569ba7eae7548e",
  "docs": "241044f79dd6a64f5b448aa16d6f40ab6668f9bc6c89ee4c721d4c80077d09b4",
  "doctests": "96a870799a342185aca050adb7443cbe2ec46efcbb05ca0e6ecbbb9fda2c8e83",
  "fast-tests": "f7b4763514540644e59fafd1e298860fb9052a0bdd2b185cf13af1aabc2fac17",
  "feature-matrix": "da43714f8fc9e99af344bfb16b6c3dc710f4374fadc2c88e23a07a139525b652",
  "hermes-compatibility-macos": "5bd4a58bc0e1b49186ccea7f28b5b44d29e91a1e93dd1dabe6d7011bc0f58100",
  "integration": "43111fbc1647f22f734185f4eb2cb77f75ed87064d6224d89ced42042b2bbd78",
  "macos-package": "32541e19c608d76e1b9f6d07901a61ee352567ae296d4e0b4c64eedcd6ec89e0",
  "macos-release-build": "ecfe1d026d1f4cf48ef5e3fd17d2220edab28a9a48e88eb504039a0b46e78221",
  "macos-tier": "4a268d85bf953110a494159a1dd89f97d03fbf1b41646751403826311b2b54d6",
  "platform-contracts": "7f8fa364afa7fee54e1a58085e98ffc3b8d540c7ad74e60e0909f9d91643a0c2",
  "release-build": "0a2c0cf6986c5d3a0233f30b7ec19827968e5759509eed92474954cc34f82b3a",
  "sdk-macos": "57458579f490959989d2a5e4b48f64f5df6fd66a29e07edd7af44035e65ad525",
  "shear": "5f429ea7bd81206e947bdaa678f1f6fec6d7d74d3a8ce295c08d107cd8b56d9f",
  "systemd-supervision": "22a4ce208a65f398b2d6a20b8595448c43d748e3ffc089cebc22a1ab56df2a49",
  "test": "bc900a088f77e2c3ee962e8e8b03fafe4865ad8d032798e9103d8761ab870aed",
  "ts-drift": "944e34491562ff54e149558e8d97d9fbde7c75915bfebba9ff94ec62bf032aa0",
  "udeps": "84433f4e2617314f4d89c01166870cc76e57d8c2a65cdfde3c4720f1596b36a0",
  "upgrade-test": "e6effae0c42865fc724c2c1b72d09aeaa84e07e88b15f5c18a587f5761dedd2e"
}

PINNED_CALLEE = {".github/workflows/upgrade.yml": "266534e2e341054912f4220623af7110a00b775b94a957cf0909001b183b128d"}

# Each entry: a token-reading line of a pinned body, with its expected
# result on both reused paths. Bodies' job-level `if:` is positional
# (the first `if:` of its own job) and exempt; `save-if:` and the
# `shard:` line are exempt by exact text. `failure()` reads false
# (a reused leg was green). Legs: `legs` are the matrix values the
# entry is computed with; each leg records both paths.
EVALUATION_TABLE = [
    {
        "text": "NARROWED: ${{ github.event_name == 'pull_request' && needs.test-selection.result == 'success' && needs.test-selection.outputs.narrowed == 'true' }}",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false"},
        },
    },
    {
        "text": "SKIP_OWNED_BY: ${{ (github.event_name != 'pull_request' || needs.changes.outputs.hermes_macos == 'true') && 'hermes-macos' || '' }}",
        "legs": {
            "": {"pull_request": "hermes-macos", "workflow_dispatch": "hermes-macos"},
        },
    },
    {
        "text": "if: ${{ !cancelled() && github.event_name == 'pull_request' && needs.test-selection.result == 'success' && needs.test-selection.outputs.narrowed == 'true' }}",
        "legs": {
            "": {"pull_request": "false", "workflow_dispatch": "false"},
        },
    },
    {
        "text": "if: ${{ github.event_name == 'schedule' || (github.event_name == 'workflow_dispatch' && inputs.tier == 'nightly') }}",
        "legs": {
            "": {"pull_request": "false", "workflow_dispatch": "false"},
        },
    },
    {
        "text": "if: env.TEST_SELECTION_SPARSE == 'true' && github.event_name == 'pull_request' && needs.test-selection.result == 'success' && needs.test-selection.outputs.narrowed == 'true'",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false"},
        },
    },
    {
        "text": "if: failure() && (github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch')",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false"},
        },
    },
    {
        "text": "if: github.event_name == 'pull_request' && needs.test-selection.result == 'success' && needs.test-selection.outputs.narrowed == 'true'",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false"},
        },
    },
    {
        "text": "if: matrix.shard == 'core' && (github.event_name != 'pull_request' || needs.test-selection.result != 'success' || needs.test-selection.outputs.narrowed != 'true' || needs.test-selection.outputs.script_tests != 'false')",
        "legs": {
            "cli": {"pull_request": "false", "workflow_dispatch": "false"},
            "core": {"pull_request": "true", "workflow_dispatch": "true"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false"},
        },
    },
]

def pinned_preamble():
    # sha256 of the workflow text before `jobs:` (env:, defaults:), taken
    # from ci.yml at pin time.
    return PINNED_PREAMBLE

PINNED_PREAMBLE = "11150566b689134605836bf908672791311c550d5bb36a773eae52c44679079a"
