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
  "build-bins": "6c171efbfb02901ff912ad89b6c3f795ef8b9faef3029ed7d70063ac623d995b",
  "build-tests": "b53949dfff5cabfd7e71bddfebdf128fcc8e67910967a9acc68152ebaebf40f3",
  "darwin-check": "36b75ba13f82a5d877bb4d8f9775fa6fda6a8f51bae99e5642184c7aea002215",
  "docs": "f4e34f2e887eed6003c852ada778aff0801f115aa0aa8a4f103dec8fa889ee9f",
  "doctests": "13600a89e29e1e7a3e33e3cd0c6119d30aac2fb420a030bd7fd71dfb6d6181c6",
  "fast-tests": "fe6488ff534aa0b64b8ef81223d476c9a5893100f691e22ae70630b059a5f05f",
  "feature-matrix": "aa5ee100347f8e163a3d12d241c1782636ac5d5ecc52a929c0ff1a855bde7c5a",
  "hermes-compatibility-macos": "fc56bda478692dc538bed1cf48204d9c997329901de44b10522c236cbf07dbc3",
  "integration": "7425c112af2339e8cf362d43eb6dfc653af31be5aaa38c943520b64e371642ec",
  "macos-package": "065adb21e50a67d097a7ab569ba1f08ee52991a397fa06bd966604174ea75ef0",
  "macos-release-build": "f3eb4ba4b0ae3bf5a28b857f8e21f4bac90e2fa8aab6b426a00238647464e171",
  "macos-tier": "b6d5867fa5aeccaf431276e6f6cd1a3e697c5f8cbc7a60f0595a986224c7cc83",
  "platform-contracts": "398cb156f68fb815ce3f58f516db3d65d37767320de23a3aa71088b7988d8e8e",
  "release-build": "116a9eff04cddbdb4f4fc419f6292be0f7221fd28f153907b0ea115d726d17f1",
  "sdk-macos": "30206a63cca0537df6e38025a794bb5fee4c1baba4c12075ed476a74ddbeccbe",
  "shear": "31218413e49db0d536a0600d22804004c201e39e623ac84bf13436cbf613511e",
  "systemd-supervision": "136b7ca4ff89be58ac9bf88690141c2ddca660d72817d7b0ff8982fc9627da3a",
  "test": "82723fd02f898762605c6824c1ab4b32097e021dc711b1e92129b5427556e478",
  "ts-drift": "d18b132a9b0a47f66735ed031448f0503a5af9840521fb481c69db78d6dc4cc1",
  "udeps": "71da2c358e8873c1d9075d2410840afa7d476d50e01a8909716a25f51dbef38f",
  "upgrade-test": "fa98403dc8fabb96c98899c166f6bdb827c65c11d382c6918aeef65b97c93971",
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

def pinned_preamble():
    # sha256 of the workflow text before `jobs:` (env:, defaults:), taken
    # from ci.yml at pin time.
    return PINNED_PREAMBLE

PINNED_PREAMBLE = "eda784daf9f79bd9773488c7aaf5acafc3b732143129d6d4ccf4149490664149"
