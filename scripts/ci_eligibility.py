#!/usr/bin/env python3
"""Shared source-eligibility rule for push reuse (issues #2203, #2204).

Both `scripts/ci-reuse` and `scripts/ci-evidence` decide from the same
GitHub Actions workflow-run payloads, so the rule lives here once: same
repository (never a fork), an allowed source event, a main-branch push
source, a tier=main dispatch source, run-level `success`, and freshness
against an injected clock. Never `cargo-audit`, never schedule or nightly.

The scripts load this module by path with importlib (the way the tests load
the scripts), because the checked-in scripts carry no `.py` suffix and share
no package.
"""

from __future__ import annotations

import datetime
import re


# Jobs whose build inputs live outside the tree: the advisory database, the
# selectors, the aggregators, the watchers. They are never reused, so the
# allowlist in `scripts/ci-reuse.json` must never name them.
NEVER_REUSED = frozenset({
    "audit", "changes", "test-selection", "package-selection",
    "package-version-bump", "test-selection-verdict", "test-duplicates",
    "reuse",
})


# The artifact a CI run uploads to record the tree it tested.
TESTED_TREE_ARTIFACT = re.compile(r"^tested-tree-([0-9a-f]{40})$")


def tested_trees(artifact_names) -> set[str]:
    """The trees named by the `tested-tree-<tree>` artifacts among
    `artifact_names`."""
    return {
        match.group(1)
        for name in artifact_names
        if (match := TESTED_TREE_ARTIFACT.fullmatch(str(name)))
    }


def recorded_tree_refusal(recorded: set[str], tree: str) -> str | None:
    """Why a run whose recorded tested trees are `recorded` did not test
    `tree`, or None when its only recorded tree is `tree`."""
    if recorded == {tree}:
        return None
    names = ", ".join(sorted(recorded)) or "none"
    return f"recorded tested tree {names}, expected {tree}"


def parse_moment(value) -> datetime.datetime | None:
    """An Actions timestamp as an aware datetime, or None when unreadable."""
    if not isinstance(value, str) or not value:
        return None
    try:
        moment = datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=datetime.timezone.utc)
    return moment


def run_tier(run: dict, workflow_path: str = ".github/workflows/ci.yml") -> str | None:
    """The CI tier a workflow run belongs to: `main` for a main-branch push
    or a tier=main dispatch, `nightly` for a schedule or tier=nightly
    dispatch, else None."""
    if (run.get("path") or "").split("@", 1)[0] != workflow_path:
        return None
    event = run.get("event")
    if event == "push" and run.get("head_branch") == "main":
        return "main"
    if event == "schedule":
        return "nightly"
    if event == "workflow_dispatch":
        title = run.get("display_title") or run.get("displayTitle") or ""
        for tier in ("main", "nightly"):
            marker = f"CI [tier={tier}]"
            if title == marker or title.startswith(marker + " "):
                return tier
    return None


def source_is_eligible(
    run: dict,
    repo: str,
    workflow_path: str,
    now: datetime.datetime,
    max_age: datetime.timedelta,
    allowed_events: tuple | list,
) -> bool:
    """Whether a workflow run may supply reused jobs: it is a run of this
    repository (never a fork), its event is allowed, a push source is a main
    push, a dispatch source is a tier=main dispatch, its run-level
    conclusion is `success`, and it was created within `max_age` of `now`.

    Never `cargo-audit` (the advisory database changes independently of the
    tree), never schedule or nightly. [`run_source_reasons`] is the rule.
    """
    return not run_source_reasons(run, repo, workflow_path, now, max_age, allowed_events)


def consumer_matches(run: dict, consumer_event: str, sha: str) -> bool:
    """Whether a candidate run answers this consumer: a push consumer may
    reuse another commit with the same tree; a delta dispatch only accepts a
    source on the same commit (`head_sha == GITHUB_SHA`)."""
    if consumer_event == "push":
        return True
    if consumer_event == "workflow_dispatch":
        return run.get("head_sha") == sha
    return False


def leg_is_fresh(
    completed_at,
    now: datetime.datetime,
    max_age: datetime.timedelta,
) -> bool:
    """Whether one source leg completed within the reuse window of `now`."""
    completed = parse_moment(completed_at)
    if completed is None:
        return False
    return now - completed <= max_age


def job_is_reused(job_id: str, conclusion, fresh: bool) -> bool:
    """Whether one source leg supplies its job: the job is allowlisted
    (never `cargo-audit`), the leg concluded `success` (a skipped or failed
    leg is never reused), and the leg is fresh. The callers decide the
    selection-dependent jobs themselves, because reading a source run's
    selection needs the network."""
    if job_id in NEVER_REUSED:
        return False
    if conclusion != "success":
        return False
    if not fresh:
        return False
    return True


def run_source_reasons(
    source: dict,
    repo: str,
    workflow_path: str,
    evidence_created,
    max_age: datetime.timedelta,
    allowed_events,
) -> list[str]:
    """Why `source` fails the shared run-level rule, one line per guard.

    This is the run-level rule: it names each failed guard so a refusal says
    which one failed. A caller passes the clock the window reads against (the
    evidence run's `created_at`, or the decision moment); the age is one-sided.
    An unreadable source timestamp is reported as `has no readable created_at`
    or as older than the window."""
    reasons: list[str] = []
    if (source.get("repository") or {}).get("full_name") != repo or (
        source.get("head_repository") or {}
    ).get("full_name") != repo:
        reasons.append("is not a run of this repository")
    if source.get("event") not in allowed_events:
        reasons.append(
            f"ran on event {source.get('event')!r}, "
            "which the candidate does not allow for reuse"
        )
    event = source.get("event")
    if event == "schedule":
        reasons.append("is a schedule run, which is never reused")
    if event == "push" and run_tier(source, workflow_path) != "main":
        reasons.append("is not a main push")
    if event == "workflow_dispatch" and run_tier(source, workflow_path) != "main":
        reasons.append("is not a tier=main dispatch")
    if source.get("conclusion") != "success" or source.get("status") != "completed":
        reasons.append("did not conclude success")
    moment = parse_moment(source.get("created_at"))
    if evidence_created is None:
        if moment is None:
            reasons.append("has no readable created_at")
    elif moment is None or evidence_created - moment > max_age:
        reasons.append("is older than the reuse window")
    return reasons


def evidence_commit_match(source: dict, sha: str) -> str | None:
    """Why `source` does not answer a dispatch consumer, or None when the
    source's commit is the evidence run's commit."""
    if source.get("head_sha") == sha:
        return None
    return f"has head SHA {source.get('head_sha')}, expected {sha}"


def evidence_tree_match(
    source: dict, sha: str, tree: str, lookup, lookup_errors: tuple[type[Exception], ...]
) -> str | None:
    """Why `source` does not answer a push consumer, or None when the
    source's tree is the evidence tree. A `pull_request` source is matched
    by its recorded tested tree (the caller compares it); any other event
    matches by commit or, via `lookup`, by the tree of its head commit. A
    lookup failure of a type in `lookup_errors` names the cause instead of
    reading as a mismatch; any other exception is a bug and propagates."""
    if source.get("event") == "pull_request":
        return None  # the caller compares the recorded tree
    if source.get("head_sha") == sha:
        return None
    head_sha = source.get("head_sha")
    if not head_sha:
        return "has neither the commit nor the tree"
    try:
        source_tree = lookup(head_sha)
    except lookup_errors as error:
        return f"tree lookup failed: {error}"
    if source_tree == tree:
        return None
    return "has neither the commit nor the tree"


def source_is_reused(
    source: dict,
    job_id: str,
    newest: dict,
    evidence_created,
    window: datetime.timedelta,
) -> str | None:
    """Why the newest leg of `job_id` in `source` is not reused, or None
    when it is: the leg concluded `success` (never skipped or failed) and is
    fresh against the evidence run's `created_at`. The caller appends the
    source URL and the evidence URL."""
    conclusion = newest.get("conclusion")
    if conclusion != "success":
        return f"concluded {conclusion}"
    if evidence_created is None:
        return "has no readable created_at"
    completed = parse_moment(newest.get("completed_at"))
    if completed is None or evidence_created - completed > window:
        return "is older than the reuse window"
    if not job_is_reused(job_id, conclusion, True):
        if job_id in NEVER_REUSED:
            return "is never reused"
        return "is not eligible for reuse"
    return None
