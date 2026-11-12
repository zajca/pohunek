//! `project.candidates` through the public control socket.
//!
//! The daemon serves the official Claude Code and Codex packages, so the
//! history locations are the ones their descriptors declare. History fixtures
//! live below a hermetic home; the daemon's temporary directories are pinned to
//! a directory of that home so the fixtures are not mistaken for scratch space.

// Rust guideline compliant 2026-10-12

use super::*;

use std::collections::BTreeSet;
use std::os::unix::fs::symlink;
use std::time::SystemTime;

/// The documented fields of the result.
const RESULT_KEYS: [&str; 3] = ["candidates", "dropped", "limits_reached"];
/// The documented fields of one candidate.
const CANDIDATE_KEYS: [&str; 6] = [
    "repo_root",
    "origin",
    "last_used_at",
    "sessions",
    "registered",
    "exists",
];
/// The documented fields of one agent's session counts.
const SESSION_KEYS: [&str; 2] = ["owned", "history"];

/// A credential-shaped string the history fixtures carry outside the working
/// directory field.
const CREDENTIAL: &str = "sk-live-4f9a1c7e2b8d3056-SECRET";

/// A started daemon serving the official runtime packages over a hermetic home.
struct Rig {
    control: Framed<UnixStream, LinesCodec>,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    home: TestEnv,
    /// A directory of the home that the daemon treats as a temporary one.
    scratch: PathBuf,
    _socket: TestSocket,
    _state: TestDir,
}

impl Rig {
    async fn start(tag: &str) -> Self {
        Self::start_with(tag, |_config| {}).await
    }

    async fn start_with(tag: &str, configure: impl FnOnce(&mut SessionRegistryConfig)) -> Self {
        let home = TestEnv::new().expect("create the home environment");
        let scratch = home.home().join("scratch");
        std::fs::create_dir(&scratch).expect("create the scratch directory");
        let scratch = scratch.canonicalize().expect("canonical scratch directory");
        let state = temp_dir(&format!("{tag}-state"));
        let socket = temp_socket(tag);
        let mut config = SessionRegistryConfig {
            shell_command: support::hermetic_shell(),
            store_path: Some(state.join("metadata.jsonl")),
            ..SessionRegistryConfig::default()
        };
        config.candidates.temp_roots = vec![scratch.clone()];
        // Far above any host load; only the scenario about time lowers it.
        config.candidates.deadline = HANG_GUARD;
        configure(&mut config);
        let (shutdown, server, _) = spawn_server_serving_official_runtimes_in(
            &socket,
            "0.0.0",
            config,
            EnvironmentSource::fixed(home.environment().clone()),
        )
        .await;
        let control = connect(&socket).await;
        Self {
            control,
            shutdown,
            server,
            home,
            scratch,
            _socket: socket,
            _state: state,
        }
    }

    /// The home as the daemon spells it.
    fn home_dir(&self) -> PathBuf {
        self.home.home().canonicalize().expect("canonical home")
    }

    /// Calls `project.candidates` and returns the raw result document.
    async fn candidates(&mut self, params: Value) -> Value {
        let request = Request::make("candidates", method::PROJECT_CANDIDATES, params);
        ok_payload(exchange(&mut self.control, &request).await)
    }

    async fn add_project(&mut self, path: &Path) {
        let request = Request::make(
            "candidates-add",
            method::PROJECT_ADD,
            serde_json::json!({ "path": path }),
        );
        ok_payload(exchange(&mut self.control, &request).await);
    }

    async fn finish(self) {
        let _ = self.shutdown.send(());
        let _ = self.server.await;
    }
}

/// A git repository with one commit at `dir`, optionally with an `origin`.
fn make_repo(env: &TestEnv, dir: &Path, origin: Option<&str>) -> PathBuf {
    std::fs::create_dir_all(dir).expect("create the repository directory");
    let git = |args: &[&str]| {
        let out = env
            .git_command()
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["-c", "init.defaultBranch=main", "init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "init\n").expect("write README");
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    if let Some(url) = origin {
        git(&["remote", "add", "origin", url]);
    }
    pohunek_test_support::git::pin_hooks(dir);
    dir.canonicalize().expect("canonical repository")
}

/// Sets the modification time of `path`.
fn touch(path: &Path, modified: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open the history file")
        .set_modified(modified)
        .expect("set the modification time");
}

/// Writes a Claude Code history file below `<home>/.claude/projects/<dir>`.
fn write_claude(home: &Path, dir: &str, name: &str, lines: &[String]) -> PathBuf {
    let directory = home.join(".claude/projects").join(dir);
    std::fs::create_dir_all(&directory).expect("create the Claude project directory");
    let path = directory.join(format!("{name}.jsonl"));
    std::fs::write(&path, lines.join("\n") + "\n").expect("write the Claude history file");
    path
}

/// Writes a Codex rollout whose first record names `cwd`.
fn write_codex(home: &Path, name: &str, cwd: &str) -> PathBuf {
    let directory = home.join(".codex/sessions/2026/11/11");
    std::fs::create_dir_all(&directory).expect("create the Codex session directory");
    let path = directory.join(format!("rollout-{name}.jsonl"));
    let first = serde_json::json!({
        "timestamp": "2026-11-11T08:00:00Z",
        "type": "session_meta",
        "payload": { "id": name, "cwd": cwd, "instructions": CREDENTIAL },
    });
    std::fs::write(&path, format!("{first}\n")).expect("write the Codex rollout");
    path
}

/// A Claude record that names `cwd` and carries a message body.
fn claude_user(cwd: &str) -> String {
    serde_json::json!({
        "type": "user",
        "cwd": cwd,
        "message": { "role": "user", "content": format!("use {CREDENTIAL} to log in") },
    })
    .to_string()
}

/// A Claude record without a working directory that carries a credential.
fn claude_summary() -> String {
    serde_json::json!({ "type": "summary", "summary": CREDENTIAL, "leafUuid": CREDENTIAL })
        .to_string()
}

fn keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .expect("a JSON object")
        .keys()
        .map(String::as_str)
        .collect()
}

/// Asserts the result carries no field outside the documented set.
fn assert_documented_shape(result: &Value) {
    let allowed: BTreeSet<&str> = RESULT_KEYS.into_iter().collect();
    assert!(
        keys(result).is_subset(&allowed),
        "undocumented result fields: {:?}",
        keys(result)
    );
    assert!(keys(result).contains("candidates") && keys(result).contains("dropped"));
    let allowed: BTreeSet<&str> = CANDIDATE_KEYS.into_iter().collect();
    for candidate in result["candidates"].as_array().expect("candidates") {
        assert!(
            keys(candidate).is_subset(&allowed),
            "undocumented candidate fields: {:?}",
            keys(candidate)
        );
        let required: BTreeSet<&str> = CANDIDATE_KEYS
            .into_iter()
            .filter(|key| *key != "origin")
            .collect();
        assert!(
            required.is_subset(&keys(candidate)),
            "missing candidate fields: {:?}",
            keys(candidate)
        );
        for counts in candidate["sessions"]
            .as_object()
            .expect("sessions")
            .values()
        {
            assert_eq!(
                keys(counts),
                SESSION_KEYS.into_iter().collect::<BTreeSet<_>>()
            );
        }
    }
}

/// The repository roots of `result`, sorted.
fn roots(result: &Value) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = result["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .map(|candidate| PathBuf::from(candidate["repo_root"].as_str().expect("repo_root")))
        .collect();
    roots.sort();
    roots
}

/// The candidate of `root`.
fn candidate<'a>(result: &'a Value, root: &Path) -> &'a Value {
    result["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .find(|candidate| candidate["repo_root"].as_str() == root.to_str())
        .unwrap_or_else(|| panic!("no candidate for {}", root.display()))
}

/// Writes history that must not be read: a linked file and a linked directory
/// that lead outside the root, and files at locations no runtime package
/// declares. All of them name `unlisted` or `decoy`.
fn write_unreachable_history(home: &Path, unlisted: &Path, decoy: &Path) {
    let path = |path: &Path| path.to_str().expect("utf-8 path").to_owned();
    // A linked file and a linked directory that lead outside the root.
    let outside = home.join("outside-history");
    std::fs::create_dir(&outside).expect("create the outside directory");
    std::fs::write(
        outside.join("linked.jsonl"),
        claude_user(&path(unlisted)) + "\n",
    )
    .expect("write the linked history file");
    let projects = home.join(".claude/projects");
    symlink(
        outside.join("linked.jsonl"),
        projects.join("-claude-app/linked.jsonl"),
    )
    .expect("link a history file");
    std::fs::create_dir(outside.join("linked-dir")).expect("create the linked directory");
    std::fs::write(
        outside.join("linked-dir/inside.jsonl"),
        claude_user(&path(unlisted)) + "\n",
    )
    .expect("write the file below the linked directory");
    symlink(outside.join("linked-dir"), projects.join("-linked-dir")).expect("link a directory");
    // History files at locations no runtime package declares.
    std::fs::write(
        home.join(".claude/loose.jsonl"),
        claude_user(&path(decoy)) + "\n",
    )
    .expect("write a file beside the projects directory");
    std::fs::write(home.join("stray.jsonl"), claude_user(&path(decoy)) + "\n")
        .expect("write a file in the home");
}

/// Asserts that no credential-shaped value, userinfo or message text of the
/// fixtures, and no path of a history file that must not be read or a
/// directory that must not be listed, appears in `result`.
fn assert_nothing_leaked(result: &Value, unread: &[&PathBuf]) {
    let text = result.to_string();
    for secret in [CREDENTIAL, "ghp_TOKEN123", "builder", "deploy", "log in"] {
        assert!(!text.contains(secret), "{secret:?} reached the output");
    }
    for unread in unread {
        assert!(
            !text.contains(unread.to_str().expect("utf-8 path")),
            "{} was named",
            unread.display()
        );
    }
}

/// Writes history that must count once or not at all: a symlinked working
/// directory, an empty one, and a session's auxiliary log.
fn write_edge_history(home: &Path, work: &Path, both_repo: &Path, claude_repo: &Path) {
    let path = |path: &Path| path.to_str().expect("utf-8 path").to_owned();
    // A symlinked working directory collapses onto its repository.
    let link = work.join("link-to-both");
    symlink(both_repo, &link).expect("link the repository");
    write_claude(home, "-link", "a", &[claude_user(&path(&link))]);
    // An empty working directory is skipped before any filesystem access.
    write_claude(home, "-empty", "a", &[claude_user("")]);
    // The auxiliary logs of a session are not sessions.
    write_claude(
        home,
        "-claude-app/one/subagents",
        "agent-1",
        &[claude_user(&path(claude_repo))],
    );
}

#[tokio::test]
async fn pohunek_sessions_collapse_worktrees_onto_one_registered_candidate() {
    let mut rig = Rig::start("pcand-default").await;
    let repo = rig.home_dir().join("work/app");
    let repo = make_repo(&rig.home, &repo, None);
    let worktree = rig.home_dir().join("work/app-feature");
    let added = rig
        .home
        .git_command()
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "add", "-q", "-b", "feature"])
        .arg(&worktree)
        .output()
        .expect("run git worktree add");
    assert!(added.status.success(), "git worktree add failed");
    // History of another repository exists but is not requested.
    let unrequested = make_repo(&rig.home, &rig.home_dir().join("work/unrequested"), None);
    write_claude(
        &rig.home_dir(),
        "-unrequested",
        "a",
        &[claude_user(unrequested.to_str().expect("utf-8 path"))],
    );

    let mut agent = None;
    for cwd in [repo.clone(), worktree.clone()] {
        let info: SessionInfo = serde_json::from_value(ok_payload(
            create_session_with_params(&mut rig.control, session_params_in(cwd)).await,
        ))
        .expect("session info");
        agent = Some(info.agent_base.as_wire().to_owned());
    }
    let agent = agent.expect("a session was created");

    for params in [
        Value::Null,
        serde_json::json!({}),
        serde_json::json!({ "sources": ["pohunek"] }),
    ] {
        let result = rig.candidates(params).await;
        assert_documented_shape(&result);
        assert_eq!(
            roots(&result),
            std::slice::from_ref(&repo),
            "one candidate for the repo"
        );
        let found = candidate(&result, &repo);
        assert_eq!(
            found["registered"], true,
            "a session registered the project"
        );
        assert_eq!(found["exists"], true);
        assert_eq!(
            found["sessions"],
            serde_json::json!({ agent.as_str(): { "owned": 2, "history": 0 } })
        );
        assert_eq!(result["dropped"], 0);
        assert!(
            !result.to_string().contains("unrequested"),
            "history is not read unless it is named"
        );
    }

    let history_only = rig
        .candidates(serde_json::json!({ "sources": ["history"] }))
        .await;
    assert_eq!(
        roots(&history_only),
        [unrequested],
        "the history source lists history alone"
    );
    rig.finish().await;
}

#[tokio::test]
async fn history_is_read_from_declared_locations_for_two_fields_only() {
    let mut rig = Rig::start("pcand-history").await;
    let home = rig.home_dir();
    let work = home.join("work");

    let claude_repo = make_repo(
        &rig.home,
        &work.join("claude-app"),
        Some("https://builder:ghp_TOKEN123@example.com/org/claude-app.git"),
    );
    let codex_repo = make_repo(
        &rig.home,
        &work.join("codex-app"),
        Some("ssh://deploy@example.com/org/codex-app"),
    );
    let both_repo = make_repo(&rig.home, &work.join("both-app"), None);
    // Outside the home: a repository below the test root but not the home.
    let foreign = make_repo(&rig.home, &rig.home.cwd().join("foreign"), None);
    // In a directory the daemon treats as temporary.
    let temporary = make_repo(&rig.home, &rig.scratch.join("temporary"), None);
    let not_git = work.join("plain-directory");
    std::fs::create_dir_all(&not_git).expect("create the plain directory");
    let deleted = work.join("deleted");
    let unlisted_via_link = make_repo(&rig.home, &work.join("via-link"), None);
    let decoy = make_repo(&rig.home, &work.join("decoy"), None);

    let path = |path: &Path| path.to_str().expect("utf-8 path").to_owned();

    // The credential sits in a record before the working directory, in the
    // message body, and in a record after it.
    write_claude(
        &home,
        "-claude-app",
        "one",
        &[
            claude_summary(),
            claude_user(&path(&claude_repo)),
            claude_user(&path(&claude_repo)),
        ],
    );
    write_claude(
        &home,
        "-claude-app",
        "two",
        &[claude_user(&path(&claude_repo))],
    );
    write_claude(&home, "-no-cwd", "none", &[claude_summary()]);
    // Not absolute, not plain: skipped before any filesystem access.
    write_claude(&home, "-relative", "a", &[claude_user("relative/dir")]);
    write_claude(
        &home,
        "-dotdot",
        "a",
        &[claude_user(&format!("{}/../work", path(&home)))],
    );
    write_claude(&home, "-nul", "a", &[claude_user("/work/a\u{0}b")]);
    // Listed paths that are dropped.
    write_claude(&home, "-foreign", "a", &[claude_user(&path(&foreign))]);
    write_claude(&home, "-temporary", "a", &[claude_user(&path(&temporary))]);
    write_claude(&home, "-plain", "a", &[claude_user(&path(&not_git))]);
    write_claude(&home, "-deleted", "a", &[claude_user(&path(&deleted))]);
    write_codex(&home, "codex-one", &path(&codex_repo));
    write_codex(&home, "codex-both", &path(&both_repo));
    write_claude(&home, "-both", "a", &[claude_user(&path(&both_repo))]);
    write_edge_history(&home, &work, &both_repo, &claude_repo);
    write_unreachable_history(&home, &unlisted_via_link, &decoy);

    rig.add_project(&codex_repo).await;

    let result = rig
        .candidates(serde_json::json!({ "sources": ["history"] }))
        .await;
    assert_documented_shape(&result);
    let mut expected = vec![claude_repo.clone(), codex_repo.clone(), both_repo.clone()];
    expected.sort();
    assert_eq!(roots(&result), expected, "{result}");
    assert_eq!(
        result["dropped"], 4,
        "outside the home, temporary, not a repository, deleted: {result}"
    );
    assert!(result.get("limits_reached").is_none(), "{result}");

    let claude = candidate(&result, &claude_repo);
    assert_eq!(
        claude["sessions"],
        serde_json::json!({ "claude": { "owned": 0, "history": 2 } }),
        "the linked file is not read"
    );
    assert_eq!(claude["registered"], false);
    assert_eq!(claude["origin"], "https://example.com/org/claude-app.git");
    let codex = candidate(&result, &codex_repo);
    assert_eq!(
        codex["sessions"],
        serde_json::json!({ "codex": { "owned": 0, "history": 1 } })
    );
    assert_eq!(codex["registered"], true);
    assert_eq!(codex["origin"], "ssh://example.com/org/codex-app");
    assert_eq!(
        candidate(&result, &both_repo)["sessions"],
        serde_json::json!({
            "claude": { "owned": 0, "history": 2 },
            "codex": { "owned": 0, "history": 1 },
        })
    );

    assert_nothing_leaked(
        &result,
        &[&unlisted_via_link, &decoy, &foreign, &temporary, &deleted],
    );
    rig.finish().await;
}

#[tokio::test]
async fn owned_and_history_merge_newest_first_and_a_deleted_repository_is_dropped() {
    let mut rig = Rig::start("pcand-merge").await;
    let home = rig.home_dir();
    let work = home.join("work");
    let merged = make_repo(&rig.home, &work.join("merged"), None);
    let newest = make_repo(&rig.home, &work.join("newest"), None);
    let deleted = make_repo(&rig.home, &work.join("deleted-session"), None);
    let hour = Duration::from_secs(3600);
    let merged_at = SystemTime::now() + hour;
    let newest_at = SystemTime::now() + 2 * hour;
    for (dir, repo, at) in [
        ("-merged", &merged, merged_at),
        ("-newest", &newest, newest_at),
    ] {
        let file = write_claude(
            &home,
            dir,
            "a",
            &[claude_user(repo.to_str().expect("utf-8 path"))],
        );
        touch(&file, at);
    }
    for cwd in [merged.clone(), deleted.clone()] {
        ok_payload(create_session_with_params(&mut rig.control, session_params_in(cwd)).await);
    }
    std::fs::remove_dir_all(&deleted).expect("delete the repository");

    let result = rig
        .candidates(serde_json::json!({ "sources": ["pohunek", "history"] }))
        .await;
    assert_documented_shape(&result);
    assert_eq!(
        roots(&result)[..],
        {
            let mut both = [merged.clone(), newest.clone()];
            both.sort();
            both
        }[..]
    );
    let order: Vec<&str> = result["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .map(|candidate| candidate["repo_root"].as_str().expect("repo_root"))
        .collect();
    assert_eq!(
        order,
        [
            newest.to_str().expect("utf-8"),
            merged.to_str().expect("utf-8")
        ],
        "newest activity first"
    );
    let found = candidate(&result, &merged);
    let sessions = found["sessions"].as_object().expect("sessions");
    assert_eq!(sessions.len(), 2, "{found}");
    assert!(sessions.values().any(|counts| counts["owned"] == 1));
    assert!(sessions.values().any(|counts| counts["history"] == 1));
    let last = OffsetDateTime::parse(found["last_used_at"].as_str().expect("time"), &Rfc3339)
        .expect("RFC 3339 time");
    assert_eq!(
        last,
        OffsetDateTime::from(merged_at),
        "the newest of the session update and the history file"
    );
    assert_eq!(result["dropped"], 1, "the deleted repository: {result}");
    rig.finish().await;
}

#[tokio::test]
async fn reached_bounds_are_reported_and_never_fail_the_call() {
    let history = |rig: &Rig| {
        let home = rig.home_dir();
        let work = home.join("work");
        let mut repos = Vec::new();
        for (index, name) in ["old", "middle", "new"].into_iter().enumerate() {
            let repo = make_repo(&rig.home, &work.join(name), None);
            let file = write_claude(
                &home,
                &format!("-{name}"),
                "a",
                &[claude_user(repo.to_str().expect("utf-8 path"))],
            );
            let age = Duration::from_secs(3600 * (3 - u64::try_from(index).expect("small index")));
            touch(&file, SystemTime::now() - age);
            repos.push(repo);
        }
        repos
    };
    let sources = serde_json::json!({ "sources": ["history"] });

    // File bound: the newest file is read, the others are not.
    let mut rig = Rig::start_with("pcand-files", |config| config.candidates.max_files = 1).await;
    let repos = history(&rig);
    let result = rig.candidates(sources.clone()).await;
    assert_eq!(roots(&result), [repos[2].clone()], "{result}");
    assert_eq!(result["limits_reached"], serde_json::json!(["files"]));
    rig.finish().await;

    // Entry bound: the walk stops before every file is listed.
    let mut rig =
        Rig::start_with("pcand-entries", |config| config.candidates.max_entries = 2).await;
    history(&rig);
    let result = rig.candidates(sources.clone()).await;
    assert_eq!(result["limits_reached"], serde_json::json!(["entries"]));
    assert_documented_shape(&result);
    rig.finish().await;

    // Time bound: an exhausted deadline returns what was found, which is nothing.
    let mut rig = Rig::start_with("pcand-time", |config| {
        config.candidates.deadline = Duration::ZERO;
    })
    .await;
    history(&rig);
    let result = rig.candidates(sources.clone()).await;
    assert_eq!(result["limits_reached"], serde_json::json!(["time"]));
    assert_eq!(result["candidates"], serde_json::json!([]));
    rig.finish().await;

    // Byte bound: a record cut by the bound is skipped and reported.
    let mut rig = Rig::start_with("pcand-bytes", |config| config.candidates.head_bytes = 64).await;
    history(&rig);
    let result = rig.candidates(sources).await;
    assert_eq!(result["limits_reached"], serde_json::json!(["line_length"]));
    assert_documented_shape(&result);
    rig.finish().await;
}

#[tokio::test]
async fn history_directory_budget_counts_only_directories_the_walk_enters() {
    // The three project directories fit the directory budget exactly. The
    // auxiliary directory of session `s1` is listed beside `s1.jsonl` but not
    // entered, so it uses no share of the budget.
    let mut rig = Rig::start_with("pcand-dirs", |config| {
        config.candidates.max_directories = 3;
    })
    .await;
    let home = rig.home_dir();
    let work = home.join("work");

    let a = make_repo(&rig.home, &work.join("dir-a"), None);
    let b = make_repo(&rig.home, &work.join("dir-b"), None);
    let c = make_repo(&rig.home, &work.join("dir-c"), None);
    let decoy = make_repo(&rig.home, &work.join("dir-decoy"), None);

    let path = |path: &Path| path.to_str().expect("utf-8 path").to_owned();
    write_claude(&home, "-a", "s1", &[claude_user(&path(&a))]);
    write_claude(
        &home,
        "-a/s1/subagents",
        "agent-1",
        &[claude_user(&path(&decoy))],
    );
    write_claude(&home, "-b", "s2", &[claude_user(&path(&b))]);
    write_claude(&home, "-c", "s3", &[claude_user(&path(&c))]);

    let result = rig
        .candidates(serde_json::json!({ "sources": ["history"] }))
        .await;
    let mut expected = vec![a, b, c];
    expected.sort();
    assert_eq!(roots(&result), expected, "{result}");
    assert!(
        !roots(&result).contains(&decoy),
        "the auxiliary log of a session is not a session: {result}"
    );
    assert_eq!(
        result.get("limits_reached"),
        None,
        "a directory the walk does not enter uses no share of the directory budget: {result}"
    );
    rig.finish().await;
}
