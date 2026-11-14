//! The session tree through the CLI process and the real daemon:
//! `session list --top-level` and the `children` counts in JSON and table
//! output.

#![cfg(unix)]

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::Harness;
use serde_json::Value;

/// Creates a shell session; `origin` is the spawning session pair.
async fn create_session(harness: &Harness, name: &str, origin: Option<(&str, &str)>) -> Value {
    let args = [
        "session", "new", "--agent", "shell", "--name", name, "--json",
    ];
    let output = match origin {
        Some((session_id, daemon_id)) => {
            harness
                .run_with_env(
                    &args,
                    &[
                        ("POHUNEK_SESSION_ID", session_id),
                        ("POHUNEK_DAEMON_ID", daemon_id),
                    ],
                )
                .await
        }
        None => harness.run(&args).await,
    };
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("session JSON envelope");
    document["ok"].clone()
}

/// A throwaway git repository for sessions that need their own worktree,
/// rooted at the harness environment so the hermetic scan passes.
fn init_git_repo(env: &pohunek_test_support::env::TestEnv) -> std::path::PathBuf {
    let dir = env.root().join("dirty-tree-origin");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create repo dir");
    let git = |args: &[&str]| {
        let output = pohunek_test_support::git::command()
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?} failed");
    };
    git(&["-c", "init.defaultBranch=main", "init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "init\n").expect("write README");
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    dir
}

/// Creates a shell session on its own worktree branch; `origin` is the
/// spawning session pair.
async fn create_session_on_branch(
    harness: &Harness,
    name: &str,
    origin: Option<(&str, &str)>,
    repo: &std::path::Path,
) -> Value {
    let repo = repo.to_str().expect("repo path").to_owned();
    let args = [
        "session",
        "new",
        "--agent",
        "shell",
        "--name",
        name,
        "--repo",
        repo.as_str(),
        "--branch",
        &format!("feat/{name}"),
        "--json",
    ];
    let output = match origin {
        Some((session_id, daemon_id)) => {
            harness
                .run_with_env(
                    &args,
                    &[
                        ("POHUNEK_SESSION_ID", session_id),
                        ("POHUNEK_DAEMON_ID", daemon_id),
                    ],
                )
                .await
        }
        None => harness.run(&args).await,
    };
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("session JSON envelope");
    document["ok"].clone()
}

fn sorted_ids(document: &Value) -> Vec<String> {
    let mut ids: Vec<String> = document["ok"]
        .as_array()
        .expect("session list array")
        .iter()
        .map(|session| session["id"].as_str().expect("session id").to_owned())
        .collect();
    ids.sort();
    ids
}

fn children_of(document: &Value, id: &str) -> Option<u64> {
    document["ok"]
        .as_array()
        .expect("session list array")
        .iter()
        .find(|session| session["id"] == id)
        .expect("the session is listed")
        .get("children")
        .and_then(serde_json::Value::as_u64)
}

#[tokio::test]
async fn session_list_top_level_lists_roots_with_full_child_counts() {
    let harness = Harness::start().await;
    let parent = create_session(&harness, "tree-parent", None).await;
    let parent_id = parent["id"].as_str().expect("parent id").to_owned();
    let first = create_session(&harness, "tree-first", Some((&parent_id, "d-tree-local"))).await;
    let first_id = first["id"].as_str().expect("child id").to_owned();
    create_session(&harness, "tree-second", Some((&parent_id, "d-tree-local"))).await;
    let gone = create_session(&harness, "tree-gone", Some(("s-tree-gone", "d-tree-local"))).await;
    let gone_id = gone["id"].as_str().expect("orphan id").to_owned();
    let unrelated = create_session(&harness, "tree-unrelated", None).await;
    let unrelated_id = unrelated["id"].as_str().expect("unrelated id").to_owned();

    // The unfiltered JSON list reports direct-child counts on every entry.
    let (status, listed) = harness.json(&["session", "list"]).await;
    assert_eq!(status, 0, "{listed}");
    assert_eq!(children_of(&listed, &parent_id), Some(2));
    assert_eq!(children_of(&listed, &first_id), Some(0));

    // `--top-level` keeps roots only: the parent, the orphan and the
    // unrelated session. The children counts stay computed over the
    // unfiltered list.
    let (status, roots) = harness.json(&["session", "list", "--top-level"]).await;
    assert_eq!(status, 0, "{roots}");
    assert_eq!(
        sorted_ids(&roots),
        {
            let mut ids = vec![parent_id.clone(), gone_id.clone(), unrelated_id.clone()];
            ids.sort();
            ids
        },
        "{roots}"
    );
    assert_eq!(children_of(&roots, &parent_id), Some(2));

    // The quiet view lists the same roots, one id per line.
    let output = harness
        .run(&["session", "list", "--top-level", "--quiet"])
        .await;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let mut quiet: Vec<String> = String::from_utf8(output.stdout)
        .expect("quiet output")
        .lines()
        .map(str::to_owned)
        .collect();
    quiet.sort();
    assert_eq!(quiet, {
        let mut ids = vec![parent_id.clone(), gone_id, unrelated_id];
        ids.sort();
        ids
    });

    harness.stop().await;
}

#[tokio::test]
async fn session_list_tree_renders_nesting_without_dropping_sessions() {
    let harness = Harness::start().await;
    let parent = create_session(&harness, "nest-parent", None).await;
    let parent_id = parent["id"].as_str().expect("parent id").to_owned();
    let child = create_session(&harness, "nest-child", Some((&parent_id, "d-nest-local"))).await;
    let child_id = child["id"].as_str().expect("child id").to_owned();
    create_session(
        &harness,
        "nest-grandchild",
        Some((&child_id, "d-nest-local")),
    )
    .await;

    let output = harness.run(&["session", "list", "--tree"]).await;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let rendered = String::from_utf8(output.stdout).expect("tree output");
    let lines: Vec<&str> = rendered.lines().collect();
    // Every session is rendered exactly once, parents before children.
    for id in [&parent_id, &child_id] {
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains(id.as_str()))
                .count(),
            1,
            "each session renders once: {rendered}"
        );
    }
    let parent_line = lines
        .iter()
        .position(|line| line.contains(parent_id.as_str()))
        .expect("the parent renders");
    let child_line = lines
        .iter()
        .position(|line| line.contains(child_id.as_str()))
        .expect("the child renders");
    assert!(
        parent_line < child_line,
        "parents render before children: {rendered}"
    );
    assert!(
        lines[child_line].len() > child_id.len(),
        "a nested session renders indented: {rendered}"
    );

    harness.stop().await;
}

/// `session list --tree` over a tree with an orphan and a foreign child:
/// parents render before children, orphans render as roots, and each
/// orphan gets one footer line naming its missing spawner.
#[tokio::test]
async fn session_list_tree_renders_orphans_with_footer_lines() {
    let harness = Harness::start().await;
    let parent = create_session(&harness, "orphan-parent", None).await;
    let parent_id = parent["id"].as_str().expect("parent id").to_owned();
    create_session(
        &harness,
        "orphan-child",
        Some((&parent_id, "d-orphan-local")),
    )
    .await;
    let orphan = create_session(&harness, "orphan-gone", Some(("s-orphan-gone", "d-orphan"))).await;
    let orphan_id = orphan["id"].as_str().expect("orphan id").to_owned();
    let foreign = create_session(
        &harness,
        "orphan-foreign",
        Some(("s-orphan-remote", "d-orphan")),
    )
    .await;
    let foreign_id = foreign["id"].as_str().expect("foreign id").to_owned();

    let output = harness.run(&["session", "list", "--tree"]).await;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let rendered = String::from_utf8(output.stdout).expect("tree output");
    let lines: Vec<&str> = rendered.lines().collect();
    let parent_line = lines
        .iter()
        .position(|line| line.contains(parent_id.as_str()))
        .expect("the parent renders");
    for id in [&orphan_id, &foreign_id] {
        let line = lines
            .iter()
            .position(|line| line.contains(id.as_str()))
            .expect("the orphan renders");
        assert!(
            parent_line < line,
            "roots render before the orphan {id}: {rendered}"
        );
    }
    for (id, spawner) in [
        (&orphan_id, "s-orphan-gone"),
        (&foreign_id, "s-orphan-remote"),
    ] {
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains(&format!(
                    "orphan {id}: spawned by {spawner}, which is not listed on this host"
                )))
                .count(),
            1,
            "one footer line per orphan: {rendered}"
        );
    }

    harness.stop().await;
}

/// A filtered human `session list` hides the spawner the daemon listed, so
/// the child must render with no orphan line: an absent spawner in a
/// filtered view may still be listed on this host, and only `--tree` (which
/// clap forces to be unfiltered) may claim otherwise.
#[tokio::test]
async fn session_list_filtered_human_hides_orphan_lines() {
    let harness = Harness::start().await;
    let parent = create_session(&harness, "filtered-parent", None).await;
    let parent_id = parent["id"].as_str().expect("parent id").to_owned();
    let child = create_session(
        &harness,
        "filtered-child",
        Some((&parent_id, "d-filtered-local")),
    )
    .await;
    let child_id = child["id"].as_str().expect("child id").to_owned();

    // `--spawned-by` narrows the daemon list to the child alone; the human
    // view still names the child but must not print an orphan line for its
    // hidden spawner.
    let output = harness
        .run(&["session", "list", "--spawned-by", &parent_id])
        .await;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let rendered = String::from_utf8(output.stdout).expect("human output");
    assert!(
        rendered.contains(child_id.as_str()),
        "the child still lists: {rendered}"
    );
    assert!(
        !rendered.contains("orphan"),
        "a filtered list names no orphan: {rendered}"
    );

    harness.stop().await;
}

/// `session list --tree` conflicts with the flat outputs and every filter:
/// usage errors, exit 2.
#[tokio::test]
async fn session_list_tree_conflicts_with_outputs_and_filters() {
    let harness = Harness::start().await;
    for args in [
        vec!["session", "list", "--tree", "--json"],
        vec!["session", "list", "--tree", "--quiet"],
        vec!["session", "list", "--tree", "--top-level"],
        vec!["session", "list", "--tree", "--filter", "state=running"],
        vec!["session", "list", "--tree", "--spawned-by", "s-tree-gone"],
    ] {
        let output = harness.run(&args).await;
        assert_eq!(
            output.status.code(),
            Some(2),
            "--tree conflicts with {:?}: {output:?}",
            &args[3..]
        );
    }
    harness.stop().await;
}

/// `session rm M --with-children` on a dirty-worktree tree: the dirty
/// leaf is kept `worktree_held: worktree_uncommitted`, the exit is non-zero,
/// and S1 is still listed with its worktree on disk.
#[tokio::test]
async fn session_rm_with_children_keeps_a_dirty_tree_leaf() {
    // Durable: the daemon binds worktrees only with a metadata store, and
    // the dirty leaf needs a real checkout for `rm` to hold.
    let harness = Harness::start_durable().await;
    let repo = init_git_repo(&harness.env);
    let parent = create_session(&harness, "dirty-parent", None).await;
    let parent_id = parent["id"].as_str().expect("parent id").to_owned();
    let first = create_session(&harness, "dirty-first", Some((&parent_id, "d-dirty-local"))).await;
    let first_id = first["id"].as_str().expect("child id").to_owned();
    let dirty = create_session_on_branch(
        &harness,
        "dirty-leaf",
        Some((&first_id, "d-dirty-local")),
        &repo,
    )
    .await;
    let dirty_id = dirty["id"].as_str().expect("leaf id").to_owned();

    // The leaf runs on its own worktree; dirty it through the inspected
    // path, the way the daemon sweep tests dirty a bound checkout.
    // A shell session never exits on its own, so stop the leaf first: the
    // cascade must still hold its dirty worktree like the finished leaf of the retention sweep test.
    let (status, stopped) = harness.json(&["session", "stop", &dirty_id]).await;
    assert_eq!(status, 0, "{stopped}");
    let worktree: std::path::PathBuf = {
        let (status, inspected) = harness.json(&["session", "inspect", &dirty_id]).await;
        assert_eq!(status, 0, "{inspected}");
        inspected["ok"]["worktree_path"]
            .as_str()
            .expect("leaf worktree")
            .into()
    };
    std::fs::write(
        worktree.join("README.md"),
        "work the agent did not commit\n",
    )
    .expect("dirty the leaf worktree");

    let output = harness
        .run(&["session", "rm", &parent_id, "--with-children"])
        .await;
    assert_ne!(
        output.status.code(),
        Some(0),
        "a kept root fails the command: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    for id in [&parent_id, &first_id, &dirty_id] {
        assert!(stdout.contains(id), "every node is reported: {stdout}");
    }
    assert!(
        stdout.contains("worktree_held: worktree_uncommitted"),
        "the dirty leaf names its hold: {stdout}"
    );

    let (status, listed) = harness.json(&["session", "list"]).await;
    assert_eq!(status, 0, "{listed}");
    assert!(
        sorted_ids(&listed).contains(&dirty_id),
        "S1 is still listed: {listed}"
    );
    assert!(worktree.is_dir(), "S1's worktree stays on disk");

    harness.stop().await;
}

/// `session rm M --with-children` on a tree with a live child: one line per
/// node, a non-zero exit, and the child still listed.
#[tokio::test]
async fn session_rm_with_children_keeps_a_live_child_and_reports_every_node() {
    let harness = Harness::start().await;
    let parent = create_session(&harness, "rm-tree-parent", None).await;
    let parent_id = parent["id"].as_str().expect("parent id").to_owned();
    let first = create_session(
        &harness,
        "rm-tree-first",
        Some((&parent_id, "d-rm-tree-local")),
    )
    .await;
    let first_id = first["id"].as_str().expect("child id").to_owned();
    harness.run(&["session", "stop", &first_id]).await;
    let live = create_session(
        &harness,
        "rm-tree-live",
        Some((&parent_id, "d-rm-tree-local")),
    )
    .await;
    let live_id = live["id"].as_str().expect("live id").to_owned();

    let output = harness
        .run(&["session", "rm", &parent_id, "--with-children"])
        .await;
    assert_ne!(
        output.status.code(),
        Some(0),
        "a kept root fails the command: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    for id in [&parent_id, &first_id, &live_id] {
        assert!(stdout.contains(id), "every node is reported: {stdout}");
    }
    assert!(
        stdout.contains("live"),
        "the live child names its reason: {stdout}"
    );
    assert!(
        stdout.contains("descendant_kept") || stdout.contains("descendant kept"),
        "the root names its reason: {stdout}"
    );

    let (status, listed) = harness.json(&["session", "list"]).await;
    assert_eq!(status, 0, "{listed}");
    let ids = sorted_ids(&listed);
    assert!(
        ids.contains(&live_id),
        "the live child is still listed: {ids:?}"
    );
    assert!(
        ids.contains(&parent_id),
        "the kept root is still listed: {ids:?}"
    );
    assert!(
        !ids.contains(&first_id),
        "the finished child is gone: {ids:?}"
    );

    // The JSON form prints the result document with the same node set.
    let (status, document) = harness
        .json(&["session", "rm", &parent_id, "--with-children"])
        .await;
    assert_ne!(status, 0, "{document}");
    assert_eq!(document["ok"]["kept"], 2, "{document}");
    assert_eq!(document["ok"]["removed"], 0, "{document}");

    // The kept sessions stay live; `rm` stops them first, and the teardown
    // must remove them or their workers outlive the test and trip the
    // leaked-worker check.
    harness.stop().await;
}
