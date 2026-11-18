//! Kit `detect-check`: replay a committed screen corpus through the detector.
//!
//! `detect-check --dir <package> --corpus <dir>` reads `cases.toml` from the
//! corpus directory and replays every case through the [`Detector`] built for
//! the package's own manifest, the way the daemon classifies screens at
//! runtime. The corpus format is the one the Muse stand-in kit established:
//! `[[case]]` tables name a screen file, a width, and the expected
//! [`AgentActivity`] name.
//!
//! The corpus directory holds:
//!
//! ```text
//! <corpus>/
//!   cases.toml   # [[case]] screen = "<file>" width = <cols> expect = "<activity>"
//!   screens/     # committed screen files, read as raw bytes
//! ```
//!
//! The `expect` names are the [`AgentActivity`] wire names: `working`,
//! `blocked`, `idle`, `unknown`.

use std::path::PathBuf;
use std::time::Instant;

use package::Limits;
use pohunek_daemon::agent::host::definition_from_archive;
use pohunek_daemon::detect::{Detector, DetectorConfig};
use protocol::AgentActivity;

use crate::archive::build_bytes;
use crate::error::{refuse, Error};
use crate::files::read_input;

/// Largest accepted `cases.toml`: a case is two lines; 64 KiB leaves room
/// for hundreds of cases without admitting an arbitrary file to the parser.
const MAX_CASES_BYTES: u64 = 64 * 1024;

/// Largest accepted screen file: a screen is a few kilobytes of terminal
/// bytes; 1 MiB leaves room for tall screens without admitting an arbitrary
/// file to the feed.
const MAX_SCREEN_BYTES: u64 = 1024 * 1024;

/// Corpus height in rows: committed screens carry no size, so every case
/// replays at a fixed tall height the rows never clip.
const CORPUS_ROWS: u16 = 50;

/// The inputs of a detect-check.
#[derive(Debug)]
pub struct DetectCheckArgs {
    /// Package directory whose manifest replays the corpus.
    pub dir: PathBuf,
    /// Corpus directory holding `cases.toml` and `screens/`.
    pub corpus: PathBuf,
}

/// One case of the corpus.
struct Case {
    /// Screen file name under `<corpus>/screens/`.
    screen: String,
    /// Terminal width in columns the case replays at.
    width: u16,
    /// Expected activity name.
    expect: String,
}

/// What a passing detect-check reports.
#[derive(Debug, serde::Serialize)]
pub struct DetectCheckReport {
    /// Package id whose manifest replayed the corpus.
    pub package_id: protocol::PackageId,
    /// Cases replayed.
    pub cases: usize,
}

impl DetectCheckReport {
    /// The single stdout line of a passing detect-check:
    /// `plugin detect-check ok: <package_id> <n> cases`.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "plugin detect-check ok: {} {} cases",
            self.package_id, self.cases
        )
    }
}

/// The corpus table: `[[case]] screen/width/expect`.
#[derive(Debug, serde::Deserialize)]
struct Corpus {
    #[serde(default)]
    case: Vec<CaseFile>,
}

/// One `[[case]]` table of `cases.toml`.
#[derive(Debug, serde::Deserialize)]
struct CaseFile {
    screen: String,
    width: u16,
    expect: String,
}

/// Replays the corpus through the package's own manifest: every case must
/// classify to its expected activity.
pub fn detect_check(args: &DetectCheckArgs) -> Result<DetectCheckReport, Error> {
    let bytes = build_bytes(&args.dir, &Limits::DEFAULT)?;
    let verified = package::read_archive(&bytes, &Limits::DEFAULT).map_err(Error::Package)?;
    let definition =
        definition_from_archive(verified.entries(), verified.digest()).map_err(|rejection| {
            Error::Usage(format!(
                "package detect-check failed: the definition is invalid ({rejection})"
            ))
        })?;
    let config = DetectorConfig::for_definition(&definition);

    let cases_path = args.corpus.join("cases.toml");
    let cases_bytes = read_input(&cases_path, MAX_CASES_BYTES)?;
    let cases_text = std::str::from_utf8(&cases_bytes)
        .map_err(|_cause| refuse("the corpus `cases.toml` is not UTF-8"))?;
    let corpus: Corpus = toml::from_str(cases_text)
        .map_err(|error| refuse(format!("the corpus `cases.toml` is invalid: {error}")))?;
    if corpus.case.is_empty() {
        return Err(refuse("the corpus names no cases"));
    }
    let mut cases: Vec<Case> = Vec::with_capacity(corpus.case.len());
    for (index, file) in corpus.case.into_iter().enumerate() {
        expect_name(&file.expect).ok_or_else(|| {
            refuse(format!(
                "case {index}: expect is not a known activity (working, blocked, idle, unknown)"
            ))
        })?;
        if file.width == 0 {
            return Err(refuse(format!("case {index}: width must be positive")));
        }
        if file.screen.is_empty()
            || file.screen.contains('/')
            || file.screen.contains('\\')
            || file.screen == "."
            || file.screen == ".."
        {
            return Err(refuse(format!(
                "case {index}: screen must be a file name under `screens/`"
            )));
        }
        cases.push(Case {
            screen: file.screen,
            width: file.width,
            expect: file.expect,
        });
    }

    let screens = args.corpus.join("screens");
    for (index, case) in cases.iter().enumerate() {
        let screen_bytes = read_input(&screens.join(&case.screen), MAX_SCREEN_BYTES)
            .map_err(|error| refuse(format!("case {index} (`{}`): {error}", case.screen)))?;
        let expected = expect_name(&case.expect).expect("expect names are validated above");
        let observed = replay(&config, case.width, &screen_bytes);
        if observed != expected {
            return Err(refuse(format!(
                "case {index} (`{}` at width {}): expected {}, observed {}",
                case.screen,
                case.width,
                case.expect,
                activity_name(observed),
            )));
        }
    }
    let protocol::BindingProvenance::Package { package, .. } = &definition.binding().provenance
    else {
        return Err(refuse("the descriptor parsed to a built-in binding"));
    };
    let package_id = package.id.clone();
    Ok(DetectCheckReport {
        package_id,
        cases: cases.len(),
    })
}

/// Replays one screen through a fresh detector for the package's manifest and
/// returns the last activity the transitions report.
///
/// No sleeps: the replay sets `startup_grace` to zero — a committed screen
/// replays a settled prompt, not a live start — and steps the clock directly
/// through the debounce windows instead. The steps clear the default
/// `recheck_after` spacing (100 ms) and `confirmations` count (3), so a
/// settled screen publishes by the last step.
fn replay(config: &DetectorConfig, width: u16, screen: &[u8]) -> AgentActivity {
    let mut replay = config.clone();
    replay.detection.startup_grace = std::time::Duration::ZERO;
    let started = Instant::now();
    let mut detector = Detector::new(CORPUS_ROWS, width, started, replay);
    let mut activity = AgentActivity::Unknown;
    for transition in detector.feed(started, screen) {
        activity = transition.activity;
    }
    for millis in [100, 200, 300, 500, 1_000] {
        for transition in detector.tick(started + std::time::Duration::from_millis(millis)) {
            activity = transition.activity;
        }
    }
    activity
}

/// The [`AgentActivity`] an `expect` name denotes, if it names one.
fn expect_name(name: &str) -> Option<AgentActivity> {
    match name {
        "working" => Some(AgentActivity::Working),
        "blocked" => Some(AgentActivity::Blocked),
        "idle" => Some(AgentActivity::Idle),
        "unknown" => Some(AgentActivity::Unknown),
        _ => None,
    }
}

/// The `expect` name of an observed activity.
fn activity_name(activity: AgentActivity) -> &'static str {
    match activity {
        AgentActivity::Working => "working",
        AgentActivity::Blocked => "blocked",
        AgentActivity::Idle => "idle",
        AgentActivity::Unknown => "unknown",
    }
}
