//! Which installed versions live workers and journals still reference.
//!
//! A version directory may be deleted only when no worker journal whose
//! worker may still run names an executable inside it, no registered worker
//! job names an executable inside it, no running process of this user
//! executes a file inside it, and the running CLI does not live there.
//! Unreadable journals, a failed job discovery, and a job whose executable
//! cannot be proven make every version referenced, because nothing can be
//! proven about the worker behind them.

// Rust guideline compliant 2026-09-25

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use pohunek_paths::{valid_worker_id, valid_worker_session_id, BasePaths, InstallLayout};
use pohunek_platform::filesystem::TrustedDir;
use pohunek_platform::process::{
    Error as ProcessError, ProcessIdentity, ProcessInspector, StartIdentity,
};
use pohunek_platform::supervisor::ServiceObservation;
use pohunek_session_worker::{Journal, RuntimePhase};
use serde::Serialize;

use super::error::{fs_error, Error};
use super::layout::version_of;

/// Mode of the owner-private journal directories.
const JOURNAL_DIR_MODE: u32 = 0o700;

/// File extension of a worker journal.
const JOURNAL_EXTENSION: &str = "json";

/// One worker journal, reduced to the facts the installer needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JournalRef {
    /// Logical session ID.
    pub session_id: String,
    /// Worker ID (the journal file stem).
    pub worker_id: String,
    /// Daemon-issued worker generation.
    pub generation: String,
    /// Recorded runtime phase, in its wire spelling.
    pub phase: String,
    /// Absolute executable the worker ran from.
    pub executable: PathBuf,
    /// Whether the phase is final: the PTY child can no longer be live.
    #[serde(skip)]
    pub final_phase: bool,
    /// Worker process identity recorded by the journal.
    #[serde(skip)]
    pub worker: Option<ProcessIdentity>,
}

impl JournalRef {
    /// Returns whether the worker behind this journal may still be running.
    ///
    /// A missing or unparsable identity counts as running.
    ///
    /// # Errors
    ///
    /// Returns the inspector's failure; callers treat it as "unknown".
    pub fn worker_running(
        &self,
        inspector: &dyn ProcessInspector,
    ) -> Result<bool, pohunek_platform::process::Error> {
        match self.worker {
            Some(identity) => inspector.is_running(identity),
            None => Ok(true),
        }
    }
}

/// All worker journals of one installation.
#[derive(Debug, Clone, Default)]
pub struct Journals {
    /// Journals that parsed.
    pub records: Vec<JournalRef>,
    /// Journal files that could not be read or parsed.
    pub unreadable: Vec<PathBuf>,
}

impl Journals {
    /// Scans `<state>/pohunek/workers/<session-id>/<worker-id>.json`.
    ///
    /// Entries whose names are not a valid session or worker ID and hidden
    /// temporary files are skipped, since a worker never writes them.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error when the journal root itself is unsafe.
    pub fn scan(paths: &BasePaths) -> Result<Self, Error> {
        let root = paths.worker_state_root();
        let directory = match TrustedDir::open_absolute(&root, JOURNAL_DIR_MODE) {
            Ok(directory) => directory,
            Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => {
                return Ok(Self::default());
            }
            Err(source) => return Err(fs_error("open worker journal root", source)),
        };
        let mut journals = Self::default();
        let mut sessions = directory
            .entry_names()
            .map_err(|source| fs_error("list worker journal root", source))?;
        sessions.sort();
        for session in sessions {
            let Some(session) = session.to_str().and_then(valid_worker_session_id) else {
                continue;
            };
            let session_dir = match directory.open_child(session, JOURNAL_DIR_MODE) {
                Ok(session_dir) => session_dir,
                Err(_unsafe) => {
                    journals.unreadable.push(root.join(session));
                    continue;
                }
            };
            let mut names = match session_dir.entry_names() {
                Ok(names) => names,
                Err(_unlistable) => {
                    journals.unreadable.push(root.join(session));
                    continue;
                }
            };
            names.sort();
            for name in names {
                let path = Path::new(&name);
                let Some(worker_id) = path
                    .extension()
                    .filter(|extension| *extension == JOURNAL_EXTENSION)
                    .and(path.file_stem())
                    .and_then(|stem| stem.to_str())
                    .and_then(valid_worker_id)
                else {
                    continue;
                };
                let file = root.join(session).join(&name);
                match Journal::new(&file).load() {
                    Ok(record) => journals.records.push(JournalRef {
                        session_id: record.session_id,
                        worker_id: worker_id.to_owned(),
                        generation: record.origin.generation,
                        phase: phase_name(&record.phase).to_owned(),
                        executable: record.origin.executable,
                        final_phase: is_final(&record.phase),
                        worker: record
                            .worker_start_identity
                            .parse::<StartIdentity>()
                            .ok()
                            .map(|start_identity| ProcessIdentity {
                                pid: record.worker_pid,
                                start_identity,
                            }),
                    }),
                    Err(_corrupt) => journals.unreadable.push(file),
                }
            }
        }
        Ok(journals)
    }

    /// Returns journals whose worker may still own a live PTY.
    ///
    /// A non-final journal whose worker process has provably exited is
    /// stale and does not count.
    #[must_use]
    pub fn live<'a>(&'a self, inspector: &dyn ProcessInspector) -> Vec<&'a JournalRef> {
        self.records
            .iter()
            .filter(|journal| {
                !journal.final_phase && journal.worker_running(inspector).unwrap_or(true)
            })
            .collect()
    }

    /// Returns the final-phase status of `(session, generation)`, if journaled.
    #[must_use]
    pub fn final_for(&self, session_id: &str, generation: &str) -> Option<bool> {
        let mut matching = self
            .records
            .iter()
            .filter(|journal| journal.session_id == session_id && journal.generation == generation)
            .peekable();
        matching.peek()?;
        Some(matching.all(|journal| journal.final_phase))
    }
}

/// Journals in these phases no longer own a PTY child.
fn is_final(phase: &RuntimePhase) -> bool {
    matches!(
        phase,
        RuntimePhase::Terminal | RuntimePhase::NeverInitialized | RuntimePhase::Faulted
    )
}

fn phase_name(phase: &RuntimePhase) -> &'static str {
    match phase {
        RuntimePhase::Bootstrap => "bootstrap",
        RuntimePhase::Starting => "starting",
        RuntimePhase::Live => "live",
        RuntimePhase::Terminal => "terminal",
        RuntimePhase::NeverInitialized => "never_initialized",
        RuntimePhase::Faulted => "faulted",
    }
}

/// Why each installed version is still in use.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    /// Journals per version whose worker may still run.
    pub journals: BTreeMap<String, Vec<JournalRef>>,
    /// Registered worker job IDs per version their definition executes.
    pub jobs: BTreeMap<String, Vec<String>>,
    /// Running process IDs per version.
    pub processes: BTreeMap<String, Vec<u32>>,
    /// The version the running CLI executes from.
    pub cli: Option<String>,
    /// Journals that could not be read; while any exist every version is kept.
    pub unreadable: Vec<PathBuf>,
    /// Why the process table could not be read, which keeps every version.
    pub process_error: Option<String>,
    /// Why worker jobs could not be attributed to versions, which keeps
    /// every version.
    pub jobs_error: Option<String>,
}

impl Usage {
    /// Collects version references from journals, processes, and the CLI.
    ///
    /// A non-final journal whose worker has provably exited references
    /// nothing: the worker never runs again and its executable is unused.
    #[must_use]
    pub fn collect(
        layout: &InstallLayout,
        journals: &Journals,
        inspector: &dyn ProcessInspector,
        cli_executable: &Path,
    ) -> Self {
        let mut usage = Self {
            cli: version_of(layout, cli_executable),
            unreadable: journals.unreadable.clone(),
            ..Self::default()
        };
        for journal in journals.live(inspector) {
            if let Some(version) = version_of(layout, &journal.executable) {
                usage
                    .journals
                    .entry(version)
                    .or_default()
                    .push(journal.clone());
            }
        }
        match inspector.same_user_processes() {
            Ok(processes) => {
                for process in processes {
                    match inspector.executable(process.pid) {
                        Ok(Some(executable)) => {
                            usage.note_process(layout, &executable, process.pid);
                        }
                        // The process exited during the scan and references nothing.
                        Ok(None) => {}
                        // A non-dumpable process hides its executable link. Pohunek
                        // binaries never drop dumpability, but an absolute argv[0]
                        // inside a version directory is still honored.
                        Err(ProcessError::PermissionDenied { .. }) => {
                            if let Some(first) = process.cmdline.first() {
                                usage.note_process(layout, Path::new(first), process.pid);
                            }
                        }
                        Err(error) if error.is_race() => {}
                        Err(error) => {
                            usage.process_error =
                                Some(format!("executable of pid {}: {error}", process.pid));
                        }
                    }
                }
            }
            Err(error) => usage.process_error = Some(error.to_string()),
        }
        usage
    }

    /// Adds the versions registered worker jobs execute or will execute.
    ///
    /// Every discovered job counts whatever its state: a job registered but
    /// not yet executed has neither a process nor a journal, and only the
    /// daemon retires ended jobs. `discovered` is the discovery result, or
    /// why discovery failed.
    pub fn note_jobs(
        &mut self,
        layout: &InstallLayout,
        discovered: Result<&[ServiceObservation], String>,
    ) {
        let jobs = match discovered {
            Ok(jobs) => jobs,
            Err(error) => {
                self.jobs_error = Some(error);
                return;
            }
        };
        for job in jobs {
            let Some(definition) = &job.definition else {
                self.jobs_error = Some(format!("worker job {} has no provable executable", job.id));
                continue;
            };
            if let Some(version) = version_of(layout, &definition.executable) {
                self.jobs
                    .entry(version)
                    .or_default()
                    .push(job.id.to_string());
            }
        }
    }

    fn note_process(&mut self, layout: &InstallLayout, executable: &Path, pid: u32) {
        if executable.is_absolute() {
            if let Some(version) = version_of(layout, executable) {
                self.processes.entry(version).or_default().push(pid);
            }
        }
    }

    /// Returns why `version` must be kept, or `None` when it may be deleted.
    #[must_use]
    pub fn keep_reason(&self, version: &str, active: Option<&str>) -> Option<String> {
        if active == Some(version) {
            return Some("active version".to_owned());
        }
        if let Some(journals) = self.journals.get(version) {
            return Some(format!(
                "referenced by live worker journals: {}",
                journals
                    .iter()
                    .map(|journal| format!("{}/{}", journal.session_id, journal.worker_id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(ids) = self.jobs.get(version) {
            return Some(format!("registered by worker jobs: {}", ids.join(", ")));
        }
        if let Some(pids) = self.processes.get(version) {
            return Some(format!(
                "executed by running processes: {}",
                pids.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if self.cli.as_deref() == Some(version) {
            return Some("contains the running pohunek CLI".to_owned());
        }
        if !self.unreadable.is_empty() {
            return Some("some worker journals are unreadable".to_owned());
        }
        if let Some(error) = &self.process_error {
            return Some(format!("the process table could not be read: {error}"));
        }
        if let Some(error) = &self.jobs_error {
            return Some(format!("worker jobs could not be attributed: {error}"));
        }
        None
    }

    // TODO: unused, consider removing
    /// Returns every version referenced by journals, jobs, or processes.
    #[must_use]
    pub fn referenced(&self) -> BTreeSet<String> {
        self.journals
            .keys()
            .chain(self.jobs.keys())
            .chain(self.processes.keys())
            .cloned()
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use pohunek_platform::process::HostInspector;
    use pohunek_platform::supervisor::{DefinitionFacts, ServiceId, ServiceState, WorkerKey};
    use pohunek_session_worker::{JournalRecord, WorkerOrigin};

    use super::*;
    use crate::service::context::tests::temp_root;

    pub(crate) const SESSION: &str = "s-01KYAPVPFVHD56Z69B9CX3XWN2";

    /// Writes a worker journal the way a worker does.
    ///
    /// The recorded worker `pid` carries start identity `1`, which no live
    /// process has, so the worker has provably exited.
    pub(crate) fn write_journal(
        paths: &BasePaths,
        session: &str,
        worker: &str,
        executable: &Path,
        phase: RuntimePhase,
        pid: u32,
    ) {
        write_journal_of(paths, session, worker, executable, phase, (pid, "1"));
    }

    /// Writes a non-final journal whose worker is this test process, which is
    /// provably alive.
    pub(crate) fn write_live_journal(
        paths: &BasePaths,
        session: &str,
        worker: &str,
        executable: &Path,
    ) {
        let own = HostInspector::new()
            .identity(std::process::id())
            .expect("identity")
            .expect("own process");
        write_journal_of(
            paths,
            session,
            worker,
            executable,
            RuntimePhase::Live,
            (own.pid, &own.start_identity.to_string()),
        );
    }

    fn write_journal_of(
        paths: &BasePaths,
        session: &str,
        worker: &str,
        executable: &Path,
        phase: RuntimePhase,
        (pid, start_identity): (u32, &str),
    ) {
        let mut record = JournalRecord::bootstrap(
            session.to_owned(),
            worker.to_owned(),
            WorkerOrigin {
                executable: executable.to_path_buf(),
                version: "1.0.0".to_owned(),
                generation: "abcd2345".to_owned(),
            },
            pid,
            start_identity.to_owned(),
            "boot".to_owned(),
            (5, 6),
            "2026-09-24T00:00:00Z".to_owned(),
        );
        record.phase = phase;
        let path = paths
            .worker_journal(session, worker)
            .expect("valid journal path");
        std::fs::create_dir_all(paths.state_dir.as_path())
            .and_then(|()| {
                std::fs::set_permissions(
                    &paths.state_dir,
                    std::os::unix::fs::PermissionsExt::from_mode(0o700),
                )
            })
            .expect("state dir");
        Journal::new(path).write(&record).expect("write journal");
    }

    #[test]
    fn non_final_journals_reference_their_version_and_final_ones_do_not() {
        let (_root, root) = temp_root();
        let paths = super::super::context::tests::paths(root.as_path());
        let layout = InstallLayout::new(root.as_path().join("prefix")).expect("layout");
        let old = layout.worker_executable("1.0.0").expect("old");
        let done = layout.worker_executable("0.9.0").expect("done");
        let exited = layout.worker_executable("0.8.0").expect("exited");
        write_live_journal(&paths, SESSION, "w-live", &old);
        write_journal(&paths, "s-2", "w-done", &done, RuntimePhase::Terminal, 1);
        // Non-final, but its worker is provably gone.
        write_journal(&paths, "s-4", "w-gone", &exited, RuntimePhase::Live, 1);

        let journals = Journals::scan(&paths).expect("scan");
        assert_eq!(journals.records.len(), 3);
        assert!(journals.unreadable.is_empty());
        assert_eq!(journals.final_for(SESSION, "abcd2345"), Some(false));
        assert_eq!(journals.final_for("s-2", "abcd2345"), Some(true));
        assert_eq!(journals.final_for("s-3", "abcd2345"), None);

        let usage = Usage::collect(
            &layout,
            &journals,
            &HostInspector::new(),
            Path::new("/usr/bin/pohunek"),
        );
        assert!(usage
            .keep_reason("1.0.0", Some("2.0.0"))
            .is_some_and(|reason| reason.contains(SESSION)));
        assert_eq!(usage.keep_reason("0.9.0", Some("2.0.0")), None);
        assert_eq!(usage.keep_reason("0.8.0", Some("2.0.0")), None);
        assert_eq!(
            usage.keep_reason("2.0.0", Some("2.0.0")).as_deref(),
            Some("active version")
        );
    }

    fn job(session: &str, state: ServiceState, executable: Option<PathBuf>) -> ServiceObservation {
        ServiceObservation {
            id: WorkerKey::new(session, "abcd2345")
                .expect("worker key")
                .service_id(),
            state,
            process: None,
            definition: executable.map(|executable| DefinitionFacts {
                executable,
                arguments: Vec::new(),
            }),
        }
    }

    #[test]
    fn a_registered_worker_job_keeps_its_version_in_any_state() {
        let (_root, root) = temp_root();
        let layout = InstallLayout::new(root.as_path().join("prefix")).expect("layout");
        let jobs = [
            job(
                SESSION,
                ServiceState::Starting,
                layout.worker_executable("1.0.0"),
            ),
            job(
                "s-2",
                ServiceState::Unknown,
                layout.worker_executable("0.9.0"),
            ),
            job(
                "s-3",
                ServiceState::Running,
                Some(PathBuf::from("/elsewhere/pohunek-sessiond")),
            ),
        ];
        let mut usage = Usage::default();
        usage.note_jobs(&layout, Ok(&jobs));

        let id: ServiceId = jobs[0].id.clone();
        assert!(usage
            .keep_reason("1.0.0", Some("2.0.0"))
            .is_some_and(|reason| reason.contains(&id.to_string())));
        assert!(usage.keep_reason("0.9.0", Some("2.0.0")).is_some());
        assert_eq!(usage.keep_reason("0.8.0", Some("2.0.0")), None);
    }

    #[test]
    fn an_unattributable_worker_job_or_failed_discovery_keeps_every_version() {
        let (_root, root) = temp_root();
        let layout = InstallLayout::new(root.as_path().join("prefix")).expect("layout");

        let mut usage = Usage::default();
        usage.note_jobs(&layout, Ok(&[job(SESSION, ServiceState::Starting, None)]));
        assert!(usage
            .keep_reason("1.0.0", None)
            .is_some_and(|reason| reason.contains("no provable executable")));

        let mut usage = Usage::default();
        usage.note_jobs(&layout, Err("manager unreachable".to_owned()));
        assert!(usage
            .keep_reason("1.0.0", None)
            .is_some_and(|reason| reason.contains("manager unreachable")));
    }

    #[test]
    fn an_unreadable_journal_keeps_every_version() {
        let (_root, root) = temp_root();
        let paths = super::super::context::tests::paths(root.as_path());
        let layout = InstallLayout::new(root.as_path().join("prefix")).expect("layout");
        let old = layout.worker_executable("1.0.0").expect("old");
        write_journal(&paths, SESSION, "w-1", &old, RuntimePhase::Terminal, 1);
        let corrupt = paths.worker_journal(SESSION, "w-2").expect("path");
        std::fs::write(&corrupt, "{").expect("corrupt journal");
        std::fs::set_permissions(
            &corrupt,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .expect("chmod");

        let journals = Journals::scan(&paths).expect("scan");
        assert_eq!(journals.unreadable, [corrupt]);
        let usage = Usage::collect(
            &layout,
            &journals,
            &HostInspector::new(),
            Path::new("/usr/bin/pohunek"),
        );
        assert!(usage.keep_reason("1.0.0", None).is_some());
    }

    #[test]
    fn a_running_process_keeps_the_version_it_executes() {
        let (_root, root) = temp_root();
        let layout = InstallLayout::new(root.as_path().join("prefix")).expect("layout");
        let dir = layout.version_dir("1.0.0").expect("dir");
        std::fs::create_dir_all(&dir).expect("version dir");
        let sleeper = dir.join("pohunek-sessiond");
        std::fs::copy("/bin/sleep", &sleeper).expect("copy sleep");
        let mut child = std::process::Command::new(&sleeper)
            .arg("30")
            .spawn()
            .expect("spawn live worker stand-in");

        let usage = Usage::collect(
            &layout,
            &Journals::default(),
            &HostInspector::new(),
            Path::new("/usr/bin/pohunek"),
        );
        child.kill().expect("kill stand-in");
        child.wait().expect("reap stand-in");
        assert_eq!(usage.processes.get("1.0.0"), Some(&vec![child.id()]));
        assert!(usage
            .keep_reason("1.0.0", None)
            .is_some_and(|reason| reason.contains("running processes")));
    }
}
