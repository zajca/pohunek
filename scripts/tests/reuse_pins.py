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
  "build-tests": "c41fed73008a67441e2c1e6777885b0498307f4d2d5fb786e146ce2bd7f0856b",
  "darwin-check": "4a82af03b6684e8d7c970cd338a799007bbca51e5dfd7681c1e4ccdbfb62612b",
  "docs": "06499e3f4e9ade20bfd77c7c4f55b0d1c8cdead3de485bcbb2be2b69528544b2",
  "doctests": "7978d844ac8b29ecbbf12db1808456dbb830fb70431c85d7afa3eb8a1ba24326",
  "fast-tests": "84c81271684fe68f2490cb3bea2f84e70cdf86eed3e9b15c69210a63e46e0eab",
  "feature-matrix": "aa5ee100347f8e163a3d12d241c1782636ac5d5ecc52a929c0ff1a855bde7c5a",
  "hermes-compatibility-macos": "31230f71891dba9c8ddd9d7ef8e758da1d2559f6419d47662ed35063db1ee325",
  "integration": "908bcf7a96414e511ea375ef32e1b9e17b15ab64b67ca1e8eda9d147ba96edb5",
  "macos-package": "065adb21e50a67d097a7ab569ba1f08ee52991a397fa06bd966604174ea75ef0",
  "macos-release-build": "3606ddebf6821c5b43a453417aa4f5077c8843fb5de0a35e0560094fc3746a38",
  "macos-tier": "b6d5867fa5aeccaf431276e6f6cd1a3e697c5f8cbc7a60f0595a986224c7cc83",
  "platform-contracts": "e8c96045f47a59e50ec5e6c95c1fac53d744a4888248add118ac939761cb8473",
  "release-build": "116a9eff04cddbdb4f4fc419f6292be0f7221fd28f153907b0ea115d726d17f1",
  "sdk-macos": "30206a63cca0537df6e38025a794bb5fee4c1baba4c12075ed476a74ddbeccbe",
  "shear": "8e9684845342c677bb4155e024c73fd205e8d1be7582a73a81b75bdff4046c62",
  "systemd-supervision": "a3f5a65e0c40ab41febe5b9e5a4505577d93338ea1399105c3c00007f6c1b3ef",
  "test": "1023fc9dbabe6f12b693140cd47eb1d7e67f76085a119cf1069f724ff771ba99",
  "ts-drift": "d18b132a9b0a47f66735ed031448f0503a5af9840521fb481c69db78d6dc4cc1",
  "udeps": "71da2c358e8873c1d9075d2410840afa7d476d50e01a8909716a25f51dbef38f",
  "upgrade-test": "5085c4d9086eb08446a2834e80a94fb0538d52e03fab110d3206f9ff17145053",
}

PINNED_CALLEE = {".github/workflows/upgrade.yml": "266534e2e341054912f4220623af7110a00b775b94a957cf0909001b183b128d"}

# sha256 of the workflow text before `jobs:` (the header comment, `on:`,
# `run-name`, `concurrency`, `permissions`, the workflow `env:`), taken from
# ci.yml at pin time.
PINNED_PREAMBLE = "540d92da36d78f82584861903b8087fd4d314b259bbadc63bba0c11ac90f31e8"


def pinned_preamble():
    return PINNED_PREAMBLE

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
        "text": "if: failure() && env.CI_INFRA_FAILURE == '' && steps.infra-1.outcome != 'failure' && steps.infra-2.outcome != 'failure' && steps.infra-3.outcome != 'failure' && (github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch')",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false"},
        },
    },
    {
        "text": "if: failure() && env.CI_INFRA_FAILURE == '' && steps.infra-1.outcome != 'failure' && (github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch')",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false"},
        },
    },
    {
        "text": "if: failure() && env.CI_INFRA_FAILURE == '' && steps.infra-1.outcome != 'failure' && steps.infra-2.outcome != 'failure' && (github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch')",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false"},
        },
    },
    {
        "text": "if: failure() && env.CI_INFRA_FAILURE == '' && steps.infra-1.outcome != 'failure' && steps.infra-2.outcome != 'failure' && steps.infra-3.outcome != 'failure' && steps.infra-4.outcome != 'failure' && (github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch')",
        "legs": {
            "core": {"pull_request": "false", "workflow_dispatch": "false"},
            "daemon": {"pull_request": "false", "workflow_dispatch": "false"},
            "cli": {"pull_request": "false", "workflow_dispatch": "false"},
            "": {"pull_request": "false", "workflow_dispatch": "false"},
        },
    },
    {
        "text": "if: failure() && env.CI_INFRA_FAILURE == '' && steps.infra-1.outcome != 'failure' && steps.infra-2.outcome != 'failure' && steps.infra-3.outcome != 'failure' && steps.infra-4.outcome != 'failure' && steps.infra-5.outcome != 'failure' && (github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch')",
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
