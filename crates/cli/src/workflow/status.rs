//! `pohunek workflow status`: the state of every job and service of the
//! installed workflow plugins.
//!
//! Each unit reports four things: its native state from the service manager,
//! its last run record, its consecutive failures, and for a service the
//! restart delay its next start waits. Nothing is written and no daemon is
//! contacted. A unit that cannot be inspected is reported as `unknown` with
//! the reason; only a store, configuration or plugin-id failure fails the
//! command.

// Rust guideline compliant 2026-10-13

use std::fmt::Write as _;

use package::registry::PackageRecord;
use pohunek_platform::supervisor::{
    self, PluginUnitKey, PluginUnitKind, PluginUnits, ServiceState,
};
use protocol::{PackageDigest, PackageId, PackageVersion};
use serde::Serialize;

use super::config::{self, Knobs};
use super::runlog::{self, Outcome, ReadRecord};
use super::runner::kind_name;
use super::store::{Store, UnitKind, UnitName};
use super::supervised::{self, Request, DAMAGED_BACKOFF, STABLE_BACKOFF_FAILURES};
use super::units::{connect, installation_at, Host};
use super::Error;

/// Reason a service's backoff file is reported as damaged.
const DAMAGED_BACKOFF_REASON: &str =
    "the restart backoff file cannot be parsed; the next start waits the longest delay";

/// The status of every reported plugin, as `pohunek workflow status --json` prints it.
#[derive(Debug, Serialize)]
pub(crate) struct Report {
    plugins: Vec<PluginStatus>,
}

/// One installed plugin and the status of its units.
#[derive(Debug, Serialize)]
struct PluginStatus {
    id: PackageId,
    version: PackageVersion,
    digest: PackageDigest,
    enabled: bool,
    units: Vec<UnitStatus>,
    /// Why the plugin's unit record could not be read, when it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    units_error: Option<String>,
    /// Why the run log could not be read, when it could not; the units then
    /// report no last run and no consecutive failures.
    #[serde(skip_serializing_if = "Option::is_none")]
    run_log_error: Option<String>,
}

/// One recorded job or service.
#[derive(Debug, Serialize)]
struct UnitStatus {
    kind: UnitKind,
    name: String,
    state: NativeState,
    /// Why the state is `unknown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    state_reason: Option<String>,
    last_run: Option<LastRun>,
    /// Jobs: trailing failed runs. Services: short runs in a row, from the
    /// backoff file. `null` when the count cannot be determined.
    consecutive_failures: Option<u32>,
    /// Services only: the delay the next start waits.
    #[serde(skip_serializing_if = "Option::is_none")]
    backoff_ms: Option<u64>,
    /// Services only: why the backoff could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    backoff_error: Option<String>,
}

/// Native state of a unit, as the service manager reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum NativeState {
    Running,
    Stopped,
    Failed,
    Unknown,
}

/// The newest run record of a unit.
#[derive(Debug, Serialize)]
struct LastRun {
    ts: String,
    outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
}

impl From<&ReadRecord> for LastRun {
    fn from(record: &ReadRecord) -> Self {
        Self {
            ts: record.ts.clone(),
            outcome: record.outcome,
            exit_code: record.exit_code,
            signal: record.signal,
            error_code: record.error_code.clone(),
        }
    }
}

/// Reports the plugin named by `plugin`, or every installed plugin with
/// recorded units when none is named.
///
/// # Errors
///
/// Returns [`Error::PluginUnknown`] when the named plugin is not installed,
/// and the store and configuration errors of reading the knobs and the
/// installed plugins.
pub(crate) async fn report(
    store: &Store,
    host: &mut Host,
    plugin: Option<&PackageId>,
) -> Result<Report, Error> {
    let knobs = config::load(store.knobs_path())?;
    let installed = match plugin {
        Some(id) => vec![store
            .installed(id)?
            .ok_or_else(|| Error::PluginUnknown { id: id.to_string() })?],
        None => store.list()?,
    };
    let mut plugins = Vec::new();
    for record in &installed {
        let status = plugin_status(store, host, &knobs, record).await;
        // Without a named plugin, only plugins that have units are reported.
        if plugin.is_some() || !status.units.is_empty() || status.units_error.is_some() {
            plugins.push(status);
        }
    }
    Ok(Report { plugins })
}

/// The status of one installed plugin and each of its recorded units.
async fn plugin_status(
    store: &Store,
    host: &mut Host,
    knobs: &Knobs,
    record: &PackageRecord,
) -> PluginStatus {
    let id = &record.identity().id;
    let mut status = PluginStatus {
        id: id.clone(),
        version: record.identity().version.clone(),
        digest: record.digest().clone(),
        enabled: record.enabled(),
        units: Vec::new(),
        units_error: None,
        run_log_error: None,
    };
    let names = match store.recorded_units(id) {
        Ok(names) => names,
        Err(error) => {
            status.units_error = Some(error.to_string());
            return status;
        }
    };
    if names.is_empty() {
        return status;
    }
    let runs = match runlog::recent(&store.state_dir(id), knobs.logs) {
        Ok(runs) => Some(runs),
        Err(error) => {
            status.run_log_error = Some(error.to_string());
            None
        }
    };
    let backend = backend(host, id).await;
    for name in &names {
        let unit = UnitContext {
            store,
            knobs,
            record,
            runs: runs.as_deref(),
            backend: backend.as_deref(),
        };
        status.units.push(unit_status(&unit, name).await);
    }
    status
}

/// Connects the service manager backend of plugin `id`, or the reason it
/// cannot be reached.
async fn backend(host: &mut Host, id: &PackageId) -> Result<Box<dyn PluginUnits>, String> {
    let context = host.context().map_err(|error| error.to_string())?;
    let installation = installation_at(context.paths(), context.uid())
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "no pohunek service installation is recorded".to_owned())?;
    connect(context, &installation, id)
        .await
        .map_err(|error| error.to_string())
}

/// What one unit's status is read from: the plugin's installed record, its
/// run log when that could be read, and the service manager backend when
/// it could be reached.
struct UnitContext<'a> {
    store: &'a Store,
    knobs: &'a Knobs,
    record: &'a PackageRecord,
    runs: Option<&'a [ReadRecord]>,
    backend: Result<&'a dyn PluginUnits, &'a String>,
}

/// The status of one recorded unit of the plugin in `unit`'s context.
async fn unit_status(context: &UnitContext<'_>, unit: &UnitName) -> UnitStatus {
    let id = &context.record.identity().id;
    let (state, state_reason) = match (unit.key(id), context.backend) {
        (Err(error), _) => (NativeState::Unknown, Some(error.to_string())),
        (_, Err(reason)) => (NativeState::Unknown, Some(reason.clone())),
        (Ok(key), Ok(backend)) => native_state(backend, &key).await,
    };
    // The unit's runs of the installed archive; an earlier archive's records
    // belong to units that no longer exist.
    let own: Vec<&ReadRecord> = context
        .runs
        .unwrap_or(&[])
        .iter()
        .filter(|run| {
            run.kind == kind_name(unit.kind.platform())
                && run.name.as_deref() == Some(unit.name.as_str())
                && &run.digest == context.record.digest()
        })
        .collect();
    let mut status = UnitStatus {
        kind: unit.kind,
        name: unit.name.clone(),
        state,
        state_reason,
        last_run: own.first().map(|run| LastRun::from(*run)),
        consecutive_failures: None,
        backoff_ms: None,
        backoff_error: None,
    };
    match unit.kind {
        UnitKind::Job => {
            status.consecutive_failures = context.runs.map(|_runs| trailing_failures(&own));
        }
        UnitKind::Service => service_backoff(context, unit, &mut status),
    }
    status
}

/// The state of `key` and, when it is not one of the four reported states,
/// the reason.
async fn native_state(
    backend: &dyn PluginUnits,
    key: &PluginUnitKey,
) -> (NativeState, Option<String>) {
    match backend.state(key).await {
        Ok(ServiceState::Running) => (NativeState::Running, None),
        Ok(ServiceState::Stopped) => (NativeState::Stopped, None),
        Ok(ServiceState::Failed) => (NativeState::Failed, None),
        Ok(other) => (
            NativeState::Unknown,
            Some(format!("the service manager reports the unit as {other:?}")),
        ),
        Err(supervisor::Error::NotFound(_)) => (
            NativeState::Unknown,
            Some("the service manager has no unit of this name".to_owned()),
        ),
        Err(error) => (
            NativeState::Unknown,
            Some(format!(
                "the service manager could not report the state: {error}"
            )),
        ),
    }
}

/// Counts the trailing failed runs of a job, newest first. An overlap skip
/// started no run, so it neither counts nor ends the count; the first success
/// ends it.
fn trailing_failures(runs: &[&ReadRecord]) -> u32 {
    let count = runs
        .iter()
        .filter(|run| run.outcome != Outcome::OverlapSkipped)
        .take_while(|run| run.outcome.is_failure())
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Fills a service's backoff fields from its backoff file, with the delay
/// the trampoline computes for that count.
fn service_backoff(context: &UnitContext<'_>, unit: &UnitName, status: &mut UnitStatus) {
    let id = &context.record.identity().id;
    let request = Request {
        plugin: id.clone(),
        digest: context.record.digest().clone(),
        kind: PluginUnitKind::Service,
        name: unit.name.clone(),
    };
    match supervised::backoff_of(&context.store.state_dir(id), &request) {
        Ok(DAMAGED_BACKOFF) => {
            status.backoff_ms = Some(delay_ms(context.knobs, DAMAGED_BACKOFF));
            status.backoff_error = Some(DAMAGED_BACKOFF_REASON.to_owned());
        }
        Ok(failures) => {
            status.backoff_ms = Some(delay_ms(context.knobs, failures));
            status.consecutive_failures = Some(failures.saturating_sub(STABLE_BACKOFF_FAILURES));
        }
        Err(error) => status.backoff_error = Some(error.to_string()),
    }
}

/// The restart delay after `failures`, in milliseconds, computed by the same
/// function the trampoline waits with.
fn delay_ms(knobs: &Knobs, failures: u32) -> u64 {
    u64::try_from(knobs.services.delay(failures).as_millis()).unwrap_or(u64::MAX)
}

/// Renders the report for a terminal, one line per unit.
pub(crate) fn render(report: &Report) -> String {
    let mut text = String::new();
    for plugin in &report.plugins {
        let enabled = if plugin.enabled {
            "enabled"
        } else {
            "disabled"
        };
        let _ = writeln!(text, "{} {} ({enabled})", plugin.id, plugin.version);
        if let Some(error) = &plugin.units_error {
            let _ = writeln!(text, "  units unreadable: {error}");
        }
        if let Some(error) = &plugin.run_log_error {
            let _ = writeln!(text, "  run log unreadable: {error}");
        }
        for unit in &plugin.units {
            let kind = match unit.kind {
                UnitKind::Job => "job",
                UnitKind::Service => "service",
            };
            let _ = write!(text, "  {kind} {}: {}", unit.name, state_text(unit));
            if let Some(run) = &unit.last_run {
                let _ = write!(text, "; last run {} {:?}", run.ts, run.outcome);
                if let Some(code) = run.exit_code {
                    let _ = write!(text, " (exit {code})");
                }
            }
            match unit.consecutive_failures {
                Some(count) => {
                    let _ = write!(text, "; consecutive failures {count}");
                }
                None => text.push_str("; consecutive failures unknown"),
            }
            if let Some(delay) = unit.backoff_ms {
                let _ = write!(text, "; next start after {delay} ms");
            }
            text.push('\n');
            if let Some(error) = &unit.backoff_error {
                let _ = writeln!(text, "    backoff: {error}");
            }
        }
    }
    if text.is_empty() {
        text.push_str("No workflow plugin has jobs or services installed\n");
    }
    text
}

fn state_text(unit: &UnitStatus) -> String {
    let state = match unit.state {
        NativeState::Running => "running",
        NativeState::Stopped => "stopped",
        NativeState::Failed => "failed",
        NativeState::Unknown => "unknown",
    };
    match &unit.state_reason {
        Some(reason) => format!("{state} ({reason})"),
        None => state.to_owned(),
    }
}
