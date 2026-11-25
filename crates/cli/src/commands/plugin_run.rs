//! `pohunek plugin run <package> <setup> [args...]`: runs one setup entry of an
//! installed package on the owner's terminal.
//!
//! The daemon resolves the entry to a verified program path and never runs it.
//! This module checks the terminal and the arguments, builds the environment the
//! program receives and starts the program with inherited stdio. Arguments after
//! the setup name belong to the owner: they follow the entry's fixed arguments
//! literally and are never parsed as a shell string.

// Rust guideline compliant 2026-11-23

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::IsTerminal as _;
use std::os::unix::process::ExitStatusExt as _;
use std::process::{Command, ExitCode, ExitStatus, Stdio};

use clap::Args;
use package::credential::credential_name_problem;
use protocol::{
    CommandName, HostInspectParams, PackageId, PackageInfo, SetupInfo, SetupResolveParams,
    SetupResolveResult,
};

use crate::client::{map_client_error, Client};
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::LOCAL_HOST;

/// Variable whose presence marks a launch from a pohunek session.
///
/// A session can read its own screen, so a token paste must never run inside
/// one. The check guards against accidents only: a same-user process can unset
/// the variable.
const SESSION_ID_ENV: &str = "POHUNEK_SESSION_ID";

/// Most arguments the owner may pass to a setup entry.
///
/// Bounds the argv a setup program receives; the sign-in flows of the packages
/// that ship setup entries take a handful of words.
const MAX_SETUP_ARGS: usize = 16;

/// Longest single owner argument, in bytes.
///
/// Bounds the argv a setup program receives; no setup argument is a document.
const MAX_SETUP_ARG_BYTES: usize = 256;

/// Variables of the caller's environment the setup program inherits by name.
///
/// The environment is an allowlist, not an inherited copy: the program must not
/// see a credential the owner exports for another tool. `DBUS_SESSION_BUS_ADDRESS`
/// and `BROWSER` let `claude setup-token` open the owner's browser.
const INHERITED_ENV: [&str; 13] = [
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "LANG",
    "TZ",
    "TMPDIR",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "BROWSER",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// Name prefixes of inherited variables: every locale and XDG base variable.
const INHERITED_ENV_PREFIXES: [&str; 2] = ["LC_", "XDG_"];

/// Prefix of every variable pohunek itself sets for a daemon or a session.
///
/// Never inherited: a setup program learns about its package only through the
/// `POHUNEK_SETUP_*` variables set below.
const POHUNEK_ENV_PREFIX: &str = "POHUNEK_";

/// Variable naming the package id the setup entry belongs to.
const SETUP_PACKAGE_ENV: &str = "POHUNEK_SETUP_PACKAGE";

/// Variable naming the verified package root the program lives in.
const SETUP_PACKAGE_ROOT_ENV: &str = "POHUNEK_SETUP_PACKAGE_ROOT";

/// Variable carrying the digest of the verified package version.
const SETUP_DIGEST_ENV: &str = "POHUNEK_SETUP_DIGEST";

/// Base added to a signal number for the exit status of a signalled program.
///
/// The shell convention `128 + n`, so a script sees the same status it would
/// see from the shell that ran the program.
const SIGNAL_EXIT_BASE: i32 = 128;

/// Exit status of a run that ends without a usable status, the generic failure.
const GENERIC_FAILURE_CODE: u8 = 1;

/// Arguments of `pohunek plugin run`.
#[derive(Debug, Clone, Args)]
pub(crate) struct RunArgs {
    /// Installed package id, or a runtime id that exactly one selected package
    /// serves, such as `claude`.
    #[arg(value_name = "PACKAGE")]
    package: String,
    /// Setup entry the package declares; `pohunek plugin inspect` lists them.
    #[arg(value_name = "SETUP")]
    setup: String,
    /// Arguments for the setup program, passed literally after its fixed ones.
    /// Hyphen values are accepted, so `--login` needs no `--` separator.
    #[arg(
        value_name = "ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    args: Vec<String>,
}

/// Why `pohunek plugin run` refused, or failed before the program started.
///
/// The messages are fixed text; none carries an argument, a path or a token.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// The command runs inside a pohunek session.
    #[error("`plugin run` is refused inside a pohunek session")]
    InSession,
    /// Stdin or stderr is not a terminal, so the owner cannot see or type.
    #[error("`plugin run` needs a terminal on stdin and stderr")]
    TerminalRequired,
    /// An owner argument breaks a bound.
    #[error("setup arguments refused: {detail}")]
    ArgumentRefused {
        /// Which bound the argument broke; names the position, never the value.
        detail: String,
    },
    /// The setup name is not a valid name.
    #[error("the setup name is not valid")]
    SetupNameInvalid,
    /// No installed package has the id, and no selected package serves the runtime.
    #[error("no installed package has that id and no selected package serves that runtime")]
    PackageNotFound,
    /// Several selected packages serve the runtime.
    #[error("several selected packages serve that runtime")]
    RuntimeAmbiguous,
    /// The daemon predates `package.setup_resolve` or its capability.
    #[error("the local daemon is too old for `plugin run`")]
    SetupUnsupported,
}

impl Error {
    /// Stable wire error code.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InSession => "setup_in_session",
            Self::TerminalRequired => "setup_tty_required",
            Self::ArgumentRefused { .. } => "setup_arguments_refused",
            Self::SetupNameInvalid => "setup_name_invalid",
            Self::PackageNotFound => "setup_package_not_found",
            Self::RuntimeAmbiguous => "setup_runtime_ambiguous",
            Self::SetupUnsupported => "setup_unsupported",
        }
    }

    /// The next step for the owner, fixed text.
    pub(crate) fn hint(&self) -> &'static str {
        match self {
            Self::InSession => "run the command from a plain terminal outside any session",
            Self::TerminalRequired => {
                "run the command directly in a terminal, not from a script or a pipe"
            }
            Self::ArgumentRefused { .. } => {
                "pass at most 16 arguments of at most 256 bytes, without control characters"
            }
            Self::SetupNameInvalid => "check the setup name with `pohunek plugin inspect`",
            Self::PackageNotFound => {
                "install the package, or name the package id from `pohunek plugin list`"
            }
            Self::RuntimeAmbiguous => {
                "name the package id from `pohunek plugin list` instead of the runtime id"
            }
            Self::SetupUnsupported => "upgrade the daemon to a release that supports setup entries",
        }
    }

    /// Whether the failure is a mistake in how the command was invoked.
    pub(crate) fn is_usage(&self) -> bool {
        matches!(
            self,
            Self::ArgumentRefused { .. }
                | Self::SetupNameInvalid
                | Self::PackageNotFound
                | Self::RuntimeAmbiguous
        )
    }
}

/// Run `pohunek plugin run`: resolve the entry, then run its program on this
/// terminal and return the program's exit status.
///
/// The refusals that need no daemon run first, so a refused command connects to
/// nothing and starts nothing.
///
/// # Errors
///
/// Returns [`CliError`] when a refusal applies, the daemon cannot resolve the
/// entry, or the program cannot be started.
pub(crate) async fn run(args: &RunArgs) -> Result<ExitCode, CliError> {
    if std::env::var_os(SESSION_ID_ENV).is_some() {
        return Err(Error::InSession.into());
    }
    checked_arguments(&args.args)?;
    let setup = CommandName::parse(&args.setup).map_err(|_invalid| Error::SetupNameInvalid)?;
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return Err(Error::TerminalRequired.into());
    }

    let paths = Paths::resolve()?;
    let mut client = Client::connect(LOCAL_HOST, &paths).await?;
    let capabilities = client
        .host_inspect_with(HostInspectParams {
            skip_runtime_probes: Some(true),
        })
        .await?;
    if !capabilities.package_setup_supported {
        return Err(Error::SetupUnsupported.into());
    }
    let listed = crate::commands::plugin::list(&mut client).await?;
    let package = resolve_package(&listed.packages, &args.package)?;
    let resolved = client
        .into_sdk()
        .package_setup_resolve(SetupResolveParams {
            package,
            setup: setup.clone(),
        })
        .await
        .map_err(map_client_error)?;

    let mut command = Command::new(&resolved.program);
    command
        .args(&resolved.args)
        .args(&args.args)
        .env_clear()
        .envs(setup_environment(&resolved))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = run_until_exit(command).await?;
    let code = status_code(status);
    tracing::info!(
        package = %resolved.package.id,
        setup = %setup,
        arguments = args.args.len(),
        exit_code = code,
        "setup program finished"
    );
    Ok(ExitCode::from(code))
}

/// Runs `command` and waits for it, surviving the terminal's interrupt and quit
/// signals.
///
/// The program shares the terminal's foreground process group, so Ctrl-C and
/// Ctrl-\ reach it directly. This process installs handlers for both before the
/// program starts and drops what they receive, so it neither dies first nor
/// leaves the program running outside job control: the program restores the
/// terminal in its own cleanup and its exit status becomes this command's.
/// Handlers are reset to the default action by `exec`, so the program keeps the
/// disposition it would have had without this command.
async fn run_until_exit(mut command: Command) -> Result<ExitStatus, std::io::Error> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut quit = signal(SignalKind::quit())?;
    let mut wait = tokio::task::spawn_blocking(move || command.status());
    loop {
        tokio::select! {
            joined = &mut wait => return joined.map_err(std::io::Error::other)?,
            _ = interrupt.recv() => {}
            _ = quit.recv() => {}
        }
    }
}

/// Refuses an owner argument list that breaks a bound.
///
/// The detail names the position of the offending argument, never its value,
/// because an owner may paste a token as an argument.
fn checked_arguments(args: &[String]) -> Result<(), Error> {
    if args.len() > MAX_SETUP_ARGS {
        return Err(Error::ArgumentRefused {
            detail: format!("more than {MAX_SETUP_ARGS} arguments were given"),
        });
    }
    for (index, arg) in args.iter().enumerate() {
        let position = index + 1;
        if arg.len() > MAX_SETUP_ARG_BYTES {
            return Err(Error::ArgumentRefused {
                detail: format!("argument {position} is longer than {MAX_SETUP_ARG_BYTES} bytes"),
            });
        }
        if arg.chars().any(char::is_control) {
            return Err(Error::ArgumentRefused {
                detail: format!("argument {position} contains a control character"),
            });
        }
    }
    Ok(())
}

/// Resolves the package argument to the package id the daemon is asked about.
///
/// An installed id wins. Otherwise a runtime id names the one selected package
/// that serves it; zero or several selected packages are a usage error.
fn resolve_package(packages: &[PackageInfo], requested: &str) -> Result<PackageId, Error> {
    if let Some(info) = packages
        .iter()
        .find(|info| info.package.id.as_str() == requested)
    {
        return Ok(info.package.id.clone());
    }
    let served: Vec<&PackageId> = packages
        .iter()
        .filter(|info| {
            info.selected
                && info
                    .runtime_id
                    .as_ref()
                    .is_some_and(|runtime| runtime.as_str() == requested)
        })
        .map(|info| &info.package.id)
        .collect();
    match served.as_slice() {
        [only] => Ok((*only).clone()),
        [] => Err(Error::PackageNotFound),
        _ => Err(Error::RuntimeAmbiguous),
    }
}

/// The environment the setup program runs with: the allowlisted variables of
/// this process, then the three `POHUNEK_SETUP_*` variables of the verified
/// package version.
fn setup_environment(resolved: &SetupResolveResult) -> Vec<(OsString, OsString)> {
    let mut environment: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(name, _value)| inherited_name(name))
        .collect();
    environment.push((
        OsString::from(SETUP_PACKAGE_ENV),
        OsString::from(resolved.package.id.as_str()),
    ));
    environment.push((
        OsString::from(SETUP_PACKAGE_ROOT_ENV),
        OsString::from(&resolved.root),
    ));
    environment.push((
        OsString::from(SETUP_DIGEST_ENV),
        OsString::from(resolved.digest.as_str()),
    ));
    environment
}

/// Whether a variable of this process is passed to the setup program.
///
/// A name that is not UTF-8 is never on the allowlist, and a credential-shaped
/// name is dropped even when its prefix is allowed.
fn inherited_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if name.starts_with(POHUNEK_ENV_PREFIX) || credential_name_problem(name).is_some() {
        return false;
    }
    INHERITED_ENV.contains(&name)
        || INHERITED_ENV_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// The exit status a finished setup program hands back to the shell: its own
/// code, or `128 + n` when signal `n` ended it.
fn status_code(status: ExitStatus) -> u8 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| SIGNAL_EXIT_BASE + signal))
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(GENERIC_FAILURE_CODE)
}

/// Lines describing the setup entries of a package for the install review and
/// `plugin inspect`.
///
/// Program, arguments and description are printed in debug form, so a control
/// character or a bidirectional override in a descriptor is shown escaped.
pub(crate) fn render_setup_details(output: &mut String, setup: &[SetupInfo]) {
    for entry in setup {
        let _ = writeln!(output, "Setup:       {}", entry.name);
        let _ = writeln!(output, "  program:     {:?}", entry.program);
        let _ = writeln!(output, "  arguments:   {:?}", entry.args);
        let _ = writeln!(output, "  description: {:?}", entry.description);
    }
}
