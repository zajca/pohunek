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
  "build-bins": "4b6f420e7a91884a3dedb9ac06b712c83c7fbd22fe00ff7312ee00f8ea7599c6",
  "build-tests": "fa460f03f432768c30793481c44c75b7a3e3ca4f527a20c5a53f168d270664e1",
  "darwin-check": "52b00915c39dcf66a5de42cae4388c0a1dd019ac2f303d98548ba5030f37a11c",
  "docs": "cae90e7b5d5faf47b6587832f8c267c265baadebbe6109cb6e102b51811c391c",
  "doctests": "ac58d854d5cbe8c5e6f2d4f1a51bc808dd94eddb30d08c0290656573f5d66c0f",
  "fast-tests": "860299f21260b143537f3e511daf7fc17e4c09f35e614cfe4f37103e3c449679",
  "feature-matrix": "aa5ee100347f8e163a3d12d241c1782636ac5d5ecc52a929c0ff1a855bde7c5a",
  "hermes-compatibility-macos": "a953afa63430c78ec4a455fb0b22e095744a963ced8b4bbc83acb7081abb4e32",
  "integration": "c87a52fe9c58fee123b50667e1ac0c058b376e8a1b49cdf993c8ce6f4c55af06",
  "macos-package": "065adb21e50a67d097a7ab569ba1f08ee52991a397fa06bd966604174ea75ef0",
  "macos-release-build": "4da092a45553db6610409afc5b7caeee8aca58e38c99a0166c51e509e691dba9",
  "macos-tier": "b6d5867fa5aeccaf431276e6f6cd1a3e697c5f8cbc7a60f0595a986224c7cc83",
  "platform-contracts": "ef71ed9e024edfce0cf6f58c575c94c9fad3f5260530421c032ba6b8d9bf2f29",  # PROBE-ONLY (#2199)
  "release-build": "116a9eff04cddbdb4f4fc419f6292be0f7221fd28f153907b0ea115d726d17f1",
  "sdk-macos": "30206a63cca0537df6e38025a794bb5fee4c1baba4c12075ed476a74ddbeccbe",
  "shear": "91fd4a3eba97ea08007c9203aeb3607395904db4fdd4799b799fdb30c98f2b76",
  "systemd-supervision": "136b7ca4ff89be58ac9bf88690141c2ddca660d72817d7b0ff8982fc9627da3a",
  "test": "57d245e3425664af417be6791a9dc8eead22f2c0eb2a2d09f21a5d5bb445600b",
  "ts-drift": "d18b132a9b0a47f66735ed031448f0503a5af9840521fb481c69db78d6dc4cc1",
  "udeps": "71da2c358e8873c1d9075d2410840afa7d476d50e01a8909716a25f51dbef38f",
  "upgrade-test": "5085c4d9086eb08446a2834e80a94fb0538d52e03fab110d3206f9ff17145053",
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

PINNED_PREAMBLE = "540d92da36d78f82584861903b8087fd4d314b259bbadc63bba0c11ac90f31e8"
