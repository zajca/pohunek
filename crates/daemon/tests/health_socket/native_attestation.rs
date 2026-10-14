//! `session.report_native_id` through the public control socket is attested
//! against the session's PTY root.
//!
//! Each scenario starts a real session whose agent is a reporting Python image
//! (see `support/root_reporter.rs`), so the reports cross the same socket and
//! worker the shipped hooks fall back to.

// Rust guideline compliant 2026-10-13

use super::*;

/// Wire code of a report the daemon refuses because the caller is not attested.
const UNATTESTED_CODE: &str = "native_report_unattested";

/// Sequence of the first report a session accepts.
const FIRST_SEQUENCE: u64 = 1;
/// Sequence of a later report, ordered after [`FIRST_SEQUENCE`].
const LATER_SEQUENCE: u64 = 2;

/// Native reference the PTY root reports as its own.
const ROOT_REFERENCE: &str = "native-attested-by-root";
/// Native reference a descendant of the PTY root tries to replace it with.
const DESCENDANT_REFERENCE: &str = "native-forged-by-descendant";

/// A started session whose agent can report from its root or a descendant.
struct ReportingRig {
    control: Framed<UnixStream, LinesCodec>,
    socket: TestSocket,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    session: SessionInfo,
    worker_instance_id: String,
    reporter: root_reporter::RootReporter,
    _bin: TestDir,
    _state: TestDir,
    _cwd: TestDir,
    _path: PathGuard,
}

impl ReportingRig {
    /// Starts a session of `agent`, whose program is named `program` on `PATH`.
    async fn start(tag: &str, agent: RuntimeRef, program: &str) -> Self {
        let bin = temp_dir(&format!("{tag}-bin"));
        let state = temp_dir(&format!("{tag}-state"));
        let cwd = temp_dir(&format!("{tag}-cwd"));
        let reporter = root_reporter::RootReporter::install(&bin, program, program, &state);
        let path = PathGuard::prepend(&bin);
        let config = SessionRegistryConfig {
            shell_command: support::hermetic_shell(),
            store_path: Some(state.join("metadata.jsonl")),
            ..SessionRegistryConfig::default()
        };
        let socket = temp_socket(tag);
        let (shutdown, server, _) =
            spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;
        let mut control = connect(&socket).await;
        let session: SessionInfo = serde_json::from_value(ok_payload(
            create_session_with_agent(&mut control, agent, cwd.to_path_buf()).await,
        ))
        .expect("create the reporting session");
        let worker_instance_id = session
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.worker_instance_id.clone())
            .expect("the created session exposes its runtime id");
        Self {
            socket,
            control,
            shutdown,
            server,
            session,
            worker_instance_id,
            reporter,
            _bin: bin,
            _state: state,
            _cwd: cwd,
            _path: path,
        }
    }

    /// The report the PTY root would send, naming the root's own pid and
    /// start identity, with `native` as the reference.
    fn report(&self, sequence: u64, native: &str) -> ProtocolRequest {
        Request::make(
            &format!("attest-{native}"),
            method::SESSION_REPORT_NATIVE_ID,
            serde_json::to_value(
                SessionReportNativeIdParams::new(
                    self.session.id.clone(),
                    self.worker_instance_id.as_str(),
                    agent_name_of(&self.session),
                    self.session.pid,
                    process_start_identity(self.session.pid),
                    ReportSequence::new(sequence),
                    (OffsetDateTime::now_utc() + time::Duration::seconds(30))
                        .format(&Rfc3339)
                        .expect("format native identity expiry"),
                    native,
                    None,
                )
                .expect("valid native identity claim"),
            )
            .expect("serialize report-native-id params"),
        )
    }

    async fn native_reference(&mut self) -> Option<String> {
        inspect_session(&mut self.control, &self.session.id)
            .await
            .native_session_id
    }

    async fn finish(self) {
        let _ = self.shutdown.send(());
        let _ = self.server.await;
    }
}

fn agent_name_of(session: &SessionInfo) -> &str {
    session.agent.as_str()
}

/// The refusal must be a typed runtime error that echoes no request value.
fn assert_unattested(response: Response, native: &str) {
    let error = err_payload(response);
    assert_eq!(error.code, UNATTESTED_CODE, "typed refusal: {error:?}");
    assert_eq!(error.class, ErrorClass::Runtime);
    assert!(
        !error.msg.contains(native),
        "the refusal must not echo the reported reference: {error:?}"
    );
}

/// A descendant of a Claude PTY root that reports the root's pid and start
/// identity is refused, and the session keeps the reference it had.
#[tokio::test]
async fn descendant_report_naming_the_pty_root_is_refused() {
    let mut rig = ReportingRig::start("attest-descendant", RuntimeRef::claude(), "claude").await;
    let request = rig.report(FIRST_SEQUENCE, DESCENDANT_REFERENCE);

    let answer = rig
        .reporter
        .send(
            "descendant",
            root_reporter::Via::Descendant,
            &rig.socket,
            &request,
        )
        .await;
    assert_unattested(answer, DESCENDANT_REFERENCE);
    assert_eq!(
        rig.native_reference().await,
        None,
        "a refused descendant report must not record a native reference"
    );
    rig.finish().await;
}

/// The Claude PTY root itself is accepted, and a later descendant report
/// cannot replace what the root recorded.
#[tokio::test]
async fn pty_root_report_is_recorded_and_a_later_descendant_cannot_replace_it() {
    let mut rig = ReportingRig::start("attest-root", RuntimeRef::claude(), "claude").await;

    let root = rig.report(FIRST_SEQUENCE, ROOT_REFERENCE);
    let answer = rig
        .reporter
        .send("root", root_reporter::Via::Root, &rig.socket, &root)
        .await;
    let recorded: SessionReportNativeIdResult =
        serde_json::from_value(ok_payload(answer)).expect("root report result");
    assert!(recorded.recorded, "the PTY root's own report is recorded");
    assert_eq!(
        rig.native_reference().await.as_deref(),
        Some(ROOT_REFERENCE)
    );

    let forged = rig.report(LATER_SEQUENCE, DESCENDANT_REFERENCE);
    let answer = rig
        .reporter
        .send(
            "descendant",
            root_reporter::Via::Descendant,
            &rig.socket,
            &forged,
        )
        .await;
    assert_unattested(answer, DESCENDANT_REFERENCE);
    assert_eq!(
        rig.native_reference().await.as_deref(),
        Some(ROOT_REFERENCE),
        "the descendant must not replace the root's reference"
    );
    rig.finish().await;
}

/// A Hermes session is `identity-v1`: its native identity is set only through
/// the worker's frozen-root rule, so even the PTY root's daemon report is
/// refused.
#[tokio::test]
async fn identity_v1_root_report_is_refused_and_records_nothing() {
    let mut rig = ReportingRig::start("attest-hermes", RuntimeRef::hermes(), "hermes").await;
    let request = rig.report(FIRST_SEQUENCE, ROOT_REFERENCE);

    let answer = rig
        .reporter
        .send("root", root_reporter::Via::Root, &rig.socket, &request)
        .await;
    assert_unattested(answer, ROOT_REFERENCE);
    assert_eq!(rig.native_reference().await, None);
    rig.finish().await;
}
