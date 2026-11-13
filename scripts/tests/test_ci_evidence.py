"""CLI scenarios for exact-commit GitHub Actions tier evidence."""

import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

import hermetic_git  # noqa: F401  (isolates git from the developer's configuration)
import virtual_clock


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
            return uses is None
        if uses and any(
            job_name_pattern(inner, inner_matrix).match(called)
            for inner, inner_matrix, _ in workflow_jobs(uses)
        ):
            return True
    return False


FAKE_GH = r'''#!/usr/bin/env python3
import base64
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
elif "/contents/" in endpoint:
    # The config in the tree of the ref, served raw as the contents API does.
    ref = endpoint.split("?ref=", 1)[1]
    if any("application/vnd.github.raw+json" in arg for arg in args) and "scripts/ci-evidence.json" not in endpoint:
        # A raw workflow file of the tree, for the reuse alias resolver.
        path = endpoint.split("/contents/", 1)[1].split("?", 1)[0]
        texts = fixture.get("tree_files", {})
        if ref in texts and path in texts[ref]:
            print(texts[ref][path], end="")
            raise SystemExit(0)
        print("gh: Not Found (HTTP 404)", file=sys.stderr)
        raise SystemExit(1)
    configs = fixture.get("tree_configs", {})
    if ref not in configs:
        configs = fixture.get("tree_config", {})
        if ref not in configs:
            print("gh: Not Found (HTTP 404)", file=sys.stderr)
            raise SystemExit(1)
    print(json.dumps(configs[ref]))
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
    entries = fixture.get("artifacts", {}).get(run_id, [])
    artifacts = []
    for entry in entries:
        if isinstance(entry, dict):
            artifacts.append({
                "name": entry["name"],
                "id": entry.get("id", 1),
                "expired": entry.get("expired", False),
            })
        else:
            artifacts.append({"name": entry})
    # The real `gh` honours the pagination flags: `--paginate
    # --slurp` prints a JSON array of page objects, bare `--paginate`
    # concatenates them, and neither prints the whole list at once. A
    # script that drops the flags gets a single page object and must fail
    # on the `isinstance(pages, list)` check.
    page = {"artifacts": artifacts}
    if "--paginate" in args and "--slurp" in args:
        print(json.dumps([page]))
    else:
        print(json.dumps(page))
elif "/actions/artifacts/" in endpoint and endpoint.endswith("/zip"):
    artifact_id = str(endpoint.split("/")[-2])
    zips = {str(key): value for key, value in fixture.get("artifact_zips", {}).items()}
    if artifact_id not in zips:
        print("gh: Not Found (HTTP 404)", file=sys.stderr)
        raise SystemExit(1)
    sys.stdout.buffer.write(base64.b64decode(zips[artifact_id]))
elif "/actions/runs/" in endpoint and "/jobs" not in endpoint and not endpoint.endswith("/artifacts"):
    # A single run by id, for the reuse source run.
    run_id = str(endpoint.rsplit("/", 1)[1])
    runs = {str(key): value for key, value in fixture.get("runs_by_id", {}).items()}
    if run_id not in runs:
        print("gh: Not Found (HTTP 404)", file=sys.stderr)
        raise SystemExit(1)
    print(json.dumps(runs[run_id]))
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
    created_at="2026-11-12T12:00:00Z",
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
        "created_at": created_at,
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


def jobs_page(*, overrides=None, omit=(), extra=(), completed_at="2026-11-12T11:00:00Z"):
    overrides = overrides or {}
    names = [name for name in REQUIRED_JOBS if name not in omit]
    jobs = [
        {
            "name": name,
            "status": "completed",
            "conclusion": overrides.get(name, "success"),
            "completed_at": completed_at,
        }
        for name in names
    ]
    for job in extra:
        jobs = [existing for existing in jobs if existing["name"] != job["name"]]
        jobs.append(job)
    return {
        "jobs": jobs
    }


REUSE_SOURCE_ID = 777
REUSE_JOB = "matrix check (linux)"


def reuse_config(base):
    """A config copy with the delta-reuse section the candidate allows."""
    config = json.loads(json.dumps(base))
    config["reuse"] = {"max_age_hours": 24, "source_events": ["pull_request"]}
    return config


def reuse_zip(tree, jobs, source_run_id=REUSE_SOURCE_ID):
    """A base64 zip holding the `ci-reuse-<tree>` record for the fixture."""
    import base64
    import io
    import zipfile

    record = {
        "schema_version": 1,
        "tree": tree,
        "source_run_id": source_run_id,
        "source_url": f"https://github.com/example/pohunek/actions/runs/{source_run_id}",
        "source_event": "pull_request",
        "source_head_sha": SHA,
        "jobs": {
            job_id: {
                "run_id": source_run_id,
                "names": names,
                "source_job_ids": [1000 + index for index, _ in enumerate(names)],
                "completed_at": ["2026-11-12T11:00:00Z" for _ in names],
            }
            for job_id, names in jobs.items()
        },
    }
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr(f"ci-reuse-{tree}.json", json.dumps(record))
    return base64.b64encode(buffer.getvalue()).decode()


def union_fixture(test, **values):
    """A fixture where the evidence run 456 on SHA skipped REUSE_JOB and its
    `ci-reuse-<TREE>` record names the green pull request run 777 on SHA."""
    source = run_doc(
        sha=SHA, event="pull_request", branch="zajca/feature", run_id=REUSE_SOURCE_ID,
        repo=REPO, head_repo=REPO,
    )
    base = {
        "trees": {SHA: TREE},
        "artifacts": {
            "456": [{"name": f"ci-reuse-{TREE}", "id": 11}],
            str(REUSE_SOURCE_ID): [f"tested-tree-{TREE}"],
        },
        "artifact_zips": {"11": reuse_zip(TREE, {"matrix": [REUSE_JOB]})},
        "runs_by_id": {str(REUSE_SOURCE_ID): source},
        "jobs_by_run": {
            "456": jobs_page(overrides={REUSE_JOB: "skipped"}),
            str(REUSE_SOURCE_ID): jobs_page(),
        },
    }
    base.update(values)
    test.write_fixture(run_pages=[page([run_doc()])], **base)
    test.config_data = reuse_config(test.config_data)
    test.write_config()


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

    def invoke(self, tier="main", *extra, config=None, sha=SHA, script=SCRIPT, tree_config=False, timeout=None):
        env = os.environ.copy()
        env.update({
            "PATH": f"{self.bin_dir}{os.pathsep}{env['PATH']}",
            "GH_FIXTURE": str(self.fixture),
            "GH_CALLS": str(self.calls),
        })
        # tree_config leaves --config out, so the script reads the config of
        # the commit it checks.
        config_arguments = [] if tree_config else ["--config", str(config or self.config)]
        return subprocess.run(
            [sys.executable, "-c", virtual_clock.VIRTUAL_CLOCK_RUNNER, str(script),
             "--repo", "example/pohunek", "--sha", sha,
             "--tier", tier, *config_arguments, *extra],
            capture_output=True,
            text=True,
            check=False,
            cwd=self.directory,
            env=env,
            timeout=timeout,
        )

    def checkout(self, committed: dict, working: dict) -> tuple[Path, str]:
        """A checkout holding this script and one commit whose config is
        `committed`, with `working` as the uncommitted config of its work
        tree. Returns the script's path and the commit."""
        repository = self.directory / "checkout"
        (repository / "scripts").mkdir(parents=True)
        script = repository / "scripts" / "ci-evidence"
        script.write_bytes(SCRIPT.read_bytes())
        script.chmod(0o755)
        (repository / "scripts" / "ci_eligibility.py").write_bytes(
            (ROOT / "scripts" / "ci_eligibility.py").read_bytes()
        )
        config = repository / "scripts" / "ci-evidence.json"
        config.write_text(json.dumps(committed))
        git = ["git", "-C", str(repository), "-c", "user.name=t", "-c", "user.email=t@example.invalid",
               "-c", "commit.gpgsign=false", "-c", "maintenance.auto=false",
               "-c", "gc.auto=0"]
        env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
        for command in (["init", "-q"], ["add", "-A"], ["commit", "-q", "-m", "config"]):
            subprocess.run([*git, *command], check=True, capture_output=True, env=env)
        sha = subprocess.run(
            [*git, "rev-parse", "HEAD"], check=True, capture_output=True, text=True, env=env
        ).stdout.strip()
        config.write_text(json.dumps(working))
        return script, sha

    def config_with(self, *extra_jobs: str) -> dict:
        config = json.loads(json.dumps(self.config_data))
        config["required_jobs"]["main"] += list(extra_jobs)
        return config

    def test_the_target_commits_config_decides_when_the_checkout_requires_more(self):
        # The checkout's config adds a job that the commit's runs never had;
        # the commit's own config does not require it, so its runs pass.
        script, sha = self.checkout(self.config_data, self.config_with("job only the checkout requires"))
        self.write_fixture(resolved_ref=sha, run_pages=[page([run_doc(sha=sha)])])
        result = self.invoke(script=script, sha=sha, tree_config=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"main tier passed for {sha}", result.stdout)

    def test_the_target_commits_config_decides_when_the_checkout_requires_less(self):
        # The commit's config requires a job its runs lack; the checkout's
        # config omits it, and would accept the runs.
        script, sha = self.checkout(self.config_with("job the commit requires"), self.config_data)
        self.write_fixture(resolved_ref=sha, run_pages=[page([run_doc(sha=sha)])])
        result = self.invoke(script=script, sha=sha, tree_config=True)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("missing required job: job the commit requires", result.stderr)

    def test_a_commit_the_checkout_lacks_takes_its_config_from_the_contents_api(self):
        # SHA is not in the checkout; the API serves its config, which requires
        # a job the run lacks, while the checkout's own config would not.
        script, _ = self.checkout(self.config_data, self.config_data)
        self.write_fixture(
            run_pages=[page([run_doc()])],
            tree_configs={SHA: self.config_with("job the commit requires")},
        )
        result = self.invoke(script=script, tree_config=True)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("missing required job: job the commit requires", result.stderr)
        contents = [call for call in self.calls_json() if any("/contents/" in arg for arg in call)]
        self.assertEqual(len(contents), 1)
        self.assertIn(f"repos/example/pohunek/contents/scripts/ci-evidence.json?ref={SHA}", contents[0])

        self.write_fixture(run_pages=[page([run_doc()])], tree_configs={SHA: self.config_data})
        result = self.invoke(script=script, tree_config=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_commit_without_a_readable_config_refuses_the_evidence(self):
        script, _ = self.checkout(self.config_data, self.config_data)
        # The API has no config for the commit, though its runs would pass.
        self.write_fixture(run_pages=[page([run_doc()])], tree_configs={})
        result = self.invoke(script=script, tree_config=True)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(f"cannot read the config {SHA}:scripts/ci-evidence.json", result.stderr)

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
                workflow_names.update(f"tests ({shard}, fast)" for shard in ("core", "daemon", "cli"))
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
        macos_job = workflow.split("  macos-package:\n", 1)[1].split("\n  test:\n", 1)[0]
        self.assertNotIn("continue-on-error", macos_job)

    def test_the_wait_outlasts_a_nightly_that_runs_the_release_gate(self):
        # The nightly tier runs the whole release gate in one job (about 35
        # minutes); the release script waits this long for its evidence.
        config = json.loads(DEFAULT_CONFIG.read_text())
        self.assertIn(
            "release gate (full test set) / fmt + clippy + test", config["required_jobs"]["nightly"]
        )
        self.assertGreaterEqual(config["wait_timeout_seconds"], 3600)

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
        self.assertFalse(names_a_workflow_job(f"{smoke} (network-isolated, ubuntu-24.04) extra", workflow))
        self.assertFalse(names_a_workflow_job("tests (gpu, fast)", workflow))
        # A caller of a reusable workflow is shown only as `caller / called job`.
        self.assertFalse(names_a_workflow_job("release rehearsal (evidence, assembly, archive smoke)", workflow))
        # The release rehearsal is nightly-tier evidence: both legs of its
        # archive smoke gate a release through the nightly run, and the main
        # tier, which never starts the rehearsal, requires none of its jobs.
        for leg in ("ubuntu-24.04", "macos-15"):
            self.assertIn(f"{smoke} (network-isolated, {leg})", config["required_jobs"]["nightly"])
        self.assertIn(
            "release rehearsal (archive smoke scenarios) / Archive smoke scenarios (network-isolated, macos-15)",
            config["required_jobs"]["nightly"],
        )
        rehearsal = [name for name in config["required_jobs"]["nightly"] if name.startswith("release rehearsal ")]
        self.assertTrue(rehearsal)
        # A rehearsal runs at most 13 runner jobs (#1208; #1259 split the macOS
        # archive smoke into the parallel scenario job).
        self.assertLessEqual(len(rehearsal), 13)
        self.assertEqual(
            [name for name in config["required_jobs"]["main"] if name.startswith("release rehearsal ")],
            [],
        )
        # Apart from the rehearsal, the nightly tier differs from main only by
        # the G3 test selection audit and the release gate's test steps.
        nightly_only = {
            "test selection audit (G3)",
            "release gate (full test set) / fmt + clippy + test",
        }
        self.assertEqual(
            set(config["required_jobs"]["main"]),
            set(config["required_jobs"]["nightly"]) - set(rehearsal) - nightly_only,
        )
        # The row jobs are named after the runners of the planned row matrix,
        # which the workflow file cannot show: every runner's job gates the
        # nightly tier.
        planned = subprocess.run(
            [str(ROOT / "scripts/release-workflow/row-matrix"), str(ROOT / "compat/matrix.json")],
            capture_output=True, text=True, check=True,
        )
        runners = [entry["runner"] for entry in json.loads(planned.stdout)["include"]]
        self.assertTrue(runners)
        rows = "release rehearsal (evidence, assembly, archive smoke) / Rows on"
        listed = [name for name in config["required_jobs"]["nightly"] if name.startswith(rows)]
        self.assertEqual(sorted(listed), sorted(f"{rows} {runner}" for runner in runners))

    def test_real_main_jobs_fixture_satisfies_the_inventory(self):
        fixture = json.loads(
            (ROOT / "scripts/tests/fixtures/ci-evidence-main-jobs.json").read_text()
        )
        config = json.loads(DEFAULT_CONFIG.read_text())
        successful = {job["name"] for job in fixture["jobs"] if job["conclusion"] == "success"}
        skipped = {job["name"] for job in fixture["jobs"] if job["conclusion"] == "skipped"}
        self.assertFalse(set(config["required_jobs"]["main"]) - successful)
        self.assertTrue(skipped)
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

    def test_a_wait_longer_than_any_test_budget_completes_on_the_virtual_clock(self):
        # The deadline and the polls run on the virtual clock; a wall-clock
        # run would hit the 60 s guard.
        self.config_data["wait_timeout_seconds"] = 3600
        self.config_data["poll_interval_seconds"] = 1200
        self.write_config()
        in_progress = run_doc(status="in_progress", conclusion=None)
        self.write_fixture(
            run_pages=[page([in_progress]), page([in_progress]), page([run_doc()])],
            jobs_page=jobs_page(),
        )
        result = self.invoke(timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        commit_listings = [
            call for call in self.calls_json()
            if any("/actions/workflows/" in arg for arg in call)
            and any(arg.startswith("head_sha=") for arg in call)
        ]
        # Initial listing plus two polls; virtual time used is 2400 s < 3600 s.
        self.assertEqual(len(commit_listings), 3, commit_listings)

    def test_a_wait_for_a_run_that_never_completes_times_out_on_the_virtual_clock(self):
        # The polls advance the clock to the 3600 s deadline while the run
        # stays in progress; a wall-clock deadline would spin instantly
        # through the polls until the 60 s guard instead.
        self.config_data["wait_timeout_seconds"] = 3600
        self.config_data["poll_interval_seconds"] = 1200
        self.write_config()
        in_progress = run_doc(status="in_progress", conclusion=None)
        self.write_fixture(
            run_pages=[page([in_progress])],
            jobs_page=jobs_page(),
        )
        result = self.invoke(timeout=60)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("timed out after 3600s", result.stderr)
        commit_listings = [
            call for call in self.calls_json()
            if any("/actions/workflows/" in arg for arg in call)
            and any(arg.startswith("head_sha=") for arg in call)
        ]
        # Initial listing plus polls at t=1200, 2400 and 3600.
        self.assertEqual(len(commit_listings), 4, commit_listings)

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
            "name": "tests (conditional example)",
            "status": "completed",
            "conclusion": "failure",
        })
        self.write_fixture(run_pages=[page([run_doc()])], jobs_page=jobs)
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("additional job tests (conditional example): failure", result.stderr)

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

    def test_a_delta_dispatch_title_with_suffix_counts_for_tier_and_dispatch(self):
        self.write_fixture(
            run_pages=[page([run_doc(event="workflow_dispatch", title="CI [tier=main] [dispatch=__REQUEST_ID__] [delta]")])],
            jobs_page=jobs_page(),
        )
        result = self.invoke("main")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_reuse_union_is_accepted_and_names_both_runs(self):
        union_fixture(self)
        result = self.invoke("main", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertIn("reused_from", report)
        self.assertEqual(report["reused_from"]["run_id"], REUSE_SOURCE_ID)
        self.assertEqual(
            report["reused_from"]["url"],
            f"https://github.com/example/pohunek/actions/runs/{REUSE_SOURCE_ID}",
        )
        self.assertEqual(report["reused_from"]["jobs"], ["matrix"])
        text = self.invoke("main")
        self.assertEqual(text.returncode, 0, text.stderr)
        self.assertIn("/actions/runs/456", text.stdout)
        self.assertIn(f"/actions/runs/{REUSE_SOURCE_ID}", text.stdout)

    def test_a_reuse_union_accepts_a_skipped_matrix_job(self):
        # A skipped matrix job is shown under its display `name:` in the
        # evidence run, exactly as the Actions API shows it on a real run:
        # `tests (${{ matrix.shard }}, fast)` with the expression
        # uninterpolated, while the record names the legs it reused
        # (`tests (core, fast)`). The union accepts it through the alias.
        workflow = """name: ci
jobs:
  fast-tests:
    name: tests (${{ matrix.shard }}, fast)
    strategy:
      matrix:
        shard: ${{ fromJSON(github.event_name == 'pull_request' && false || '["core"]') }}
    runs-on: ubuntu-24.04
    steps:
      - run: echo hi
  required:
    name: required check
    runs-on: ubuntu-24.04
    steps:
      - run: echo hi
"""
        evidence_page = jobs_page(overrides={}, extra=[
            {"name": "tests (${{ matrix.shard }}, fast)", "status": "completed", "conclusion": "skipped",
             "completed_at": "2026-11-12T11:00:00Z"},
        ])
        source_page = jobs_page(extra=[
            {"name": "tests (core, fast)", "status": "completed", "conclusion": "success",
             "completed_at": "2026-11-12T11:00:00Z"},
        ])
        union_fixture(
            self,
            tree_files={SHA: {WORKFLOW_PATH: workflow}},
            artifact_zips={"11": reuse_zip(TREE, {"fast-tests": ["tests (core, fast)"]})},
            jobs_by_run={"456": evidence_page, str(REUSE_SOURCE_ID): source_page},
        )
        self.config_data["required_jobs"]["main"].append("tests (core, fast)")
        self.write_config()
        result = self.invoke("main", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["reused_from"]["jobs"], ["fast-tests"])

    def test_a_reuse_union_accepts_a_skipped_called_workflow_job(self):
        # A skipped called-workflow job is shown under the caller display
        # `name:` (`upgrade from the previous release`), while the record
        # names `caller / called job`.
        workflow = """name: ci
jobs:
  upgrade-test:
    name: upgrade from the previous release
    uses: ./.github/workflows/callee.yml
  required:
    name: required check
    runs-on: ubuntu-24.04
    steps:
      - run: echo hi
"""
        callee = """name: callee
jobs:
  inner:
    name: inner job
    runs-on: ubuntu-24.04
    steps:
      - run: echo hi
"""
        leg = "upgrade from the previous release / inner job"
        evidence_page = jobs_page(overrides={}, extra=[
            {"name": "upgrade from the previous release", "status": "completed", "conclusion": "skipped",
             "completed_at": "2026-11-12T11:00:00Z"},
        ])
        source_page = jobs_page(extra=[
            {"name": leg, "status": "completed", "conclusion": "success",
             "completed_at": "2026-11-12T11:00:00Z"},
        ])
        union_fixture(
            self,
            tree_files={SHA: {WORKFLOW_PATH: workflow, ".github/workflows/callee.yml": callee}},
            artifact_zips={"11": reuse_zip(TREE, {"upgrade-test": [leg]})},
            jobs_by_run={"456": evidence_page, str(REUSE_SOURCE_ID): source_page},
        )
        self.config_data["required_jobs"]["main"].append(leg)
        self.write_config()
        result = self.invoke("main", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["reused_from"]["jobs"], ["upgrade-test"])

    def test_a_reuse_union_on_the_tree_path_is_accepted_and_refused(self):
        # The tree-matching dispatch run carries the reuse record; its union
        # decides like the exact-commit path. The source
        # run and the dispatch run share one commit (OTHER_SHA — the
        # branch head); the candidate SHA only names its tree.
        union_fixture(self)
        # Move the record onto the tree-matched dispatch run 789.
        fixture = json.loads(self.fixture.read_text())
        fixture["trees"] = {SHA: TREE, OTHER_SHA: TREE}
        fixture["tree_runs"] = [dispatch_doc()]
        fixture["artifacts"]["789"] = [
            {"name": f"tested-tree-{TREE}", "id": 21},
            {"name": f"ci-reuse-{TREE}", "id": 22},
        ]
        fixture["artifact_zips"]["22"] = fixture["artifact_zips"]["11"]
        del fixture["artifact_zips"]["11"]
        fixture["jobs_by_run"]["789"] = jobs_page(overrides={REUSE_JOB: "skipped"})
        fixture["runs_by_id"][str(REUSE_SOURCE_ID)] = run_doc(
            sha=OTHER_SHA, event="pull_request", branch="zajca/feature",
            run_id=REUSE_SOURCE_ID, repo=REPO, head_repo=REPO,
        )
        self.fixture.write_text(json.dumps(fixture))
        fixture["run_pages"] = [page([]), page([])]
        self.fixture.write_text(json.dumps(fixture))
        result = self.invoke("main", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["match"], "tree")
        self.assertEqual(report["reused_from"]["run_id"], REUSE_SOURCE_ID)

    def test_a_push_reuse_union_accepts_jobs_from_two_source_runs(self):
        # Push (issue #2204): the record names one job from PR run 777 and
        # one from push run 888; both must verify against their own run IDs.
        OTHER_SOURCE = 888
        union_fixture(self)
        self.config_data["reuse"] = {
            "max_age_hours": 24,
            "source_events": ["pull_request", "push", "workflow_dispatch"],
        }
        self.write_config()
        fixture = json.loads(self.fixture.read_text())
        fixture["artifact_zips"]["11"] = reuse_zip(
            TREE,
            {"matrix": [REUSE_JOB], "required": ["required check"]},
            source_run_id=REUSE_SOURCE_ID,
        )
        # The per-job run of the second job is overridden to 888.
        import base64, io, zipfile as _zip
        record_zip = _zip.ZipFile(io.BytesIO(base64.b64decode(fixture["artifact_zips"]["11"])))
        record = json.loads(record_zip.read(f"ci-reuse-{TREE}.json"))
        record["jobs"]["required"]["run_id"] = OTHER_SOURCE
        buffer = io.BytesIO()
        with _zip.ZipFile(buffer, "w", _zip.ZIP_DEFLATED) as archive:
            archive.writestr(f"ci-reuse-{TREE}.json", json.dumps(record))
        fixture["artifact_zips"]["11"] = base64.b64encode(buffer.getvalue()).decode()
        fixture["runs_by_id"][str(OTHER_SOURCE)] = run_doc(
            sha=SHA, event="push", branch="main", run_id=OTHER_SOURCE,
            repo=REPO, head_repo=REPO,
        )
        fixture["artifacts"][str(OTHER_SOURCE)] = [f"tested-tree-{TREE}"]
        source_jobs = jobs_page(overrides={"required check": "success"})
        second = [job for job in source_jobs["jobs"] if job["name"] == "required check"]
        fixture["jobs_by_run"][str(OTHER_SOURCE)] = {"jobs": second}
        # The evidence run 456 shows both jobs skipped.
        fixture["jobs_by_run"]["456"] = jobs_page(overrides={REUSE_JOB: "skipped", "required check": "skipped"})
        self.fixture.write_text(json.dumps(fixture))
        result = self.invoke("main", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(sorted(report["reused_from"]["run_ids"]), [REUSE_SOURCE_ID, OTHER_SOURCE])
        text = self.invoke("main")
        self.assertEqual(text.returncode, 0, text.stderr)
        self.assertIn(f"/actions/runs/{REUSE_SOURCE_ID}", text.stdout)
        self.assertIn(f"/actions/runs/{OTHER_SOURCE}", text.stdout)

    def test_a_push_reuse_union_by_tree_accepts_a_source_on_another_commit(self):
        # A pull_request source whose head_sha differs from the push
        # commit is accepted by its recorded tested tree.
        OTHER = "b" * 40
        union_fixture(self)
        fixture = json.loads(self.fixture.read_text())
        fixture["runs_by_id"][str(REUSE_SOURCE_ID)] = run_doc(
            sha=OTHER, event="pull_request", branch="zajca/feature",
            run_id=REUSE_SOURCE_ID, repo=REPO, head_repo=REPO,
        )
        fixture["trees"][OTHER] = "d" * 40  # tree(head) != tree(push)
        self.fixture.write_text(json.dumps(fixture))
        result = self.invoke("main", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["reused_from"]["run_ids"], [REUSE_SOURCE_ID])

    def test_a_push_reuse_union_is_refused_for_an_audit_record_entry(self):
        # Cargo-audit is never excused — a record naming the audit job
        # refuses the union even when the source leg is green. The
        # evidence run shows the audit leg skipped (reused) and the
        # source 777 lists it green.
        union_fixture(self)
        fixture = json.loads(self.fixture.read_text())
        fixture["artifact_zips"]["11"] = reuse_zip(
            TREE, {"matrix": [REUSE_JOB], "audit": ["cargo-audit (RustSec advisories)"]},
            source_run_id=REUSE_SOURCE_ID,
        )
        source_jobs = fixture["jobs_by_run"][str(REUSE_SOURCE_ID)]["jobs"]
        source_jobs.append(dict(
            source_jobs[0], name="cargo-audit (RustSec advisories)",
            conclusion="success",
        ))
        fixture["jobs_by_run"]["456"]["jobs"].append({
            "name": "cargo-audit (RustSec advisories)",
            "status": "completed",
            "conclusion": "skipped",
            "completed_at": "2026-11-12T11:00:00Z",
        })
        self.fixture.write_text(json.dumps(fixture))
        result = self.invoke("main", "--json")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is never reused", result.stderr)
        self.assertIn("cargo-audit (RustSec advisories)", result.stderr)
        self.assertIn(f"/actions/runs/{REUSE_SOURCE_ID}", result.stderr)

    def test_a_dispatch_evidence_refuses_a_same_tree_source_on_another_commit(self):
        # Row 15 regression: evidence on the tree path (dispatch
        # consumer) must not accept a same-tree PR source on another commit.
        union_fixture(self)
        fixture = json.loads(self.fixture.read_text())
        fixture["trees"] = {SHA: TREE, OTHER_SHA: TREE}
        fixture["tree_runs"] = [dispatch_doc()]
        fixture["artifacts"]["789"] = [
            {"name": f"tested-tree-{TREE}", "id": 21},
            {"name": f"ci-reuse-{TREE}", "id": 22},
        ]
        fixture["artifact_zips"]["22"] = fixture["artifact_zips"]["11"]
        del fixture["artifact_zips"]["11"]
        fixture["jobs_by_run"]["789"] = jobs_page(overrides={REUSE_JOB: "skipped"})
        fixture["runs_by_id"][str(REUSE_SOURCE_ID)] = run_doc(
            sha=THIRD_SHA, event="pull_request", branch="zajca/feature",
            run_id=REUSE_SOURCE_ID, repo=REPO, head_repo=REPO,
        )
        fixture["artifacts"]["456"] = []
        fixture["run_pages"] = [page([]), page([])]
        self.fixture.write_text(json.dumps(fixture))
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"has head SHA {THIRD_SHA}, expected", result.stderr)

    def test_a_dispatch_evidence_accepts_a_same_commit_source(self):
        # The dispatch evidence run is 789 on OTHER_SHA (the dispatch
        # commit); a source on that same commit is accepted.
        union_fixture(self)
        fixture = json.loads(self.fixture.read_text())
        fixture["trees"] = {SHA: TREE, OTHER_SHA: TREE}
        fixture["tree_runs"] = [dispatch_doc()]
        fixture["artifacts"]["789"] = [
            {"name": f"tested-tree-{TREE}", "id": 21},
            {"name": f"ci-reuse-{TREE}", "id": 22},
        ]
        fixture["artifact_zips"]["22"] = fixture["artifact_zips"]["11"]
        del fixture["artifact_zips"]["11"]
        fixture["jobs_by_run"]["789"] = jobs_page(overrides={REUSE_JOB: "skipped"})
        fixture["runs_by_id"][str(REUSE_SOURCE_ID)] = run_doc(
            sha=OTHER_SHA, event="pull_request", branch="zajca/feature",
            run_id=REUSE_SOURCE_ID, repo=REPO, head_repo=REPO,
        )
        fixture["artifacts"]["456"] = []
        fixture["run_pages"] = [page([]), page([])]
        self.fixture.write_text(json.dumps(fixture))
        result = self.invoke("main", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_required_step_is_checked_by_display_name(self):
        self.config_data["required_steps"]["main"] = [{"job": REUSE_JOB, "step": "Check formatting"}]
        self.write_config()
        union_fixture(self, jobs_by_run={
            "456": jobs_page(overrides={REUSE_JOB: "skipped"}),
            str(REUSE_SOURCE_ID): {"jobs": [{"name": REUSE_JOB, "conclusion": "success", "completed_at": "2026-11-12T11:00:00Z", "steps": [{"name": "Check formatting", "conclusion": "failure"}]}]},
        })
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("required step Check formatting", result.stderr)

    def test_per_job_run_id_from_the_record_is_honoured(self):
        union_fixture(self)
        fixture = json.loads(self.fixture.read_text())
        import base64, io, zipfile as _zip
        import json as _json
        raw = _zip.ZipFile(io.BytesIO(base64.b64decode(fixture["artifact_zips"]["11"])))
        record = _json.loads(raw.read(f"ci-reuse-{TREE}.json"))
        record["jobs"]["matrix"]["run_id"] = 999
        buffer = io.BytesIO()
        with _zip.ZipFile(buffer, "w", _zip.ZIP_DEFLATED) as archive:
            archive.writestr(f"ci-reuse-{TREE}.json", _json.dumps(record))
        fixture["artifact_zips"]["11"] = base64.b64encode(buffer.getvalue()).decode()
        fixture["runs_by_id"]["999"] = run_doc(sha=SHA, event="pull_request", branch="zajca/feature", run_id=999, repo=REPO, head_repo=REPO)
        fixture["artifacts"]["999"] = [f"tested-tree-{TREE}"]
        fixture["jobs_by_run"]["999"] = jobs_page(overrides={REUSE_JOB: "failure"})
        self.fixture.write_text(json.dumps(fixture))
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)

    def test_two_source_runs_with_a_required_job_in_each(self):
        union_fixture(self)
        fixture = json.loads(self.fixture.read_text())
        import base64, io, zipfile as _zip
        import json as _json
        raw = _zip.ZipFile(io.BytesIO(base64.b64decode(fixture["artifact_zips"]["11"])))
        record = _json.loads(raw.read(f"ci-reuse-{TREE}.json"))
        record["jobs"]["required"] = {"run_id": 888, "names": ["required check"], "source_job_ids": [], "completed_at": []}
        buffer = io.BytesIO()
        with _zip.ZipFile(buffer, "w", _zip.ZIP_DEFLATED) as archive:
            archive.writestr(f"ci-reuse-{TREE}.json", _json.dumps(record))
        fixture["artifact_zips"]["11"] = base64.b64encode(buffer.getvalue()).decode()
        fixture["runs_by_id"]["888"] = run_doc(sha=SHA, event="pull_request", branch="zajca/feature", run_id=888, repo=REPO, head_repo=REPO)
        fixture["artifacts"]["888"] = [f"tested-tree-{TREE}"]
        fixture["jobs_by_run"]["888"] = {"jobs": [{"name": "required check", "status": "completed", "conclusion": "success", "completed_at": "2026-11-12T11:00:00Z"}]}
        fixture["jobs_by_run"]["456"] = jobs_page(overrides={REUSE_JOB: "skipped", "required check": "skipped"})
        self.fixture.write_text(json.dumps(fixture))
        result = self.invoke("main", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_source_leg_age_is_one_sided(self):
        union_fixture(self, jobs_by_run={
            "456": jobs_page(overrides={REUSE_JOB: "skipped"}),
            str(REUSE_SOURCE_ID): jobs_page(completed_at="2026-11-12T18:00:00Z"),
        })
        result = self.invoke("main", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_reuse_union_on_the_tree_path_is_refused_for_a_third_commit(self):
        # A source run on neither the candidate nor the dispatch commit is
        # refused even when its recorded tree matches.
        union_fixture(self)
        fixture = json.loads(self.fixture.read_text())
        fixture["trees"] = {SHA: TREE, OTHER_SHA: TREE, THIRD_SHA: TREE}
        fixture["tree_runs"] = [dispatch_doc()]
        fixture["artifacts"]["789"] = [
            {"name": f"tested-tree-{TREE}", "id": 21},
            {"name": f"ci-reuse-{TREE}", "id": 22},
        ]
        fixture["artifact_zips"]["22"] = fixture["artifact_zips"]["11"]
        del fixture["artifact_zips"]["11"]
        fixture["jobs_by_run"]["789"] = jobs_page(overrides={REUSE_JOB: "skipped"})
        fixture["runs_by_id"][str(REUSE_SOURCE_ID)] = run_doc(
            sha=THIRD_SHA, event="pull_request", branch="zajca/feature",
            run_id=REUSE_SOURCE_ID, repo=REPO, head_repo=REPO,
        )
        fixture["artifacts"]["456"] = []
        fixture["run_pages"] = [page([]), page([])]
        self.fixture.write_text(json.dumps(fixture))
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tree match", result.stderr)
        self.assertIn("head SHA", result.stderr)

    def test_a_dispatch_union_by_recorded_tree_refuses_a_third_commit(self):
        union_fixture(self)
        fixture = json.loads(self.fixture.read_text())
        fixture["trees"] = {SHA: TREE, OTHER_SHA: TREE, THIRD_SHA: TREE}
        fixture["tree_runs"] = [dispatch_doc()]
        fixture["artifacts"]["789"] = [
            {"name": f"tested-tree-{TREE}", "id": 21},
            {"name": f"ci-reuse-{TREE}", "id": 22},
        ]
        fixture["artifact_zips"]["22"] = fixture["artifact_zips"]["11"]
        del fixture["artifact_zips"]["11"]
        fixture["jobs_by_run"]["789"] = jobs_page(overrides={REUSE_JOB: "skipped"})
        fixture["runs_by_id"][str(REUSE_SOURCE_ID)] = run_doc(
            sha=THIRD_SHA, event="pull_request", branch="zajca/feature",
            run_id=REUSE_SOURCE_ID, repo=REPO, head_repo=REPO,
        )
        fixture["artifacts"]["456"] = []
        fixture["run_pages"] = [page([]), page([])]
        self.fixture.write_text(json.dumps(fixture))
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tree match", result.stderr)
        self.assertIn(f"has head SHA {THIRD_SHA}, expected", result.stderr)

    def test_a_reuse_union_on_the_tree_path_is_refused_for_a_red_source(self):
        union_fixture(self)
        fixture = json.loads(self.fixture.read_text())
        fixture["trees"] = {SHA: TREE, OTHER_SHA: TREE}
        fixture["tree_runs"] = [dispatch_doc()]
        fixture["artifacts"]["789"] = [
            {"name": f"tested-tree-{TREE}", "id": 21},
            {"name": f"ci-reuse-{TREE}", "id": 22},
        ]
        fixture["artifact_zips"]["22"] = fixture["artifact_zips"]["11"]
        del fixture["artifact_zips"]["11"]
        fixture["jobs_by_run"]["789"] = jobs_page(overrides={REUSE_JOB: "skipped"})
        fixture["jobs_by_run"][str(REUSE_SOURCE_ID)] = jobs_page(overrides={REUSE_JOB: "failure"})
        fixture["artifacts"]["456"] = []
        fixture["run_pages"] = [page([]), page([])]
        self.fixture.write_text(json.dumps(fixture))
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tree match", result.stderr)

    def test_a_reuse_union_is_refused_when_a_source_job_is_red(self):
        union_fixture(
            self,
            jobs_by_run={
                "456": jobs_page(overrides={REUSE_JOB: "skipped"}),
                str(REUSE_SOURCE_ID): jobs_page(overrides={REUSE_JOB: "failure"}),
            },
        )
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"source job {REUSE_JOB}", result.stderr)
        self.assertIn("concluded failure", result.stderr)
        self.assertIn("/actions/runs/456", result.stderr)
        self.assertIn(f"/actions/runs/{REUSE_SOURCE_ID}", result.stderr)

    def test_a_reuse_union_is_refused_when_the_source_job_is_too_old(self):
        union_fixture(
            self,
            jobs_by_run={
                "456": jobs_page(overrides={REUSE_JOB: "skipped"}),
                str(REUSE_SOURCE_ID): jobs_page(completed_at="2026-11-10T11:00:00Z"),
            },
        )
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("older than 24h", result.stderr)
        self.assertIn("/actions/runs/456", result.stderr)
        self.assertIn(f"/actions/runs/{REUSE_SOURCE_ID}", result.stderr)

    def test_a_reuse_union_is_refused_for_a_fork_source_run(self):
        union_fixture(self)
        fixture = json.loads(self.fixture.read_text())
        source = run_doc(
            sha=SHA, event="pull_request", branch="fork/feature", run_id=REUSE_SOURCE_ID,
            repo=REPO, head_repo="someone/pohunek",
        )
        fixture["runs_by_id"][str(REUSE_SOURCE_ID)] = source
        self.fixture.write_text(json.dumps(fixture))
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a run of this repository", result.stderr)
        self.assertIn("/actions/runs/456", result.stderr)
        self.assertIn(f"/actions/runs/{REUSE_SOURCE_ID}", result.stderr)

    def test_a_reuse_union_is_refused_when_the_source_tree_differs(self):
        union_fixture(
            self,
            artifacts={
                "456": [{"name": f"ci-reuse-{TREE}", "id": 11}],
                str(REUSE_SOURCE_ID): [f"tested-tree-{OTHER_TREE}"],
            },
        )
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"recorded tested tree {OTHER_TREE}, expected {TREE}", result.stderr)
        self.assertIn("/actions/runs/456", result.stderr)
        self.assertIn(f"/actions/runs/{REUSE_SOURCE_ID}", result.stderr)

    def test_a_reuse_union_is_refused_when_the_record_names_a_job_that_ran(self):
        # The evidence run skipped REUSE_JOB, so the union is read; the record
        # also names "required check", which ran green in the evidence run.
        union_fixture(
            self,
            artifact_zips={
                "11": reuse_zip(
                    TREE, {"matrix": [REUSE_JOB], "check": ["required check"]}
                )
            },
        )
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("reuse record names required check, but it ran", result.stderr)
        self.assertIn("/actions/runs/456", result.stderr)

    def test_a_reuse_union_is_refused_for_a_disallowed_source_event(self):
        # `workflow_dispatch` is a lawful source event; narrowing the
        # allowlist to just `push` refuses the dispatch source by name.
        union_fixture(self)
        self.config_data["reuse"] = {
            "max_age_hours": 24,
            "source_events": ["push"],
        }
        self.write_config()
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not allow for reuse", result.stderr)
        self.assertIn(f"/actions/runs/{REUSE_SOURCE_ID}", result.stderr)

    def test_a_reuse_union_is_refused_for_a_nightly_source_event(self):
        union_fixture(self)
        self.config_data["reuse"] = {
            "max_age_hours": 24,
            "source_events": ["pull_request", "push", "workflow_dispatch"],
        }
        self.write_config()
        fixture = json.loads(self.fixture.read_text())
        source = run_doc(
            sha=SHA, event="schedule", branch="main", run_id=REUSE_SOURCE_ID,
            repo=REPO, head_repo=REPO,
        )
        fixture["runs_by_id"][str(REUSE_SOURCE_ID)] = source
        self.fixture.write_text(json.dumps(fixture))
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not allow for reuse", result.stderr)
        self.assertIn(f"/actions/runs/{REUSE_SOURCE_ID}", result.stderr)

    def test_a_run_with_a_skipped_job_and_no_reuse_record_keeps_the_old_output(self):
        union_fixture(self)
        fixture = json.loads(self.fixture.read_text())
        fixture["artifacts"]["456"] = []
        self.fixture.write_text(json.dumps(fixture))
        result = self.invoke("main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"required job {REUSE_JOB}: skipped", result.stderr)
        self.assertNotIn("reused_from", result.stderr)
        self.assertFalse(any(
            "/actions/artifacts/" in arg for call in self.calls_json() for arg in call
        ))

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
