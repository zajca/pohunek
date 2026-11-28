"""Checked-in pins for the allowlisted jobs, the called workflow and the preamble.

If a job body, `upgrade.yml` or the workflow text before `jobs:` changes,
the guard fails: re-check reuse eligibility for the change, then update the
pin and, for a job body, the evaluation table below in the same commit.
This file is the source data - it is never recomputed from the workflow
under test.
"""
import hashlib
import re


def pinned_hash(text):
    return hashlib.sha256(text.encode()).hexdigest()


def split_preamble(text):
    # The workflow text before the `jobs:` section, cut at the line that
    # starts the section: an earlier `jobs:` substring (for example in a
    # header comment) stays in the preamble.
    return re.split(r"(?m)^jobs:", text, maxsplit=1)[0]


def pinned_bodies():
    # sha256 of each allowlisted job body, taken from ci.yml at pin time.
    return dict(PINNED_BODY_HASHES)

def pinned_callee():
    return dict(PINNED_CALLEE)

# Populated by the pin-update procedure; see docs/development.md (Delta dispatch).
PINNED_BODY_HASHES = {
  "build-bins": "fe2f2655ee5dd89c177f8bd4bf72a4cb7b766ca5c133304d2a5611ff7bf3d983",
  "build-tests": "8349ca98ff12ec3b8188910d2bf15ab52e403c2d98bc9744e837aa4010ca1415",
  "darwin-check": "9d57e45236707f62a3a961302343b80a05bbc111eb3b8b9dcbc0c70fb665b496",
  "docs": "8bcc9f4b97574b58f126265bb747f9b13c0dca8a9fc9d9212f016dd94e16a188",
  "doctests": "d1ae3f494b743d380b638141be939d3a42c597f7928c4fe915670efbb05be943",
  "fast-tests": "bb8a335dd5ec94c10c91f9889eaaa5196254987b611ecea329858ac6e6289a8a",
  "feature-matrix": "e8a32d260282f7abf85cc323e5e9b3e5d2066f95c9454824c8c41821b31ec9d9",
  "hermes-compatibility-macos": "0c849d5255e245198c4b073e63febe7c81e1c202d468628c985531e02a5b73a7",
  "integration": "1626dbb6661d7b5cd5c2baf308d3fd3b8e3a985f7f8269a57aa515fccdd1160f",
  "macos-package": "065adb21e50a67d097a7ab569ba1f08ee52991a397fa06bd966604174ea75ef0",
  "macos-release-build": "3606ddebf6821c5b43a453417aa4f5077c8843fb5de0a35e0560094fc3746a38",
  "macos-tier": "5c1c709f20f1ed7f77248e9fe66d28174d0174c2f0571fc185eb79367e44c5d1",
  "platform-contracts": "2478c75e6149df6dc0c3f1968b7a69be8918f0d69ea0e1a88a2e7b6b3a0910aa",
  "release-build": "116a9eff04cddbdb4f4fc419f6292be0f7221fd28f153907b0ea115d726d17f1",
  "script-tests": "bc3133fb57737612bb959a951d7e8fb11c10d928cb50458c023abc7f19d5f1bd",
  "sdk-macos": "30206a63cca0537df6e38025a794bb5fee4c1baba4c12075ed476a74ddbeccbe",
  "shear": "8e9684845342c677bb4155e024c73fd205e8d1be7582a73a81b75bdff4046c62",
  "systemd-supervision": "8e5004294d8f298c6e5be025b03d5cec5b479e0f5b664d271b1eeee3421fca20",
  "test": "7df6c1fe0ae04f22e2df311364f52a8d96e0f6df4794c10c15cec5862e2929b5",
  "ts-drift": "d18b132a9b0a47f66735ed031448f0503a5af9840521fb481c69db78d6dc4cc1",
  "udeps": "016994212a2c3306cb518b5a4f155de98fe0b7c58ef5c2f806eee674faffebdd",
  "upgrade-test": "5085c4d9086eb08446a2834e80a94fb0538d52e03fab110d3206f9ff17145053",
}

PINNED_CALLEE = {".github/workflows/upgrade.yml": "266534e2e341054912f4220623af7110a00b775b94a957cf0909001b183b128d"}

# sha256 of the workflow text before `jobs:` (the header comment, `on:`,
# `run-name`, `concurrency`, `permissions`, the workflow `env:`), taken from
# ci.yml at pin time.
PINNED_PREAMBLE = "befbb451de400878a965c551cbd28868b741eba0ff2ab0650e262ba071207a4d"


def pinned_preamble():
    return PINNED_PREAMBLE

# Each entry: a token-reading line of a pinned body, with its expected
# result on all three reused paths (pull request, dispatch, push). Bodies' job-level `if:` is positional
# (the first `if:` of its own job) and exempt; `save-if:` and the
# `shard:` line are exempt by exact text. `failure()` reads false
# (a reused leg was green). Legs: `legs` are the matrix values the
# entry is computed with; each leg records all three paths.
EVALUATION_TABLE = [
    {
        "text": "NARROWED: ${{ github.event_name == 'pull_request' && needs.test-selection.result == 'success' && needs.test-selection.outputs.narrowed == 'true' }}",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "1": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "2": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "3": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
        },
    },
    {
        "text": "SKIP_OWNED_BY: ${{ (github.event_name != 'pull_request' || needs.changes.outputs.hermes_macos == 'true') && 'hermes-macos' || '' }}",
        "legs": {
            "": {"pull_request": "hermes-macos", "workflow_dispatch": "hermes-macos", "push": "hermes-macos"},
        },
    },
    {
        "text": "if: ${{ !cancelled() && github.event_name == 'pull_request' && needs.test-selection.result == 'success' && needs.test-selection.outputs.narrowed == 'true' }}",
        "legs": {
            "": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "1": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "2": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "3": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
        },
    },
    {
        "text": "if: ${{ github.event_name == 'schedule' || (github.event_name == 'workflow_dispatch' && inputs.tier == 'nightly') }}",
        "legs": {
            "": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "1": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "2": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "3": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
        },
    },
    {
        "text": "if: env.TEST_SELECTION_SPARSE == 'true' && github.event_name == 'pull_request' && needs.test-selection.result == 'success' && needs.test-selection.outputs.narrowed == 'true'",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "1": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "2": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "3": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
        },
    },
    {
        "text": "if: failure() && env.CI_INFRA_FAILURE == '' && steps.infra-1.outcome != 'failure' && steps.infra-2.outcome != 'failure' && steps.infra-3.outcome != 'failure' && (github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch')",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "1": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "2": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "3": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
        },
    },
    {
        "text": "if: failure() && env.CI_INFRA_FAILURE == '' && steps.infra-1.outcome != 'failure' && (github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch')",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "1": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "2": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "3": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
        },
    },
    {
        "text": "if: failure() && env.CI_INFRA_FAILURE == '' && steps.infra-1.outcome != 'failure' && steps.infra-2.outcome != 'failure' && (github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch')",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "1": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "2": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "3": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
        },
    },
    {
        "text": "if: failure() && env.CI_INFRA_FAILURE == '' && steps.infra-1.outcome != 'failure' && steps.infra-2.outcome != 'failure' && steps.infra-3.outcome != 'failure' && steps.infra-4.outcome != 'failure' && (github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch')",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "1": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "2": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "3": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
        },
    },
    {
        "text": "if: failure() && env.CI_INFRA_FAILURE == '' && steps.infra-1.outcome != 'failure' && steps.infra-2.outcome != 'failure' && steps.infra-3.outcome != 'failure' && steps.infra-4.outcome != 'failure' && steps.infra-5.outcome != 'failure' && (github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch')",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "1": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "2": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "3": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
        },
    },
    {
        "text": "if: github.event_name == 'pull_request' && needs.test-selection.result == 'success' && needs.test-selection.outputs.narrowed == 'true'",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "1": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "2": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
            "3": {"pull_request": "false", "workflow_dispatch": "false", "push": "false"},
        },
    },
]
