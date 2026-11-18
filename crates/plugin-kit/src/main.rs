//! `pohunek-plugin-kit`: owner-side plugin kit for plugin repositories.
//!
//! Every subcommand takes `--dir <package directory>` and reports through
//! stdout lines a CI log greps for, with `--json` switching the report to a
//! single JSON document. The process exit status is 0 for a pass, 1 for a
//! failed proof or a refused index write, and 2 for every other refusal.
//!
//! A failed `prove` or `smoke` that ran on an isolation root names it on
//! stderr as `isolation root: <path>`: the kit has removed the root unless
//! the failure is the removal itself. A refusal before the root exists, or
//! the failure to write the evidence after the run, names no root.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use plugin_kit::{build, prove, smoke, verify, Error};
#[cfg(feature = "daemon")]
use plugin_kit::{detect_check, lint};

/// Owner-side plugin kit for plugin repositories.
#[derive(Debug, Parser)]
#[command(name = "pohunek-plugin-kit", version)]
struct Cli {
    /// Subcommand to run.
    #[command(subcommand)]
    command: Command,
}

/// One kit operation over one package directory.
#[derive(Debug, Subcommand)]
enum Command {
    /// Load the descriptor and the detection manifest through the same
    /// parser an install runs.
    Lint {
        /// Package directory holding `runtime.toml`.
        #[arg(long)]
        dir: PathBuf,
        /// Print the report as a single JSON document.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Build the canonical archive of the package directory.
    Build {
        /// Package directory to pack.
        #[arg(long)]
        dir: PathBuf,
        /// Directory the `<package_id>-<version>.tar.zst` archive goes to.
        #[arg(long)]
        output_dir: PathBuf,
        /// Print the report as a single JSON document.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Check the archive matches the directory, twice built, and the digest.
    Verify {
        /// Package directory the archive must rebuild from. Without it,
        /// the check only re-reads the archive with the default limits.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Archive file to check.
        #[arg(long)]
        archive: PathBuf,
        /// Expected `sha256:<64 hex>` digest of the archive bytes. Without
        /// it, the digest is reported, not compared.
        #[arg(long)]
        sha256: Option<String>,
        /// Print the report as a single JSON document.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Replay every committed screen of the corpus through the detector.
    DetectCheck {
        /// Package directory whose manifest replays the corpus.
        #[arg(long)]
        dir: PathBuf,
        /// Corpus directory holding `cases.toml` and `screens/`.
        #[arg(long)]
        corpus: PathBuf,
        /// Print the report as a single JSON document.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Start the built runtime on an isolated daemon through a throwaway
    /// index, wait for detection past `unknown`, and require a non-empty
    /// screen. Stops after the screen check: no full agent run.
    Smoke {
        /// Package directory to build and smoke.
        #[arg(long)]
        dir: PathBuf,
        /// Release tree the isolated daemon copies its executables from.
        #[arg(long)]
        release_dir: PathBuf,
        /// Upstream directory (or executable inside one) put in front of
        /// the isolated daemon's `PATH`, so the runtime program resolves to
        /// the locked upstream install there.
        #[arg(long)]
        upstream: Option<PathBuf>,
        /// Smoke script: a plugin-owned executable run once inside the
        /// live isolated root after every generic smoke check passed.
        /// Without it, the smoke stops after the screen check. A bare
        /// `pohunek` in the script reaches the isolated daemon through
        /// the isolated `XDG_RUNTIME_DIR`; the script's stdout and stderr
        /// both go to the kit's stderr.
        #[arg(long)]
        script: Option<PathBuf>,
        /// Print the report as a single JSON document.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Prove the package: build from the directory, then run the install
    /// path on an isolated daemon through a throwaway index. The runtime
    /// program is never launched. With `--archive` and `--entry`, prove
    /// the admitted release asset against its real entry instead (#1618).
    Prove {
        /// Package directory to build and prove.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Package archive to prove against its entry.
        #[arg(long, requires = "entry")]
        archive: Option<PathBuf>,
        /// Index entry file that lists exactly the archive.
        #[arg(long, requires = "archive")]
        entry: Option<PathBuf>,
        /// Release tree the isolated daemon copies its executables from.
        #[arg(long)]
        release_dir: PathBuf,
        /// Evidence JSON file to create exclusively.
        #[arg(long)]
        evidence: PathBuf,
        /// Install the package disabled, so the inspect must refuse it.
        #[arg(long, default_value_t = false)]
        no_enable: bool,
        /// Print the report as a single JSON document.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let cli = Cli::parse();
    match dispatch(cli.command) {
        Ok(()) => 0,
        Err(error) => {
            if let Error::Prove(verdict) = &error {
                if let Some(root) = verdict.root() {
                    eprintln!("isolation root: {}", root.display());
                }
            }
            if let Error::Smoke { root, .. } = &error {
                eprintln!("isolation root: {}", root.display());
            }
            eprintln!("pohunek-plugin-kit: {error}");
            error.exit_code()
        }
    }
}

fn dispatch(command: Command) -> Result<(), Error> {
    match command {
        #[cfg(feature = "daemon")]
        Command::Lint { dir, json } => {
            let report = lint::lint(&lint::LintArgs { dir })?;
            emit(json, &report.line(), &report)
        }
        Command::Build {
            dir,
            output_dir,
            json,
        } => {
            let built = build::build(&build::BuildArgs { dir, output_dir })?;
            emit(json, &built.line(), &built)
        }
        Command::Verify {
            dir,
            archive,
            sha256,
            json,
        } => {
            let report = verify::verify(&verify::VerifyArgs {
                dir,
                archive,
                sha256,
            })?;
            emit(json, &report.line(), &report)
        }
        #[cfg(feature = "daemon")]
        Command::DetectCheck { dir, corpus, json } => {
            let report =
                detect_check::detect_check(&detect_check::DetectCheckArgs { dir, corpus })?;
            emit(json, &report.line(), &report)
        }
        #[cfg(not(feature = "daemon"))]
        Command::DetectCheck { .. } | Command::Lint { .. } => {
            unreachable!("daemon-gated subcommands need the daemon feature")
        }
        Command::Smoke {
            dir,
            release_dir,
            upstream,
            script,
            json,
        } => {
            let report = smoke::smoke(&smoke::SmokeArgs {
                dir,
                release_dir,
                upstream,
                script,
            })?;
            emit(json, &report.line(), &report)
        }
        Command::Prove {
            dir,
            archive,
            entry,
            release_dir,
            evidence,
            no_enable,
            json,
        } => {
            // The digest is computed before any isolation root exists: a
            // proof must not pass while its attestation digest is unknown.
            let kit_sha256 = prove::kit_digest()?;
            match (dir, archive, entry) {
                (Some(dir), None, None) => {
                    let report = prove::prove_package(&prove::ProvePackageArgs {
                        dir,
                        release_dir,
                        evidence,
                        no_enable,
                        kit_sha256: Some(kit_sha256),
                    })?;
                    emit(json, &report.line(), &report)
                }
                (None, Some(archive), Some(entry)) => {
                    let report = prove::prove_archive(&prove::ProveArgs {
                        archive,
                        entry,
                        release_dir,
                        evidence,
                        no_enable,
                        pre_install: None,
                        kit_sha256: Some(kit_sha256),
                    })?;
                    emit(
                        json,
                        &report.line,
                        &serde_json::json!({ "line": report.line, "root": report.root }),
                    )
                }
                _ => Err(Error::Usage(
                    "prove needs `--dir`, or `--archive` with `--entry`".to_owned(),
                )),
            }
        }
    }
}

/// Prints the human line, or the JSON document when `json` holds.
fn emit(json: bool, line: &str, report: &impl serde::Serialize) -> Result<(), Error> {
    if json {
        println!("{}", serde_json::to_string(report).map_err(Error::Json)?);
    } else {
        println!("{line}");
    }
    Ok(())
}
