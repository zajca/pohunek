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
  "build-bins": "29c8fb9fcce22ee02e151f7cf6c3995d8605be40975af45ec11e7346bf27cf4a",
  "build-tests": "8349ca98ff12ec3b8188910d2bf15ab52e403c2d98bc9744e837aa4010ca1415",
  "darwin-check": "9d57e45236707f62a3a961302343b80a05bbc111eb3b8b9dcbc0c70fb665b496",
  "docs": "78ccd8f3892606250b9353ee35b0612154f48f8e04fb072d3bd1aff3a88a52c1",
  "doctests": "d1ae3f494b743d380b638141be939d3a42c597f7928c4fe915670efbb05be943",
  "fast-tests": "bb8a335dd5ec94c10c91f9889eaaa5196254987b611ecea329858ac6e6289a8a",
  "feature-matrix": "e8a32d260282f7abf85cc323e5e9b3e5d2066f95c9454824c8c41821b31ec9d9",
  "hermes-compatibility-macos": "56edd524fac5d84d03790e3d45da1444e173bd583014016cb34dfbc00d29e03b",
  "integration": "1626dbb6661d7b5cd5c2baf308d3fd3b8e3a985f7f8269a57aa515fccdd1160f",
  "macos-package": "695749ad8aa0a68ca4e45b56371312ac6e5bb47be5979bfcb79ad8d9e246ed37",
  "macos-release-build": "c31c520474e9da5f9a637eac189beaaeee09ad81c43133b2c41a76d54aa1f3b0",
  "macos-tier": "12bd7a778ae1083f059b357f4387aabc5da933d7d12050ab03316d4ec17a1e96",
  "platform-contracts": "9ff126ad10634a9f1314219b809bb2d28058e5191dd512b929436c8e74544a03",
  "release-build": "42af2fa1d3e30e9ec7aba7bd11762167c5d7dcf5dd933d8f386559834a831518",
  "script-tests": "bc3133fb57737612bb959a951d7e8fb11c10d928cb50458c023abc7f19d5f1bd",
  "sdk-macos": "81ad20f2f81b188749b686b4a1738b58ca925568d246bf3647458dc5e96a8d76",
  "shear": "27dd031530cdb44c7e1185673f4264bd213c00e0e3dcc5deddbf59082b4c6538",
  "systemd-supervision": "b42a21dc0e2dfeff4c0fec6349ef24a6c8e97c10ff716cbc4dde5deeb467f73c",
  "test": "7df6c1fe0ae04f22e2df311364f52a8d96e0f6df4794c10c15cec5862e2929b5",
  "ts-drift": "6fc715f6b14333912f2fdf71dc4f854fd5250b30b6f8b6497cd775aa6430af0c",
  "udeps": "016994212a2c3306cb518b5a4f155de98fe0b7c58ef5c2f806eee674faffebdd",
  "upgrade-test": "5085c4d9086eb08446a2834e80a94fb0538d52e03fab110d3206f9ff17145053",
}

PINNED_CALLEE = {".github/workflows/upgrade.yml": "6c37d571a3fab093e32962c56dc4c570fefb0a7a11530972902248b3483662fe"}

# sha256 of the workflow text before `jobs:` (the header comment, `on:`,
# `run-name`, `concurrency`, `permissions`, the workflow `env:`), taken from
# ci.yml at pin time.
PINNED_PREAMBLE = "e8efc3a006506bbe0fdba96ba9b85c291c774476ceaa08025256e1131fc53a93"


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
