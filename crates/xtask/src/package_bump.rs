//! `cargo xtask package bump`: the version gate of the official packages.
//!
//! The gate takes the verdicts of [`package_baseline::evaluate`] for the
//! release `v<version>` and decides every package:
//!
//! - `fail` with `version_not_bumped` when the archive differs from every
//!   released archive of the runtime and the descriptor version is not
//!   strictly above the released versions (the baseline's
//!   `version_not_bumped`, which also covers a lower version).
//! - `fail` with the catalog reason when the release catalog is present but
//!   does not verify against the anchor now, or names another release or
//!   sequence. An unverifiable catalog must not waive the rule.
//! - `undecided` when the catalog is absent. The CI download leaves the
//!   directory empty when it fails, so an absent catalog reports an
//!   unavailable release side, not a package that passed.
//! - `pass` otherwise: a verified package, a package new to the release, and
//!   a package whose archive equals a released archive.
//!
//! The overall verdict is `fail` when any package fails, else `undecided` when
//! any package is undecided, else `pass`. The command exits non-zero only for
//! `fail`; an `undecided` run exits 0 and its JSON verdict says so.

// Rust guideline compliant 2026-10-13

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::package_baseline::{self, Options, Outcome, Reason, Verdicts};
use crate::XtaskError;

/// Gate decision for one package or for the whole checkout.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Decision {
    Pass,
    Undecided,
    Fail,
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Undecided => "undecided",
            Self::Fail => "fail",
        }
    }
}

/// Stable code of a decision that is not a pass; JSON spells it as [`Code::as_str`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Code {
    /// The archive changed and the version is not above the released versions.
    VersionNotBumped,
    /// The baseline could not use the release catalog for this reason.
    Catalog(Reason),
}

impl Code {
    fn as_str(self) -> &'static str {
        match self {
            Self::VersionNotBumped => "version_not_bumped",
            Self::Catalog(reason) => reason.code(),
        }
    }
}

impl Serialize for Code {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// The gate decision for one package, as printed by `--json`.
#[derive(Debug, Serialize)]
struct PackageGate {
    verdict: Decision,
    /// Stable code of a fail or undecided verdict; `null` when it passes.
    reason: Option<Code>,
    /// Human explanation, for the report only.
    #[serde(skip)]
    detail: String,
}

/// Every gate decision, as printed by `--json`.
#[derive(Debug, Serialize)]
struct Report {
    /// The release the gate was evaluated against.
    version: String,
    verdict: Decision,
    /// One decision per official package, by directory name.
    packages: BTreeMap<String, PackageGate>,
}

/// Prints the version gate of every official package; fails when one fails.
pub(crate) fn run(options: &Options) -> Result<(), XtaskError> {
    let verdicts = package_baseline::evaluate(options)?;
    let report = report(&verdicts);
    let text = render(&report);
    if options.json {
        let json = serde_json::to_string(&report).map_err(XtaskError::Json)?;
        eprint!("{text}");
        println!("{json}");
    } else {
        print!("{text}");
    }
    let failed = report
        .packages
        .values()
        .filter(|gate| gate.verdict == Decision::Fail)
        .count();
    if failed > 0 {
        return Err(XtaskError::Usage(format!(
            "package bump: {failed} package(s) fail the version gate"
        )));
    }
    Ok(())
}

/// Decides every package of the baseline verdicts.
fn report(verdicts: &Verdicts) -> Report {
    let packages: BTreeMap<String, PackageGate> = verdicts
        .packages
        .iter()
        .map(|(name, outcome)| (name.clone(), package_gate(outcome)))
        .collect();
    let verdict = if packages.values().any(|gate| gate.verdict == Decision::Fail) {
        Decision::Fail
    } else if packages
        .values()
        .any(|gate| gate.verdict == Decision::Undecided)
    {
        Decision::Undecided
    } else {
        Decision::Pass
    };
    Report {
        version: verdicts.version.clone(),
        verdict,
        packages,
    }
}

/// The gate decision for one package outcome of the baseline.
fn package_gate(outcome: &Outcome) -> PackageGate {
    let (verdict, reason) = decide(outcome);
    PackageGate {
        verdict,
        reason,
        detail: if reason.is_some() {
            outcome.detail.clone()
        } else {
            String::new()
        },
    }
}

/// Maps one baseline outcome to a gate decision and its stable code.
fn decide(outcome: &Outcome) -> (Decision, Option<Code>) {
    if outcome.version_not_bumped {
        return (Decision::Fail, Some(Code::VersionNotBumped));
    }
    match outcome.reason {
        Some(Reason::CatalogAbsent) => (
            Decision::Undecided,
            Some(Code::Catalog(Reason::CatalogAbsent)),
        ),
        Some(
            reason @ (Reason::CatalogUnreadable
            | Reason::CatalogUnverified
            | Reason::ReleaseMismatch),
        ) => (Decision::Fail, Some(Code::Catalog(reason))),
        // A verified package, a package new to the release, a changed package
        // whose version is above the released ones, and every reason the
        // baseline finds only after the rebuilt digest matched a catalog
        // entry: none of them breaks the rule. Listed one by one, so a new
        // reason has to be decided here.
        None
        | Some(
            Reason::NotInCatalog
            | Reason::DigestMismatch
            | Reason::ArchiveAbsent
            | Reason::ArchiveUnreadable
            | Reason::ArchiveMismatch
            | Reason::PlatformsUncovered
            | Reason::AttestationAbsent
            | Reason::AttestationUnreadable
            | Reason::AttestationDigestMismatch
            | Reason::AttestationInvalid
            | Reason::AttestationPackageMismatch
            | Reason::AttestationLockMismatch
            | Reason::AttestationSuiteMismatch
            | Reason::ReleaseCommitUnreachable
            | Reason::ChangedSinceRelease,
        ) => (Decision::Pass, None),
    }
}

/// Human report: one line per package with its decision and the reason.
fn render(report: &Report) -> String {
    let mut out = format!(
        "package bump: release v{} {}\n",
        report.version,
        report.verdict.as_str()
    );
    for (name, gate) in &report.packages {
        // Writing to a String cannot fail.
        let _ = match gate.reason {
            None => writeln!(out, "package bump: {name} pass"),
            Some(code) => writeln!(
                out,
                "package bump: {name} {} ({}): {}",
                gate.verdict.as_str(),
                code.as_str(),
                gate.detail
            ),
        };
    }
    out
}
