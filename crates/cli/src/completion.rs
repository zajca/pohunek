//! Static and opt-in dynamic shell completion for `pohunek`.
//!
//! Static scripts are generated entirely from the clap command tree. Dynamic
//! scripts register clap's environment completer, which augments `--host` and
//! session-target arguments with bounded live lookups. Completion failures are
//! deliberately silent: pressing Tab must never turn a missing daemon, stale
//! mesh state, or invalid local environment into shell noise.

use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io::{self, Write as _};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Command, CommandFactory, ValueEnum};
use clap_complete::engine::ValueCompleter;
use clap_complete::{ArgValueCompleter, CompleteEnv, CompletionCandidate};
use pohunek_client::{Client, ClientOptions};
use protocol::{HostClass, HostRecord, SessionInfo, SessionListParams};
use serde::Serialize;

use crate::error::CliError;
use crate::paths::Paths;
use crate::{Cli, LOCAL_HOST};

/// Environment variable used by clap's dynamic completion protocol.
const COMPLETE_ENV: &str = "POHUNEK_COMPLETE";

/// Dynamic completion is interactive, so all discovery and daemon I/O shares a
/// short deadline rather than inheriting the normal multi-second CLI timeouts.
const COMPLETION_DEADLINE: Duration = Duration::from_millis(750);

/// Shells supported by the public completion commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum CompletionShell {
    /// Bash completion.
    Bash,
    /// Zsh completion.
    Zsh,
    /// Fish completion.
    Fish,
}

impl CompletionShell {
    const fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
        }
    }

    const fn aot(self) -> clap_complete::aot::Shell {
        match self {
            Self::Bash => clap_complete::aot::Shell::Bash,
            Self::Zsh => clap_complete::aot::Shell::Zsh,
            Self::Fish => clap_complete::aot::Shell::Fish,
        }
    }
}

/// Result of installing one managed completion script.
#[derive(Debug, Serialize)]
struct InstallResult {
    shell: &'static str,
    dynamic: bool,
    path: String,
    next_steps: Vec<String>,
}

/// Let clap service a dynamic completion request before normal CLI parsing.
///
/// When [`COMPLETE_ENV`] is absent this is a no-op. In completion mode clap
/// writes the registration or candidates and exits the process successfully.
pub(crate) fn complete_env() {
    let context = CompletionContext::from_process();
    CompleteEnv::with_factory(move || dynamic_command(context.clone()))
        .var(COMPLETE_ENV)
        .bin("pohunek")
        .complete();
}

/// Print a completion script for one shell.
///
/// # Errors
///
/// Returns [`CliError::Io`] when stdout cannot be written.
pub(crate) fn run(shell: CompletionShell, dynamic: bool) -> Result<(), CliError> {
    io::stdout().write_all(&render_script(shell, dynamic))?;
    Ok(())
}

/// Install a completion script in the shell's conventional per-user path.
///
/// Re-running the command overwrites the managed file with the current command
/// tree, making installation deterministic and idempotent.
///
/// # Errors
///
/// Returns [`CliError`] when XDG paths cannot be resolved or the script cannot
/// be created.
pub(crate) fn install(
    paths: &Paths,
    shell: CompletionShell,
    dynamic: bool,
    json: bool,
) -> Result<(), CliError> {
    let data_home = Paths::data_home_only()?;
    let path = completion_path(paths, &data_home, shell);
    write_script(&path, shell, dynamic)?;

    let result = InstallResult {
        shell: shell.name(),
        dynamic,
        path: path.display().to_string(),
        next_steps: install_next_steps(shell, &path),
    };
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        println!("Installed {} completion: {}", shell.name(), result.path);
        for step in &result.next_steps {
            println!("Next: {step}");
        }
    }
    Ok(())
}

fn write_script(path: &Path, shell: CompletionShell, dynamic: bool) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, render_script(shell, dynamic))?;
    Ok(())
}

fn render_script(shell: CompletionShell, dynamic: bool) -> Vec<u8> {
    if dynamic {
        return dynamic_bootstrap(shell).as_bytes().to_vec();
    }

    let mut command = Cli::command();
    let mut output = Vec::new();
    clap_complete::aot::generate(shell.aot(), &mut command, "pohunek", &mut output);
    output
}

fn dynamic_bootstrap(shell: CompletionShell) -> &'static str {
    match shell {
        CompletionShell::Bash => "source <(POHUNEK_COMPLETE=bash pohunek)\n",
        CompletionShell::Zsh => "source <(POHUNEK_COMPLETE=zsh pohunek)\n",
        CompletionShell::Fish => "POHUNEK_COMPLETE=fish pohunek | source\n",
    }
}

fn completion_path(paths: &Paths, data_home: &Path, shell: CompletionShell) -> PathBuf {
    match shell {
        CompletionShell::Bash => data_home
            .join("bash-completion")
            .join("completions")
            .join("pohunek"),
        CompletionShell::Zsh => data_home
            .join("zsh")
            .join("site-functions")
            .join("_pohunek"),
        CompletionShell::Fish => paths
            .config_home
            .join("fish")
            .join("completions")
            .join("pohunek.fish"),
    }
}

fn install_next_steps(shell: CompletionShell, path: &Path) -> Vec<String> {
    match shell {
        CompletionShell::Bash => {
            vec!["start a new shell (bash-completion loads the file on demand)".to_owned()]
        }
        CompletionShell::Fish => vec!["start a new fish shell".to_owned()],
        CompletionShell::Zsh => {
            let directory = path.parent().unwrap_or_else(|| Path::new("."));
            vec![format!(
                "add 'fpath=({} $fpath)' before 'compinit' in .zshrc, then start a new shell",
                directory.display()
            )]
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CompletionContext {
    explicit_host: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct HostCompleter;

impl ValueCompleter for HostCompleter {
    fn complete(&self, current: &OsStr) -> Vec<CompletionCandidate> {
        complete_hosts(current)
    }
}

#[derive(Clone, Debug)]
struct TargetCompleter {
    context: CompletionContext,
}

#[derive(Debug, Eq, PartialEq)]
enum TargetScope<'a> {
    Qualified { host: &'a str, prefix: &'a str },
    Selected { host: &'a str, prefix: &'a str },
    LocalAndHosts { prefix: &'a str },
}

impl ValueCompleter for TargetCompleter {
    fn complete(&self, current: &OsStr) -> Vec<CompletionCandidate> {
        complete_targets(current, &self.context)
    }
}

impl CompletionContext {
    fn from_process() -> Self {
        let args: Vec<OsString> = std::env::args_os().collect();
        let request = args
            .iter()
            .position(|arg| arg == "--")
            .map_or(&[][..], |separator| &args[separator + 1..]);
        let index = std::env::var("_CLAP_COMPLETE_INDEX")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(request.len());
        Self::from_words(&request[..index.min(request.len())])
    }

    fn from_words(words: &[OsString]) -> Self {
        let mut explicit_host = None;
        let mut index = 1;
        while index < words.len() {
            let word = words[index].to_string_lossy();
            if word == "--" {
                break;
            }
            if let Some(host) = word.strip_prefix("--host=") {
                explicit_host = valid_host(host).then(|| host.to_owned());
            } else if word == "--host" {
                if let Some(host) = words.get(index + 1).and_then(|value| value.to_str()) {
                    explicit_host = valid_host(host).then(|| host.to_owned());
                    index += 1;
                }
            }
            index += 1;
        }
        Self { explicit_host }
    }
}

fn dynamic_command(context: CompletionContext) -> Command {
    let host_context = context.clone();
    let target_context = context;
    Cli::command()
        .mut_arg("host", |arg| arg.add(ArgValueCompleter::new(HostCompleter)))
        .mut_subcommand("attach", {
            let context = target_context.clone();
            move |command| with_target_completer(command, &context)
        })
        .mut_subcommand("session", move |command| {
            command.mut_subcommands({
                let context = host_context;
                move |subcommand| {
                    if subcommand
                        .get_arguments()
                        .any(|arg| arg.get_id() == "target")
                    {
                        with_target_completer(subcommand, &context)
                    } else {
                        subcommand
                    }
                }
            })
        })
}

fn with_target_completer(command: Command, context: &CompletionContext) -> Command {
    command.mut_args(|arg| {
        if arg.get_id() == "target" {
            arg.add(ArgValueCompleter::new(TargetCompleter {
                context: context.clone(),
            }))
        } else {
            arg
        }
    })
}

fn complete_hosts(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let Some(records) = run_bounded(async {
        let cache_dir = Paths::cache_dir_only().ok()?;
        crate::commands::host::fetch_records(&cache_dir, false)
            .await
            .ok()
    }) else {
        return local_host_candidate(current);
    };

    host_names(&records)
        .into_iter()
        .filter(|host| host.starts_with(current))
        .map(CompletionCandidate::new)
        .collect()
}

fn local_host_candidate(current: &str) -> Vec<CompletionCandidate> {
    if LOCAL_HOST.starts_with(current) {
        vec![CompletionCandidate::new(LOCAL_HOST)]
    } else {
        Vec::new()
    }
}

fn complete_targets(current: &OsStr, context: &CompletionContext) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let current = current.to_owned();
    let context = context.clone();
    run_bounded(async move { target_candidates(&current, &context).await }).unwrap_or_default()
}

async fn target_candidates(
    current: &str,
    context: &CompletionContext,
) -> Option<Vec<CompletionCandidate>> {
    match target_scope(current, context)? {
        TargetScope::Qualified { host, prefix } => {
            let sessions = query_sessions(host).await?;
            Some(session_candidates(&sessions, prefix, Some(host)))
        }
        TargetScope::Selected { host, prefix } => {
            let sessions = query_sessions(host).await?;
            Some(session_candidates(&sessions, prefix, None))
        }
        TargetScope::LocalAndHosts { prefix } => local_and_host_candidates(prefix).await,
    }
}

fn target_scope<'a>(current: &'a str, context: &'a CompletionContext) -> Option<TargetScope<'a>> {
    if let Some((host, prefix)) = current.split_once('/') {
        return valid_host(host).then_some(TargetScope::Qualified { host, prefix });
    }
    if let Some(host) = context.explicit_host.as_deref() {
        return Some(TargetScope::Selected {
            host,
            prefix: current,
        });
    }
    Some(TargetScope::LocalAndHosts { prefix: current })
}

async fn local_and_host_candidates(current: &str) -> Option<Vec<CompletionCandidate>> {
    let cache_dir = Paths::cache_dir_only().ok()?;
    let (sessions, records) = tokio::join!(
        query_sessions(LOCAL_HOST),
        crate::commands::host::fetch_records(&cache_dir, false)
    );
    let mut candidates = sessions.as_deref().map_or_else(Vec::new, |sessions| {
        session_candidates(sessions, current, None)
    });
    if let Ok(records) = records {
        candidates.extend(
            host_names(&records)
                .into_iter()
                .filter(|host| host != LOCAL_HOST)
                .map(|host| format!("{host}/"))
                .filter(|target| target.starts_with(current))
                .map(|target| CompletionCandidate::new(target).help(Some("host".into()))),
        );
    }
    Some(candidates)
}

async fn query_sessions(host: &str) -> Option<Vec<SessionInfo>> {
    if !valid_host(host) {
        return None;
    }
    let paths = Paths::resolve().ok()?;
    let options = ClientOptions::default()
        .with_connect_timeout(COMPLETION_DEADLINE)
        .with_request_timeout(COMPLETION_DEADLINE);
    let mut client = if host == LOCAL_HOST {
        Client::connect_local_with_options(&paths.socket, options)
            .await
            .ok()?
    } else {
        let records = crate::commands::host::fetch_records(&paths.cache_dir, false)
            .await
            .ok()?;
        let record = records.iter().find(|record| {
            record.name.as_deref() == Some(host)
                && matches!(record.class, HostClass::ReachableDaemon { .. })
        })?;
        let ip: IpAddr = record.netbird_ip.as_deref()?.parse().ok()?;
        let port = netbird::remote_port().ok()?;
        Client::connect_tcp_addr_with_options(host, SocketAddr::new(ip, port), options)
            .await
            .ok()?
    };
    client
        .call::<protocol::method::SessionList>(SessionListParams::default())
        .await
        .ok()
}

fn host_names(records: &[HostRecord]) -> Vec<String> {
    let mut hosts = vec![LOCAL_HOST.to_owned()];
    hosts.extend(records.iter().filter_map(|record| {
        matches!(record.class, HostClass::ReachableDaemon { .. })
            .then(|| record.name.as_deref())
            .flatten()
            .filter(|name| valid_host(name))
            .map(str::to_owned)
    }));
    hosts.sort();
    hosts.dedup();
    hosts
}

fn session_candidates(
    sessions: &[SessionInfo],
    current: &str,
    host_prefix: Option<&str>,
) -> Vec<CompletionCandidate> {
    let mut candidates: Vec<_> = sessions
        .iter()
        .filter_map(|session| {
            session_candidate(
                &session.id.0,
                &session.agent,
                session.name.as_deref(),
                current,
                host_prefix,
            )
        })
        .collect();
    candidates.sort_by(|left, right| left.get_value().cmp(right.get_value()));
    candidates
}

fn session_candidate(
    session_id: &str,
    agent: &str,
    name: Option<&str>,
    current: &str,
    host_prefix: Option<&str>,
) -> Option<CompletionCandidate> {
    if !valid_session_id(session_id) || !session_id.starts_with(current) {
        return None;
    }
    let value = host_prefix.map_or_else(
        || session_id.to_owned(),
        |host| format!("{host}/{session_id}"),
    );
    let agent: String = agent
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    let help = name
        .filter(|name| !name.chars().any(char::is_control))
        .map_or_else(|| agent.clone(), |name| format!("{name} ({agent})"));
    Some(CompletionCandidate::new(value).help(Some(help.into())))
}

fn valid_host(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value.chars().any(|character| {
            character.is_control() || character.is_whitespace() || character == '\\'
        })
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
}

fn run_bounded<F, T>(future: F) -> Option<T>
where
    F: Future<Output = Option<T>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime
            .block_on(async { tokio::time::timeout(COMPLETION_DEADLINE, future).await })
            .ok()
            .flatten()
    })
    .join()
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_record(name: &str, class: HostClass) -> HostRecord {
        HostRecord {
            name: Some(name.to_owned()),
            fqdn: Some(format!("{name}.example.test")),
            netbird_ip: Some("100.64.0.2".to_owned()),
            class,
        }
    }

    #[test]
    fn static_scripts_cover_supported_shells() {
        for (shell, marker, host_marker) in [
            (CompletionShell::Bash, "complete", "--host"),
            (CompletionShell::Zsh, "#compdef pohunek", "--host"),
            (CompletionShell::Fish, "complete -c pohunek", "-l host"),
        ] {
            let script = String::from_utf8(render_script(shell, false)).expect("UTF-8 script");
            assert!(script.contains(marker), "missing {marker:?} in {script}");
            assert!(script.contains("session"));
            assert!(script.contains(host_marker));
        }
    }

    #[test]
    fn dynamic_bootstraps_use_private_completion_environment() {
        for shell in [
            CompletionShell::Bash,
            CompletionShell::Zsh,
            CompletionShell::Fish,
        ] {
            let script = String::from_utf8(render_script(shell, true)).expect("UTF-8 script");
            assert!(script.contains(COMPLETE_ENV));
            assert!(script.contains(shell.name()));
            assert!(script.contains("pohunek"));
        }
    }

    #[test]
    fn context_honors_last_global_host_before_cursor() {
        let words = [
            "pohunek",
            "--host=host-a",
            "session",
            "--host",
            "host-b",
            "inspect",
        ]
        .map(OsString::from);
        assert_eq!(
            CompletionContext::from_words(&words),
            CompletionContext {
                explicit_host: Some("host-b".to_owned())
            }
        );
    }

    #[test]
    fn context_rejects_unsafe_host_values() {
        let words = ["pohunek", "--host", "../peer"].map(OsString::from);
        assert_eq!(
            CompletionContext::from_words(&words),
            CompletionContext::default()
        );
    }

    #[test]
    fn qualified_target_overrides_selected_host() {
        let context = CompletionContext {
            explicit_host: Some("host-a".to_owned()),
        };
        assert_eq!(
            target_scope("host-b/s-4", &context),
            Some(TargetScope::Qualified {
                host: "host-b",
                prefix: "s-4"
            })
        );
        assert_eq!(
            target_scope("s-4", &context),
            Some(TargetScope::Selected {
                host: "host-a",
                prefix: "s-4"
            })
        );
    }

    #[test]
    fn session_candidate_preserves_qualified_target_and_sanitizes_help() {
        let candidate = session_candidate(
            "s-42",
            "codex\nignored",
            Some("Fix parser"),
            "s-",
            Some("host-b"),
        )
        .expect("matching candidate");
        assert_eq!(candidate.get_value(), "host-b/s-42");
        assert_eq!(
            candidate.get_help().map(ToString::to_string).as_deref(),
            Some("Fix parser (codexignored)")
        );
        assert!(session_candidate("bad/id", "shell", None, "", None).is_none());
        assert!(session_candidate("s-42", "shell", None, "other", None).is_none());
    }

    #[test]
    fn host_candidates_include_local_and_only_reachable_safe_peers() {
        let records = vec![
            host_record(
                "host-b",
                HostClass::ReachableDaemon {
                    daemon_version: "1.0.0".to_owned(),
                },
            ),
            host_record("host-c", HostClass::Unreachable),
            host_record(
                "bad/name",
                HostClass::ReachableDaemon {
                    daemon_version: "1.0.0".to_owned(),
                },
            ),
        ];
        assert_eq!(host_names(&records), vec!["host-b", "local"]);
    }

    #[test]
    fn completion_paths_follow_shell_conventions() {
        let paths = Paths {
            runtime_dir: PathBuf::from("/runtime/pohunek"),
            socket: PathBuf::from("/runtime/pohunek/control.sock"),
            data_dir: PathBuf::from("/data/pohunek"),
            log_dir: PathBuf::from("/state/pohunek/logs"),
            cache_dir: PathBuf::from("/cache/pohunek"),
            config_home: PathBuf::from("/config"),
            config_dir: PathBuf::from("/config/pohunek"),
        };
        let data_home = Path::new("/data");
        assert_eq!(
            completion_path(&paths, data_home, CompletionShell::Bash),
            PathBuf::from("/data/bash-completion/completions/pohunek")
        );
        assert_eq!(
            completion_path(&paths, data_home, CompletionShell::Zsh),
            PathBuf::from("/data/zsh/site-functions/_pohunek")
        );
        assert_eq!(
            completion_path(&paths, data_home, CompletionShell::Fish),
            PathBuf::from("/config/fish/completions/pohunek.fish")
        );
    }

    #[test]
    fn managed_completion_write_is_idempotent_and_updates_mode() {
        let root = std::env::temp_dir().join(format!(
            "pohunek-completion-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let path = root.join("nested/pohunek");
        write_script(&path, CompletionShell::Bash, false).expect("write static completion");
        let static_script = std::fs::read_to_string(&path).expect("read static completion");
        assert!(static_script.contains("complete"));

        write_script(&path, CompletionShell::Bash, true).expect("replace with dynamic completion");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read dynamic completion"),
            dynamic_bootstrap(CompletionShell::Bash)
        );
        std::fs::remove_dir_all(&root).expect("remove completion fixture");
    }

    #[test]
    fn dynamic_command_marks_host_and_session_targets() {
        let command = dynamic_command(CompletionContext::default());
        command.clone().debug_assert();
        assert!(command
            .get_arguments()
            .find(|arg| arg.get_id() == "host")
            .and_then(|arg| arg.get::<ArgValueCompleter>())
            .is_some());

        let attach = command.find_subcommand("attach").expect("attach command");
        assert!(attach
            .get_arguments()
            .find(|arg| arg.get_id() == "target")
            .and_then(|arg| arg.get::<ArgValueCompleter>())
            .is_some());

        let session = command.find_subcommand("session").expect("session command");
        for subcommand in session.get_subcommands() {
            if let Some(target) = subcommand
                .get_arguments()
                .find(|arg| arg.get_id() == "target")
            {
                assert!(
                    target.get::<ArgValueCompleter>().is_some(),
                    "{} target has no dynamic completer",
                    subcommand.get_name()
                );
            }
        }
    }
}
