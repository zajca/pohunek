//! The one way a test runs `git`.
//!
//! A `git` child that inherits the developer's configuration reads their global
//! and system files: `commit.gpgSign` and `gpg.*` programs (an SSH agent or a
//! keychain prompt), `url.*.insteadOf` rewrites, hook paths, template
//! directories and credential helpers. A test must never reach any of them, so
//! every test-side `git` is built here and gets an environment that makes the
//! outcome independent of the machine: no global or system file, a private
//! `HOME`, no inherited `GIT_*` selection of a repository or of configuration,
//! no terminal prompt, and a fixed author and committer. Repository-local
//! configuration a test sets itself still applies.
//!
//! Product code that runs `git` (`crates/daemon/src/git.rs`) deliberately
//! inherits the user's configuration files and does not use this module.

// Rust guideline compliant 2026-10-31

use std::process::Command;

/// Author and committer name of a commit a test makes.
pub const IDENTITY_NAME: &str = "Pohunek Test";

/// Author and committer email of a commit a test makes.
pub const IDENTITY_EMAIL: &str = "test@example.invalid";

/// A path no account owns: the `HOME` and `XDG_CONFIG_HOME` of a test `git`, so
/// a file it would look up there cannot exist.
const NO_HOME: &str = "/nonexistent-pohunek-test-git-home";

/// Variables that select a repository, an index or configuration for `git`;
/// a test's own value, not the host's, decides them.
const REPOSITORY_SELECTION: [&str; 14] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_CONFIG_PARAMETERS",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_TEMPLATE_DIR",
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
];

/// A `git` command for a test, by absolute path, in an environment that ignores
/// the developer's configuration.
///
/// The program is resolved under the process-environment lock, as
/// [`crate::process_env::command`] does. Do not call it while holding a
/// [`crate::process_env::ProcessEnv`] on the same thread.
#[must_use]
pub fn command() -> Command {
    let mut command = crate::process_env::command("git");
    isolate(&mut command);
    command
        .env("HOME", NO_HOME)
        .env("XDG_CONFIG_HOME", format!("{NO_HOME}/config"));
    command
}

/// A `git` command by absolute path that inherits the process environment
/// untouched, for the scenario whose subject is how `git` reacts to
/// `GIT_*` variables or configuration files the test itself puts in place (a
/// control that proves such a setting is effective). Every other test uses
/// [`command`].
#[must_use]
pub fn command_with_inherited_environment() -> Command {
    crate::process_env::command("git")
}

/// Gives `command`, a `git` invocation, the configuration isolation of
/// [`command`] without touching `HOME`, for a command a [`crate::env::TestEnv`]
/// already pins to its private directories.
pub fn isolate(command: &mut Command) -> &mut Command {
    for name in REPOSITORY_SELECTION {
        command.env_remove(name);
    }
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", IDENTITY_NAME)
        .env("GIT_AUTHOR_EMAIL", IDENTITY_EMAIL)
        .env("GIT_COMMITTER_NAME", IDENTITY_NAME)
        .env("GIT_COMMITTER_EMAIL", IDENTITY_EMAIL)
}

/// Makes the repository at `repository` run the hooks in its own `.git/hooks`,
/// whatever `core.hooksPath` the developer's global or system configuration
/// sets. A value injected through `-c` or the `GIT_CONFIG_*` variables still
/// outranks the pin.
///
/// For a test that installs a native hook and then runs `git` through product
/// code, which inherits the process's Git configuration; a repository-local
/// setting outranks the global one.
///
/// # Panics
///
/// Panics when `git config` fails, which a repository the test just created
/// never does.
pub fn pin_hooks(repository: &std::path::Path) {
    let hooks = repository.join(".git").join("hooks");
    let output = command()
        .arg("-C")
        .arg(repository)
        .args(["config", "core.hooksPath"])
        .arg(&hooks)
        .output()
        .expect("run git config");
    assert!(
        output.status.success(),
        "pinning the hooks of {} failed: {}",
        repository.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::process_env::{in_own_process, ProcessEnv};

    /// A global configuration that signs, rewrites https to SSH, and sets a
    /// signing program that fails.
    const HOSTILE_CONFIG: &str = "[user]\n\tname = Hostile\n\temail = hostile@invalid.example\n[commit]\n\tgpgsign = true\n[tag]\n\tgpgSign = true\n[gpg]\n\tformat = ssh\n[gpg \"ssh\"]\n\tprogram = /nonexistent/failing-signer\n[url \"ssh://git@invalid.example/\"]\n\tinsteadOf = https://invalid.example/\n";

    fn output(mut command: Command, dir: &Path, args: &[&str]) -> (bool, String) {
        let output = command
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git");
        (
            output.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr)
            ),
        )
    }

    /// With a hostile developer configuration in place, the plain inherited form
    /// sees it (the control), and the helper neither sees it nor is stopped by
    /// it: init, commit, annotated tag and the rewritten URL all behave as on a
    /// machine with no configuration.
    #[test]
    fn the_helper_ignores_a_hostile_global_git_configuration() {
        if !in_own_process() {
            return;
        }
        // Commands are built before the environment lock is taken, because
        // building one resolves the program under that lock. They read the
        // process environment when they are spawned.
        let mut control: Vec<Command> =
            (0..3).map(|_| crate::process_env::command("git")).collect();
        let mut git: Vec<Command> = (0..8).map(|_| command()).collect();
        let mut next = move || git.pop().expect("a prepared command");

        // The hostile configuration reaches git through the user's files and the
        // system file, the channels a developer machine has.
        let home = crate::tempdir().expect("hostile home");
        for file in [
            home.path().join(".gitconfig"),
            home.path().join("xdg").join("git").join("config"),
            home.path().join("system-gitconfig"),
        ] {
            fs::create_dir_all(file.parent().expect("parent")).expect("create a directory");
            fs::write(&file, HOSTILE_CONFIG).expect("write the hostile configuration");
        }
        let repo = crate::tempdir().expect("repository directory");
        let mut env = ProcessEnv::lock();
        env.set("HOME", home.path());
        env.set("XDG_CONFIG_HOME", home.path().join("xdg"));
        env.set("GIT_CONFIG_SYSTEM", home.path().join("system-gitconfig"));
        env.remove("GIT_CONFIG_GLOBAL");
        env.remove("GIT_CONFIG_NOSYSTEM");

        // Control: the inherited form is stopped by the hostile configuration.
        let (ok, text) = output(
            control.pop().expect("control"),
            repo.path(),
            &["init", "-q", "-b", "main"],
        );
        assert!(ok, "{text}");
        let (ok, text) = output(
            control.pop().expect("control"),
            repo.path(),
            &["commit", "-q", "--allow-empty", "-m", "control"],
        );
        assert!(!ok, "the control commit is stopped by signing: {text}");
        fs::remove_dir_all(repo.path().join(".git")).expect("reset the repository");

        assert!(output(next(), repo.path(), &["init", "-q", "-b", "main"]).0);
        let (ok, text) = output(next(), repo.path(), &["config", "--get", "commit.gpgsign"]);
        assert!(!ok && text.is_empty(), "no signing is configured: {text}");
        fs::write(repo.path().join("file"), "x").expect("write a file");
        assert!(output(next(), repo.path(), &["add", "file"]).0);
        let (ok, text) = output(next(), repo.path(), &["commit", "-q", "-m", "one"]);
        assert!(ok, "an unsigned commit succeeds: {text}");
        let (ok, text) = output(next(), repo.path(), &["tag", "-a", "-m", "tag", "v1"]);
        assert!(ok, "an unsigned annotated tag succeeds: {text}");
        let (ok, text) = output(
            next(),
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://invalid.example/repo.git",
            ],
        );
        assert!(ok, "{text}");
        let (ok, text) = output(next(), repo.path(), &["remote", "get-url", "origin"]);
        assert!(
            ok && text.starts_with("https://"),
            "the URL is not rewritten: {text}"
        );
        let (ok, text) = output(
            control.pop().expect("control"),
            repo.path(),
            &["remote", "get-url", "origin"],
        );
        assert!(
            ok && text.starts_with("ssh://"),
            "the control URL is rewritten: {text}"
        );
        let (ok, text) = output(next(), repo.path(), &["log", "-1", "--format=%an <%ae>"]);
        assert!(
            ok && text.starts_with(&format!("{IDENTITY_NAME} <{IDENTITY_EMAIL}>")),
            "the identity is fixed: {text}"
        );
        drop(env);
    }

    /// A native hook a test installs in its repository runs under a developer
    /// configuration whose global `core.hooksPath` names a failing hook, when
    /// the repository is pinned; the unpinned control is stopped by the global
    /// hook, which shows the configuration is hostile.
    #[test]
    fn a_pinned_repository_runs_its_own_hook_under_a_hostile_global_hooks_path() {
        use std::os::unix::fs::PermissionsExt as _;

        if !in_own_process() {
            return;
        }
        let write_hook = |path: &Path, body: &str| {
            fs::create_dir_all(path.parent().expect("hooks dir")).expect("create hooks dir");
            fs::write(path, body).expect("write the hook");
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("executable hook");
        };
        let home = crate::tempdir().expect("hostile home");
        let global_marker = home.path().join("global-hook-ran");
        write_hook(
            &home.path().join("hooks").join("pre-commit"),
            &format!("#!/bin/sh\ntouch '{}'\nexit 1\n", global_marker.display()),
        );
        let global = home.path().join("gitconfig");
        fs::write(
            &global,
            format!(
                "[core]\n\thooksPath = {}\n",
                home.path().join("hooks").display()
            ),
        )
        .expect("write the hostile configuration");

        let own_marker = home.path().join("own-hook-ran");
        let mut repositories = Vec::new();
        for name in ["pinned", "unpinned"] {
            let repository = home.path().join(name);
            fs::create_dir(&repository).expect("create the repository");
            assert!(output(command(), &repository, &["init", "-q", "-b", "main"]).0);
            write_hook(
                &repository.join(".git/hooks/pre-commit"),
                &format!(
                    "#!/bin/sh\ntouch '{}-{name}'\nexit 0\n",
                    own_marker.display()
                ),
            );
            fs::write(repository.join("file"), "x").expect("write a file");
            assert!(output(command(), &repository, &["add", "file"]).0);
            repositories.push(repository);
        }
        pin_hooks(&repositories[0]);
        let mut inherited: Vec<Command> =
            (0..2).map(|_| crate::process_env::command("git")).collect();

        let mut env = ProcessEnv::lock();
        env.set("GIT_CONFIG_GLOBAL", &global);
        env.set("GIT_CONFIG_NOSYSTEM", "1");
        for variable in [
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
        ] {
            env.set(variable, "t");
        }
        let (ok, text) = output(
            inherited.pop().expect("command"),
            &repositories[1],
            &["commit", "-q", "-m", "unpinned"],
        );
        assert!(
            !ok,
            "the unpinned control is stopped by the global hook: {text}"
        );
        assert!(
            global_marker.exists(),
            "the global hook ran for the control"
        );
        fs::remove_file(&global_marker).expect("reset the global marker");

        let (ok, text) = output(
            inherited.pop().expect("command"),
            &repositories[0],
            &["commit", "-q", "-m", "pinned"],
        );
        assert!(ok, "the pinned repository commits: {text}");
        assert!(
            home.path().join("own-hook-ran-pinned").exists(),
            "the repository's own hook ran"
        );
        assert!(!global_marker.exists(), "the global hook did not run");
        drop(env);
    }
}
