"""Behavioural checks for `scripts/ci-budgets` (stdlib only).

Most cases drive the real script against `--input` snapshots built in the
test, using the snapshot format `ci-timings --cache` writes, and a fake `gh`
on PATH for the issue calls. The live-path scenario seeds the same fake
with `run list`/`run view` payloads and runs without `--input`, so the
cache-then-`--input` round trip shares one selection with the offline path.
The fake records its write calls (issue create, edit, comment, label
create) so the tests assert which writes happened; no case touches the
network or any host state.
"""

import hermetic_git  # noqa: F401  (isolates git from the developer's configuration)

import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "ci-budgets"
ROOT = Path(__file__).resolve().parents[2]


def stamp(hour, minute=0, day=20):
    """One UTC timestamp inside the fixture window."""
    return f"2026-09-{day:02d}T{hour:02d}:{minute:02d}:00Z"


class BudgetsHarness(unittest.TestCase):
    """Fixture helpers: snapshots, budget files, and a fake `gh`."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.bin = self.dir / "bin"
        self.bin.mkdir()
        self.calls_path = self.dir / "gh-calls.jsonl"
        self.state_path = self.dir / "gh-state.json"
        self.write_state({"issues": [], "labels": []})
        fake = self.bin / "gh"
        fake.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            f"STATE = {str(self.state_path)!r}\n"
            f"CALLS = {str(self.calls_path)!r}\n"
            "state = json.loads(Path(STATE).read_text())\n"
            "calls = []\n"
            "p = Path(CALLS)\n"
            "if p.exists():\n"
            "    calls = [json.loads(l) for l in p.read_text().splitlines() if l.strip()]\n"
            "args = sys.argv[1:]\n"
            "body = None\n"
            "if '--body-file' in args:\n"
            "    i = args.index('--body-file')\n"
            "    name = args[i + 1]\n"
            "    body = sys.stdin.read() if name == '-' else Path(name).read_text()\n"
            "    args = args[:i] + args[i + 2:]\n"
            "elif '--body' in args:\n"
            "    i = args.index('--body')\n"
            "    body = args[i + 1]\n"
            "    args = args[:i] + args[i + 2:]\n"
            "def record(kind, extra=None):\n"
            "    entry = {'args': args}\n"
            "    if body is not None:\n"
            "        entry['body'] = body\n"
            "    if extra:\n"
            "        entry.update(extra)\n"
            "    calls.append(entry)\n"
            "    Path(CALLS).write_text(''.join(json.dumps(c) + chr(10) for c in calls))\n"
            "handled = True\n"
            "if args[:2] == ['issue', 'list']:\n"
            "    if '--json' not in args:\n"
            "        print('fake gh: issue list without --json', file=sys.stderr)\n"
            "        sys.exit(1)\n"
            "    listed = state['issues']\n"
            "    if isinstance(listed, list):\n"
            "        listed = [i for i in listed if not i.get('closed')]\n"
            "    if '--state' in args:\n"
            "        i = args.index('--state')\n"
            "        if args[i + 1] != 'open':\n"
            "            print('fake gh: issue list state must be open', file=sys.stderr)\n"
            "            sys.exit(1)\n"
            "    print(json.dumps(listed))\n"
            "elif args[:2] == ['label', 'list']:\n"
            "    if '--json' not in args:\n"
            "        print('fake gh: label list without --json', file=sys.stderr)\n"
            "        sys.exit(1)\n"
            "    print(json.dumps([{'name': n} for n in state['labels']]))\n"
            "elif args[:3] == ['repo', 'view', '--json']:\n"
            "    print(json.dumps({'nameWithOwner': 'o/r'}))\n"
            "elif args[:2] == ['run', 'list']:\n"
            "    if '--json' not in args:\n"
            "        print('fake gh: run list without --json', file=sys.stderr)\n"
            "        sys.exit(1)\n"
            "    runs = state.get('runs', [])\n"
            "    if '--limit' in args:\n"
            "        i = args.index('--limit')\n"
            "        runs = runs[:int(args[i + 1])]\n"
            "    fields = args[args.index('--json') + 1].split(',')\n"
            "    print(json.dumps([{f: r.get(f) for f in fields} for r in runs]))\n"
            "elif args[:2] == ['run', 'view']:\n"
            "    run_id = int(args[2])\n"
            "    attempt = 1\n"
            "    if '--attempt' in args:\n"
            "        attempt = int(args[args.index('--attempt') + 1])\n"
            "    for r in state.get('runs', []):\n"
            "        if r.get('databaseId') == run_id and r.get('attempt', 1) == attempt:\n"
            "            print(json.dumps(r))\n"
            "            break\n"
            "    else:\n"
            "        print(f'fake gh: no run {run_id} attempt {attempt}', file=sys.stderr)\n"
            "        sys.exit(1)\n"
            "elif args[:3] == ['label', 'create', 'ci-budget']:\n"
            "    state['labels'].append('ci-budget')\n"
            "    Path(STATE).write_text(json.dumps(state))\n"
            "    record('label-create')\n"
            "elif args[:2] == ['issue', 'create']:\n"
            "    number = max([i['number'] for i in state['issues']] or [0]) + 1\n"
            "    state['issues'].append({'number': number, 'title': 'CI critical-path budget exceeded', 'body': body})\n"
            "    Path(STATE).write_text(json.dumps(state))\n"
            "    record('issue-create', {'number': number})\n"
            "    print(f'https://github.com/o/r/issues/{number}')\n"
            "elif args[:2] == ['issue', 'comment']:\n"
            "    record('issue-comment')\n"
            "elif args[:2] == ['issue', 'close']:\n"
            "    if os.environ.get('GH_FAIL_CLOSE'):\n"
            "        print('fake gh: issue close refused', file=sys.stderr)\n"
            "        sys.exit(1)\n"
            "    for i in state['issues']:\n"
            "        if str(i['number']) == args[2]:\n"
            "            i['closed'] = True\n"
            "    Path(STATE).write_text(json.dumps(state))\n"
            "    record('issue-close')\n"
            "elif args[:2] == ['issue', 'edit']:\n"
            "    if os.environ.get('GH_FAIL_EDIT'):\n"
            "        print('fake gh: issue edit refused', file=sys.stderr)\n"
            "        sys.exit(1)\n"
            "    for i in state['issues']:\n"
            "        if str(i['number']) == args[2]:\n"
            "            i['body'] = body\n"
            "    Path(STATE).write_text(json.dumps(state))\n"
            "    record('issue-edit')\n"
            "else:\n"
            "    handled = False\n"
            "if not handled:\n"
            "    print(f'fake gh: unexpected {args}', file=sys.stderr)\n"
            "    sys.exit(1)\n"
        )
        fake.chmod(fake.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)

    def write_state(self, state):
        self.state_path.write_text(json.dumps(state))

    def write_runs_state(self, runs):
        """Seed the fake `gh run list`/`run view` listing."""
        state = json.loads(self.state_path.read_text())
        state["runs"] = runs
        self.write_state(state)

    def write_calls(self):
        if not self.calls_path.exists():
            return []
        return [
            json.loads(line)
            for line in self.calls_path.read_text().splitlines()
            if line.strip()
        ]

    def rendered_body(self, snapshot, budgets):
        """Render the issue body without touching `gh` (no write calls)."""
        namespace = {"__file__": str(SCRIPT)}
        source = SCRIPT.read_text().replace(
            "if __name__ == ", "if False and __name__ == ")
        exec(compile(source, str(SCRIPT), "exec"), namespace)
        config = namespace["load_budgets"](budgets)
        medians = namespace["measure"](
            window_runs=config["window_runs"],
            input_path=snapshot, branch="main",
        )
        rows = namespace["check_budgets"](config["budgets"], medians)
        return namespace["render_body"](
            rows, window_runs=config["window_runs"], medians=medians,
            slug="o/r",
        )

    def write_budgets(self, budgets, *, window_runs=30, label="ci-budget"):
        path = self.dir / "budgets.json"
        path.write_text(json.dumps({
            "window_runs": window_runs,
            "issue_label": label,
            "budgets": budgets,
        }, indent=1) + "\n")
        return str(path)

    def step_budget(self, job, step, limit, *, event="push"):
        return {
            "event": event, "job": job, "step": step,
            "median_seconds_max": limit,
            "reason": f"test budget for {job}/{step}",
        }

    def wall_budget(self, limit, *, event="push", job="tests (core, fast)"):
        entry = {
            "event": event, "job": job, "step": None,
            "median_seconds_max": limit,
            "reason": f"test wall budget for {job}",
        }
        if event == "pull_request":
            entry["job"] = None
        return entry

    def document(self, run_id, *, event="push", branch="main",
                 jobs=None, hour=10, day=20, wall_minutes=30,
                 conclusion="success", attempt=1, created_minute=0):
        return {
            "databaseId": run_id,
            "name": "ci.yml",
            "displayTitle": "CI",
            "event": event,
            "conclusion": conclusion,
            "createdAt": stamp(hour, minute=created_minute, day=day),
            "startedAt": stamp(hour, day=day),
            "updatedAt": stamp(hour, minute=wall_minutes, day=day),
            "attempt": attempt,
            "headBranch": branch,
            "workflowName": "CI",
            "jobs": list(jobs) if jobs is not None else [],
        }

    def green_lane_jobs(self, lane_seconds, hour):
        """One green push run's jobs for the macOS lane step."""
        return [self.job(
            "platform contracts (arm64)", lane_seconds + 300,
            steps=[self.step("Run macOS lane tests", lane_seconds)],
            start_hour=hour,
        )]

    def job(self, name, seconds, *, start_hour=10, conclusion="success",
            steps=None):
        start = stamp(start_hour)
        end_hour, end_minute = start_hour, 0
        total = int(seconds)
        end_hour += total // 3600
        end_minute += (total % 3600) // 60
        entry = {
            "name": name,
            "startedAt": start,
            "completedAt": f"2026-09-20T{end_hour:02d}:{end_minute:02d}:00Z",
            "conclusion": conclusion,
        }
        if steps is not None:
            entry["steps"] = steps
        return entry

    def step(self, name, seconds, *, number=1, start_hour=10):
        total = int(seconds)
        return {
            "name": name,
            "number": number,
            "startedAt": stamp(start_hour),
            "completedAt": (
                f"2026-09-20T{start_hour + total // 3600:02d}"
                f":{(total % 3600) // 60:02d}:{total % 60:02d}Z"
            ),
            "conclusion": "success",
        }

    def run_cli(self, *arguments, extra_env=None):
        env = dict(os.environ)
        env["PATH"] = str(self.bin) + os.pathsep + env.get("PATH", "")
        env["PYTHONSAFEPATH"] = "1"
        if extra_env:
            env.update(extra_env)
        return subprocess.run(
            [sys.executable, str(SCRIPT), *arguments],
            capture_output=True, text=True, check=False,
            cwd=str(self.dir), env=env,
        )

    def write_call_kinds(self):
        return [call["args"][:2] for call in self.write_calls()]

    def comment_count(self):
        return len([c for c in self.write_calls()
                    if c["args"][:2] == ["issue", "comment"]])


class BudgetCheckTests(BudgetsHarness):
    """Exit codes and stdout through the real `ci-budgets` boundary."""

    def snapshot(self, name, documents):
        path = self.dir / name
        path.write_text(json.dumps(documents, indent=1) + "\n")
        return str(path)

    def lane_jobs(self, lane_seconds, hour):
        """Push-run jobs with per-run lane seconds (scalar or per-run list)."""
        values = lane_seconds if isinstance(lane_seconds, list) else [lane_seconds]
        jobs = []
        for offset, seconds in enumerate(values):
            jobs.extend(self.green_lane_jobs(seconds, hour + offset))
        return jobs

    def lane_snapshot(self, lane_seconds=960, *, runs=4, name="snapshot.json"):
        """Push runs where the macOS lane step takes `lane_seconds`.

        A list names one duration per green run, newest last, so a mixed
        window can check that the median covers the whole window instead
        of the newest runs. Non-green push runs sit between the green
        ones (a cancelled run, a failed run, and a green rerun whose
        first attempt failed), so the window must skip them and still
        hold `runs` green first attempts. Pull-request runs carry their
        own job for the PR wall median.
        """
        if isinstance(lane_seconds, list):
            durations = list(lane_seconds)
        else:
            durations = [lane_seconds] * runs
        documents = []
        run_id = 0
        for green, seconds in enumerate(durations):
            hour = 10 + green
            run_id += 1
            documents.append(self.document(
                run_id, jobs=self.green_lane_jobs(seconds, hour),
                hour=hour, created_minute=green,
            ))
            run_id += 1
            documents.append(self.document(
                run_id, jobs=self.green_lane_jobs(seconds, hour),
                hour=hour, created_minute=green,
                conclusion="cancelled",
            ))
        # A failed first attempt with a green rerun: the rerun must never
        # count as a second green run. (Older than every green run above,
        # so the window keeps the newest `runs` greens.)
        stale = lane_seconds[0] if isinstance(lane_seconds, list) else lane_seconds
        run_id += 1
        documents.append(self.document(
            run_id, jobs=self.green_lane_jobs(stale, 8),
            hour=7, day=19, conclusion="failure",
        ))
        documents.append(self.document(
            run_id, jobs=self.green_lane_jobs(stale, 8),
            hour=7, day=19, attempt=2,
        ))
        run_id += 1
        documents.append(self.document(
            run_id, jobs=self.green_lane_jobs(stale, 9),
            hour=7, day=18, conclusion="failure",
        ))
        for index in range(runs):
            run_id += 1
            jobs = [self.job("tests (core, fast)", 600, start_hour=12)]
            documents.append(self.document(
                run_id, event="pull_request", branch="topic",
                jobs=jobs, hour=12, created_minute=index,
            ))
        # The PR wall median over these `runs` pull_request runs.
        return self.snapshot(name, documents)

    def test_within_budget_exits_zero_and_makes_no_issue_call(self):
        snapshot = self.lane_snapshot()
        budgets = self.write_budgets([
            self.step_budget("platform contracts (arm64)",
                             "Run macOS lane tests", 1200),
            self.wall_budget(2000, event="pull_request"),
        ], window_runs=4)
        # Without --report-issue nothing writes, not even the label probe.
        quiet = self.run_cli("--input", snapshot, "--budgets", budgets)
        self.assertEqual(quiet.returncode, 0, quiet.stderr)
        self.assertIn("within budget", quiet.stdout)
        self.assertEqual(self.write_calls(), [])
        # With --report-issue the green night never writes when no
        # issue is open and the window is green.
        result = self.run_cli("--input", snapshot, "--budgets", budgets,
                              "--report-issue")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.write_calls(), [])
        # With an open issue the green night rewrites the body and closes
        # it; the comment set stays empty.
        self.write_state({"issues": [{"number": 1, "title": "t",
                                      "body": "stale"}], "labels": ["ci-budget"]})
        repaired = self.run_cli("--input", snapshot, "--budgets", budgets,
                                "--report-issue")
        self.assertEqual(repaired.returncode, 0, repaired.stderr)
        self.assertIn(["issue", "edit"], self.write_call_kinds())
        self.assertIn(["issue", "close"], self.write_call_kinds())
        self.assertEqual(self.comment_count(), 0)

    def test_window_skips_cancelled_failed_and_rerun_attempts(self):
        # The fixture interleaves cancelled and failed push runs with a
        # failed-first/green-rerun pair: the table still reports 4 green
        # runs per budget, never the 9 listed push documents.
        snapshot = self.lane_snapshot()
        budgets = self.write_budgets([
            self.step_budget("platform contracts (arm64)",
                             "Run macOS lane tests", 1200),
            self.wall_budget(2000, event="pull_request"),
            self.wall_budget(2000),
            self.wall_budget(2000, job="build tests (nextest archive)"),
        ], window_runs=4)
        result = self.run_cli("--input", snapshot, "--budgets", budgets)
        self.assertEqual(result.returncode, 1, result.stderr)
        # The archive wall budget names a job no green run carries.
        self.assertIn("budget names a missing job/step", result.stdout)
        body = self.rendered_body(snapshot, budgets)
        self.assertIn("| push | platform contracts (arm64) | "
                      "Run macOS lane tests | 20m00s | 16m00s | 4 |",
                      body)
        self.assertIn("| pull_request | (run wall) | (job wall) | 33m20s | "
                      "30m00s | 4 |", body)

    def test_planted_slow_step_trips_the_check(self):
        # The lane step median (1200 s) clears its 1000 s budget.
        snapshot = self.lane_snapshot(lane_seconds=1200)
        budgets = self.write_budgets([
            self.step_budget("platform contracts (arm64)",
                             "Run macOS lane tests", 1000),
        ], window_runs=4)
        result = self.run_cli("--input", snapshot, "--budgets", budgets)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("over budget", result.stdout)
        # Without --report-issue nothing writes.
        self.assertEqual(self.write_calls(), [])

    def test_step_median_covers_the_whole_window(self):
        # The newest two greens run the lane step at 1000 s, the
        # older two at 500 s, so the true window median is 750 s. A budget
        # of 900 s must pass; a median over only the newest runs
        # reports 1000 s.
        snapshot = self.lane_snapshot(
            lane_seconds=[500, 500, 1000, 1000])
        budgets = self.write_budgets([
            self.step_budget("platform contracts (arm64)",
                             "Run macOS lane tests", 900),
        ], window_runs=4)
        result = self.run_cli("--input", snapshot, "--budgets", budgets)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("median 12m30s", result.stdout)

    def test_live_window_skips_non_green_first_attempts(self):
        # The live fetch lists newest runs first, so a cancelled
        # run, a failed run, and a green rerun sitting among the newest
        # six listings must not shrink the four-green window. The window
        # median over the surviving greens is 2500 s; a window-sized
        # fetch limited to the newest four listings would report 3500 s.
        listings = [
            self.document(60, jobs=self.green_lane_jobs(5000, hour=15),
                          created_minute=50),
            self.document(50, jobs=self.green_lane_jobs(4000, hour=14),
                          conclusion="failure", created_minute=40),
            self.document(40, jobs=self.green_lane_jobs(3000, hour=13),
                          conclusion="cancelled", created_minute=30),
            self.document(30, jobs=self.green_lane_jobs(2000, hour=12),
                          created_minute=20),
            self.document(20, jobs=self.green_lane_jobs(2000, hour=11),
                          conclusion="failure", attempt=2,
                          created_minute=19),
            self.document(20, jobs=self.green_lane_jobs(3000, hour=11),
                          created_minute=10),
            self.document(10, jobs=self.green_lane_jobs(1000, hour=10),
                          created_minute=0),
        ]
        self.write_runs_state(listings)
        budgets = self.write_budgets([
            self.step_budget("platform contracts (arm64)",
                             "Run macOS lane tests", 2600),
        ], window_runs=4)
        result = self.run_cli("--budgets", budgets)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("median 41m40s", result.stdout)

    def test_report_issue_creates_then_edits_with_a_comment_once(self):
        snapshot = self.lane_snapshot(lane_seconds=1200)
        budgets = self.write_budgets([
            self.step_budget("platform contracts (arm64)",
                             "Run macOS lane tests", 1000),
        ], window_runs=4)
        first = self.run_cli("--input", snapshot, "--budgets", budgets,
                             "--report-issue")
        self.assertEqual(first.returncode, 1, first.stderr)
        self.assertIn(["issue", "create"], self.write_call_kinds())
        self.assertIn(["issue", "comment"], self.write_call_kinds())
        # The first run creates the label beside the issue.
        self.assertIn(["label", "create"], self.write_call_kinds())
        # The body carries the machine-readable marker for this set.
        state = json.loads(self.state_path.read_text())
        body = state["issues"][0]["body"]
        self.assertIn("<!-- ci-budgets:exceeded=", body)
        self.assertIn("EXCEEDED", body)
        # The same exceedance twice rewrites the body but adds no comment.
        before = self.comment_count()
        second = self.run_cli("--input", snapshot, "--budgets", budgets,
                              "--report-issue")
        self.assertEqual(second.returncode, 1, second.stderr)
        self.assertEqual(self.comment_count(), before)
        edits = [c for c in self.write_calls()
                 if c["args"][:2] == ["issue", "edit"]]
        self.assertEqual(len(edits), 1)

    def test_changed_exceedance_set_comments_once_per_change(self):
        # Row 8: the step alone trips, then the slow lane wall joins it
        # (the set grows from one key to two), then both recover and the
        # night closes the issue. Each set change adds exactly one
        # comment; repeating a set adds none. The third snapshot keeps
        # the slow lane step while its runs also carry a slow lane job
        # wall, so the second step grows the set instead of repeating it.
        green = self.lane_snapshot()
        slow = self.lane_snapshot(lane_seconds=1200, name="slow.json")
        # Same slow lane step, but the newest four greens also run a
        # slow lane job wall: add two minutes to each lane job so the
        # 1500 s wall budget trips beside the already-tripped step.
        slower_documents = []
        bumped = 0
        for document in json.loads(Path(slow).read_text()):
            if (document.get("event") == "push"
                    and document.get("conclusion") == "success"
                    and document.get("attempt") == 1
                    and bumped < 4):
                jobs = [
                    (self.job(
                        job.get("name"), 1620,
                        steps=job.get("steps"),
                        start_hour=10)
                     if job.get("name") == "platform contracts (arm64)"
                     else job)
                    for job in document["jobs"]
                ]
                slower_documents.append(dict(document, jobs=jobs))
                bumped += 1
            else:
                slower_documents.append(document)
        both_slow = self.snapshot("both-slow.json", slower_documents)
        budgets = self.write_budgets([
            self.step_budget("platform contracts (arm64)",
                             "Run macOS lane tests", 1000),
            # Wall 1500 s trips only for the lane job's wall (lane
            # runs cost lane_seconds + 300 s), never for the core job
            # the plain snapshot lacks; the third snapshot adds a slow
            # lane wall beside the same slow step.
            self.wall_budget(1500, job="platform contracts (arm64)"),
        ], window_runs=4)
        one = self.run_cli("--input", slow, "--budgets", budgets,
                           "--report-issue")
        self.assertEqual(one.returncode, 1, one.stderr)
        self.assertEqual(self.comment_count(), 1)
        # Slow step only, no slow lane wall yet: at exactly 1500 s the
        # wall budget does not trip, so the marker holds one key.
        state = json.loads(self.state_path.read_text())
        marker = state["issues"][0]["body"].split(
            "<!-- ci-budgets:exceeded=")[1].split("-->")[0]
        self.assertEqual(len([key for key in marker.split(",") if key]), 1)
        both = self.run_cli("--input", both_slow, "--budgets", budgets,
                            "--report-issue")
        self.assertEqual(both.returncode, 1, both.stderr)
        self.assertEqual(self.comment_count(), 2)
        # Recover the lane step and its wall: the set empties, so the
        # night closes the open issue with no new comment.
        recovered = self.run_cli("--input", green, "--budgets", budgets,
                                 "--report-issue")
        self.assertEqual(recovered.returncode, 0, recovered.stderr)
        self.assertEqual(self.comment_count(), 2)
        repeats = self.run_cli("--input", green, "--budgets", budgets,
                               "--report-issue")
        self.assertEqual(repeats.returncode, 0, repeats.stderr)
        self.assertEqual(self.comment_count(), 2)

    def test_recovery_rewrites_and_closes_then_regression_comments(self):
        # Row 7: exceed, recover, exceed again. Recovery closes the issue
        # with the empty marker; the next regression comments again.
        slow = self.lane_snapshot(lane_seconds=1200)
        green = self.lane_snapshot(name="green.json")
        budgets = self.write_budgets([
            self.step_budget("platform contracts (arm64)",
                             "Run macOS lane tests", 1000),
        ], window_runs=4)
        exceeded = self.run_cli("--input", slow, "--budgets", budgets,
                                "--report-issue")
        self.assertEqual(exceeded.returncode, 1, exceeded.stderr)
        self.assertEqual(self.comment_count(), 1)
        self.assertNotIn(["issue", "close"], self.write_call_kinds())
        recovered = self.run_cli("--input", green, "--budgets", budgets,
                                 "--report-issue")
        self.assertEqual(recovered.returncode, 0, recovered.stderr)
        self.assertIn(["issue", "close"], self.write_call_kinds())
        state = json.loads(self.state_path.read_text())
        self.assertIn("<!-- ci-budgets:exceeded=-->",
                      state["issues"][0]["body"])
        # The close removes the issue from the open list, so the
        # regression opens a second issue and comments on it.
        regressed = self.run_cli("--input", slow, "--budgets", budgets,
                                 "--report-issue")
        self.assertEqual(regressed.returncode, 1, regressed.stderr)
        self.assertEqual(self.comment_count(), 2)
        creations = [c for c in self.write_calls()
                     if c["args"][:2] == ["issue", "create"]]
        self.assertEqual(len(creations), 2)

    def test_issue_edit_failure_is_a_usage_error(self):
        # Row 6: a `gh` failure on the write path exits 2 with no
        # comment. The first run opens the issue (create path); the second
        # hits the failing edit.
        snapshot = self.lane_snapshot(lane_seconds=1200)
        budgets = self.write_budgets([
            self.step_budget("platform contracts (arm64)",
                             "Run macOS lane tests", 1000),
        ], window_runs=4)
        opened = self.run_cli("--input", snapshot, "--budgets", budgets,
                              "--report-issue")
        self.assertEqual(opened.returncode, 1, opened.stderr)
        result = self.run_cli("--input", snapshot, "--budgets", budgets,
                              "--report-issue",
                              extra_env={"GH_FAIL_EDIT": "1"})
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("over budget", result.stdout)
        self.assertIn("issue edit refused", result.stderr)
        self.assertEqual(self.comment_count(), 1)

    def test_issue_close_failure_on_recovery_is_a_usage_error(self):
        # Row 6 on the repair path: the window is green, the issue is
        # open, and `gh issue close` refuses. The night must exit 2 and
        # leave the issue open.
        snapshot = self.lane_snapshot()
        budgets = self.write_budgets([
            self.step_budget("platform contracts (arm64)",
                             "Run macOS lane tests", 1200),
        ], window_runs=4)
        self.write_state({"issues": [{"number": 1, "title": "t",
                                      "body": "stale"}],
                          "labels": ["ci-budget"]})
        result = self.run_cli("--input", snapshot, "--budgets", budgets,
                              "--report-issue",
                              extra_env={"GH_FAIL_CLOSE": "1"})
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("issue close refused", result.stderr)
        self.assertNotIn(["issue", "close"], self.write_call_kinds()[-1:])
        state = json.loads(self.state_path.read_text())
        self.assertFalse(state["issues"][0].get("closed", False))

    def test_push_job_wall_and_pr_wall_exceedances_trip(self):
        # The two push job-wall budgets and the PR run-wall budget go
        # through the wall-median branches of `check_budgets`.
        documents = []
        for run_id in range(1, 5):
            jobs = [
                self.job("tests (core, fast)", 800, start_hour=10),
                self.job("build tests (nextest archive)", 400,
                         start_hour=10),
            ]
            documents.append(self.document(run_id, jobs=jobs, hour=10,
                                           wall_minutes=30))
        for run_id in range(5, 9):
            jobs = [self.job("tests (core, fast)", 600, start_hour=12)]
            documents.append(self.document(
                run_id, event="pull_request", branch="topic", jobs=jobs,
                hour=12, wall_minutes=50, created_minute=run_id,
            ))
        snapshot = self.snapshot("walls.json", documents)
        budgets = self.write_budgets([
            self.wall_budget(700),
            self.wall_budget(300, job="build tests (nextest archive)"),
            self.wall_budget(2000, event="pull_request"),
        ], window_runs=4)
        result = self.run_cli("--input", snapshot, "--budgets", budgets,
                              "--report-issue")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("push tests (core, fast) (job wall)", result.stdout)
        self.assertIn("over budget", result.stdout)
        self.assertIn(["issue", "create"], self.write_call_kinds())

    def test_bad_issue_json_is_a_usage_error(self):
        # Row 6: `gh issue list` printing text (a proxy that forgets
        # `--json`) exits 2 before any write.
        snapshot = self.lane_snapshot(lane_seconds=1200)
        budgets = self.write_budgets([
            self.step_budget("platform contracts (arm64)",
                             "Run macOS lane tests", 1000),
        ], window_runs=4)
        self.write_state({"issues": "not-a-list", "labels": []})
        result = self.run_cli("--input", snapshot, "--budgets", budgets,
                              "--report-issue")
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("over budget", result.stdout)
        self.assertIn("invalid JSON", result.stderr)
        self.assertEqual(self.write_calls(), [])

    def test_missing_step_fails_with_the_update_message(self):
        snapshot = self.lane_snapshot()
        budgets = self.write_budgets([
            self.step_budget("platform contracts (arm64)",
                             "No such step after a lane rename", 1000),
        ], window_runs=4)
        result = self.run_cli("--input", snapshot, "--budgets", budgets)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(
            "budget names a missing job/step: update scripts/ci-budgets.json",
            result.stdout,
        )

    def test_dry_run_makes_no_gh_write_call(self):
        snapshot = self.lane_snapshot(lane_seconds=1200)
        budgets = self.write_budgets([
            self.step_budget("platform contracts (arm64)",
                             "Run macOS lane tests", 1000),
        ], window_runs=4)
        result = self.run_cli("--input", snapshot, "--budgets", budgets,
                              "--report-issue", "--dry-run")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("dry run: no issue write", result.stdout)
        self.assertEqual(self.write_calls(), [])

    def test_bad_snapshot_is_a_usage_error(self):
        budgets = self.write_budgets([
            self.step_budget("platform contracts (arm64)",
                             "Run macOS lane tests", 1000),
        ], window_runs=4)
        missing = str(self.dir / "absent.json")
        result = self.run_cli("--input", missing, "--budgets", budgets)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(self.write_calls(), [])


class BudgetWorkflowTests(unittest.TestCase):
    """Static guards for `.github/workflows/ci-budgets.yml`."""

    WORKFLOW = (ROOT / ".github" / "workflows" / "ci-budgets.yml").read_text()

    def test_workflow_has_no_pr_or_push_trigger(self):
        self.assertNotIn("pull_request", self.WORKFLOW)
        # `push` appears in prose ("push runs"); the trigger form is `on:
        # push`, which must not appear.
        self.assertNotRegex(self.WORKFLOW, r"(?m)^\s*push\s*:")

    def test_workflow_permissions_are_minimal(self):
        # Every `permissions:` mapping is parsed, so an extra grant on
        # this job (`contents: write`, `pull-requests: write`, ...), a
        # second job with its own write grant, or a top-level grant
        # cannot hide behind a substring match.
        blocks = []
        lines = self.WORKFLOW.splitlines()
        for index, line in enumerate(lines):
            if "permissions:" not in line:
                continue
            indent = len(line) - len(line.lstrip())
            rest = line.partition("permissions:")[2].strip()
            if rest:
                blocks.append((indent, rest))
                continue
            mapping = {}
            for following in lines[index + 1:]:
                if not following.strip():
                    continue
                depth = len(following) - len(following.lstrip())
                if depth <= indent:
                    break
                key, _, value = following.strip().partition(":")
                mapping[key.strip()] = value.strip()
            blocks.append((indent, mapping))
        self.assertEqual(len(blocks), 2, blocks)
        self.assertEqual(blocks[0][1], "{}")
        self.assertEqual(
            blocks[1][1],
            {"actions": "read", "contents": "read",
             "issues": "write"},
        )
        self.assertNotRegex(self.WORKFLOW, r"write-all")

    def test_workflow_uses_a_pinned_checkout_without_credentials(self):
        self.assertIn(
            "actions/checkout@11d5960a326750d5838078e36cf38b85af677262",
            self.WORKFLOW,
        )
        self.assertIn("persist-credentials: false", self.WORKFLOW)

    def test_every_budget_has_a_reason(self):
        config = json.loads(
            (ROOT / "scripts" / "ci-budgets.json").read_text()
        )
        self.assertGreater(len(config["budgets"]), 0)
        for entry in config["budgets"]:
            self.assertTrue(entry["reason"].strip(), entry)
