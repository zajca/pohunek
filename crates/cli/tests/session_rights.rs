//! Caller classification by kernel peer and the per-session target rule, through
//! the real daemon, workers and CLI (#2005).
//!
//! A manager session and its workers run a fake Claude agent that executes the
//! commands the test sends it through `session input`, so a `pohunek` command
//! runs inside the PTY tree of a session. The same command run from the test
//! process is the owner's terminal. The daemon is a `pohunekd` subprocess with
//! subprocess workers, so there are no worker units: ancestry is the only
//! evidence that places a caller.

// Rust guideline compliant 2026-11-15

#![cfg(unix)]

#[path = "support/background_host.rs"]
#[expect(
    dead_code,
    reason = "this file uses the daemon start, the CLI calls and session inspection of the host"
)]
mod background_host;
#[path = "../../daemon/tests/support/official_runtimes.rs"]
mod official_runtimes;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use background_host::Host;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::fs::write_executable;
use pohunek_test_support::wait::poll_until;
use serde_json::Value;

/// Agent profile whose sessions run the `claude` runtime.
const AGENT_PROFILE: &str = "rights-claude";

/// Agent profile whose sessions run the `shell` runtime.
const SHELL_PROFILE: &str = "rights-shell";

/// Daemon id the scenario gives to the sessions that spawn others. The daemon
/// records the pair as claimed by the owner's own process.
const SPAWNER_DAEMON: &str = "d-rights-scenario";

/// Owner-only directory mode for the config tree the daemon trusts.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Text the audit line of a refusal starts with.
const REFUSAL_LINE: &str = "session right refused";

/// Input text of a refused request; no audit line may carry it.
const INPUT_MARKER: &str = "RightsMarkerQ9k4";

/// The fake agent. It answers the version probe of the official package, then
/// runs `run:<command>` lines in a child shell and prints the frame file a
/// `show:<file>` line names.
fn agent_script() -> String {
    format!(
        "#!/bin/sh\n{}set -u\nprintf 'rights-agent ready\\n'\n\
         while IFS= read -r line; do\n\
         case \"$line\" in\n\
         run:*) sh -c \"${{line#run:}}\"; printf 'rights-ran\\n';;\n\
         show:*) cat \"${{line#show:}}\";;\n\
         quit) exit 0;;\n\
         esac\n\
         done\n",
        official_runtimes::sh_version_answer("claude")
    )
}

/// A daemon with a manager session, workers and a shell session.
struct Scenario {
    host: Host,
    cli: PathBuf,
    results: PathBuf,
    next_result: std::cell::Cell<u32>,
}

impl Scenario {
    fn start() -> Self {
        let inputs = pohunek_test_support::tempdir().expect("input directory");
        let env = TestEnv::new().expect("hermetic test environment");
        let host = Host::start(env, inputs.path());
        let agent = host.env.root().join("agents").join("rights-agent.sh");
        write_executable(&agent, agent_script()).expect("write the rights agent");
        let agents_dir = host.env.config_home().join("pohunek").join("agents");
        let claude = format!(
            "base = \"claude\"\n{}program = \"{}\"\nargs = []\n\n\
             [input_rules]\nbracketed_paste = false\n",
            official_runtimes::profile_pin("claude"),
            agent.display(),
        );
        let shell = format!(
            "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"{}\"]\n\n\
             [input_rules]\nbracketed_paste = false\n",
            agent.display(),
        );
        for (name, body) in [(AGENT_PROFILE, claude), (SHELL_PROFILE, shell)] {
            let path = agents_dir.join(format!("{name}.toml"));
            fs::write(&path, body).expect("write the agent profile");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .expect("keep the agent profile private");
        }
        let results = host.env.root().join("rights-results");
        fs::DirBuilder::new()
            .mode(PRIVATE_DIR_MODE)
            .create(&results)
            .expect("create the results directory");
        Self {
            host,
            cli: pohunek_test_support::bin_exe("pohunek"),
            results,
            next_result: std::cell::Cell::new(0),
        }
    }

    /// Writes the profile `name`, the claude scenario profile plus `rights`
    /// as its `[rights]` table body.
    fn add_profile(&self, name: &str, rights: &str) {
        let agents_dir = self.host.env.config_home().join("pohunek").join("agents");
        let base = fs::read_to_string(agents_dir.join(format!("{AGENT_PROFILE}.toml")))
            .expect("read the scenario profile");
        let path = agents_dir.join(format!("{name}.toml"));
        fs::write(&path, format!("{base}\n[rights]\n{rights}\n")).expect("write the profile");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("keep the profile private");
    }

    /// The session `id` records as its parent, if any.
    fn parent_of(&self, id: &str) -> Option<String> {
        self.state(id)["spawned_by"]["session_id"]
            .as_str()
            .map(str::to_owned)
    }

    /// Creates a session of `profile`, spawned by `parent` when given, and
    /// waits until its agent runs.
    fn spawn(&self, profile: &str, name: &str, parent: Option<&str>) -> String {
        let cwd = self.host.env.cwd().display().to_string();
        let args = [
            "session", "new", "--agent", profile, "--cwd", &cwd, "--name", name, "--json",
        ];
        let markers: Vec<(&str, &OsStr)> = parent
            .map(|id| {
                vec![
                    ("POHUNEK_SESSION_ID", OsStr::new(id)),
                    ("POHUNEK_DAEMON_ID", OsStr::new(SPAWNER_DAEMON)),
                ]
            })
            .unwrap_or_default();
        let output = self.host.run_with_env(&args, &markers);
        let document: Value = serde_json::from_slice(&output.stdout).expect("session JSON");
        assert_eq!(output.status.code(), Some(0), "{document}");
        let created = &document["ok"];
        let id = created["session"]["id"]
            .as_str()
            .or_else(|| created["id"].as_str())
            .expect("created session id")
            .to_owned();
        self.host.wait_screen_line(&id, "rights-agent ready");
        id
    }

    /// Runs `pohunek <arguments>` inside the PTY tree of `session` and returns
    /// its exit code and combined output.
    fn inside(&self, session: &str, arguments: &str) -> (i32, String) {
        self.inside_with(session, "", arguments)
    }

    /// [`Self::inside`] with `assignments` (`NAME=value ...`) in the command's
    /// environment.
    fn inside_with(&self, session: &str, assignments: &str, arguments: &str) -> (i32, String) {
        let number = self.next_result.get();
        self.next_result.set(number + 1);
        let result = self.results.join(format!("{number}.txt"));
        let command = format!(
            "{assignments} {} {arguments} >{} 2>&1; echo rc=$? >>{}",
            self.cli.display(),
            result.display(),
            result.display()
        );
        self.send(session, &format!("run:{command}"));
        read_result(&result)
    }

    /// Runs `pohunek <arguments>` from a process that left the session's tree.
    ///
    /// The escape is the reparent, which is what a double fork does and which
    /// `setsid` alone does not do (it keeps the parent; macOS has no `setsid`
    /// command). A background subshell waits on a FIFO until the agent's child
    /// shell has exited. The agent reads its next line only after that shell
    /// ended, so by the time the second line releases the subshell it has been
    /// reparented away from the PTY root.
    fn detached(&self, session: &str, arguments: &str) -> (i32, String) {
        let number = self.next_result.get();
        self.next_result.set(number + 1);
        let result = self.results.join(format!("{number}.txt"));
        let fifo = self.results.join(format!("{number}.fifo"));
        self.send(session, &format!("run:mkfifo {}", fifo.display()));
        let command = format!(
            "( read _ <{} ; {} {arguments} >{} 2>&1; echo rc=$? >>{} ) &",
            fifo.display(),
            self.cli.display(),
            result.display(),
            result.display()
        );
        self.send(session, &format!("run:{command}"));
        self.send(session, &format!("run:echo go >{}", fifo.display()));
        read_result(&result)
    }

    fn send(&self, session: &str, line: &str) {
        self.host.ok(&["session", "input", session, line]);
    }

    fn state(&self, session: &str) -> Value {
        self.host.inspect(session)
    }

    fn is_live(&self, session: &str) -> bool {
        self.state(session)["runtime"]["state"] == "live"
    }

    /// The distinct audit lines of refused requests the daemon logged so far.
    fn refusals(&self) -> Vec<String> {
        let mut lines = Vec::new();
        collect_lines(self.host.env.root(), &mut lines);
        lines.sort_unstable();
        lines.dedup();
        lines
    }

    /// Stops and removes every session, so no worker outlives the test.
    fn finish(self) {
        let (code, listed) = self.host.json(&["session", "list"]);
        assert_eq!(code, 0, "{listed}");
        let ids: Vec<String> = listed["ok"]
            .as_array()
            .expect("sessions")
            .iter()
            .filter_map(|session| session["id"].as_str().map(str::to_owned))
            .collect();
        for id in &ids {
            if self.is_live(id) {
                self.host.stop_session(id);
            }
            let (code, removed) = self.host.json(&["session", "rm", id]);
            assert_eq!(code, 0, "{removed}");
        }
    }
}

fn read_result(path: &Path) -> (i32, String) {
    poll_until("the command run inside the session to finish", || {
        let text = fs::read_to_string(path).ok()?;
        let (output, code) = text.rsplit_once("rc=")?;
        let code = code.trim().parse().ok()?;
        Some((code, output.to_owned()))
    })
}

/// Lines holding [`REFUSAL_LINE`] in the regular files below `dir`.
fn collect_lines(dir: &Path, lines: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_lines(&path, lines);
        } else if kind.is_file() {
            if let Ok(text) = fs::read_to_string(&path) {
                lines.extend(
                    text.lines()
                        .filter(|l| l.contains(REFUSAL_LINE))
                        .map(str::to_owned),
                );
            }
        }
    }
}

fn assert_denied(result: &(i32, String), reason: &str) {
    assert_ne!(result.0, 0, "{}", result.1);
    assert!(result.1.contains("session_right_denied"), "{}", result.1);
    assert!(
        result.1.contains(&format!("reason={reason}")),
        "reason {reason}: {}",
        result.1
    );
}

/// Rows 1-4, 8 and 9: a worker is confined to its descendants, the owner is
/// not, a manager reaches its worker, and a worker answers its manager.
#[test]
fn a_session_reaches_its_descendants_and_its_parent_only() {
    let scenario = Scenario::start();
    let manager = scenario.spawn(AGENT_PROFILE, "manager", None);
    let w1 = scenario.spawn(AGENT_PROFILE, "worker-one", Some(&manager));
    let w2 = scenario.spawn(AGENT_PROFILE, "worker-two", Some(&manager));

    // Row 1: a sibling is out of reach, and nothing happened to it.
    let stop = scenario.inside(&w1, &format!("session stop {w2} --json"));
    assert_denied(&stop, "not_descendant");
    assert!(
        scenario.is_live(&w2),
        "the refused stop left the sibling running"
    );
    let input = scenario.inside(&w1, &format!("session input {w2} {INPUT_MARKER} --json"));
    assert_denied(&input, "not_descendant");
    let attach = scenario.inside(&w1, &format!("attach {w2}"));
    assert_denied(&attach, "not_descendant");

    // A session's own session keeps the existing refusals, with or without
    // the markers the CLI sends.
    let own_stop = scenario.inside(&w1, &format!("session stop {w1} --json"));
    assert_ne!(own_stop.0, 0, "{}", own_stop.1);
    assert!(
        own_stop.1.contains("plugin_self_target_denied"),
        "{}",
        own_stop.1
    );
    let own_attach = scenario.inside(&w1, &format!("attach {w1}"));
    assert_ne!(own_attach.0, 0, "{}", own_attach.1);
    assert!(
        own_attach.1.contains("refusing to attach to session"),
        "{}",
        own_attach.1
    );

    // The audit lines name the refusal and never the request's text.
    poll_until("the audit lines of the three refusals", || {
        (scenario.refusals().len() >= 3).then_some(())
    });
    let lines = scenario.refusals();
    assert!(
        lines
            .iter()
            .all(|line| line.contains("\"reason\":\"not_descendant\"")
                && line.contains(&w2)
                && !line.contains(INPUT_MARKER)),
        "{lines:?}"
    );

    // Row 3: a manager reaches its worker.
    let to_worker = scenario.inside(&manager, &format!("session input {w1} hello --json"));
    assert_eq!(to_worker.0, 0, "{}", to_worker.1);
    // Row 4: a worker answers its idle manager.
    let to_manager = scenario.inside(&w1, &format!("session input {manager} done --json"));
    assert_eq!(to_manager.0, 0, "{}", to_manager.1);
    // Toward the manager only input is allowed.
    let stop_parent = scenario.inside(&w1, &format!("session stop {manager} --json"));
    assert_denied(&stop_parent, "not_descendant");

    // Row 9: a manager removes a worker tree.
    let w1a = scenario.spawn(AGENT_PROFILE, "worker-one-child", Some(&w1));
    scenario.host.stop_session(&w1a);
    let removed = scenario.inside(&manager, &format!("session rm {w1} --with-children --json"));
    assert_eq!(removed.0, 0, "{}", removed.1);
    let (code, listed) = scenario.host.json(&["session", "list"]);
    assert_eq!(code, 0, "{listed}");
    let ids: Vec<&str> = listed["ok"]
        .as_array()
        .expect("sessions")
        .iter()
        .filter_map(|session| session["id"].as_str())
        .collect();
    assert!(
        !ids.contains(&w1.as_str()) && !ids.contains(&w1a.as_str()),
        "{ids:?}"
    );

    // Row 2: the owner's terminal is not confined.
    scenario.host.stop_session(&w2);
    scenario.finish();
}

/// Row 5: a worker cannot type into a parent that waits on a dialog.
#[test]
fn a_worker_cannot_answer_a_blocked_parent() {
    let scenario = Scenario::start();
    let manager = scenario.spawn(AGENT_PROFILE, "manager", None);
    let worker = scenario.spawn(AGENT_PROFILE, "worker", Some(&manager));
    let frame = pohunek_test_support::workspace_root().join("compat/claude/screens/approval.txt");
    scenario.host.ok(&[
        "session",
        "input",
        &manager,
        &format!("show:{}", frame.display()),
    ]);
    poll_until("the manager to read as blocked", || {
        (scenario.state(&manager)["activity"] == "blocked").then_some(())
    });
    let answer = scenario.inside(&worker, &format!("session input {manager} 1 --json"));
    assert_denied(&answer, "parent_blocked");
    scenario.finish();
}

/// Row 6: a managed shell session keeps owner rights unless a session spawned it.
#[test]
fn a_shell_session_keeps_owner_rights() {
    let scenario = Scenario::start();
    let manager = scenario.spawn(AGENT_PROFILE, "manager", None);
    let victim = scenario.spawn(AGENT_PROFILE, "victim", Some(&manager));
    let worker = scenario.spawn(AGENT_PROFILE, "worker", Some(&manager));
    let shell = scenario.spawn(SHELL_PROFILE, "shell", None);
    // The control: the same command from a worker of the same tree is refused.
    let refused = scenario.inside(&worker, &format!("session stop {victim} --json"));
    assert_denied(&refused, "not_descendant");
    assert!(scenario.is_live(&victim));
    // A shell a worker spawned has the rights of its own tree, not the owner's.
    let spawned = scenario.inside(
        &worker,
        &format!("session new --agent {SHELL_PROFILE} --json"),
    );
    assert_eq!(spawned.0, 0, "{}", spawned.1);
    let document: Value = serde_json::from_str(&spawned.1).expect("session new JSON");
    let child_shell = document["ok"]["session"]["id"]
        .as_str()
        .or_else(|| document["ok"]["id"].as_str())
        .expect("spawned shell id")
        .to_owned();
    scenario
        .host
        .wait_screen_line(&child_shell, "rights-agent ready");
    let from_child = scenario.inside(&child_shell, &format!("session stop {victim} --json"));
    assert_denied(&from_child, "not_descendant");
    let stop = scenario.inside(&shell, &format!("session stop {victim} --json"));
    assert_eq!(stop.0, 0, "{}", stop.1);
    poll_until("the victim to stop", || {
        (!scenario.is_live(&victim)).then_some(())
    });
    scenario.finish();
}

/// Row 7: a process that leaves the session tree keeps owner rights where only
/// ancestry places a caller, and the doctor states that limit.
#[test]
fn a_process_that_left_the_tree_keeps_owner_rights() {
    let scenario = Scenario::start();
    let manager = scenario.spawn(AGENT_PROFILE, "manager", None);
    let w1 = scenario.spawn(AGENT_PROFILE, "worker-one", Some(&manager));
    let w2 = scenario.spawn(AGENT_PROFILE, "worker-two", Some(&manager));
    let inside = scenario.inside(&w1, &format!("session stop {w2} --json"));
    assert_denied(&inside, "not_descendant");
    let escaped = scenario.detached(&w1, &format!("session stop {w2} --json"));
    assert_eq!(escaped.0, 0, "{}", escaped.1);
    poll_until("the sibling to stop", || {
        (!scenario.is_live(&w2)).then_some(())
    });

    // The doctor exits non-zero when a provider on the host is out of range,
    // which says nothing about the rights check read below.
    let (_, doctor) = scenario.host.json(&["doctor"]);
    let check = doctor["ok"]["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .find(|check| check["name"] == "session_rights")
        .expect("the session_rights check");
    assert_eq!(check["status"], "ok", "{check}");
    let detail = check["detail"].as_str().expect("detail");
    assert!(
        detail.contains("processes that leave a session tree keep owner rights"),
        "{detail}"
    );
    // Workers of this daemon are direct children, so only ancestry places a caller.
    assert!(
        detail.contains("placement: ancestry") && !detail.contains("cgroup"),
        "{detail}"
    );
    scenario.finish();
}

/// Runs `session new` for `profile` inside `session` and returns the created
/// session's id, or the refused result.
fn spawn_inside(
    scenario: &Scenario,
    session: &str,
    profile: &str,
    prefix: &str,
) -> Result<String, (i32, String)> {
    let cwd = scenario.host.env.cwd().display().to_string();
    let result = scenario.inside_with(
        session,
        prefix,
        &format!("session new --agent {profile} --cwd {cwd} --json"),
    );
    if result.0 != 0 {
        return Err(result);
    }
    let document: Value = serde_json::from_str(&result.1).expect("session new JSON");
    Ok(document["ok"]["session"]["id"]
        .as_str()
        .or_else(|| document["ok"]["id"].as_str())
        .expect("created session id")
        .to_owned())
}

/// Rows 10 and 11: a session spawns under itself whatever markers it sends.
#[test]
fn a_session_spawns_under_itself_and_cannot_name_another_origin() {
    let scenario = Scenario::start();
    let manager = scenario.spawn(AGENT_PROFILE, "manager", None);
    let w1 = scenario.spawn(AGENT_PROFILE, "worker-one", Some(&manager));

    let child = spawn_inside(&scenario, &w1, AGENT_PROFILE, "").expect("a worker spawns a child");
    assert_eq!(scenario.parent_of(&child).as_deref(), Some(w1.as_str()));

    let before = scenario.host.json(&["session", "list"]).1["ok"]
        .as_array()
        .expect("sessions")
        .len();
    let forged = format!("POHUNEK_SESSION_ID={manager} POHUNEK_DAEMON_ID={SPAWNER_DAEMON}");
    let refused = spawn_inside(&scenario, &w1, AGENT_PROFILE, &forged)
        .expect_err("markers naming another session are refused");
    assert_denied(&refused, "origin_mismatch");
    let after = scenario.host.json(&["session", "list"]).1["ok"]
        .as_array()
        .expect("sessions")
        .len();
    assert_eq!(before, after, "the refused request created no session");

    // The owner's own request records the markers it sends.
    let adopted = scenario.spawn(AGENT_PROFILE, "owner-made", Some(&manager));
    assert_eq!(
        scenario.parent_of(&adopted).as_deref(),
        Some(manager.as_str())
    );
    scenario.finish();
}

/// Rows 12-15: the owner moves a session under a new parent, and the rights
/// follow the link; a session cannot adopt, and a loop is refused.
#[test]
fn adopt_moves_a_session_and_its_rights_to_the_new_parent() {
    let scenario = Scenario::start();
    let m1 = scenario.spawn(AGENT_PROFILE, "manager-one", None);
    let m2 = scenario.spawn(AGENT_PROFILE, "manager-two", None);
    let w1 = scenario.spawn(AGENT_PROFILE, "worker", Some(&m1));

    let before = scenario.inside(&m2, &format!("session input {w1} hello --json"));
    assert_denied(&before, "not_descendant");

    let adopt = scenario.host.ok(&["session", "adopt", &w1, &m2]);
    assert_eq!(
        adopt["session"]["spawned_by"]["session_id"],
        m2.as_str(),
        "{adopt}"
    );
    assert_eq!(scenario.parent_of(&w1).as_deref(), Some(m2.as_str()));

    let after = scenario.inside(&m2, &format!("session input {w1} hello --json"));
    assert_eq!(after.0, 0, "{}", after.1);
    let old = scenario.inside(&m1, &format!("session input {w1} hello --json"));
    assert_denied(&old, "not_descendant");

    // A session cannot give itself, or anything, a new parent.
    let from_inside = scenario.inside(&m1, &format!("session adopt {w1} {m1} --json"));
    assert_denied(&from_inside, "owner_only");
    assert_eq!(scenario.parent_of(&w1).as_deref(), Some(m2.as_str()));

    // A loop and a self-parent are refused for the owner too.
    let (code, cycle) = scenario.host.json(&["session", "adopt", &m2, &w1]);
    assert_ne!(code, 0, "{cycle}");
    assert!(
        cycle.to_string().contains("session_adopt_refused"),
        "{cycle}"
    );
    assert!(cycle.to_string().contains("reason=cycle"), "{cycle}");
    let (code, own) = scenario.host.json(&["session", "adopt", &w1, &w1]);
    assert_ne!(code, 0, "{own}");
    assert!(own.to_string().contains("reason=self_parent"), "{own}");
    assert_eq!(scenario.parent_of(&m2), None);
    scenario.finish();
}

/// Rows 16-18: `spawn_profiles` lists exactly the profiles a session may
/// launch, and an empty list forbids spawning.
#[test]
fn spawn_profiles_limit_what_a_session_launches() {
    let scenario = Scenario::start();
    scenario.add_profile(
        "rights-lead",
        &format!("spawn_profiles = [\"{AGENT_PROFILE}\"]"),
    );
    scenario.add_profile("rights-closed", "spawn_profiles = []");
    let lead = scenario.spawn("rights-lead", "lead", None);
    let closed = scenario.spawn("rights-closed", "closed", None);

    spawn_inside(&scenario, &lead, AGENT_PROFILE, "").expect("a listed profile is launched");
    let refused = spawn_inside(&scenario, &lead, SHELL_PROFILE, "")
        .expect_err("a profile off the list is refused");
    assert_denied(&refused, "spawn_profile_not_allowed");
    let none = spawn_inside(&scenario, &closed, AGENT_PROFILE, "")
        .expect_err("an empty list forbids spawning");
    assert_denied(&none, "spawn_profile_not_allowed");

    // A table the daemon cannot read grants nothing and does not load.
    scenario.add_profile("rights-bad", "owner_equivalent = \"yes\"");
    let (code, bad) = scenario
        .host
        .json(&["session", "new", "--agent", "rights-bad"]);
    assert_ne!(code, 0, "{bad}");
    assert!(bad.to_string().contains("invalid_profile"), "{bad}");
    scenario.finish();
}

/// Row 19: a profile with `owner_equivalent` gives its session the owner's
/// rights, and a profile without it does not.
#[test]
fn an_owner_equivalent_profile_acts_like_the_owner() {
    let scenario = Scenario::start();
    scenario.add_profile("rights-manager", "owner_equivalent = true");
    let manager = scenario.spawn("rights-manager", "manager", None);
    let plain = scenario.spawn(AGENT_PROFILE, "plain", None);
    let w1 = scenario.spawn(AGENT_PROFILE, "worker-one", None);
    let w2 = scenario.spawn(AGENT_PROFILE, "worker-two", None);

    // The control: a session without the flag cannot reach a non-descendant.
    let refused = scenario.inside(&plain, &format!("session stop {w1} --json"));
    assert_denied(&refused, "not_descendant");
    assert!(scenario.is_live(&w1));

    let stop = scenario.inside(&manager, &format!("session stop {w2} --json"));
    assert_eq!(stop.0, 0, "{}", stop.1);
    poll_until("the worker to stop", || {
        (!scenario.is_live(&w2)).then_some(())
    });
    // The same profile spawns under the caller's own session and may adopt.
    let adopt = scenario.inside(&manager, &format!("session adopt {w1} {manager} --json"));
    assert_eq!(adopt.0, 0, "{}", adopt.1);
    assert_eq!(scenario.parent_of(&w1).as_deref(), Some(manager.as_str()));
    scenario.finish();
}
