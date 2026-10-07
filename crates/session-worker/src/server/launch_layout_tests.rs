//! Launch-identity selection over macOS-style Node/Bun wrapper layouts.
//!
//! The scripted inspector supplies process facts, so no interpreter needs to
//! be installed and no path has to exist on the host; executable paths are
//! already the canonical (symlink-resolved) form `proc_pidpath` reports.

// Rust guideline compliant 2026-09-29

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use pohunek_platform::process::{
    Error, ExitWatch, OwnershipMarkers, Pid, ProcessFact, ProcessIdentity, ProcessInspector,
    StartIdentity,
};

use crate::journal::{LaunchIdentity, PendingLaunchClaim};
use crate::ChildIdentity;

use super::{
    designated_launch_process_with, is_provider_child_of_launch, verify_launch_claim_with,
};

const ROOT_PID: Pid = 500;
const ROOT_START: u64 = 1_000;
const AGENT_PID: Pid = 501;
const AGENT_START: u64 = 1_001;
const WORKER_UNRELATED_PID: Pid = 900;

/// Process table with per-process executable paths and optional PID reuse.
#[derive(Debug, Default)]
struct Table {
    facts: Vec<ProcessFact>,
    executables: HashMap<Pid, PathBuf>,
    /// PIDs whose identity lookups start answering with a different start
    /// identity after the given number of calls (PID reuse mid-inspection).
    reused_after: HashMap<Pid, (usize, StartIdentity)>,
    identity_calls: Mutex<HashMap<Pid, usize>>,
}

impl Table {
    fn with(mut self, pid: Pid, parent: Pid, start: u64, executable: &str, argv: &[&str]) -> Self {
        self.facts.push(ProcessFact {
            pid,
            pgid: ROOT_PID,
            ppid: parent,
            start_identity: StartIdentity::new(start),
            comm: PathBuf::from(executable)
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or_default()
                .to_owned(),
            cmdline: argv.iter().map(|value| (*value).to_owned()).collect(),
        });
        self.executables.insert(pid, PathBuf::from(executable));
        self
    }

    /// A process of another user: visible in the table, but the same-user
    /// inspector reports no executable for it.
    fn with_foreign(mut self, pid: Pid, parent: Pid, start: u64, argv: &[&str]) -> Self {
        self = self.with(pid, parent, start, "/placeholder", argv);
        self.executables.remove(&pid);
        self
    }

    fn fact(&self, pid: Pid) -> Option<&ProcessFact> {
        self.facts.iter().find(|fact| fact.pid == pid)
    }
}

impl ProcessInspector for Table {
    fn identity(&self, pid: Pid) -> Result<Option<ProcessIdentity>, Error> {
        let calls = {
            let mut calls = self.identity_calls.lock().expect("call counter");
            let entry = calls.entry(pid).or_default();
            *entry += 1;
            *entry
        };
        let Some(fact) = self.fact(pid) else {
            return Ok(None);
        };
        let start_identity = match self.reused_after.get(&pid) {
            Some((after, replacement)) if calls > *after => *replacement,
            _ => fact.start_identity,
        };
        Ok(Some(ProcessIdentity {
            pid,
            start_identity,
        }))
    }

    fn parent_pid(&self, pid: Pid) -> Result<Option<Pid>, Error> {
        Ok(self.fact(pid).map(|fact| fact.ppid))
    }

    fn process(&self, pid: Pid) -> Result<Option<ProcessFact>, Error> {
        Ok(self.fact(pid).cloned())
    }

    fn same_user_processes(&self) -> Result<Vec<ProcessFact>, Error> {
        Ok(self.facts.clone())
    }

    fn descendants(&self, root: Pid) -> Result<Vec<ProcessFact>, Error> {
        let mut found: Vec<ProcessFact> = Vec::new();
        let mut frontier = vec![root];
        while let Some(parent) = frontier.pop() {
            for fact in self.facts.iter().filter(|fact| fact.ppid == parent) {
                if fact.pid != root && !found.iter().any(|seen| seen.pid == fact.pid) {
                    found.push(fact.clone());
                    frontier.push(fact.pid);
                }
            }
        }
        Ok(found)
    }

    fn cwd(&self, _pid: Pid) -> Result<PathBuf, Error> {
        Err(Error::Unavailable {
            operation: "scripted_cwd",
        })
    }

    fn executable(&self, pid: Pid) -> Result<Option<PathBuf>, Error> {
        Ok(self.executables.get(&pid).cloned())
    }

    fn exit_watch(&self, _identity: ProcessIdentity) -> Result<ExitWatch, Error> {
        Err(Error::Unavailable {
            operation: "scripted_exit_watch",
        })
    }

    fn ownership_markers(&self, _pid: Pid) -> Result<OwnershipMarkers, Error> {
        Ok(OwnershipMarkers::default())
    }

    fn foreground_process_group(&self, _root_pid: Pid) -> Result<Option<Pid>, Error> {
        Ok(None)
    }
}

fn root() -> ProcessIdentity {
    ProcessIdentity {
        pid: ROOT_PID,
        start_identity: StartIdentity::new(ROOT_START),
    }
}

/// A shell PTY root with one agent process below it.
fn table_with_agent(executable: &str, argv: &[&str]) -> Table {
    Table::default()
        .with(ROOT_PID, 1, ROOT_START, "/bin/zsh", &["-zsh"])
        .with(AGENT_PID, ROOT_PID, AGENT_START, executable, argv)
}

fn selected(table: &Table, provider: &str) -> Option<(Pid, u64)> {
    designated_launch_process_with(table, root(), provider)
        .expect("inspection succeeds")
        .map(|process| (process.pid, process.start_identity))
}

fn claim(provider: &str, pid: Pid, start: u64) -> PendingLaunchClaim {
    let child = |pid, start: u64| ChildIdentity {
        pid,
        process_group: i32::try_from(ROOT_PID).expect("pid fits"),
        start_identity: start.to_string(),
    };
    PendingLaunchClaim {
        retry_pending: true,
        worker_instance_id: "runtime-a".to_owned(),
        root: child(ROOT_PID, ROOT_START),
        identity: LaunchIdentity {
            provider: provider.to_owned(),
            process: child(pid, start),
            reference_kind: "session".to_owned(),
            native_reference: "ref".to_owned(),
        },
        expires_at: "2099-01-01T00:00:00Z".to_owned(),
        sequence: 0,
        latest_reference: None,
    }
}

/// Layouts whose running image is named for the provider are accepted.
#[test]
fn provider_named_images_in_macos_install_prefixes_are_selected() {
    let layouts: [(&str, &str, &[&str]); 5] = [
        (
            "codex",
            "/opt/homebrew/Caskroom/codex/0.1.0/codex-aarch64-apple-darwin",
            &["codex"],
        ),
        ("codex", "/usr/local/bin/codex", &["codex", "--resume"]),
        (
            "codex",
            "/Users/dev/.bun/install/global/node_modules/@openai/codex/vendor/aarch64-apple-darwin/codex/codex",
            &["codex"],
        ),
        (
            "codex",
            "/Users/dev/.nvm/versions/node/v22.1.0/lib/node_modules/@openai/codex/vendor/aarch64-apple-darwin/codex/codex",
            &["codex"],
        ),
        ("hermes", "/opt/homebrew/bin/hermes", &["hermes", "chat"]),
    ];
    for (provider, executable, argv) in layouts {
        let table = table_with_agent(executable, argv);
        assert_eq!(
            selected(&table, provider),
            Some((AGENT_PID, AGENT_START)),
            "{executable}"
        );
    }
}

/// The interpreter image is never the identity, whatever script it runs.
#[test]
fn interpreter_images_are_not_selected_for_any_wrapper_layout() {
    let layouts: [(&str, &[&str]); 6] = [
        (
            "/opt/homebrew/bin/node",
            &[
                "node",
                "/opt/homebrew/lib/node_modules/@openai/codex/bin/codex.js",
            ],
        ),
        (
            "/usr/local/bin/node",
            &[
                "node",
                "/usr/local/lib/node_modules/@openai/codex/bin/codex.js",
            ],
        ),
        (
            "/Users/dev/.bun/bin/bun",
            &[
                "bun",
                "/Users/dev/.bun/install/global/node_modules/@openai/codex/bin/codex.js",
            ],
        ),
        (
            "/Users/dev/.nvm/versions/node/v22.1.0/bin/node",
            &["node", "/Users/dev/.nvm/versions/node/v22.1.0/bin/codex"],
        ),
        (
            // `#!/usr/bin/env node` script: the kernel runs node with the
            // script path as argv[1].
            "/usr/local/bin/node",
            &["node", "/usr/local/bin/codex", "--resume"],
        ),
        (
            // npm-global wrapper resolved through `npm exec`.
            "/opt/homebrew/bin/node",
            &[
                "node",
                "/opt/homebrew/lib/node_modules/npm/bin/npx-cli.js",
                "@openai/codex",
            ],
        ),
    ];
    for (executable, argv) in layouts {
        let table = table_with_agent(executable, argv);
        assert_eq!(selected(&table, "codex"), None, "{executable} {argv:?}");
    }
}

#[test]
fn a_wrapper_whose_child_image_is_provider_named_selects_only_that_child() {
    let table = Table::default()
        .with(ROOT_PID, 1, ROOT_START, "/bin/zsh", &["-zsh"])
        .with(
            AGENT_PID,
            ROOT_PID,
            AGENT_START,
            "/opt/homebrew/bin/node",
            &[
                "node",
                "/opt/homebrew/lib/node_modules/@openai/codex/bin/codex.js",
            ],
        )
        .with(
            502,
            AGENT_PID,
            1_002,
            "/opt/homebrew/lib/node_modules/@openai/codex/vendor/aarch64-apple-darwin/codex/codex",
            &["codex"],
        );

    assert_eq!(selected(&table, "codex"), Some((502, 1_002)));
}

#[test]
fn a_different_provider_name_never_selects_a_process() {
    let table = table_with_agent("/opt/homebrew/bin/hermes", &["hermes"]);
    assert_eq!(selected(&table, "codex"), None);
    assert_eq!(selected(&table, "claude"), None);
}

#[test]
fn a_same_named_process_outside_the_pty_tree_is_ignored() {
    let table = table_with_agent("/opt/homebrew/bin/node", &["node", "codex.js"]).with(
        WORKER_UNRELATED_PID,
        1,
        2_000,
        "/opt/homebrew/bin/codex",
        &["codex"],
    );
    assert_eq!(selected(&table, "codex"), None);
}

#[test]
fn a_foreign_user_process_without_an_executable_is_never_selected() {
    let table = table_with_agent("/opt/homebrew/bin/node", &["node", "codex.js"]).with_foreign(
        502,
        AGENT_PID,
        1_002,
        &["codex"],
    );
    assert_eq!(selected(&table, "codex"), None);
}

#[test]
fn pid_reuse_during_inspection_fails_closed() {
    let mut table = table_with_agent("/opt/homebrew/bin/codex", &["codex"]);
    // The first identity lookup sees the real generation, the recheck sees a
    // different process under the same PID.
    table
        .reused_after
        .insert(AGENT_PID, (1, StartIdentity::new(AGENT_START + 77)));

    let error = designated_launch_process_with(&table, root(), "codex")
        .expect_err("a swapped generation must not be accepted");
    assert!(
        error.to_string().contains("launch candidate changed"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_retry_claim_for_the_provider_image_verifies() {
    let table = table_with_agent("/opt/homebrew/bin/codex", &["codex"]);
    assert!(verify_launch_claim_with(&table, &claim("codex", AGENT_PID, AGENT_START)).unwrap());
}

#[test]
fn a_retry_claim_naming_the_interpreter_never_verifies() {
    let table = table_with_agent(
        "/opt/homebrew/bin/node",
        &[
            "node",
            "/opt/homebrew/lib/node_modules/@openai/codex/bin/codex.js",
        ],
    );
    assert!(!verify_launch_claim_with(&table, &claim("codex", AGENT_PID, AGENT_START)).unwrap());
}

#[test]
fn a_retry_claim_with_a_stale_start_identity_never_verifies() {
    let table = table_with_agent("/opt/homebrew/bin/codex", &["codex"]);
    assert!(
        !verify_launch_claim_with(&table, &claim("codex", AGENT_PID, AGENT_START + 1)).unwrap()
    );
}

#[test]
fn a_retry_claim_for_another_provider_never_verifies() {
    let table = table_with_agent("/opt/homebrew/bin/codex", &["codex"]);
    assert!(!verify_launch_claim_with(&table, &claim("claude", AGENT_PID, AGENT_START)).unwrap());
}

#[test]
fn a_retry_claim_for_a_reused_root_pid_never_verifies() {
    let table = table_with_agent("/opt/homebrew/bin/codex", &["codex"]);
    let mut stale = claim("codex", AGENT_PID, AGENT_START);
    stale.root.start_identity = (ROOT_START + 5).to_string();
    assert!(!verify_launch_claim_with(&table, &stale).unwrap());
}

const LAUNCHER_PID: Pid = 501;
const LAUNCHER_START: u64 = 1_001;
const NATIVE_PID: Pid = 502;
const NATIVE_START: u64 = 1_002;
const APP_SERVER_PID: Pid = 503;
const APP_SERVER_START: u64 = 1_003;
const NATIVE_CODEX: &str = "/opt/homebrew/lib/node_modules/@openai/codex/vendor/codex/codex";

/// The npm layout: a Node launcher, the native binary below it, and the
/// `app-server` below that, which is the process Codex runs its hooks from.
fn npm_layout() -> Table {
    Table::default()
        .with(ROOT_PID, 1, ROOT_START, "/bin/zsh", &["-zsh"])
        .with(
            LAUNCHER_PID,
            ROOT_PID,
            LAUNCHER_START,
            "/opt/homebrew/bin/node",
            &[
                "node",
                "/opt/homebrew/lib/node_modules/@openai/codex/bin/codex.js",
            ],
        )
        .with(
            NATIVE_PID,
            LAUNCHER_PID,
            NATIVE_START,
            NATIVE_CODEX,
            &["codex"],
        )
        .with(
            APP_SERVER_PID,
            NATIVE_PID,
            APP_SERVER_START,
            "/home/dev/.codex/packages/app-server-daemon/bin/codex",
            &["codex", "app-server", "--managed-daemon"],
        )
}

fn app_server_claim() -> PendingLaunchClaim {
    claim("codex", APP_SERVER_PID, APP_SERVER_START)
}

/// The hook reporter is a provider-named direct child of the launch process.
#[test]
fn a_provider_named_direct_child_of_the_launch_process_verifies() {
    let table = npm_layout();
    assert_eq!(selected(&table, "codex"), Some((NATIVE_PID, NATIVE_START)));
    assert!(verify_launch_claim_with(&table, &app_server_claim()).unwrap());
    assert!(verify_launch_claim_with(&table, &claim("codex", NATIVE_PID, NATIVE_START)).unwrap());
}

/// A native install has no launcher: the app-server is a child of the root's
/// direct provider child.
#[test]
fn a_child_of_a_native_launch_process_verifies() {
    let table = Table::default()
        .with(ROOT_PID, 1, ROOT_START, "/bin/zsh", &["-zsh"])
        .with(NATIVE_PID, ROOT_PID, NATIVE_START, NATIVE_CODEX, &["codex"])
        .with(
            APP_SERVER_PID,
            NATIVE_PID,
            APP_SERVER_START,
            NATIVE_CODEX,
            &["codex", "app-server"],
        );
    assert!(verify_launch_claim_with(&table, &app_server_claim()).unwrap());
}

/// A provider-named process two levels below the launch process, such as an
/// agent a tool command started, never speaks for the session.
#[test]
fn a_grandchild_of_the_launch_process_never_verifies() {
    let table = npm_layout()
        .with(
            600,
            APP_SERVER_PID,
            1_100,
            "/usr/bin/bash",
            &["bash", "-c", "codex exec"],
        )
        .with(601, 600, 1_101, NATIVE_CODEX, &["codex", "exec"]);
    assert!(!verify_launch_claim_with(&table, &claim("codex", 601, 1_101)).unwrap());
}

/// A provider-named process below the launcher but beside the launch process
/// is not the launch process's child.
#[test]
fn a_sibling_of_the_launch_process_never_verifies() {
    let table = npm_layout().with(
        600,
        LAUNCHER_PID,
        1_100,
        NATIVE_CODEX,
        &["codex", "app-server"],
    );
    assert_eq!(selected(&table, "codex"), Some((NATIVE_PID, NATIVE_START)));
    assert!(!verify_launch_claim_with(&table, &claim("codex", 600, 1_100)).unwrap());
}

/// A child whose image is not named for the provider never verifies.
#[test]
fn a_child_not_named_for_the_provider_never_verifies() {
    let table = npm_layout().with(
        600,
        NATIVE_PID,
        1_100,
        "/usr/bin/python3",
        &["python3", "hook.py"],
    );
    assert!(!verify_launch_claim_with(&table, &claim("codex", 600, 1_100)).unwrap());
}

/// A child of another provider's launch process never verifies for this one.
#[test]
fn a_child_claim_for_another_provider_never_verifies() {
    let table = npm_layout();
    assert!(
        !verify_launch_claim_with(&table, &claim("claude", APP_SERVER_PID, APP_SERVER_START))
            .unwrap()
    );
}

#[test]
fn a_child_claim_with_a_stale_start_identity_never_verifies() {
    let table = npm_layout();
    assert!(!verify_launch_claim_with(
        &table,
        &claim("codex", APP_SERVER_PID, APP_SERVER_START + 1)
    )
    .unwrap());
}

/// A claim of a PID that no longer runs never verifies.
#[test]
fn a_child_that_has_exited_never_verifies() {
    let table = Table::default()
        .with(ROOT_PID, 1, ROOT_START, "/bin/zsh", &["-zsh"])
        .with(NATIVE_PID, ROOT_PID, NATIVE_START, NATIVE_CODEX, &["codex"]);
    assert!(!verify_launch_claim_with(&table, &app_server_claim()).unwrap());
}

/// Either generation changing while the parent link is checked fails closed.
#[test]
fn pid_reuse_of_the_child_or_the_launch_process_never_verifies() {
    let designated = pohunek_worker_protocol::ProcessIdentity {
        pid: NATIVE_PID,
        start_identity: NATIVE_START,
    };
    let candidate = ProcessIdentity {
        pid: APP_SERVER_PID,
        start_identity: StartIdentity::new(APP_SERVER_START),
    };
    let steady = npm_layout();
    assert!(is_provider_child_of_launch(&steady, candidate, &designated, "codex").unwrap());
    for reused in [NATIVE_PID, APP_SERVER_PID] {
        let mut table = npm_layout();
        table
            .reused_after
            .insert(reused, (0, StartIdentity::new(9_999)));
        assert!(
            !is_provider_child_of_launch(&table, candidate, &designated, "codex").unwrap(),
            "{reused}"
        );
    }
}

/// A same-provider child that is not the hook helper (a second agent the launch
/// process started) never verifies, for either provider.
#[test]
fn an_independent_same_provider_child_never_verifies() {
    for (provider, executable) in [
        ("codex", NATIVE_CODEX),
        ("claude", "/opt/homebrew/bin/claude"),
    ] {
        let table = Table::default()
            .with(ROOT_PID, 1, ROOT_START, "/bin/zsh", &["-zsh"])
            .with(NATIVE_PID, ROOT_PID, NATIVE_START, executable, &[provider])
            .with(
                APP_SERVER_PID,
                NATIVE_PID,
                APP_SERVER_START,
                executable,
                &[provider, "exec", "app-server-not-first"],
            );
        assert!(
            !verify_launch_claim_with(&table, &claim(provider, APP_SERVER_PID, APP_SERVER_START))
                .unwrap(),
            "{provider}"
        );
    }
}

/// `app-server` as the second argument is Codex's helper role only; another
/// provider has no helper role, so the same command line grants nothing.
#[test]
fn the_helper_role_of_one_provider_grants_nothing_to_another() {
    let table = Table::default()
        .with(ROOT_PID, 1, ROOT_START, "/bin/zsh", &["-zsh"])
        .with(
            NATIVE_PID,
            ROOT_PID,
            NATIVE_START,
            "/bin/claude",
            &["claude"],
        )
        .with(
            APP_SERVER_PID,
            NATIVE_PID,
            APP_SERVER_START,
            "/bin/claude",
            &["claude", "app-server"],
        );
    assert!(
        !verify_launch_claim_with(&table, &claim("claude", APP_SERVER_PID, APP_SERVER_START))
            .unwrap()
    );
}
