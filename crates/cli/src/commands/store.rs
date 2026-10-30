//! `pohunek store rejected` commands: list the metadata-store lines the daemon
//! cannot load and set one aside into the quarantine file.
//!
//! Both are requests to the daemon on this machine's local control socket. The
//! store holds the owner's durable records, so any other effective host is
//! rejected before a connection is made.

// Rust guideline compliant 2026-10-29

use std::fmt::Write as _;
use std::process::ExitCode;

use clap::{Subcommand, ValueHint};
use protocol::{
    method, ErrorClass, ProtocolError, StoreRejectedLine, StoreRejectedListResult,
    StoreRejectedSetAsideParams, StoreRejectedSetAsideResult,
};

use crate::client::Client;
use crate::commands::plugin::{print_output, sanitize};
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::{is_local_host, LOCAL_HOST};

/// Hex characters in a content hash (SHA-256).
const CONTENT_HASH_HEX_CHARS: usize = 64;

/// `pohunek store` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Action {
    /// Inspect the metadata-store lines this daemon cannot load.
    ///
    /// A line no loader accepts stays in the store through every rewrite, and
    /// package removal and selection fail closed while one may pin a package.
    Rejected {
        #[command(subcommand)]
        action: RejectedAction,
    },
}

/// `pohunek store rejected` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum RejectedAction {
    /// List the lines no loader accepts: line number, kind, whether the line
    /// may pin a package, why it is rejected, and its content hash.
    List {
        /// Emit machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Move the lines with this content hash into the quarantine file.
    ///
    /// The quarantine file `metadata.rejected.jsonl` sits next to the store and
    /// keeps each line with an audit record; nothing deletes it. A line that
    /// may belong to a session with a live runtime, or a `resume` line whose
    /// pin cannot be read, is refused unless `--accept-pin-loss` states that
    /// no runtime depends on its package pin.
    SetAside {
        /// Content hash from `pohunek store rejected list`.
        #[arg(value_hint = ValueHint::Other)]
        content_hash: String,
        /// Accept dropping the package pin of a line that may own a live
        /// runtime or a `resume` line whose pin cannot be read.
        #[arg(long)]
        accept_pin_loss: bool,
        /// Emit machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

impl Action {
    /// Whether the subcommand requested `--json` output.
    pub(crate) fn wants_json(&self) -> bool {
        match self {
            Self::Rejected { action } => match action {
                RejectedAction::List { json } | RejectedAction::SetAside { json, .. } => *json,
            },
        }
    }
}

/// Runs a `store` subcommand against the daemon on this machine.
///
/// # Errors
///
/// A remote effective host, an invalid content hash, or a daemon error.
pub(crate) async fn run(action: Action, host: &str) -> Result<ExitCode, CliError> {
    if !is_local_host(host) {
        return Err(local_only(host));
    }
    let Action::Rejected { action } = action;
    match action {
        RejectedAction::List { json } => {
            let mut client = connect().await?;
            let result = client
                .call::<method::StoreRejectedList>(())
                .await
                .map_err(missing_method_hint)?;
            print_output(json, &result, || render_list(&result))?;
        }
        RejectedAction::SetAside {
            content_hash,
            accept_pin_loss,
            json,
        } => {
            validate_hash(&content_hash)?;
            let mut client = connect().await?;
            let result = client
                .call::<method::StoreRejectedSetAside>(StoreRejectedSetAsideParams {
                    content_hash,
                    accept_pin_loss,
                })
                .await
                .map_err(missing_method_hint)?;
            print_output(json, &result, || render_set_aside(&result))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn connect() -> Result<Client, CliError> {
    let paths = Paths::resolve()?;
    Client::connect(LOCAL_HOST, &paths).await
}

fn local_only(host: &str) -> CliError {
    CliError::Protocol(ProtocolError::new(
        ErrorClass::Configuration,
        "store_local_only",
        format!("store commands manage the daemon on this machine only; --host {host:?} is not supported"),
        Some("run the command on the machine that runs the daemon, without --host".to_owned()),
    ))
}

/// A content hash is the lowercase hex SHA-256 `list` prints.
fn validate_hash(hash: &str) -> Result<(), CliError> {
    let valid = hash.len() == CONTENT_HASH_HEX_CHARS
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if valid {
        Ok(())
    } else {
        Err(CliError::Protocol(ProtocolError::bad_request(
            "the content hash is 64 lowercase hex characters, as `pohunek store rejected list` prints it",
        )))
    }
}

/// Names the upgrade when the daemon predates the `store` methods.
fn missing_method_hint(error: CliError) -> CliError {
    match error {
        CliError::Protocol(mut source) if source.code == "method_not_found" => {
            source.recover = Some(MISSING_METHOD_HINT.to_owned());
            CliError::Protocol(source)
        }
        other => other,
    }
}

/// Recovery hint for a daemon that does not know the `store` methods.
const MISSING_METHOD_HINT: &str = "this daemon predates `store rejected`; upgrade it, or stop the daemon and move the line out of metadata.jsonl into a backup file as the runtime package recovery runbook describes";

fn render_list(result: &StoreRejectedListResult) -> String {
    if result.lines.is_empty() {
        return "The metadata store holds no line this build cannot load.\n".to_owned();
    }
    let mut out = String::new();
    for line in &result.lines {
        render_line(&mut out, line);
    }
    if usize::try_from(result.total).is_ok_and(|total| total > result.lines.len()) {
        let _ = writeln!(
            out,
            "{} more line(s) are not listed; set the listed ones aside and list again.",
            usize::try_from(result.total).unwrap_or(usize::MAX) - result.lines.len()
        );
    }
    out
}

fn render_line(out: &mut String, line: &StoreRejectedLine) {
    let yes_no = |flag: bool| if flag { "yes" } else { "no" };
    let _ = writeln!(
        out,
        "line {}: kind={} may_pin={} may_own_runtime={}",
        line.line,
        sanitize(line.kind.as_deref().unwrap_or("-")),
        yes_no(line.may_pin),
        yes_no(line.may_own_runtime),
    );
    let _ = writeln!(out, "  hash:   {}", line.content_hash);
    let _ = writeln!(out, "  reason: {}", sanitize(&line.reason));
}

fn render_set_aside(result: &StoreRejectedSetAsideResult) -> String {
    let numbers = result
        .lines
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Set aside {} line(s) (was line {numbers}) into {} next to the store.\n",
        result.lines.len(),
        result.quarantine_file
    )
}
