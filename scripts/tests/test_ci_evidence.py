"""CLI scenarios for exact-commit GitHub Actions tier evidence."""

import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "ci-evidence"
DEFAULT_CONFIG = ROOT / "scripts" / "ci-evidence.json"
SHA = "a" * 40
OTHER_SHA = "b" * 40
THIRD_SHA = "e" * 40
TREE = "c" * 40
OTHER_TREE = "d" * 40
REPO = "example/pohunek"
WORKFLOW_PATH = ".github/workflows/ci.yml"
REQUIRED_JOBS = ["required check", "matrix check (linux)"]


def static_matrix(block):
    """The combinations of a job's literal `strategy.matrix`, each a dict in
    declaration order; None when the job has no matrix, "dynamic" when the
    matrix is an expression such as `fromJSON(...)`. A key whose expression
    ends in a literal JSON list fallback (`|| '[...]') }}`) takes that list,
    the values of the run that selects nothing away."""
    head = re.search(r"(?m)^      matrix:(.*)$", block)
    if head is None:
        return None
    if head.group(1).strip():
        return "dynamic"
    lists, include, current = {}, [], None
    for line in block[head.end():].splitlines()[1:]:
        if line.strip().startswith("#") or not line.strip():
            continue
        if not line.startswith("        "):
            break
        entry = re.fullmatch(r"( *)(- )?([a-z_]+): *(.*)", line)
        if entry is None:
            continue
        key, value = entry.group(3), entry.group(4).strip()
        if key == "include" and not entry.group(2):
            continue
        if value.startswith("${{"):
            fallback = re.search(r"\|\| '(\[[^']*\])'\) \}\}\Z", value)
            if fallback is None:
                return "dynamic"
            lists[key] = json.loads(fallback.group(1))
        elif value.startswith("["):
            lists[key] = [item.strip().strip('"') for item in value.strip("[]").split(",")]
        elif entry.group(2):
            current = {key: value.strip('"')}
            include.append(current)
        elif current is not None:
            current[key] = value.strip('"')
    combos = [{}]
    for key, values in lists.items():
        combos = [dict(combo, **{key: value}) for combo in combos for value in values]
    return include if include and not lists else combos


def workflow_jobs(path):
    """Returns (name, matrix, called_workflow) for each job of a workflow.

    A job without `name:` is shown under its id; `matrix` is what
    `static_matrix` returns; `called_workflow` is the local reusable workflow
    the job `uses:`, or None.
    """
    job_text = (ROOT / path).read_text().split("jobs:\n", 1)[1]
    matches = list(re.finditer(r"(?m)^  ([a-z][a-z0-9-]*):\n", job_text))
    jobs = []
    for index, match in enumerate(matches):
        end = matches[index + 1].start() if index + 1 < len(matches) else len(job_text)
        block = job_text[match.end():end]
        name = re.search(r"(?m)^    name: (.+)$", block)
        uses = re.search(r"(?m)^    uses: \./(\.github/workflows/\S+)$", block)
        jobs.append((
            name.group(1) if name else match.group(1),
            static_matrix(block),
            uses.group(1) if uses else None,
        ))
    return jobs


def job_name_pattern(name, matrix):
    """Matches the names Actions shows for a job.

    With a literal matrix each combination gives exact names: `${{ matrix.<key> }}`
    takes the combination's value, and a name without an expression gets the
    combination's values appended in parentheses. With a dynamic matrix an
    expression stands for any value.
    """
    if matrix is None or matrix == "dynamic":
        pattern = ".+?".join(re.escape(part) for part in re.split(r"\$\{\{.*?\}\}", name))
        if matrix == "dynamic" and "${{" not in name:
            pattern += r"( \(.+\))?"
        return re.compile(pattern + r"\Z")
    names = []
    for combo in matrix:
        if "${{" in name:
            names.append(re.sub(
                r"\$\{\{ *matrix\.([a-z_]+) *\}\}",
                lambda key: combo.get(key.group(1), key.group(0)),
                name,
            ))
        else:
            names.append(f"{name} ({', '.join(combo.values())})")
    return re.compile("(?:" + "|".join(re.escape(shown) for shown in names) + r")\Z")


def names_a_workflow_job(required, workflow_path):
    """Whether a required job name, `caller` or `caller / called job`, is a job
    Actions can show for the workflow, looking through a local reusable
    workflow for the part after ` / `."""
    caller, _, called = required.partition(" / ")
    for name, matrix, uses in workflow_jobs(workflow_path):
        if not job_name_pattern(name, matrix).match(caller):
            continue
        if not called:
            return True
        if uses and any(
            job_name_pattern(inner, inner_matrix).match(called)
            for inner, inner_matrix, _ in workflow_jobs(uses)
        ):
            return True
    return False


FAKE_GH = r'''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

fixture_path = Path(os.environ["GH_FIXTURE"])
fixture = json.loads(fixture_path.read_text())
args = sys.argv[1:]
with Path(os.environ["GH_CALLS"]).open("a") as stream:
    stream.write(json.dumps(args) + "\n")

if args[:2] == ["workflow", "run"]:
    if fixture.get("dispatch_error"):
        print(fixture["dispatch_error"], file=sys.stderr)
        raise SystemExit(1)
    fixture["dispatches"] = fixture.get("dispatches", []) + [args]
    fixture_path.write_text(json.dumps(fixture))
    raise SystemExit(0)

if args[0] != "api":
    raise SystemExit("unexpected gh invocation: " + repr(args))

endpoint = next((arg for arg in args[1:] if arg.startswith("repos/")), "")
if "/actions/workflows/" in endpoint and any(arg.startswith("created=") for arg in args):
    # The tree lookup lists recent dispatch runs; it has its own fixture so the
    # dispatch flow's query sequence is unaffected.
    print(json.dumps([{"workflow_runs": fixture.get("tree_runs", [])}]))
elif "/actions/workflows/" in endpoint:
    if "event=workflow_dispatch" in args:
        sequence = fixture.get("dispatch_pages", [])
        index = fixture.get("dispatch_queries", 0)
        query_key = "dispatch_queries"
    else:
        sequence = fixture.get("run_pages", [])
        index = fixture.get("run_queries", 0)
        query_key = "run_queries"
    page = sequence[min(index, len(sequence) - 1)] if sequence else {"workflow_runs": []}
    dispatch_call = next((call for call in fixture.get("dispatches", []) if call[:2] == ["workflow", "run"]), None)
    dispatch_id = None
    if dispatch_call:
        dispatch_id = next((arg.split("=", 1)[1] for arg in dispatch_call if arg.startswith("dispatch_id=")), None)
    for run in page.get("workflow_runs", []):
        if "__REQUEST_ID__" in (run.get("display_title") or ""):
            run["display_title"] = run["display_title"].replace("__REQUEST_ID__", dispatch_id or "missing")
    fixture[query_key] = index + 1
    fixture_path.write_text(json.dumps(fixture))
    print(json.dumps([page]))
elif "/git/commits/" in endpoint:
    sha = endpoint.rsplit("/", 1)[1]
    if sha in fixture.get("unknown_commits", []):
        print("gh: Not Found (HTTP 404)", file=sys.stderr)
        raise SystemExit(1)
    print(json.dumps({"sha": sha, "tree": {"sha": fixture.get("trees", {}).get(sha, "f" * 40)}}))
elif "/commits" in endpoint:
    print(json.dumps([{"sha": fixture.get("resolved_ref")}]))
elif endpoint.endswith("/artifacts"):
    run_id = endpoint.split("/")[-2]
    names = fixture.get("artifacts", {}).get(run_id, [])
    print(json.dumps([{"artifacts": [{"name": name} for name in names]}]))
elif "/actions/runs/" in endpoint:
    run_id = endpoint.split("/")[-2]
    page = fixture.get("jobs_by_run", {}).get(run_id, fixture.get("jobs_page", {"jobs": []}))
    print(json.dumps([page]))
else:
    raise SystemExit("unexpected Actions API endpoint: " + repr(endpoint))
'''


def run_doc(
    *,
    sha=SHA,
    event="push",
    branch="main",
    title="CI",
    path=f"{WORKFLOW_PATH}@refs/heads/main",
    status="completed",
    conclusion="success",
    run_id=456,
    repo=None,
    head_repo=None,
):
    document = {
        "id": run_id,
        "run_number": run_id,
        "run_attempt": 1,
        "head_sha": sha,
        "head_branch": branch,
        "path": path,
        "event": event,
        "display_title": title,
        "status": status,
        "conclusion": conclusion,
        "html_url": f"https://github.com/example/pohunek/actions/runs/{run_id}",
    }
    if repo is not None:
        document["repository"] = {"full_name": repo}
        document["head_repository"] = {"full_name": head_repo or repo}
    return document


def dispatch_doc(*, sha=OTHER_SHA, tier="main", run_id=789, head_repo=REPO, **values):
    """A completed dispatch run of `tier` on `sha` in the evidence repository."""
    return run_doc(
        sha=sha,
        event="workflow_dispatch",
        branch="zajca/feature",
        title=f"CI [tier={tier}]",
        path=f"{WORKFLOW_PATH}@refs/heads/zajca/feature",
        run_id=run_id,
        repo=REPO,
        head_repo=head_repo,
        **values,
    )


def page(runs):
    return {"workflow_runs": runs}


def jobs_page(*, overrides=None, omit=(), extra=()):
    overrides = overrides or {}
    names = [name for name in REQUIRED_JOBS if name not in omit]
    jobs = [
        {"name": name, "status": "completed", "conclusion": overrides.get(name, "success")}
        for name in names
    ]
    for job in extra:
        jobs = [existing for existing in jobs if existing["name"] != job["name"]]
        jobs.append(job)
    return {
        "jobs": jobs
    }


class CiEvidenceCliTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.bin_dir = self.directory / "bin"
        self.bin_dir.mkdir()
        gh = self.bin_dir / "gh"
        gh.write_text(FAKE_GH)
        gh.chmod(0o755)
        self.fixture = self.directory / "fixture.json"
        self.calls = self.directory / "calls.jsonl"
        self.config = self.directory / "config.json"
        self.config_data = {
            "workflow_path": WORKFLOW_PATH,
            "workflow_file": "ci.yml",
            "wait_timeout_seconds": 3,
            "poll_interval_seconds": 1,
            "tree_match_max_age_days": 14,
            "required_jobs": {"main": list(REQUIRED_JOBS), "nightly": list(REQUIRED_JOBS)},
            "required_steps": {"main": [], "nightly": []},
        }
        self.write_config()

    def write_config(self):
        self.config.write_text(json.dumps(self.config_data))

    def require_nightly_release_evidence(self):
        self.config_data["required_jobs"]["nightly"].append(
            "macOS package install and upgrade (macos-15)",
        )
        self.config_data["required_steps"]["nightly"] = [
            {
                "job": "macOS package install and upgrade (macos-15)",
                "step": "Install, upgrade, and uninstall from the archives",
            },
            {
                "job": "macOS package install and upgrade (macos-15)",
                "step": "Rebuild from a cold release directory and compare",
            },
        ]
        self.write_config()

    @staticmethod
    def nightly_macos_jobs(
        *,
        acceptance_conclusion="success",
        cold_conclusion="success",
        omit_acceptance=False,
        omit_cold=False,
    ):
        steps = []
        if not omit_acceptance:
            steps.append({
                "name": "Install, upgrade, and uninstall from the archives",
                "conclusion": acceptance_conclusion,
            })
        if not omit_cold:
            steps.append({
                "name": "Rebuild from a cold release directory and compare",
                "conclusion": cold_conclusion,
            })
        return [
            {
                "name": "macOS package install and upgrade (macos-15)",
                "status": "completed",
                "conclusion": "success",
                "steps": steps,
            },
        ]

    def write_fixture(self, **values):
        base = {
            "resolved_ref": SHA,
            "run_pages": [page([])],
            "jobs_page": jobs_page(),
        }
        base.update(values)
        self.fixture.write_text(json.dumps(base))

    def invoke(self, tier="main", *extra, config=None, sha=SHA):
        env = os.environ.copy()
        env.update({
            "PATH": f"{self.bin_dir}{os.pathsep}{env['PATH']}",
            "GH_FIXTURE": str(self.fixture),
            "GH_CALLS": str(self.calls),
        })
        return subprocess.run(
            [str(SCRIPT), "--repo", "example/pohunek", "--sha", sha,
             "--tier", tier, "--config", str(config or self.config), *extra],
            capture_output=True,
            text=True,
            check=False,
            cwd=self.directory,
            env=env,
        )

    def calls_json(self):
        return [json.loads(line) for line in self.calls.read_text().splitlines()]

    def test_checked_in_job_inventory_matches_workflow_names_and_tier_scope(self):
        config = json.loads(DEFAULT_CONFIG.read_text())
        workflow = (ROOT / config["workflow_path"]).read_text()
        job_text = workflow.split("jobs:\n", 1)[1]
        matches = list(re.finditer(r"(?m)^  ([a-z][a-z0-9-]*):\n", job_text))
        workflow_names = set()
        for index, match in enumerate(matches):
            end = matches[index + 1].start() if index + 1 < len(matches) else len(job_text)
            block = job_text[match.end():end]
            name_match = re.search(r"(?m)^    name: (.+)$", block)
            if name_match is None:
                continue
            name = name_match.group(1)
            if name == "tests (${{ matrix.shard }}, fast)":
                workflow_names.update(f"tests ({shard}, fast)" for shard in ("core", "daemon", "relay", "cli"))
            else:
                workflow_names.add(name)
        for tier in ("main", "nightly"):
            for name in config["required_jobs"][tier]:
                self.assertIn(name.split(" / ", 1)[0], workflow_names)
        # macOS 15 is the only package acceptance leg, in both tiers.
        for tier in ("main", "nightly"):
            self.assertEqual(
                [name for name in config["required_jobs"][tier] if name.startswith("macOS package install")],
                ["macOS package install and upgrade (macos-15)"],
            )
        self.assertIn(
            {
                "job": "macOS package install and upgrade (macos-15)",
                "step": "Install, upgrade, and uninstall from the archives",
            },
            config["required_steps"]["nightly"],
        )
        self.assertNotIn("tests (relay DB, PostgreSQL)", config["required_jobs"]["main"])
        self.assertIn("tests (relay DB, PostgreSQL)", config["required_jobs"]["nightly"])
        macos_job = workflow.split("  macos-package:\n", 1)[1].split("\n  test:\n", 1)[0]
        self.assertNotIn("continue-on-error", macos_job)

    def test_every_required_job_names_a_job_of_the_workflow_or_its_reusable_workflows(self):
        config = json.loads(DEFAULT_CONFIG.read_text())
        workflow = config["workflow_path"]
        for tier in ("main", "nightly"):
            for name in config["required_jobs"][tier]:
                with self.subTest(tier=tier, job=name):
                    self.assertTrue(names_a_workflow_job(name, workflow))
        # A required name that a matrix rename left behind, or a matrix value
        # the workflow does not run, is refused.
        smoke = "release rehearsal (evidence, assembly, archive smoke) / Archive smoke"
        self.assertFalse(names_a_workflow_job(f"{smoke} (network-isolated)", workflow))
        self.assertFalse(names_a_workflow_job(f"{smoke} (network-isolated, macos-13)", workflow))
        self.assertFalse(names_a_workflow_job(f"{smoke} (network-isolated, ubuntu-latest) extra", workflow))
        consumer = "release rehearsal (evidence, assembly, archive smoke) / Build the release consumer executable"
        self.assertFalse(names_a_workflow_job(f"{consumer} (macos-13)", workflow))
        self.assertFalse(names_a_workflow_job("tests (gpu, fast)", workflow))
        # The release rehearsal is nightly-tier evidence: both legs of its
        # archive smoke gate a release through the nightly run, and the main
        # tier, which never starts the rehearsal, requires none of its jobs.
        for leg in ("ubuntu-latest", "macos-15"):
            self.assertIn(f"{smoke} (network-isolated, {leg})", config["required_jobs"]["nightly"])
        rehearsal = [name for name in config["required_jobs"]["nightly"] if name.startswith("release rehearsal ")]
        self.assertTrue(rehearsal)
        self.assertEqual(
            [name for name in config["required_jobs"]["main"] if name.startswith("release rehearsal ")],
            [],
        )
        # Apart from the rehearsal, the nightly tier differs from main only by
        # its nightly-only relay database suite.
        nightly_only = {"tests (relay DB, PostgreSQL)"}
        self.assertEqual(
            set(config["required_jobs"]["main"]),
            set(config["required_jobs"]["nightly"]) - set(rehearsal) - nightly_only,
        )

    def test_real_main_jobs_fixture_satisfies_the_inventory(self):
        fixture = json.loads(
            (ROOT / "scripts/tests/fixtures/ci-evidence-main-jobs.json").read_text()
        )
        config = json.loads(DEFAULT_CONFIG.read_text())
        successful = {job["name"] for job in fixture["jobs"] if job["conclusion"] == "success"}
        skipped = {job["name"] for job in fixture["jobs"] if job["conclusion"] == "skipped"}
        self.assertFalse(set(config["required_jobs"]["main"]) - successful)
        self.assertTrue(skipped)
        self.assertNotIn("tests (relay DB, PostgreSQL)", config["required_jobs"]["main"])
        self.write_fixture(
            run_pages=[page([run_doc(sha="a3523d7365677852cf9321499e9f4cfa65a749fd")])],
            jobs_page=fixture,
        )
        result = self.invoke(
            "main", "--sha", "a3523d7365677852cf9321499e9f4cfa65a749fd",
            config=DEFAULT_CONFIG,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_successful_main_push_requires_exact_workflow_sha_and_all_jobs(self):
        self.write_fixture(
            run_pages=[page([run_doc()])],
            jobs_page=jobs_page(),
        )
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(SHA, result.stdout)
        self.assertIn("/actions/runs/456", result.stdout)
        api_calls = [call for call in self.calls_json() if call[0] == "api"]
        self.assertTrue(api_calls)
        self.assertTrue(all(
            "--method" in call and call[call.index("--method") + 1] == "GET"
            for call in api_calls
        ))

    def test_nightly_accepts_schedule_on_exact_sha(self):
        self.require_nightly_release_evidence()
        self.write_fixture(
            run_pages=[page([run_doc(event="schedule", branch="main")])],
            jobs_page=jobs_page(extra=self.nightly_macos_jobs()),
        )
        result = self.invoke("nightly")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_nightly_requires_green_macos15_acceptance(self):
        self.require_nightly_release_evidence()
        self.write_fixture(
            run_pages=[page([run_doc(event="schedule", branch="main")])],
            jobs_page=jobs_page(extra=self.nightly_macos_jobs(
                acceptance_conclusion="failure"
            )),
        )
        result = self.invoke("nightly")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("conclusion=failure", result.stderr)
        self.assertIn("/actions/runs/456", result.stderr)
        for macos_jobs in (
            self.nightly_macos_jobs(acceptance_conclusion="skipped"),
            self.nightly_macos_jobs(omit_acceptance=True),
        ):
            with self.subTest(macos_jobs=macos_jobs):
                self.write_fixture(
                    run_pages=[page([run_doc(event="schedule", branch="main")])],
                    jobs_page=jobs_page(extra=macos_jobs),
                )
                result = self.invoke("nightly")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("Install, upgrade, and uninstall from the archives", result.stderr)

    def test_nightly_refuses_missing_skipped_or_failed_cold_rebuild_step(self):
        self.require_nightly_release_evidence()
        cases = (
            self.nightly_macos_jobs(cold_conclusion="skipped"),
            self.nightly_macos_jobs(cold_conclusion="failure"),
            self.nightly_macos_jobs(omit_cold=True),
        )
        for macos_jobs in cases:
            with self.subTest(macos_jobs=macos_jobs):
                self.write_fixture(
                    run_pages=[page([run_doc(event="schedule", branch="main")])],
                    jobs_page=jobs_page(extra=macos_jobs),
                )
                result = self.invoke("nightly")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("required step", result.stderr)

    def tree_fixture(self, **values):
        """A fixture where SHA and OTHER_SHA have the same tree, and dispatch
        run 789 on OTHER_SHA recorded that tree with every required job green."""
        base = {
            "trees": {SHA: TREE, OTHER_SHA: TREE},
            "tree_runs": [dispatch_doc()],
            "artifacts": {"789": [f"tested-tree-{TREE}"]},
            "jobs_by_run": {"789": jobs_page()},
        }
        base.update(values)
        self.write_fixture(**base)

    def test_a_run_on_the_commit_matches_by_sha(self):
        self.write_fixture(run_pages=[page([run_doc()])], jobs_page=jobs_page())
        result = self.invoke("main", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["match"], "sha")
        self.assertEqual(report["run_sha"], SHA)
        self.assertNotIn("tree", report)
        # A completed run on the commit decides without a tree lookup.
        self.assertFalse(any("/git/commits/" in " ".join(call) for call in self.calls_json()))

    def test_a_dispatch_run_on_another_branch_with_the_same_tree_matches_by_tree(self):
        # The intended case (#1215 D4): a tier dispatch on a pull request's
        # up-to-date head, a commit on another branch, is evidence for the
        # merge commit of the same tree.
        self.tree_fixture()
        result = self.invoke("main", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["match"], "tree")
        self.assertEqual(report["tree"], TREE)
        self.assertEqual(report["run_sha"], OTHER_SHA)
        self.assertEqual(report["run_id"], 789)
        text = self.invoke("main")
        self.assertEqual(text.returncode, 0, text.stderr)
        self.assertIn(f"by tree {TREE} (dispatch run on {OTHER_SHA})", text.stdout)
        self.assertIn("/actions/runs/789", text.stdout)
        # The lookup is bounded by tree_match_max_age_days.
        listing = next(
            call for call in self.calls_json()
            if any(arg.startswith("created=") for arg in call)
        )
        self.assertIn("event=workflow_dispatch", listing)
        since = next(arg for arg in listing if arg.startswith("created="))
        self.assertRegex(since, r"^created=>=\d{4}-\d{2}-\d{2}$")

    def test_a_tree_match_does_not_wait_for_a_running_push_on_the_commit(self):
        running = run_doc(status="in_progress", conclusion=None)
        self.tree_fixture(run_pages=[page([running])])
        result = self.invoke("main", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["match"], "tree")

    def test_a_tree_match_never_hides_a_failed_run_on_the_commit(self):
        failed = run_doc(conclusion="failure")
        self.tree_fixture(
            run_pages=[page([failed])],
            jobs_page=jobs_page(overrides={"required check": "failure"}),
        )
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("workflow conclusion is failure", result.stderr)
        self.assertIn("/actions/runs/456", result.stderr)

    def test_a_tree_match_never_hides_a_failed_run_behind_a_running_rerun(self):
        failed = run_doc(conclusion="failure")
        rerun = run_doc(status="in_progress", conclusion=None, run_id=457)
        self.tree_fixture(
            run_pages=[page([failed, rerun])],
            jobs_page=jobs_page(overrides={"required check": "failure"}),
        )
        result = self.invoke("main", "--json")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("timed out", result.stderr)
        self.assertFalse(any(
            arg.startswith("repos/example/pohunek/actions/runs/789/")
            for call in self.calls_json() for arg in call
        ))

    def test_a_tree_match_never_hides_a_failed_attempt_of_a_rerun(self):
        # A re-run shows only its newest attempt; the failed first attempt is
        # implied by run_attempt.
        rerun = run_doc(status="in_progress", conclusion=None)
        rerun["run_attempt"] = 2
        self.tree_fixture(run_pages=[page([rerun])])
        result = self.invoke("main", "--json")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("timed out", result.stderr)

    def test_the_tree_of_an_unpushed_local_commit_is_read_from_the_checkout(self):
        head = subprocess.run(
            ["git", "-C", str(ROOT), "rev-parse", "HEAD"],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
        tree = subprocess.run(
            ["git", "-C", str(ROOT), "rev-parse", "HEAD^{tree}"],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
        self.tree_fixture(
            trees={OTHER_SHA: tree},
            unknown_commits=[head],
            artifacts={"789": [f"tested-tree-{tree}"]},
        )
        result = self.invoke("main", "--json", sha=head)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual((report["match"], report["tree"]), ("tree", tree))
        self.assertFalse(any(f"/git/commits/{head}" in arg for call in self.calls_json() for arg in call))

    def test_a_failed_tree_lookup_leaves_the_decision_to_the_commit(self):
        self.tree_fixture(unknown_commits=[SHA])
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no main tier run exists", result.stderr)
        self.assertIn("tree lookup failed", result.stderr)

    def test_a_run_on_the_commit_that_completes_during_the_tree_match_decides(self):
        # The run on SHA is still running when the tree match is read and has
        # failed by the time the match would be accepted.
        running = run_doc(status="in_progress", conclusion=None)
        failed = run_doc(conclusion="failure")
        self.tree_fixture(
            run_pages=[page([running]), page([failed])],
            jobs_page=jobs_page(overrides={"required check": "failure"}),
        )
        result = self.invoke("main", "--json")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("workflow conclusion is failure", result.stderr)
        self.assertIn("/actions/runs/456", result.stderr)

    def test_a_newer_failed_run_of_the_tree_blocks_an_older_passing_one(self):
        newer = dispatch_doc(sha=THIRD_SHA, run_id=790, conclusion="failure")
        self.tree_fixture(
            trees={SHA: TREE, OTHER_SHA: TREE, THIRD_SHA: TREE},
            tree_runs=[dispatch_doc(), newer],
            artifacts={"789": [f"tested-tree-{TREE}"], "790": [f"tested-tree-{TREE}"]},
            jobs_by_run={
                "789": jobs_page(),
                "790": jobs_page(overrides={"required check": "failure"}),
            },
        )
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tree match https://github.com/example/pohunek/actions/runs/790 failed", result.stderr)
        self.assertFalse(any(
            arg.startswith("repos/example/pohunek/actions/runs/789/")
            for call in self.calls_json() for arg in call
        ))

    def test_a_newer_run_of_the_tree_that_is_still_running_blocks_an_older_one(self):
        for attempt in (1, 2):
            with self.subTest(attempt=attempt):
                newer = dispatch_doc(sha=THIRD_SHA, run_id=790, status="in_progress", conclusion=None)
                newer["run_attempt"] = attempt
                self.tree_fixture(
                    trees={SHA: TREE, OTHER_SHA: TREE, THIRD_SHA: TREE},
                    tree_runs=[dispatch_doc(), newer],
                )
                result = self.invoke("main")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"the newest run of tree {TREE}", result.stderr)
                self.assertIn("/actions/runs/790, is still in_progress", result.stderr)

    def test_a_failed_lookup_of_an_older_run_does_not_block_the_newest_match(self):
        older = dispatch_doc(sha=THIRD_SHA, run_id=700)
        self.tree_fixture(tree_runs=[older, dispatch_doc()], unknown_commits=[THIRD_SHA])
        result = self.invoke("main", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["run_id"], 789)

    def test_a_failed_lookup_of_a_newer_run_refuses_the_older_match(self):
        newer = dispatch_doc(sha=THIRD_SHA, run_id=790)
        self.tree_fixture(tree_runs=[dispatch_doc(), newer], unknown_commits=[THIRD_SHA])
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("may not be the newest run of tree", result.stderr)
        self.assertIn("/actions/runs/790", result.stderr)

    def test_a_tree_match_with_a_skipped_required_job_is_refused(self):
        self.tree_fixture(
            jobs_by_run={"789": jobs_page(overrides={"matrix check (linux)": "skipped"})},
        )
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no main tier run exists", result.stderr)
        self.assertIn("tree match https://github.com/example/pohunek/actions/runs/789 failed", result.stderr)
        self.assertIn("required job matrix check (linux): skipped", result.stderr)

    def test_a_tree_match_from_a_fork_head_is_refused(self):
        self.tree_fixture(tree_runs=[dispatch_doc(head_repo="someone/pohunek")])
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no main tier run exists", result.stderr)
        # The fork-head run is dropped before its record or jobs are read.
        self.assertFalse(any(
            arg.startswith("repos/example/pohunek/actions/runs/789/")
            for call in self.calls_json() for arg in call
        ))

    def test_only_dispatch_runs_match_by_tree(self):
        for event, branch in (("pull_request", "zajca/feature"), ("push", "main"), ("schedule", "main")):
            with self.subTest(event=event):
                other = run_doc(
                    sha=OTHER_SHA, event=event, branch=branch, run_id=789, repo=REPO,
                )
                self.tree_fixture(tree_runs=[other])
                result = self.invoke("main")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("no main tier run exists", result.stderr)

    def test_a_tree_match_without_its_recorded_tree_is_refused(self):
        for names, shown in (([], "none"), ([f"tested-tree-{OTHER_TREE}"], OTHER_TREE)):
            with self.subTest(names=names):
                self.tree_fixture(artifacts={"789": names})
                result = self.invoke("main")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"recorded tested tree {shown}, expected {TREE}", result.stderr)

    def test_a_dispatch_run_of_another_tree_does_not_match(self):
        self.tree_fixture(trees={SHA: TREE, OTHER_SHA: OTHER_TREE})
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no main tier run exists", result.stderr)
        self.assertNotIn("tree match", result.stderr)

    def test_a_tree_match_counts_only_for_its_own_tier(self):
        self.tree_fixture(tree_runs=[dispatch_doc(tier="nightly")])
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no main tier run exists", result.stderr)

    def test_the_tree_lookup_window_stays_within_the_record_retention(self):
        config = json.loads(DEFAULT_CONFIG.read_text())
        workflow = (ROOT / config["workflow_path"]).read_text()
        upload = workflow.split("      - name: Upload the tested-tree record\n", 1)[1].split("\n      - ", 1)[0]
        self.assertIn("name: tested-tree-${{ steps.tree.outputs.tree }}", upload)
        retention = int(re.search(r"retention-days: (\d+)", upload).group(1))
        self.assertLess(config["tree_match_max_age_days"], retention)
        record = workflow.split("  changes:\n", 1)[1]
        self.assertTrue(record.startswith("    name: required jobs (paths filter)\n"))
        self.assertLess(
            record.index("- name: Record the tested tree"),
            record.index("dorny/paths-filter"),
        )
        self.assertIn("git rev-parse 'HEAD^{tree}'", record)

    def test_existing_in_progress_run_is_waited_on_until_it_completes(self):
        running = run_doc(status="in_progress", conclusion=None)
        self.write_fixture(
            run_pages=[page([running]), page([run_doc()])],
            jobs_page=jobs_page(),
        )
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_manual_tier_requires_its_matching_run_title_marker(self):
        self.write_fixture(
            run_pages=[page([run_doc(event="workflow_dispatch", title="CI [tier=main]")])],
            jobs_page=jobs_page(),
        )
        result = self.invoke("main")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_missing_tier_resolves_dispatch_ref_dispatches_and_waits_for_result(self):
        self.write_fixture(
            run_pages=[page([]), page([])],
            dispatch_pages=[page([]), page([run_doc(
                event="workflow_dispatch", branch="main",
                title="CI [tier=main] [dispatch=__REQUEST_ID__]", run_id=457
            )])],
            jobs_page=jobs_page(),
        )
        result = self.invoke("main", "--dispatch-ref", "main")
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls_json()
        self.assertTrue(any(call[0:2] == ["workflow", "run"] for call in calls))
        dispatch_call = next(call for call in calls if call[0:2] == ["workflow", "run"])
        self.assertIn("--ref", dispatch_call)
        self.assertEqual(dispatch_call[dispatch_call.index("--ref") + 1], "main")
        self.assertIn("tier=main", dispatch_call)
        self.assertRegex(next(arg for arg in dispatch_call if arg.startswith("dispatch_id=")), r"dispatch_id=[0-9a-f]{32}$")

    def test_unrelated_concurrent_dispatch_on_another_branch_is_ignored(self):
        unrelated = run_doc(
            sha=OTHER_SHA, event="workflow_dispatch", branch="feature/other",
            title="CI [tier=main]", run_id=458,
        )
        requested = run_doc(
            event="workflow_dispatch", title="CI [tier=main] [dispatch=__REQUEST_ID__]", run_id=459,
        )
        self.write_fixture(
            run_pages=[page([]), page([])],
            dispatch_pages=[page([]), page([unrelated, requested])],
        )
        result = self.invoke("main", "--dispatch-ref", "main")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("/actions/runs/459", result.stdout)

    def test_same_ref_concurrent_dispatch_does_not_claim_another_request(self):
        collision = run_doc(
            sha=OTHER_SHA, event="workflow_dispatch", branch="main",
            title="CI [tier=main]", run_id=460,
        )
        requested = run_doc(
            event="workflow_dispatch", branch="main",
            title="CI [tier=main] [dispatch=__REQUEST_ID__]", run_id=461,
        )
        self.write_fixture(
            run_pages=[page([]), page([])],
            dispatch_pages=[page([]), page([collision, requested])],
        )
        result = self.invoke("main", "--dispatch-ref", "main")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("/actions/runs/461", result.stdout)

    def test_dispatch_that_runs_after_ref_moved_fails_with_run_link(self):
        moved = run_doc(
            sha=OTHER_SHA,
            event="workflow_dispatch",
            title="CI [tier=main] [dispatch=__REQUEST_ID__]",
            run_id=458,
        )
        self.write_fixture(
            run_pages=[page([]), page([])],
            dispatch_pages=[page([]), page([moved])],
        )
        result = self.invoke("main", "--dispatch-ref", "main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("dispatch ref 'main' moved", result.stderr)
        self.assertIn("/actions/runs/458", result.stderr)

    def test_missing_dispatched_run_times_out_at_configured_bound(self):
        self.config_data["wait_timeout_seconds"] = 1
        self.config_data["poll_interval_seconds"] = 1
        self.write_config()
        self.write_fixture(
            run_pages=[page([]), page([])],
            dispatch_pages=[page([]), page([])],
        )
        result = self.invoke("main", "--dispatch-ref", "main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("timed out after 1s", result.stderr)

    def test_ref_that_moved_since_candidate_sha_is_refused_without_dispatch(self):
        self.write_fixture(run_pages=[page([])], resolved_ref=OTHER_SHA)
        result = self.invoke("main", "--dispatch-ref", "main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing dispatch", result.stderr)
        self.assertFalse(any(call[0:2] == ["workflow", "run"] for call in self.calls_json()))

    def test_wrong_sha_workflow_path_or_event_does_not_count_as_evidence(self):
        for run in (
            run_doc(sha=OTHER_SHA),
            run_doc(path=".github/workflows/other.yml@refs/heads/main"),
            run_doc(event="pull_request"),
            run_doc(event="push", branch="release"),
        ):
            with self.subTest(run=run):
                self.write_fixture(run_pages=[page([run])])
                result = self.invoke()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("no main tier run exists", result.stderr)

    def test_wrong_dispatch_tier_does_not_count(self):
        self.write_fixture(
            run_pages=[page([run_doc(event="workflow_dispatch", title="CI [tier=nightly]")])]
        )
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no main tier run exists", result.stderr)

    def test_red_cancelled_skipped_or_missing_required_job_fails_with_run_link(self):
        cases = (
            (jobs_page(overrides={"required check": "failure"}), "failure"),
            (jobs_page(overrides={"required check": "cancelled"}), "cancelled"),
            (jobs_page(overrides={"required check": "skipped"}), "skipped"),
            (jobs_page(omit=("required check",)), "missing required job"),
        )
        for job_list, expected in cases:
            with self.subTest(expected=expected):
                self.write_fixture(run_pages=[page([run_doc()])], jobs_page=job_list)
                result = self.invoke()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, result.stderr)
                self.assertIn("/actions/runs/456", result.stderr)

    def test_failed_conditionally_selected_job_cannot_be_ignored(self):
        jobs = jobs_page()
        jobs["jobs"].append({
            "name": "tests (relay DB, PostgreSQL)",
            "status": "completed",
            "conclusion": "failure",
        })
        self.write_fixture(run_pages=[page([run_doc()])], jobs_page=jobs)
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("additional job tests (relay DB, PostgreSQL): failure", result.stderr)

    def test_missing_job_conclusion_does_not_use_success_status_as_fallback(self):
        jobs = jobs_page()
        jobs["jobs"][0]["conclusion"] = None
        jobs["jobs"][0]["status"] = "success"
        self.write_fixture(run_pages=[page([run_doc()])], jobs_page=jobs)
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing", result.stderr)

    def test_cancelled_or_failed_workflow_fails_with_run_link(self):
        for status, conclusion in (("completed", "failure"), ("completed", "cancelled")):
            with self.subTest(conclusion=conclusion):
                self.write_fixture(
                    run_pages=[page([run_doc(status=status, conclusion=conclusion)])],
                    jobs_page=jobs_page(),
                )
                result = self.invoke()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("workflow conclusion", result.stderr)
                self.assertIn("/actions/runs/456", result.stderr)

    def test_invalid_sha_and_missing_dispatch_ref_fail_closed(self):
        self.write_fixture(run_pages=[page([])])
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--dispatch-ref is required", result.stderr)

        env = os.environ.copy()
        env.update({"PATH": f"{self.bin_dir}{os.pathsep}{env['PATH']}", "GH_FIXTURE": str(self.fixture), "GH_CALLS": str(self.calls)})
        invalid = subprocess.run(
            [str(SCRIPT), "--repo", "example/pohunek", "--sha", "abc", "--tier", "main"],
            capture_output=True,
            text=True,
            env=env,
        )
        self.assertNotEqual(invalid.returncode, 0)
        self.assertIn("full 40-character", invalid.stderr)


if __name__ == "__main__":
    unittest.main()
