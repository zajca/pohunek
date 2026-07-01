use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pohunek-script-{tag}-{}-{nanos}-{n}",
        std::process::id(),
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn script_path(name: &str) -> PathBuf {
    repo_root().join("scripts").join(name)
}

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).expect("write executable");
    let mut perms = fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod executable");
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Poll `path` until it contains every needle, or fail after a short deadline.
///
/// The switcher spawns attach/banner terminals in the background, so the mock
/// terminal may finish writing its arg log slightly after the script returns
/// (especially under parallel test load). Polling avoids a flaky read race
/// without changing the script's fire-and-forget behavior.
fn wait_for_file_contains(path: &Path, needles: &[&str], label: &str) {
    let deadline = SystemTime::now() + std::time::Duration::from_secs(5);
    loop {
        let content = read(path);
        if needles.iter().all(|needle| content.contains(needle)) {
            return;
        }
        assert!(
            SystemTime::now() < deadline,
            "{label}: timed out waiting for {needles:?} in:\n{content}"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn write_config(root: &Path, lines: &[(&str, &str)]) -> PathBuf {
    let config_dir = root.join("config").join("pohunek");
    fs::create_dir_all(config_dir.join("prompts")).expect("create config dirs");
    let mut config = String::new();
    for (key, value) in lines {
        config.push_str(key);
        config.push('=');
        config.push_str(value);
        config.push('\n');
    }
    fs::write(config_dir.join("launcher.conf"), config).expect("write launcher config");
    config_dir
}

#[test]
fn launch_pr_resolves_action_from_daemon_and_starts_one_session_without_token_leak() {
    let root = temp_dir("launch-pr");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");
    let gh = bin.join("gh");
    let pohunek = bin.join("pohunek");
    let pohunek_args = root.join("pohunek.args");
    let gh_args = root.join("gh.args");

    write_executable(
        &gh,
        r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >>"$POHUNEK_TEST_GH_ARGS"; done
printf '{"title":"Fix filters","body":"Body text","headRefName":"feature/filters","url":"https://example.test/pr/7"}\n'
"#,
    );
    // The mock daemon answers `project action` with a recipe (the agent + prompt
    // template are daemon-resolved now, Part A); every invocation logs its argv so
    // we can assert the resolve call and the session-new call. The optional
    // POHUNEK_TEST_RECIPE_FAIL makes the resolve fail (prompt_not_found).
    write_executable(
        &pohunek,
        r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >>"$POHUNEK_TEST_POHUNEK_ARGS"; done
if [ "${1:-}" = "prompt" ] && [ "${2:-}" = "render" ]; then
  exec "$POHUNEK_TEST_REAL_POHUNEK" "$@"
fi
case " $* " in
  *" project action "*)
    if [ -n "${POHUNEK_TEST_RECIPE_FAIL:-}" ]; then
      printf 'pohunek: prompt_not_found\n' >&2
      exit 1
    fi
    printf '%s' "$POHUNEK_TEST_RECIPE_JSON"
    ;;
esac
"#,
    );

    let config_dir = write_config(
        &root,
        &[
            ("pohunek_bin", pohunek.to_str().expect("utf8 path")),
            ("gh_bin", gh.to_str().expect("utf8 path")),
            ("host", "local"),
            ("yes", "true"),
        ],
    );
    let recipe = r#"{"provider":"github_pr","agent":"claude","prompt_name":"pr","prompt_content":"PR ${number}: ${title}\n${body}\nbranch=${branch}\nurl=${url}\n"}"#;

    let out = Command::new("sh")
        .arg(script_path("pohunek-launch-pr"))
        .arg("ui")
        .arg("7")
        .arg("review-pr")
        .env("POHUNEK_CONFIG_DIR", &config_dir)
        .env("POHUNEK_TEST_REAL_POHUNEK", env!("CARGO_BIN_EXE_pohunek"))
        .env("POHUNEK_TEST_GH_ARGS", &gh_args)
        .env("POHUNEK_TEST_POHUNEK_ARGS", &pohunek_args)
        .env("POHUNEK_TEST_RECIPE_JSON", recipe)
        .env("GITHUB_TOKEN", "ghp_secret_should_not_leak")
        .output()
        .expect("run launch-pr");

    assert!(
        out.status.success(),
        "launch-pr failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let args = read(&pohunek_args);
    // The launcher first resolves the action recipe from the daemon, then starts
    // the session with the daemon-resolved agent.
    assert!(
        args.contains("project\naction\nui\nreview-pr\n--json\n"),
        "{args}"
    );
    assert!(args.contains("prompt\nrender\n"), "{args}");
    assert!(args.contains("--provider\ngithub_pr\n"), "{args}");
    assert!(args.contains("--item-id\n7\n"), "{args}");
    assert!(args.contains("--host\nlocal\nsession\nnew\n"), "{args}");
    assert!(args.contains("--agent\nclaude\n"), "{args}");
    // The launcher references the project (resolved on the host); no --repo path
    // crosses the wire.
    assert!(args.contains("--project\nui\n"), "{args}");
    assert!(!args.contains("--repo"), "no --repo leaks: {args}");
    assert!(args.contains("--branch\nfeature/filters\n"), "{args}");
    assert!(args.contains("--yes\n"), "{args}");
    assert!(
        args.contains("PR 7: Fix filters\nBody text\nbranch=feature/filters\n"),
        "{args}"
    );
    assert!(!args.contains("ghp_secret_should_not_leak"), "{args}");
    assert!(
        read(&gh_args).contains("pr\nview\n7\n--json\n"),
        "{}",
        read(&gh_args)
    );
}

#[test]
fn launch_issue_uses_linear_seam_and_daemon_resolved_recipe() {
    let root = temp_dir("launch-issue");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");
    let linear = bin.join("linear-wrapper");
    let pohunek = bin.join("pohunek");
    let pohunek_args = root.join("pohunek.args");

    write_executable(
        &linear,
        r#"#!/bin/sh
printf '{"id":"LIN-123","title":"Fix launcher","description":"Issue body","branchName":"lin-123-fix-launcher","url":"https://linear.test/LIN-123"}\n'
"#,
    );
    write_executable(
        &pohunek,
        r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >>"$POHUNEK_TEST_POHUNEK_ARGS"; done
if [ "${1:-}" = "prompt" ] && [ "${2:-}" = "render" ]; then
  exec "$POHUNEK_TEST_REAL_POHUNEK" "$@"
fi
case " $* " in
  *" project action "*) printf '%s' "$POHUNEK_TEST_RECIPE_JSON" ;;
esac
"#,
    );

    let config_dir = write_config(
        &root,
        &[
            ("pohunek_bin", pohunek.to_str().expect("utf8 path")),
            ("linear_cli", linear.to_str().expect("utf8 path")),
            ("host", "build-box"),
        ],
    );
    // The recipe sets a base_branch; the launcher must thread it as --base-branch.
    let recipe = r#"{"provider":"linear_issue","agent":"codex","base_branch":"develop","prompt_name":"issue","prompt_content":"Issue ${id}: ${title}\n${body}\nbranch=${branch}\n"}"#;

    let out = Command::new("sh")
        .arg(script_path("pohunek-launch-issue"))
        .arg("ui")
        .arg("LIN-123")
        .env("POHUNEK_CONFIG_DIR", &config_dir)
        .env("POHUNEK_TEST_REAL_POHUNEK", env!("CARGO_BIN_EXE_pohunek"))
        .env("POHUNEK_TEST_POHUNEK_ARGS", &pohunek_args)
        .env("POHUNEK_TEST_RECIPE_JSON", recipe)
        .env("LINEAR_API_KEY", "lin_secret_should_not_leak")
        .output()
        .expect("run launch-issue");

    assert!(
        out.status.success(),
        "launch-issue failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let args = read(&pohunek_args);
    assert!(
        args.contains("--host\nbuild-box\nproject\naction\nui\nprocess-issue\n--json\n"),
        "{args}"
    );
    assert!(args.contains("prompt\nrender\n"), "{args}");
    assert!(args.contains("--provider\nlinear_issue\n"), "{args}");
    assert!(args.contains("--item-id\nLIN-123\n"), "{args}");
    assert!(args.contains("--host\nbuild-box\nsession\nnew\n"), "{args}");
    assert!(args.contains("--agent\ncodex\n"), "{args}");
    assert!(args.contains("--project\nui\n"), "{args}");
    assert!(!args.contains("--repo"), "no --repo leaks: {args}");
    assert!(args.contains("--branch\nlin-123-fix-launcher\n"), "{args}");
    // The template's base branch is honored.
    assert!(args.contains("--base-branch\ndevelop\n"), "{args}");
    assert!(
        args.contains("Issue LIN-123: Fix launcher\nIssue body\n"),
        "{args}"
    );
    // Credential isolation: the Linear token never reaches the pohunek command
    // line — auth stays inside the linear CLI's own seam.
    assert!(!args.contains("lin_secret_should_not_leak"), "{args}");
}

#[test]
fn launch_issue_agent_diverges_per_project_recipe() {
    // Two projects (driven entirely by the daemon-resolved recipe) launch issues
    // with different agents — the launcher passes through whatever the daemon's
    // action resolves, with no agent in the client config.
    let root = temp_dir("launch-divergence");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");
    let linear = bin.join("linear-wrapper");
    let pohunek = bin.join("pohunek");

    write_executable(
        &linear,
        r#"#!/bin/sh
printf '{"id":"LIN-1","title":"T","description":"B","branchName":"lin-1","url":"u"}\n'
"#,
    );
    write_executable(
        &pohunek,
        r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >>"$POHUNEK_TEST_POHUNEK_ARGS"; done
if [ "${1:-}" = "prompt" ] && [ "${2:-}" = "render" ]; then
  exec "$POHUNEK_TEST_REAL_POHUNEK" "$@"
fi
case " $* " in
  *" project action "*) printf '%s' "$POHUNEK_TEST_RECIPE_JSON" ;;
esac
"#,
    );

    let config_dir = write_config(
        &root,
        &[
            ("pohunek_bin", pohunek.to_str().expect("utf8 path")),
            ("linear_cli", linear.to_str().expect("utf8 path")),
            ("host", "local"),
        ],
    );

    let claude_recipe = r#"{"provider":"linear_issue","agent":"claude","prompt_name":"issue","prompt_content":"P ${title}\n"}"#;
    let codex_recipe = r#"{"provider":"linear_issue","agent":"codex-fast","prompt_name":"issue","prompt_content":"P ${title}\n"}"#;

    let run = |project: &str, recipe: &str, args_file: &Path| {
        let out = Command::new("sh")
            .arg(script_path("pohunek-launch-issue"))
            .arg(project)
            .arg("LIN-1")
            .env("POHUNEK_CONFIG_DIR", &config_dir)
            .env("POHUNEK_TEST_REAL_POHUNEK", env!("CARGO_BIN_EXE_pohunek"))
            .env("POHUNEK_TEST_POHUNEK_ARGS", args_file)
            .env("POHUNEK_TEST_RECIPE_JSON", recipe)
            .output()
            .expect("run launch-issue");
        assert!(out.status.success(), "launch-issue failed");
        read(args_file)
    };

    let a = run("project-a", claude_recipe, &root.join("a.args"));
    let b = run("project-b", codex_recipe, &root.join("b.args"));
    assert!(
        a.contains("project\naction\nproject-a\nprocess-issue\n"),
        "{a}"
    );
    assert!(
        b.contains("project\naction\nproject-b\nprocess-issue\n"),
        "{b}"
    );
    assert!(a.contains("--agent\nclaude\n"), "project A agent: {a}");
    assert!(b.contains("--agent\ncodex-fast\n"), "project B agent: {b}");
    assert!(!a.contains("--agent\ncodex-fast\n"), "{a}");
}

#[test]
fn launch_issue_aborts_without_session_on_prompt_not_found() {
    // A daemon resolution failure (prompt_not_found) aborts the launch before any
    // session is started — no silent fallback.
    let root = temp_dir("launch-abort");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");
    let linear = bin.join("linear-wrapper");
    let pohunek = bin.join("pohunek");
    let pohunek_args = root.join("pohunek.args");

    write_executable(
        &linear,
        r#"#!/bin/sh
printf '{"id":"LIN-1","title":"T","description":"B","branchName":"lin-1","url":"u"}\n'
"#,
    );
    write_executable(
        &pohunek,
        r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >>"$POHUNEK_TEST_POHUNEK_ARGS"; done
if [ "${1:-}" = "prompt" ] && [ "${2:-}" = "render" ]; then
  exec "$POHUNEK_TEST_REAL_POHUNEK" "$@"
fi
case " $* " in
  *" project action "*) printf 'pohunek: prompt_not_found\n' >&2; exit 1 ;;
esac
"#,
    );

    let config_dir = write_config(
        &root,
        &[
            ("pohunek_bin", pohunek.to_str().expect("utf8 path")),
            ("linear_cli", linear.to_str().expect("utf8 path")),
            ("host", "local"),
        ],
    );

    let out = Command::new("sh")
        .arg(script_path("pohunek-launch-issue"))
        .arg("ui")
        .arg("LIN-1")
        .env("POHUNEK_CONFIG_DIR", &config_dir)
        .env("POHUNEK_TEST_REAL_POHUNEK", env!("CARGO_BIN_EXE_pohunek"))
        .env("POHUNEK_TEST_POHUNEK_ARGS", &pohunek_args)
        .output()
        .expect("run launch-issue");

    assert!(
        !out.status.success(),
        "launch-issue must fail when the action does not resolve"
    );
    let args = read(&pohunek_args);
    // The resolve was attempted, but no session was ever started.
    assert!(args.contains("project\naction\n"), "{args}");
    assert!(
        !args.contains("session\nnew\n"),
        "no session must be started: {args}"
    );
}

#[test]
fn launch_issue_rejects_action_with_non_linear_provider_before_fetch() {
    let root = temp_dir("launch-issue-provider-mismatch");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");
    let linear = bin.join("linear-wrapper");
    let pohunek = bin.join("pohunek");
    let linear_args = root.join("linear.args");
    let pohunek_args = root.join("pohunek.args");

    write_executable(
        &linear,
        r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >>"$POHUNEK_TEST_LINEAR_ARGS"; done
printf '{"id":"LIN-1","title":"T","description":"B","branchName":"lin-1","url":"u"}\n'
"#,
    );
    write_executable(
        &pohunek,
        r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >>"$POHUNEK_TEST_POHUNEK_ARGS"; done
if [ "${1:-}" = "prompt" ] && [ "${2:-}" = "render" ]; then
  exec "$POHUNEK_TEST_REAL_POHUNEK" "$@"
fi
case " $* " in
  *" project action "*) printf '%s' "$POHUNEK_TEST_RECIPE_JSON" ;;
esac
"#,
    );
    let config_dir = write_config(
        &root,
        &[
            ("pohunek_bin", pohunek.to_str().expect("utf8 path")),
            ("linear_cli", linear.to_str().expect("utf8 path")),
            ("host", "local"),
        ],
    );
    let recipe = r#"{"provider":"github_pr","agent":"codex","prompt_name":"issue","prompt_content":"Issue ${id}\n"}"#;

    let out = Command::new("sh")
        .arg(script_path("pohunek-launch-issue"))
        .arg("ui")
        .arg("LIN-1")
        .env("POHUNEK_CONFIG_DIR", &config_dir)
        .env("POHUNEK_TEST_REAL_POHUNEK", env!("CARGO_BIN_EXE_pohunek"))
        .env("POHUNEK_TEST_LINEAR_ARGS", &linear_args)
        .env("POHUNEK_TEST_POHUNEK_ARGS", &pohunek_args)
        .env("POHUNEK_TEST_RECIPE_JSON", recipe)
        .output()
        .expect("run launch-issue");

    assert!(
        !out.status.success(),
        "launch-issue must reject a non-linear provider"
    );
    let args = read(&pohunek_args);
    assert!(
        args.contains("project\naction\nui\nprocess-issue\n--json\n"),
        "{args}"
    );
    assert!(
        !args.contains("session\nnew\n"),
        "no session must be started: {args}"
    );
    assert_eq!(read(&linear_args), "", "Linear must not be fetched");
}

#[test]
fn launch_pr_rejects_provider_none_static_branch_before_fetch() {
    let root = temp_dir("launch-pr-provider-none");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");
    let gh = bin.join("gh");
    let pohunek = bin.join("pohunek");
    let gh_args = root.join("gh.args");
    let pohunek_args = root.join("pohunek.args");

    write_executable(
        &gh,
        r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >>"$POHUNEK_TEST_GH_ARGS"; done
printf '{"title":"T","body":"B","headRefName":"feat/x","url":"u"}\n'
"#,
    );
    write_executable(
        &pohunek,
        r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >>"$POHUNEK_TEST_POHUNEK_ARGS"; done
if [ "${1:-}" = "prompt" ] && [ "${2:-}" = "render" ]; then
  exec "$POHUNEK_TEST_REAL_POHUNEK" "$@"
fi
case " $* " in
  *" project action "*) printf '%s' "$POHUNEK_TEST_RECIPE_JSON" ;;
esac
"#,
    );
    let config_dir = write_config(
        &root,
        &[
            ("pohunek_bin", pohunek.to_str().expect("utf8 path")),
            ("gh_bin", gh.to_str().expect("utf8 path")),
            ("host", "local"),
        ],
    );
    let recipe = r#"{"provider":"none","agent":"claude","branch":"feature/static","prompt_name":"pr","prompt_content":"PR ${number}\n"}"#;

    let out = Command::new("sh")
        .arg(script_path("pohunek-launch-pr"))
        .arg("ui")
        .arg("7")
        .env("POHUNEK_CONFIG_DIR", &config_dir)
        .env("POHUNEK_TEST_REAL_POHUNEK", env!("CARGO_BIN_EXE_pohunek"))
        .env("POHUNEK_TEST_GH_ARGS", &gh_args)
        .env("POHUNEK_TEST_POHUNEK_ARGS", &pohunek_args)
        .env("POHUNEK_TEST_RECIPE_JSON", recipe)
        .output()
        .expect("run launch-pr");

    assert!(
        !out.status.success(),
        "launch-pr must reject provider=none recipes"
    );
    let args = read(&pohunek_args);
    assert!(
        args.contains("project\naction\nui\nprocess-pr\n--json\n"),
        "{args}"
    );
    assert!(
        !args.contains("session\nnew\n"),
        "no session must be started: {args}"
    );
    assert_eq!(read(&gh_args), "", "GitHub must not be fetched");
}

#[test]
fn launch_issue_rejects_unknown_template_variable_without_session() {
    let root = temp_dir("launch-issue-unknown-var");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");
    let linear = bin.join("linear-wrapper");
    let pohunek = bin.join("pohunek");
    let pohunek_args = root.join("pohunek.args");

    write_executable(
        &linear,
        r#"#!/bin/sh
printf '{"id":"LIN-1","title":"T","description":"B","branchName":"lin-1","url":"u"}\n'
"#,
    );
    write_executable(
        &pohunek,
        r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >>"$POHUNEK_TEST_POHUNEK_ARGS"; done
if [ "${1:-}" = "prompt" ] && [ "${2:-}" = "render" ]; then
  exec "$POHUNEK_TEST_REAL_POHUNEK" "$@"
fi
case " $* " in
  *" project action "*) printf '%s' "$POHUNEK_TEST_RECIPE_JSON" ;;
esac
"#,
    );
    let config_dir = write_config(
        &root,
        &[
            ("pohunek_bin", pohunek.to_str().expect("utf8 path")),
            ("linear_cli", linear.to_str().expect("utf8 path")),
            ("host", "local"),
        ],
    );
    let recipe = r#"{"provider":"linear_issue","agent":"codex","prompt_name":"issue","prompt_content":"Issue ${id}: ${missing}\n"}"#;

    let out = Command::new("sh")
        .arg(script_path("pohunek-launch-issue"))
        .arg("ui")
        .arg("LIN-1")
        .env("POHUNEK_CONFIG_DIR", &config_dir)
        .env("POHUNEK_TEST_REAL_POHUNEK", env!("CARGO_BIN_EXE_pohunek"))
        .env("POHUNEK_TEST_POHUNEK_ARGS", &pohunek_args)
        .env("POHUNEK_TEST_RECIPE_JSON", recipe)
        .output()
        .expect("run launch-issue");

    assert!(
        !out.status.success(),
        "unknown template variables must reject the launch"
    );
    let args = read(&pohunek_args);
    assert!(
        args.contains("project\naction\nui\nprocess-issue\n--json\n"),
        "{args}"
    );
    assert!(
        !args.contains("session\nnew\n"),
        "no session must be started: {args}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown variable"),
        "stderr should explain the template failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn rofi_issue_lists_my_issues_and_hands_selection_to_launch_issue() {
    let root = temp_dir("rofi-issue");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");
    let linear = bin.join("linear-wrapper");
    let rofi = bin.join("rofi");
    let terminal = bin.join("terminal");
    let linear_args = root.join("linear.args");
    let rofi_stdin = root.join("rofi.stdin");
    let terminal_args = root.join("terminal.args");

    // The issue query logs its args (so we can assert the assignee/state filter)
    // and returns two issues. The second title carries a JSON-escaped tab to prove
    // the picker flattens it rather than letting it forge an extra rofi column.
    write_executable(
        &linear,
        r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >>"$POHUNEK_TEST_LINEAR_ARGS"; done
if [ "$1" = "issue" ] && [ "$2" = "query" ]; then
  printf '[{"identifier":"AI-1","title":"First issue","state":{"name":"Todo"}},{"identifier":"AI-2","title":"Second\\ttab","state":{"name":"In Progress"}}]\n'
fi
"#,
    );
    // rofi: capture the offered rows, then "select" the first one.
    write_executable(
        &rofi,
        "#!/bin/sh
cat >\"$POHUNEK_TEST_ROFI_STDIN\"
head -n 1 \"$POHUNEK_TEST_ROFI_STDIN\"
",
    );
    write_executable(
        &terminal,
        "#!/bin/sh
for arg in \"$@\"; do printf '%s\\n' \"$arg\" >>\"$POHUNEK_TEST_TERMINAL_ARGS\"; done
",
    );

    let config_dir = write_config(
        &root,
        &[
            ("linear_cli", linear.to_str().expect("utf8 path")),
            ("rofi_bin", rofi.to_str().expect("utf8 path")),
            ("terminal", terminal.to_str().expect("utf8 path")),
            // Explicit assignee avoids depending on a `linear auth whoami` stub.
            ("linear_assignee", "zajca"),
            ("host", "local"),
        ],
    );

    let out = Command::new("sh")
        .arg(script_path("pohunek-rofi-issue"))
        .arg("ui")
        .env("POHUNEK_CONFIG_DIR", &config_dir)
        .env("POHUNEK_TEST_LINEAR_ARGS", &linear_args)
        .env("POHUNEK_TEST_ROFI_STDIN", &rofi_stdin)
        .env("POHUNEK_TEST_TERMINAL_ARGS", &terminal_args)
        .env("LINEAR_API_KEY", "lin_secret_should_not_leak")
        .output()
        .expect("run rofi-issue");

    assert!(
        out.status.success(),
        "rofi-issue failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The query targets my issues across teams, filtered to the actionable states.
    let largs = read(&linear_args);
    assert!(largs.contains("query\n"), "{largs}");
    assert!(largs.contains("--all-teams\n"), "{largs}");
    assert!(largs.contains("--assignee\nzajca\n"), "{largs}");
    assert!(largs.contains("--state\nstarted\n"), "{largs}");
    assert!(largs.contains("--state\nunstarted\n"), "{largs}");

    // Rows are "identifier<TAB>state<TAB>title"; the tab inside the second title
    // is flattened, and the workflow state is surfaced as its own column.
    let rows = read(&rofi_stdin);
    assert!(rows.contains("AI-1\tTodo\tFirst issue"), "{rows}");
    assert!(rows.contains("AI-2\tIn Progress\tSecond tab"), "{rows}");

    // The selected identifier is handed to pohunek-launch-issue inside a terminal.
    // The launch runs in the background, so poll for the spawned args.
    wait_for_file_contains(
        &terminal_args,
        &["pohunek-launch-issue", "ui", "AI-1"],
        "rofi-issue terminal",
    );
    // The Linear token never reaches the spawned command line.
    assert!(
        !read(&terminal_args).contains("lin_secret_should_not_leak"),
        "{}",
        read(&terminal_args)
    );
}

#[test]
fn rofi_issue_derives_assignee_from_whoami_when_unset() {
    let root = temp_dir("rofi-issue-derive");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");
    let linear = bin.join("linear-wrapper");
    let rofi = bin.join("rofi");
    let terminal = bin.join("terminal");
    let linear_args = root.join("linear.args");
    let terminal_args = root.join("terminal.args");

    // `auth whoami` carries the display name in its labelled output (it has no
    // --json mode); the picker must derive "zajca" from it and feed --assignee.
    write_executable(
        &linear,
        r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >>"$POHUNEK_TEST_LINEAR_ARGS"; done
if [ "$1" = "auth" ] && [ "$2" = "whoami" ]; then
  printf 'Workspace: Keboola\n  Display name: zajca\n  Email: x@example.test\n'
  exit 0
fi
if [ "$1" = "issue" ] && [ "$2" = "query" ]; then
  printf '[{"identifier":"AI-9","title":"Derived"}]\n'
fi
"#,
    );
    write_executable(
        &rofi,
        r"#!/bin/sh
cat >/dev/null
printf 'AI-9\tDerived\n'
",
    );
    write_executable(
        &terminal,
        r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >>"$POHUNEK_TEST_TERMINAL_ARGS"; done
"#,
    );

    // No linear_assignee key → the picker must derive it.
    let config_dir = write_config(
        &root,
        &[
            ("linear_cli", linear.to_str().expect("utf8 path")),
            ("rofi_bin", rofi.to_str().expect("utf8 path")),
            ("terminal", terminal.to_str().expect("utf8 path")),
            ("host", "local"),
        ],
    );

    let out = Command::new("sh")
        .arg(script_path("pohunek-rofi-issue"))
        .arg("ui")
        .env("POHUNEK_CONFIG_DIR", &config_dir)
        .env("POHUNEK_TEST_LINEAR_ARGS", &linear_args)
        .env("POHUNEK_TEST_TERMINAL_ARGS", &terminal_args)
        .output()
        .expect("run rofi-issue");

    assert!(
        out.status.success(),
        "rofi-issue failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let largs = read(&linear_args);
    assert!(largs.contains("whoami\n"), "must call whoami: {largs}");
    assert!(
        largs.contains("--assignee\nzajca\n"),
        "derived assignee must reach the query: {largs}"
    );
    wait_for_file_contains(
        &terminal_args,
        &["pohunek-launch-issue", "ui", "AI-9"],
        "rofi-issue derive terminal",
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "single end-to-end scenario; splitting it would obscure the narrative under test"
)]
fn rofi_merges_local_and_remote_hosts_multi_selects_and_reconciles_marks() {
    let root = temp_dir("rofi");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");
    let pohunek = bin.join("pohunek");
    let rofi = bin.join("rofi");
    let swaymsg = bin.join("swaymsg");
    let terminal = bin.join("terminal");
    let calls = root.join("calls.log");
    let rofi_stdin = root.join("rofi.stdin");
    let terminal_args = root.join("terminal.args");

    // `host discover` lists NetBird peers only (box, down) — NOT the local
    // daemon. The switcher must add `local` itself, so `--host local` is queried
    // and local sessions appear. A real `host discover` never returns `local`.
    write_executable(
        &pohunek,
        r#"#!/bin/sh
printf 'pohunek' >>"$POHUNEK_TEST_CALLS"
for arg in "$@"; do printf ' %s' "$arg" >>"$POHUNEK_TEST_CALLS"; done
printf '\n' >>"$POHUNEK_TEST_CALLS"
if [ "$1" = "host" ] && [ "$2" = "discover" ]; then
  printf '[{"name":"box","classification":"reachable_daemon"},{"name":"down","classification":"reachable_daemon"}]\n'
  exit 0
fi
if [ "$1" = "--host" ] && [ "$3" = "session" ] && [ "$4" = "list" ]; then
  case "$2" in
    # project_label carries a JSON newline escape (\n): the switcher must collapse
    # it to a space so the row stays one tab-safe line and no fragment leaks as a
    # target. `printf '%s\n'` keeps the arg's backslash-n literal for the JSON.
    local) printf '%s\n' '[{"id":"s-1","agent":"claude","state":"running","activity":"blocked","project_id":"p-ui","project_label":"ui\nevil","branch":"feat/x"}]' ;;
    box) printf '[{"id":"s-2","agent":"codex","state":"running","activity":"working"}]\n' ;;
    down) printf 'host down\n' >&2; exit 9 ;;
    *) printf '[]\n' ;;
  esac
  exit 0
fi
exit 1
"#,
    );
    // Multi-select: echo every offered row (the error row is filtered out by the
    // target extraction, leaving the two real sessions selected).
    write_executable(
        &rofi,
        r#"#!/bin/sh
cat >"$POHUNEK_TEST_ROFI_STDIN"
cat "$POHUNEK_TEST_ROFI_STDIN"
"#,
    );
    write_executable(
        &swaymsg,
        r#"#!/bin/sh
printf 'swaymsg' >>"$POHUNEK_TEST_CALLS"
for arg in "$@"; do printf ' %s' "$arg" >>"$POHUNEK_TEST_CALLS"; done
printf '\n' >>"$POHUNEK_TEST_CALLS"
if [ "$1" = "-t" ] && [ "$2" = "get_tree" ]; then
  printf '{"nodes":[{"marks":["pohunek:box/s-old","pohunek-banner:box/s-old"],"nodes":[],"floating_nodes":[]}],"floating_nodes":[]}\n'
fi
"#,
    );
    write_executable(
        &terminal,
        r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >>"$POHUNEK_TEST_TERMINAL_ARGS"; done
"#,
    );

    let config_dir = write_config(
        &root,
        &[
            ("pohunek_bin", pohunek.to_str().expect("utf8 path")),
            ("rofi_bin", rofi.to_str().expect("utf8 path")),
            ("swaymsg_bin", swaymsg.to_str().expect("utf8 path")),
            ("terminal", terminal.to_str().expect("utf8 path")),
            // Generous so the stub `session list` is never spuriously killed by
            // the per-host `timeout` under parallel test load (which would drop
            // a host to an error row and fail the multi-select assertion).
            ("list_timeout_seconds", "30"),
            ("banner", "true"),
            ("banner_height_px", "24"),
            // Keep the mark-retry loop a single fast attempt: the stub swaymsg's
            // get_tree never reflects newly added marks, so verification cannot
            // succeed; one attempt keeps the test fast while still issuing the
            // mark --add we assert on.
            ("mark_retry_count", "1"),
            ("mark_retry_interval_seconds", "0"),
        ],
    );

    let out = Command::new("sh")
        .arg(script_path("pohunek-rofi"))
        .args(["--filter", "state=running"])
        .env("POHUNEK_CONFIG_DIR", &config_dir)
        .env("POHUNEK_TEST_CALLS", &calls)
        .env("POHUNEK_TEST_ROFI_STDIN", &rofi_stdin)
        .env("POHUNEK_TEST_TERMINAL_ARGS", &terminal_args)
        .output()
        .expect("run rofi");

    assert!(
        out.status.success(),
        "rofi failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Both selected sessions are returned (multi-select), local first. Crucially,
    // the newline in local's project label did NOT split its row into a second
    // line whose fragment ("evil") would leak through as an extra target.
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf8 stdout"),
        "local/s-1\nbox/s-2\n"
    );
    let rows = read(&rofi_stdin);
    // The control char is collapsed to a space, keeping the row one tab-safe line.
    assert!(
        rows.contains("local/s-1\tui evil\tfeat/x\tclaude\trunning\tblocked"),
        "label newline must be flattened to a space: {rows}"
    );
    assert!(
        !rows.lines().any(|line| line.starts_with("evil")),
        "a label fragment must never become its own (target) row: {rows}"
    );
    // The local daemon is enumerated even though `host discover` omits it: the
    // mock discover returns only box+down, so a local/s-1 row can ONLY appear if
    // the switcher queried `--host local` on its own (the F2 guarantee). The
    // local row (with its flattened project label + branch) is asserted above.
    // Row columns: host/session  PROJECT  BRANCH  agent  state  activity. box has
    // no project/branch, so both fall back to `-`; the first field is the key.
    assert!(
        rows.contains("box/s-2\t-\t-\tcodex\trunning\tworking"),
        "{rows}"
    );
    assert!(rows.contains("!down\tERROR\t"), "{rows}");
    // NOTE: do not assert on the per-host `pohunek ... session list` lines in the
    // calls log — those queries run as concurrent background subshells appending
    // to one file, so their lines interleave. The swaymsg calls below are issued
    // sequentially and are safe to assert on.
    let calls_text = read(&calls);
    // Deselected windows (box/s-old) are closed; closing detaches, never stops.
    assert!(
        calls_text.contains("[con_mark=\"pohunek:box/s-old\"] kill"),
        "{calls_text}"
    );
    assert!(
        calls_text.contains("[con_mark=\"pohunek-banner:box/s-old\"] kill"),
        "{calls_text}"
    );
    assert!(
        calls_text
            .contains("[title=\"pohunek-banner:local/s-1\"] mark --add pohunek-banner:local/s-1"),
        "{calls_text}"
    );
    assert!(
        calls_text.contains("[title=\"pohunek-banner:local/s-1\"] floating disable"),
        "banner must be forced back into the tiling tree: {calls_text}"
    );
    assert!(
        calls_text.contains(
            "[title=\"pohunek-banner:local/s-1\"] move container to mark pohunek:local/s-1"
        ),
        "banner must be moved to its attach window mark: {calls_text}"
    );
    // Both selected sessions get an attach window (terminals are spawned in the
    // background, so wait for the mock terminal's arg log to settle).
    wait_for_file_contains(
        &terminal_args,
        &["local/s-1", "box/s-2", "pohunek-session-banner"],
        "terminal args",
    );
}

#[test]
fn banner_reflects_agent_state_transition() {
    // Slice E: the banner re-renders on a state transition fed from the event
    // stream. A stub `pohunek subscribe` emits working -> blocked for the target
    // session; the banner line must end up reflecting `activity=blocked`.
    let root = temp_dir("banner");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");
    let pohunek = bin.join("pohunek");

    write_executable(
        &pohunek,
        r#"#!/bin/sh
if [ "$1" = "session" ] && [ "$2" = "inspect" ]; then
  printf '{"id":"s-1","agent":"claude","state":"running","activity":"working"}\n'
  exit 0
fi
if [ "$1" = "subscribe" ]; then
  printf '{"event":"agent_state","session_id":"s-1","activity":"working"}\n'
  printf '{"event":"agent_state","session_id":"s-1","activity":"blocked"}\n'
  exit 0
fi
exit 1
"#,
    );

    let config_dir = write_config(
        &root,
        &[("pohunek_bin", pohunek.to_str().expect("utf8 path"))],
    );

    // The banner never exits while its window is open (it falls back to polling
    // when the subscribe stream ends), so bound the run with `timeout`. The
    // blocked transition is rendered during the subscribe phase, before the
    // fallback, so it is present in the captured output.
    let out = Command::new("timeout")
        .arg("2")
        .arg("sh")
        .arg(script_path("pohunek-session-banner"))
        .arg("local/s-1")
        .env("POHUNEK_CONFIG_DIR", &config_dir)
        .output()
        .expect("run banner");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("local/s-1  agent=claude  state=running  activity=blocked"),
        "banner did not reflect the blocked transition: {stdout}"
    );
}
