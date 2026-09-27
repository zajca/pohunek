//! Which installed versions live workers and journals still reference.
//!
//! A version directory may be deleted only when no worker journal whose
//! worker may still run names an executable inside it, no registered worker
//! job names an executable inside it, no running process of this user
//! executes a file inside it, and the running CLI does not live there.
//! Unreadable journals, a failed job discovery, and a job whose executable
//! cannot be proven make every version referenced, because nothing can be
//! proven about the worker behind them.
//!
//! A journal an older pohunek wrote under an earlier schema names no
//! executable. Only its worker's process identity and phase are read: once
//! that worker is provably gone or its phase is final the journal references
//! nothing, and until then it makes every version referenced.

// Rust guideline compliant 2026-09-27

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pohunek_paths::{valid_worker_id, valid_worker_session_id, BasePaths, InstallLayout};
use pohunek_platform::filesystem::TrustedDir;
use pohunek_platform::process::{
    Error as ProcessError, ProcessIdentity, ProcessInspector, StartIdentity,
};
use pohunek_platform::supervisor::ServiceObservation;
use pohunek_session_worker::{Journal, LoadedJournal, OutdatedJournal, RuntimePhase};
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

/// One journal an older pohunek wrote, reduced to its liveness facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutdatedRef {
    /// Journal file.
    pub path: PathBuf,
    /// Schema version the journal records.
    pub schema_version: u32,
    /// Recorded worker process ID, when the schema has a readable one.
    pub worker_pid: Option<u32>,
    /// Whether the recorded phase is final; an unreadable phase is not.
    pub final_phase: bool,
    /// Worker process identity, when both of its fields are readable.
    pub worker: Option<ProcessIdentity>,
}

impl OutdatedRef {
    fn new(path: PathBuf, journal: &OutdatedJournal) -> Self {
        let worker = journal
            .worker_pid
            .zip(journal.worker_start_identity.as_deref())
            .and_then(|(pid, start)| {
                start
                    .parse::<StartIdentity>()
                    .ok()
                    .map(|start_identity| ProcessIdentity {
                        pid,
                        start_identity,
                    })
            });
        Self {
            path,
            schema_version: journal.schema_version,
            worker_pid: journal.worker_pid,
            final_phase: journal.phase.as_ref().is_some_and(is_final),
            worker,
        }
    }

    /// Returns whether the worker behind this journal may still run.
    ///
    /// An unreadable identity and a failed inspection both count as running.
    fn may_run(&self, inspector: &dyn ProcessInspector) -> bool {
        !self.final_phase
            && self
                .worker
                .is_none_or(|identity| inspector.is_running(identity).unwrap_or(true))
    }
}

/// All worker journals of one installation.
#[derive(Debug, Clone, Default)]
pub struct Journals {
    /// Journals that parsed.
    pub records: Vec<JournalRef>,
    /// Journals of an older schema.
    pub outdated: Vec<OutdatedRef>,
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
                match Journal::new(&file).load_any() {
                    Ok(LoadedJournal::Current(record)) => journals.records.push(JournalRef {
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
                    Ok(LoadedJournal::Outdated(outdated)) => {
                        journals.outdated.push(OutdatedRef::new(file, &outdated));
                    }
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

    /// Returns outdated journals whose worker may still run.
    #[must_use]
    pub fn outdated_live<'a>(&'a self, inspector: &dyn ProcessInspector) -> Vec<&'a OutdatedRef> {
        self.outdated
            .iter()
            .filter(|journal| journal.may_run(inspector))
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
    /// Outdated journals whose worker may still run, from an executable they
    /// do not name; while any exist every version is kept.
    pub outdated: Vec<PathBuf>,
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
            outdated: journals
                .outdated_live(inspector)
                .into_iter()
                .map(|journal| journal.path.clone())
                .collect(),
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
        if !self.outdated.is_empty() {
            return Some(
                "worker journals of an older pohunek name workers that may still run".to_owned(),
            );
        }
        if let Some(error) = &self.process_error {
            return Some(format!("the process table could not be read: {error}"));
        }
        if let Some(error) = &self.jobs_error {
            return Some(format!("worker jobs could not be attributed: {error}"));
        }
        None
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
        let (pid, start_identity) = own_identity();
        write_journal_of(
            paths,
            session,
            worker,
            executable,
            RuntimePhase::Live,
            (pid, &start_identity),
        );
    }

    /// Worker identity of this test process, which is provably alive.
    pub(crate) fn own_identity() -> (u32, String) {
        let own = HostInspector::new()
            .identity(std::process::id())
            .expect("identity")
            .expect("own process");
        (own.pid, own.start_identity.to_string())
    }

    /// Writes a schema-3 journal, which names no executable, with `phase`
    /// and the worker identity `worker` (`None` omits both identity keys).
    pub(crate) fn write_outdated_journal(
        paths: &BasePaths,
        session: &str,
        worker: &str,
        phase: &str,
        identity: Option<(u32, &str)>,
    ) -> PathBuf {
        let path = paths
            .worker_journal(session, worker)
            .expect("valid journal path");
        write_journal_of(
            paths,
            session,
            worker,
            Path::new("/unused"),
            RuntimePhase::Live,
            (1, "1"),
        );
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read journal")).expect("json");
        let object = json.as_object_mut().expect("journal object");
        for key in [
            "executable",
            "version",
            "generation",
            "worker_pid",
            "worker_start_identity",
        ] {
            object.remove(key);
        }
        object.insert("schema_version".to_owned(), 3.into());
        object.insert("phase".to_owned(), phase.into());
        if let Some((pid, start_identity)) = identity {
            object.insert("worker_pid".to_owned(), pid.into());
            object.insert("worker_start_identity".to_owned(), start_identity.into());
        }
        std::fs::write(&path, serde_json::to_vec(&json).expect("encode")).expect("rewrite");
        path
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
    fn an_outdated_journal_keeps_every_version_only_while_its_worker_may_run() {
        let (_root, root) = temp_root();
        let paths = super::super::context::tests::paths(root.as_path());
        let layout = InstallLayout::new(root.as_path().join("prefix")).expect("layout");
        let (pid, start) = own_identity();
        let collect = |journals: &Journals| {
            Usage::collect(
                &layout,
                journals,
                &HostInspector::new(),
                Path::new("/usr/bin/pohunek"),
            )
        };

        // Exited worker, final phase: neither is unreadable nor referencing.
        write_outdated_journal(&paths, SESSION, "w-gone", "live", Some((pid, "1")));
        write_outdated_journal(&paths, "s-2", "w-done", "terminal", Some((pid, &start)));
        let journals = Journals::scan(&paths).expect("scan");
        assert!(journals.unreadable.is_empty());
        assert_eq!(journals.outdated.len(), 2);
        assert!(journals.outdated_live(&HostInspector::new()).is_empty());
        assert_eq!(collect(&journals).keep_reason("1.0.0", None), None);

        // A live worker, and one whose identity is unreadable, may still run.
        let live = write_outdated_journal(&paths, "s-3", "w-live", "live", Some((pid, &start)));
        let unknown = write_outdated_journal(&paths, "s-4", "w-unknown", "starting", None);
        let journals = Journals::scan(&paths).expect("scan");
        let usage = collect(&journals);
        assert_eq!(usage.outdated, [live, unknown]);
        assert!(usage
            .keep_reason("1.0.0", None)
            .is_some_and(|reason| reason.contains("older pohunek")));
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
