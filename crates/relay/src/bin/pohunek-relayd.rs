//! Starts the Pohunek team relay.

#![forbid(unsafe_code)]
#![forbid(clippy::disallowed_types, clippy::disallowed_methods)]

use clap::{value_parser, Arg, ArgMatches, Command};
use pohunek_relay::{
    config::Config,
    operator::{execute, Action},
    runtime::{init_logging, run, shutdown_signal},
};
use std::{path::PathBuf, process::ExitCode};

fn command() -> Command {
    Command::new("pohunek-relayd")
        .about("Runs and locally administers a Pohunek team relay")
        .arg(
            Arg::new("config")
                .long("config")
                .required(true)
                .value_parser(value_parser!(PathBuf))
                .help("Owner-private relay TOML configuration path"),
        )
        .subcommand(Command::new("serve").about("Start the fenced HTTPS relay"))
        .subcommand(Command::new("migrate").about("Apply migrations while the relay is stopped"))
        .subcommand(
            Command::new("bootstrap")
                .about("Create only the first infrastructure administrator")
                .arg(
                    Arg::new("identity-file")
                        .long("identity-file")
                        .required(true)
                        .value_parser(value_parser!(PathBuf))
                        .help(
                            "Owner-private JSON containing the administrator's stable OIDC subject",
                        ),
                ),
        )
        .subcommand(
            Command::new("recovery")
                .subcommand_required(true)
                .about("Review and recover stopped relay authority")
                .subcommand(
                    Command::new("manifest")
                        .about("Print the current authority manifest and digest for review"),
                )
                .subcommand(
                    Command::new("advance")
                        .about("Advance the independent witness before restoring PostgreSQL")
                        .arg(reviewed_digest()),
                )
                .subcommand(
                    Command::new("quarantine")
                        .about("Invalidate restored authority against the advanced witness"),
                )
                .subcommand(
                    Command::new("reopen")
                        .about("Reopen after reviewing the exact quarantine manifest")
                        .arg(reviewed_digest()),
                )
                .subcommand(
                    Command::new("resume-reopen")
                        .about("Complete an already committed reviewed reopen after a crash"),
                ),
        )
}

fn reviewed_digest() -> Arg {
    Arg::new("reviewed-digest")
        .long("reviewed-digest")
        .required(true)
        .help("Exact lower-case SHA-256 printed by recovery manifest after operator review")
}

fn local_action(args: &ArgMatches) -> Option<Action> {
    Some(match args.subcommand() {
        None | Some(("serve", _)) => return None,
        Some(("migrate", _)) => Action::Migrate,
        Some(("bootstrap", args)) => Action::Bootstrap {
            identity_file: args
                .get_one::<PathBuf>("identity-file")
                .expect("required identity file")
                .clone(),
        },
        Some(("recovery", args)) => match args.subcommand() {
            Some(("manifest", _)) => Action::Manifest,
            Some(("advance", args)) => Action::AdvanceRestore {
                reviewed_digest: args
                    .get_one::<String>("reviewed-digest")
                    .expect("required review digest")
                    .clone(),
            },
            Some(("quarantine", _)) => Action::Quarantine,
            Some(("reopen", args)) => Action::Reopen {
                reviewed_digest: args
                    .get_one::<String>("reviewed-digest")
                    .expect("required review digest")
                    .clone(),
            },
            Some(("resume-reopen", _)) => Action::ResumeReopen,
            _ => unreachable!("clap validates recovery subcommands"),
        },
        _ => unreachable!("clap validates relay subcommands"),
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = command().get_matches();
    let config_path = args
        .get_one::<PathBuf>("config")
        .expect("required configuration path");
    let config = match Config::load(config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = init_logging(&config) {
        eprintln!("{error}");
        return ExitCode::FAILURE;
    }
    if let Some(action) = local_action(&args) {
        return match execute(&config, action).await {
            Ok(result) => match serde_json::to_string_pretty(&result) {
                Ok(output) => {
                    println!("{output}");
                    ExitCode::SUCCESS
                }
                Err(_error) => {
                    eprintln!("local relay result could not be encoded");
                    ExitCode::FAILURE
                }
            },
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        };
    }
    match run(config, shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_commands_require_explicit_identity_or_review_coordinates() {
        let base = ["pohunek-relayd", "--config", "/private/relay.toml"];
        let args = command()
            .try_get_matches_from(base)
            .expect("default server");
        assert!(local_action(&args).is_none());
        for tail in [
            vec!["bootstrap"],
            vec!["recovery", "advance"],
            vec!["recovery", "reopen"],
        ] {
            command()
                .try_get_matches_from(base.into_iter().chain(tail))
                .expect_err("missing local approval coordinate");
        }
        let args = command()
            .try_get_matches_from(base.into_iter().chain([
                "bootstrap",
                "--identity-file",
                "/private/admin.json",
            ]))
            .expect("explicit bootstrap identity");
        assert!(matches!(
            local_action(&args),
            Some(Action::Bootstrap { .. })
        ));
        let digest = "ab".repeat(32);
        let args = command()
            .try_get_matches_from(base.into_iter().chain([
                "recovery",
                "reopen",
                "--reviewed-digest",
                &digest,
            ]))
            .expect("explicit reviewed reopen");
        assert!(matches!(local_action(&args), Some(Action::Reopen { .. })));
    }
}
