"""Release script tests proving evidence gates the tag and publication ref."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "release"
EVIDENCE = ROOT / "scripts" / "ci-evidence"
CONFIG = ROOT / "scripts" / "ci-evidence.json"


FAKE_GIT = r'''#!/usr/bin/env python3
import json
import os
import subprocess
import sys

args = sys.argv[1:]
real = os.environ["REAL_GIT"]
repo = args[args.index("-C") + 1] if "-C" in args else os.getcwd()
command = args[args.index("-C") + 2] if "-C" in args else args[0]
if command in ("push", "tag"):
    with open(os.environ["EVENTS"], "a") as stream:
        stream.write(json.dumps(["git", command, *args[args.index("-C") + 2:]]) + "\n")
if command == "tag" and os.environ.get("ADVANCE_HEAD_BEFORE_TAG") == "1":
    subprocess.run(
        [real, "-C", repo, "-c", "user.name=Concurrent Commit", "-c",
         "user.email=concurrent@example.invalid", "commit", "--allow-empty",
         "-m", "Concurrent commit"],
        check=True,
        capture_output=True,
        text=True,
    )
if command == "push":
    raise SystemExit(0)
if command == "ls-remote":
    sha = os.environ.get("REMOTE_SHA")
    if sha is None:
        sha = subprocess.check_output([real, "-C", repo, "rev-parse", "HEAD"], text=True).strip()
    print(sha + "\trefs/heads/main")
    raise SystemExit(0)
os.execv(real, [real, *args])
'''

FAKE_CARGO = "#!/bin/sh\nexit 0\n"

# Stands in for `cargo xtask docs log fold`: records the call, and outside a dry
# run files the fragments the way the real fold does (log written, log.d gone).
FOLDING_CARGO = r'''#!/bin/sh
case "$*" in
  "xtask docs log fold "*)
    printf '%s\n' "cargo $*" >> "$EVENTS"
    case "$*" in
      *--dry-run*) ;;
      *)
        version=$(printf '%s\n' "$*" | sed 's/.*--version \([^ ]*\).*/\1/')
        printf '\n## %s\n\n- Folded.\n' "$version" >> docs/knowledge/log.md
        rm -rf docs/knowledge/log.d
        ;;
    esac
    ;;
esac
exit 0
'''

LOG = "# Knowledge Bundle Log\n\n## 0.1.0\n\n- First release.\n"
FRAGMENT = "## Unreleased (2026-10-14, a change, #1)\n\n- A change.\n"

FAKE_GH = r'''#!/usr/bin/env python3
import json
import os
import subprocess
import sys
from pathlib import Path

state_path = Path(os.environ["GH_STATE"])
events = Path(os.environ["EVENTS"])
state = json.loads(state_path.read_text())
if not state["sha"]:
    state["sha"] = subprocess.check_output(
        [os.environ["REAL_GIT"], "-C", os.environ["RELEASE_REPO"], "rev-parse", "HEAD"],
        text=True,
    ).strip()
    state_path.write_text(json.dumps(state))
args = sys.argv[1:]
with events.open("a") as stream:
    if args[:2] == ["workflow", "run"]:
        tier = next(value.split("=", 1)[1] for value in args if value.startswith("tier="))
        dispatch_id = next(value.split("=", 1)[1] for value in args if value.startswith("dispatch_id="))
        run_id = 900 + len(state["runs"])
        run = {
            "id": run_id,
            "run_number": run_id,
            "run_attempt": 1,
            "head_sha": state["sha"],
            "head_branch": "main",
            "path": ".github/workflows/ci.yml@refs/heads/main",
            "event": "workflow_dispatch",
            "display_title": f"CI [tier={tier}] [dispatch={dispatch_id}]",
            "status": "completed",
            "conclusion": "success",
            "html_url": f"https://github.com/example/pohunek/actions/runs/{run_id}",
            "tier": tier,
        }
        state["runs"].append(run)
        state_path.write_text(json.dumps(state))
        stream.write(f"gh-dispatch-{tier}\n")
        raise SystemExit(0)
    if args[0] != "api":
        raise SystemExit("unexpected gh call: " + repr(args))
    endpoint = next(value for value in args if value.startswith("repos/"))
    if endpoint.endswith("/commits"):
        print(json.dumps([{"sha": state["sha"]}]))
    elif "/actions/workflows/" in endpoint:
        wanted = next((value.split("=", 1)[1] for value in args if value.startswith("event=")), None)
        runs = [run for run in state["runs"] if wanted is None or run["event"] == wanted]
        print(json.dumps([{"workflow_runs": runs}]))
    elif "/actions/runs/" in endpoint:
        run_id = int(endpoint.split("/")[-2])
        run = next(run for run in state["runs"] if run["id"] == run_id)
        config = json.loads(Path(os.environ["EVIDENCE_CONFIG"]).read_text())
        jobs = [{"name": name, "status": "completed", "conclusion": "success", "steps": []}
                for name in config["required_jobs"][run["tier"]]]
        for requirement in config["required_steps"][run["tier"]]:
            job = next(job for job in jobs if job["name"] == requirement["job"])
            job["steps"].append({"name": requirement["step"], "status": "completed", "conclusion": "success"})
        print(json.dumps([{"jobs": jobs}]))
    else:
        raise SystemExit("unexpected Actions API endpoint: " + endpoint)
'''


class ReleaseEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.bin = self.repo / "bin"
        self.bin.mkdir()
        self.events = self.root / "events.jsonl"
        self.state = self.root / "gh-state.json"
        self.config = json.loads(CONFIG.read_text())
        self.config["wait_timeout_seconds"] = 4
        self.config["poll_interval_seconds"] = 1
        self.write_copy("scripts/release", SCRIPT.read_text(), executable=True)
        self.write_copy("scripts/ci-evidence", EVIDENCE.read_text(), executable=True)
        self.write_copy("scripts/ci-evidence.json", json.dumps(self.config))
        self.write_copy("bin/git", FAKE_GIT, executable=True)
        self.write_copy("bin/cargo", FAKE_CARGO, executable=True)
        self.write_copy("bin/gh", FAKE_GH, executable=True)
        self.write_copy("Cargo.toml", '[workspace.package]\nversion = "0.1.0"\n')
        self.write_copy("Cargo.lock", "version = 3\n")
        self.git("init", "-b", "main")
        self.git("config", "user.name", "Release Test")
        self.git("config", "user.email", "release-test@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "tag.gpgsign", "false")
        self.git("add", "Cargo.toml", "Cargo.lock", "scripts", "bin")
        self.git("commit", "-m", "Initial state")
        self.git("remote", "add", "origin", "https://github.com/example/pohunek.git")
        self.state.write_text(json.dumps({"sha": "", "runs": []}))

    def write_copy(self, relative, content, executable=False):
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        if executable:
            path.chmod(0o755)

    def git(self, *args):
        subprocess.run(
            [shutil.which("git"), "-C", str(self.repo), *args],
            check=True,
            capture_output=True,
            text=True,
        )

    def run_release(self, *args, remote_sha=None, advance_head_before_tag=False):
        env = os.environ.copy()
        env.update({
            "PATH": f"{self.bin}{os.pathsep}{env['PATH']}",
            "REAL_GIT": shutil.which("git"),
            "RELEASE_REPO": str(self.repo),
            "EVENTS": str(self.events),
            "GH_STATE": str(self.state),
            "EVIDENCE_CONFIG": str(self.repo / "scripts/ci-evidence.json"),
        })
        if remote_sha is not None:
            env["REMOTE_SHA"] = remote_sha
        if advance_head_before_tag:
            env["ADVANCE_HEAD_BEFORE_TAG"] = "1"
        return subprocess.run(
            [str(self.repo / "scripts/release"), "patch", *args],
            cwd=self.repo,
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )

    def read_events(self):
        return self.events.read_text().splitlines() if self.events.exists() else []

    def prepare_existing_candidate(self):
        (self.repo / "Cargo.toml").write_text('[workspace.package]\nversion = "0.1.1"\n')
        self.git("add", "Cargo.toml")
        self.git("commit", "-m", "Prepare release candidate")
        sha = subprocess.check_output(
            [shutil.which("git"), "-C", str(self.repo), "rev-parse", "HEAD"], text=True
        ).strip()
        runs = []
        for index, tier in enumerate(("main", "nightly"), start=1):
            run_id = 700 + index
            runs.append({
                "id": run_id,
                "run_number": run_id,
                "run_attempt": 1,
                "head_sha": sha,
                "head_branch": "main",
                "path": ".github/workflows/ci.yml@refs/heads/main",
                "event": "workflow_dispatch",
                "display_title": f"CI [tier={tier}]",
                "status": "completed",
                "conclusion": "success",
                "html_url": f"https://github.com/example/pohunek/actions/runs/{run_id}",
                "tier": tier,
            })
        self.state.write_text(json.dumps({"sha": sha, "runs": runs}))

    def add_knowledge_log(self):
        self.write_copy("docs/knowledge/log.md", LOG)
        self.write_copy("docs/knowledge/log.d/1-a-change.md", FRAGMENT)
        self.write_copy("bin/cargo", FOLDING_CARGO, executable=True)
        self.git("add", "docs", "bin")
        self.git("commit", "-m", "Record a knowledge-log fragment")

    def tracked(self, revision):
        return subprocess.check_output(
            [shutil.which("git"), "-C", str(self.repo), "ls-tree", "-r", "--name-only", revision],
            text=True,
        ).split()

    def test_release_commit_files_the_knowledge_log_fragments(self):
        self.add_knowledge_log()
        result = self.run_release()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cargo xtask docs log fold --version 0.1.1", self.read_events())
        candidate = json.loads(self.state.read_text())["sha"]
        self.assertNotIn("docs/knowledge/log.d/1-a-change.md", self.tracked(candidate))
        log = subprocess.check_output(
            [shutil.which("git"), "-C", str(self.repo), "show", f"{candidate}:docs/knowledge/log.md"],
            text=True,
        )
        self.assertIn("## 0.1.1", log)
        status = subprocess.check_output(
            [shutil.which("git"), "-C", str(self.repo), "status", "--porcelain"], text=True
        )
        self.assertEqual(status, "", "the release commit holds the whole fold")

    def test_dry_run_plans_the_fold_and_changes_nothing(self):
        self.add_knowledge_log()
        result = self.run_release("--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cargo xtask docs log fold --version 0.1.1 --dry-run", self.read_events())
        self.assertIn("docs/knowledge/log.d/1-a-change.md", self.tracked("HEAD"))
        self.assertTrue((self.repo / "docs/knowledge/log.d/1-a-change.md").exists())

    def test_already_versioned_candidate_with_unfolded_fragments_is_refused(self):
        self.add_knowledge_log()
        self.prepare_existing_candidate()
        result = self.run_release("0.1.1", "--no-push")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("still has knowledge-log fragments", result.stderr)
        self.assertFalse((self.repo / ".git/refs/tags/v0.1.1").exists())

    def test_push_checks_both_tiers_before_tag_and_tag_push(self):
        result = self.run_release()
        self.assertEqual(result.returncode, 0, result.stderr)
        events = self.read_events()
        decoded = [json.loads(event) if event.startswith("[") else event for event in events]
        self.assertLess(decoded.index(["git", "push", "push", "origin", "main"]), decoded.index("gh-dispatch-main"))
        self.assertLess(decoded.index("gh-dispatch-main"), decoded.index("gh-dispatch-nightly"))
        tag_event = next(
            event
            for event in decoded
            if isinstance(event, list) and event[:5] == ["git", "tag", "tag", "-a", "v0.1.1"]
        )
        self.assertIn(json.loads(self.state.read_text())["sha"], tag_event)
        self.assertLess(decoded.index("gh-dispatch-nightly"), decoded.index(tag_event))
        self.assertLess(decoded.index(tag_event), decoded.index(["git", "push", "push", "origin", "v0.1.1"]))

    def test_no_push_refuses_to_tag_without_existing_evidence(self):
        result = self.run_release("--no-push")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no main tier run exists", result.stderr)
        self.assertFalse(any(event.startswith('["git", "tag"') for event in self.read_events()))
        self.assertFalse(any(event.startswith('["git", "push"') for event in self.read_events()))

    def test_no_push_tags_an_already_versioned_commit_with_exact_evidence(self):
        self.prepare_existing_candidate()
        result = self.run_release("0.1.1", "--no-push")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.repo / ".git/refs/tags/v0.1.1").exists())
        self.assertFalse(any(event.startswith('["git", "push"') for event in self.read_events()))

    def test_remote_branch_movement_after_evidence_refuses_to_tag(self):
        result = self.run_release(remote_sha="f" * 40)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("moved while CI evidence was running", result.stderr)
        self.assertFalse(any(event.startswith('["git", "tag"') for event in self.read_events()))
        self.assertFalse(any(event.startswith('["git", "push", "push", "origin", "v') for event in self.read_events()))

    def test_concurrent_head_advance_tags_the_verified_candidate(self):
        result = self.run_release(advance_head_before_tag=True)
        self.assertEqual(result.returncode, 0, result.stderr)

        candidate_sha = json.loads(self.state.read_text())["sha"]
        head_sha = subprocess.check_output(
            [shutil.which("git"), "-C", str(self.repo), "rev-parse", "HEAD"],
            text=True,
        ).strip()
        tagged_sha = subprocess.check_output(
            [shutil.which("git"), "-C", str(self.repo), "rev-parse", "v0.1.1^{}"],
            text=True,
        ).strip()
        self.assertNotEqual(head_sha, candidate_sha)
        self.assertEqual(tagged_sha, candidate_sha)


if __name__ == "__main__":
    unittest.main()
