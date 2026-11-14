//! Profile revision guard for presentation-only profile keys (`#2150` row 1)
//! and the presentation-only label scenarios (rows 2-9).
//!
//! A profile without `display_name` or `audience` keeps the revision the
//! base build froze: the `expected.json` fixture pins those revisions so a
//! later change to the revision inputs fails loudly. The guard pins `SHELL`
//! to `/bin/sh`, and the fixture carries the `/bin/sh` revisions, so the
//! guard is host-independent. To regenerate after an intentional input
//! change, run the guard once with `POHUNEK_PROFILE_REVISION_CAPTURE` set
//! and commit the rewritten fixture alongside the change.

// Rust guideline compliant 2026-11-13

use super::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

/// Profiles in the guard fixture, in creation order.
const GUARD_PROFILES: [&str; 3] = ["labelled-shell", "rules-shell", "manifest-shell"];

/// A profile base the label scenarios start from: hermetic shell, no keys.
const LABEL_BASE: &str = "base = \"shell\"\nargs = [\"-c\", \"exec sleep 30\"]\n";

fn fixture_dir() -> PathBuf {
    pohunek_test_support::workspace_root()
        .join("crates/daemon/tests/fixtures/profile-revision-base")
}

/// Make the seeded host-state tree owner-private: `HostStateDir` opens it
/// descriptor-by-descriptor and refuses any wider mode, so set the modes
/// explicitly instead of relying on the umask.
#[cfg(unix)]
fn set_private_modes(host_state: &Path, host_dir: &Path, key_path: &Path) {
    for path in [host_state, host_dir] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("make the seeded host state owner-private");
    }
    std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600))
        .expect("make the seeded key owner-private");
}

fn copy_dir_all(source: &Path, target: &Path) {
    for entry in std::fs::read_dir(source).expect("read fixture dir") {
        let entry = entry.expect("fixture entry");
        let to = target.join(entry.file_name());
        if entry.file_type().expect("fixture type").is_dir() {
            std::fs::create_dir(&to).expect("create fixture subdir");
            copy_dir_all(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).expect("copy fixture file");
        }
    }
}

/// The stored `profile_revision` hex of `id` in the store at `path`.
fn stored_revision_hex(path: &Path, id: &SessionId) -> String {
    Store::new(path.to_path_buf())
        .load_sessions()
        .expect("read the durable records")
        .into_iter()
        .find(|record| record.session_id == id.0)
        .expect("the session has a durable record")
        .recovery
        .expect("a profile session has a recovery record")
        .profile_revision
        .expect("a profile session freezes its profile revision")
        .to_string()
}

async fn create_profile_session(
    control: &mut Framed<UnixStream, LinesCodec>,
    agent: &str,
    cwd: PathBuf,
) -> SessionInfo {
    let mut params = session_params_in(cwd);
    agent.to_owned().clone_into(&mut params.agent);
    serde_json::from_value(ok_payload(
        create_session_with_params(control, params).await,
    ))
    .expect("session info")
}

/// The base build froze these revisions for profiles without the new keys;
/// the current daemon must reproduce them byte for byte. The revision
/// digests the shell program, which a profile over `base = "shell"` inherits
/// from the daemon's login shell (`$SHELL`), so the guard pins `SHELL` to
/// `/bin/sh` before resolving anything: the fixture carries the `/bin/sh`
/// revisions on every host.
#[tokio::test]
async fn a_profile_without_new_keys_keeps_the_revision_the_base_build_froze() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let mut shell = pohunek_test_support::process_env::ProcessEnv::lock();
    shell.set("SHELL", "/bin/sh");
    let fixture = fixture_dir();
    let expected: BTreeMap<String, String> = serde_json::from_str(
        &std::fs::read_to_string(fixture.join("expected.json")).expect("read expected revisions"),
    )
    .expect("expected revisions are a name-to-hex map");

    let socket = temp_socket("profile-revision-base");
    let agents_dir = socket.dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
    copy_dir_all(&fixture.join("agents"), &agents_dir);
    let host_state = socket.dir.join("host-state");
    // The host-state tree must be owner-private (0700): `HostStateDir` opens
    // it descriptor-by-descriptor and refuses any wider mode, so set the
    // modes explicitly instead of relying on the umask.
    std::fs::create_dir(&host_state).expect("create host state dir");
    let host_dir = host_state.join("host");
    std::fs::create_dir(&host_dir).expect("create host record dir");
    let key_path = host_dir.join("profile-revision.key");
    std::fs::copy(fixture.join("profile-revision.key"), &key_path)
        .expect("seed the fixed revision key");
    #[cfg(unix)]
    set_private_modes(&host_state, &host_dir, &key_path);
    let store_path = socket.dir.join("metadata.jsonl");
    let config = SessionRegistryConfig {
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        host_state_dir: Some(host_state),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let mut revisions = BTreeMap::new();
    for agent in GUARD_PROFILES {
        let session =
            create_profile_session(&mut control, agent, socket.dir.join(WORK_DIR_NAME)).await;
        revisions.insert(
            agent.to_owned(),
            stored_revision_hex(&store_path, &session.id),
        );
    }
    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;

    if std::env::var_os("POHUNEK_PROFILE_REVISION_CAPTURE").is_some() {
        std::fs::write(
            fixture.join("expected.json"),
            serde_json::to_string_pretty(&revisions).expect("encode revisions"),
        )
        .expect("capture expected revisions");
        return;
    }
    assert_eq!(
        revisions, expected,
        "a profile without the new keys keeps its base revision"
    );
}

// Rows 2-9: presentation-only labels (brief pi-2150-a, case table).
//
// These scenarios run against the current daemon through the public control
// socket: no per-function unit tests, only boundary behaviour.

/// A rig with one unlabelled shell profile `night` and a control connection.
struct LabelRig {
    socket: TestSocket,
    shutdown: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
    control: Framed<UnixStream, LinesCodec>,
    agents_dir: PathBuf,
    store_path: PathBuf,
}

impl LabelRig {
    async fn start(tag: &str) -> Self {
        let socket = temp_socket(tag);
        let agents_dir = socket.dir.join("agents");
        std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
        std::fs::write(agents_dir.join("night.toml"), LABEL_BASE).expect("seed night");
        // Resume compares profile revisions, so the rig seeds the same fixed
        // revision key the guard fixture uses.
        let host_state = socket.dir.join("host-state");
        std::fs::create_dir(&host_state).expect("create host state dir");
        let host_dir = host_state.join("host");
        std::fs::create_dir(&host_dir).expect("create host record dir");
        std::fs::copy(
            fixture_dir().join("profile-revision.key"),
            host_dir.join("profile-revision.key"),
        )
        .expect("seed the fixed revision key");
        #[cfg(unix)]
        set_private_modes(
            &host_state,
            &host_dir,
            &host_dir.join("profile-revision.key"),
        );
        let store_path = socket.dir.join("metadata.jsonl");
        let config = SessionRegistryConfig {
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents_dir.clone()),
            host_state_dir: Some(host_state),
            ..support::hermetic_registry_config()
        };
        let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
        let control = connect(&socket).await;
        Self {
            socket,
            shutdown,
            handle,
            control,
            agents_dir,
            store_path,
        }
    }

    async fn finish(self) {
        drop(self.control);
        let _ = self.shutdown.send(());
        let _ = self.handle.await;
    }

    /// `night.toml` as the plain base with `lines` inserted before it: the
    /// label-only shorthand for cases with no special layout.
    fn rewrite_night(&self, lines: &str) {
        let mut text = lines.to_owned();
        text.push_str(LABEL_BASE);
        self.write_night(&text);
    }

    /// `night.toml` as written, with the full `text` as the file content.
    fn write_night(&self, text: &str) {
        std::fs::write(self.agents_dir.join("night.toml"), text).expect("rewrite night");
    }

    async fn new_night_session(&mut self) -> SessionInfo {
        create_profile_session(
            &mut self.control,
            "night",
            self.socket.dir.join(WORK_DIR_NAME),
        )
        .await
    }

    async fn host_runtimes(&mut self) -> Vec<protocol::AgentRuntime> {
        let req = Request::make("label-host-inspect", method::HOST_INSPECT, Value::Null);
        let caps: protocol::HostCapabilities =
            serde_json::from_value(ok_payload(exchange(&mut self.control, &req).await))
                .expect("host capabilities");
        caps.runtimes
    }

    async fn list(&mut self) -> Vec<SessionInfo> {
        let req = Request::make("label-session-list", method::SESSION_LIST, Value::Null);
        serde_json::from_value(ok_payload(exchange(&mut self.control, &req).await))
            .expect("session list")
    }
}

/// Adding only label lines keeps the stored revision: each case launches
/// once from a laid-out base file — a comment line, a blank line after
/// `base`, CRLF endings, no final newline, or a label with a trailing
/// comment — then again after inserting only the label lines, and the stored
/// revision is identical. The span cut removes each label's own line (a
/// trailing `#` comment on that line leaves with the label), so the
/// labelled file digests to the same text as its laid-out base. The real
/// `args` edit below still moves the revision.
#[tokio::test]
async fn editing_only_labels_keeps_the_stored_revision() {
    // Each case is `(tag, laid-out base, label lines to add)`: the rig
    // launches once from the laid-out base, then again after inserting
    // only the label lines.
    let layouts: [(&str, &str, &str); 5] = [
        // The base file already carries a comment line; the label goes
        // above it.
        (
            "profile-label-comment",
            "# the night agent\nbase = \"shell\"\nargs = [\"-c\", \"exec sleep 30\"]\n",
            "display_name = \"Night Shift\"\n",
        ),
        // The base file already carries a blank line after `base`; the
        // label goes above it.
        (
            "profile-label-blank",
            "base = \"shell\"\n\nargs = [\"-c\", \"exec sleep 30\"]\n",
            "display_name = \"Night Shift\"\n",
        ),
        // The base file uses CRLF; the added labels keep CRLF.
        (
            "profile-label-crlf",
            "base = \"shell\"\r\nargs = [\"-c\", \"exec sleep 30\"]\r\n",
            "display_name = \"Night Shift\"\r\naudience = \"agent\"\r\n",
        ),
        // The base file has no final newline; the added labels are
        // terminated lines above the unterminated launch line.
        (
            "profile-label-noeol",
            "base = \"shell\"\nargs = [\"-c\", \"exec sleep 30\"]",
            "display_name = \"Night Shift\"\naudience = \"agent\"\n",
        ),
        // The label carries a trailing comment, as the docs' own example
        // does: the comment documents the label, so it leaves with it and
        // the revision is the bare base either way.
        (
            "profile-label-trailing",
            "base = \"shell\"\nargs = [\"-c\", \"exec sleep 30\"]\n",
            "display_name = \"Night Shift\" # trimmed free text\naudience = \"agent\" # absent means owner\n",
        ),
    ];
    for (tag, laid_out, labels) in layouts {
        let mut rig = LabelRig::start(tag).await;
        rig.write_night(laid_out);
        let before = rig.new_night_session().await;
        let frozen = stored_revision_hex(&rig.store_path, &before.id);
        let mut text = labels.to_owned();
        text.push_str(laid_out);
        rig.write_night(&text);
        let after = rig.new_night_session().await;
        assert_eq!(
            stored_revision_hex(&rig.store_path, &after.id),
            frozen,
            "label-only edits keep the stored revision ({tag})"
        );
        rig.finish().await;
    }
}

/// Editing `args` still changes the revision: the exclusion covers only the
/// two label keys, so launch inputs keep their guard.
#[tokio::test]
async fn editing_launch_inputs_still_changes_the_revision() {
    let mut rig = LabelRig::start("profile-label-args").await;
    let before = rig.new_night_session().await;
    let frozen = stored_revision_hex(&rig.store_path, &before.id);
    rig.write_night("base = \"shell\"\nargs = [\"-c\", \"exec sleep 31\"]\n");
    let after = rig.new_night_session().await;
    assert_ne!(
        stored_revision_hex(&rig.store_path, &after.id),
        frozen,
        "args edits change the stored revision"
    );
    rig.finish().await;
}

/// Editing only an `audience` key keeps the stored revision as well: the
/// rig launches from a commented base, then adds only the `audience` line
/// under the comment.
#[tokio::test]
async fn editing_only_the_audience_keeps_the_stored_revision() {
    let mut rig = LabelRig::start("profile-label-audience").await;
    rig.write_night("# the night agent\nbase = \"shell\"\nargs = [\"-c\", \"exec sleep 30\"]\n");
    let before = rig.new_night_session().await;
    let frozen = stored_revision_hex(&rig.store_path, &before.id);
    rig.write_night(
        "# the night agent\naudience = \"agent\"\nbase = \"shell\"\nargs = [\"-c\", \"exec sleep 30\"]\n",
    );
    let after = rig.new_night_session().await;
    assert_eq!(
        stored_revision_hex(&rig.store_path, &after.id),
        frozen,
        "audience edits keep the stored revision"
    );
    rig.finish().await;
}

/// A `session list` over a bare `shell` session logs no unreadable-label
/// line: a runtime id with no profile file reads `NoLabel`.
#[tokio::test]
async fn listing_a_bare_runtime_logs_no_unreadable_label() {
    let log_dir = pohunek_test_support::tempdir().expect("log directory");
    let _capture = pohunek_daemon::logging::capture(&log_dir.path().join("daemon-logs"))
        .expect("the daemon log capture starts");
    let mut rig = LabelRig::start("profile-label-bare").await;
    let params = {
        let mut params = session_params_in(rig.socket.dir.join(WORK_DIR_NAME));
        "shell".to_owned().clone_into(&mut params.agent);
        params
    };
    let req = Request::make(
        "label-new-bare",
        method::SESSION_NEW,
        serde_json::to_value(params).expect("serialize params"),
    );
    let created: SessionInfo =
        serde_json::from_value(ok_payload(exchange(&mut rig.control, &req).await))
            .expect("bare session info");
    assert_eq!(created.agent_display_name, None);
    let listed = rig.list().await;
    assert_eq!(
        listed
            .iter()
            .find(|info| info.id == created.id)
            .expect("the bare session is listed")
            .agent_display_name,
        None
    );
    assert!(
        unreadable_label_events(&log_dir.path().join("daemon-logs")).is_empty(),
        "a bare runtime logs no unreadable label"
    );
    rig.finish().await;
}

/// Labelling a profile and then deleting the file logs one
/// `agent_label.unreadable` line: the labelled reads succeed silently and
/// the missing file is `Gone`.
#[tokio::test]
async fn labelling_then_deleting_the_profile_logs_once() {
    let log_dir = pohunek_test_support::tempdir().expect("log directory");
    let _capture = pohunek_daemon::logging::capture(&log_dir.path().join("daemon-logs"))
        .expect("the daemon log capture starts");
    let mut rig = LabelRig::start("profile-label-delete").await;
    rig.new_night_session().await;
    rig.list().await;
    rig.rewrite_night("display_name = \"Night Shift\"\naudience = \"agent\"\n");
    rig.list().await;
    std::fs::remove_file(rig.agents_dir.join("night.toml")).expect("delete the profile");
    rig.list().await;
    assert_eq!(
        unreadable_label_events(&log_dir.path().join("daemon-logs"))
            .iter()
            .map(|record| record["fields"]["agent"].clone())
            .collect::<Vec<_>>(),
        vec!["night".to_owned()],
        "deleting the profile logs one unreadable label"
    );
    rig.finish().await;
}

/// Each invalid label value fails the load with its fixed message; the
/// message never echoes the value (the marker comes from the value), and
/// neither does the daemon log: the load path rejects the label before any
/// `warn!` could quote raw TOML text.
#[tokio::test]
async fn invalid_label_values_fail_with_fixed_messages() {
    const MARKER: &str = "Marker-Q7x2-label";
    let cases = [
        (
            // A TOML escape: the parser decodes it before validation sees it.
            format!("display_name = \"before\\u000a{MARKER}\"\n"),
            "display_name must not contain control, separator or format characters",
        ),
        (
            format!("display_name = \"a{MARKER}\u{202e}b\"\n"),
            "display_name must not contain control, separator or format characters",
        ),
        (
            format!("display_name = \"{}\"\n", "x".repeat(129)),
            "display_name exceeds 128 bytes",
        ),
        (
            "display_name = 5\n".to_owned(),
            "display_name must be a string",
        ),
        (
            // Whitespace-only: the check trims before testing emptiness.
            "display_name = \"   \"\n".to_owned(),
            "display_name must not be empty",
        ),
        (
            "audience = \"public\"\n".to_owned(),
            "audience must be one of owner, agent, hidden",
        ),
        (
            // A non-string TOML value: the visitor names the expected type,
            // never the value.
            "audience = 5\n".to_owned(),
            "audience must be one of owner, agent, hidden",
        ),
    ];
    for (index, (prefix, message)) in cases.into_iter().enumerate() {
        let log_dir = pohunek_test_support::tempdir().expect("log directory");
        let _capture = pohunek_daemon::logging::capture(&log_dir.path().join("daemon-logs"))
            .expect("the daemon log capture starts");
        let mut rig = LabelRig::start(&format!("profile-label-bad-{index}")).await;
        rig.rewrite_night(&prefix);
        let params = {
            let mut params = session_params_in(rig.socket.dir.join(WORK_DIR_NAME));
            "night".to_owned().clone_into(&mut params.agent);
            params
        };
        let req = Request::make(
            "label-new",
            method::SESSION_NEW,
            serde_json::to_value(params).expect("serialize params"),
        );
        let error = exchange(&mut rig.control, &req)
            .await
            .into_result()
            .expect_err("an invalid label refuses the launch");
        // `invalid_profile` prefixes the fixed detail with the name; the
        // assertion pins the detail as a suffix so the fixed text stays exact.
        let detail = error
            .msg
            .strip_prefix("invalid agent profile 'night': line 1: ")
            .expect("the invalid_profile prefix");
        assert_eq!(detail, message, "case {index}: {error:?}");
        assert!(
            !error.msg.contains(MARKER),
            "case {index}: the value is not echoed: {error:?}"
        );
        let log_text = std::fs::read_dir(log_dir.path().join("daemon-logs"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
            .collect::<String>();
        assert!(
            !log_text.contains(MARKER),
            "case {index}: the log echoes no label text"
        );
        rig.finish().await;
    }
}

/// An unknown audience fails with its fixed message, without echoing.
#[tokio::test]
async fn an_unknown_audience_value_is_refused() {
    let mut rig = LabelRig::start("profile-label-unknown").await;
    rig.rewrite_night("audience = \"public-Marker-Q7x2\"\n");
    let params = {
        let mut params = session_params_in(rig.socket.dir.join(WORK_DIR_NAME));
        "night".to_owned().clone_into(&mut params.agent);
        params
    };
    let req = Request::make(
        "label-new",
        method::SESSION_NEW,
        serde_json::to_value(params).expect("serialize params"),
    );
    let error = exchange(&mut rig.control, &req)
        .await
        .into_result()
        .expect_err("an unknown audience refuses the launch");
    let detail = error
        .msg
        .strip_prefix("invalid agent profile 'night': line 1: ")
        .expect("the invalid_profile prefix");
    assert_eq!(
        detail, "audience must be one of owner, agent, hidden",
        "{error:?}"
    );
    assert!(
        !error.msg.contains("Marker-Q7x2"),
        "the value is not echoed: {error:?}"
    );
    rig.finish().await;
}

/// `host.inspect` reports labels on resolvable profiles only: the labelled
/// profile carries both, an unlabelled file carries neither, and the bare
/// `shell` runtime has no file and carries neither.
#[tokio::test]
async fn host_inspect_reports_labels_on_profiles_only() {
    let mut rig = LabelRig::start("profile-label-inspect").await;
    rig.rewrite_night("display_name = \"Night Shift\"\naudience = \"hidden\"\n");
    std::fs::write(rig.agents_dir.join("plain.toml"), LABEL_BASE).expect("seed plain");
    let runtimes = rig.host_runtimes().await;
    let labelled = runtimes
        .iter()
        .find(|entry| entry.agent == "night")
        .expect("the labelled profile is listed");
    assert_eq!(labelled.display_name.as_deref(), Some("Night Shift"));
    assert_eq!(labelled.audience, Some(protocol::ProfileAudience::Hidden));
    let unlabelled = runtimes
        .iter()
        .find(|entry| entry.agent == "plain")
        .expect("the plain profile is listed");
    assert_eq!(unlabelled.display_name, None);
    assert_eq!(unlabelled.audience, None);
    let bare = runtimes
        .iter()
        .find(|entry| entry.agent == "shell")
        .expect("the bare runtime is listed");
    assert_eq!(bare.display_name, None);
    assert_eq!(bare.audience, None);
    rig.finish().await;
}

/// `session.list` and `session.inspect` carry the live label; deleting the
/// profile file clears it without touching the stored record. The daemon's
/// JSON log capture records the one `agent_label.unreadable` line the
/// missing file logs per list and inspect; the record names the agent and
/// carries no file content. A file whose profile no longer parses reads
/// `Broken`: the label is equally unset but nothing logs, since the launch
/// rules own unparsable files and only the missing file is worth an
/// operator-visible line.
#[tokio::test]
async fn session_responses_carry_the_live_label_until_the_file_is_gone() {
    let log_dir = pohunek_test_support::tempdir().expect("log directory");
    let _capture = pohunek_daemon::logging::capture(&log_dir.path().join("daemon-logs"))
        .expect("the daemon log capture starts");
    let mut rig = LabelRig::start("profile-label-live").await;
    rig.rewrite_night("display_name = \"Night Shift\"\n");
    let session = rig.new_night_session().await;
    let listed = rig.list().await;
    let entry = listed
        .iter()
        .find(|info| info.id == session.id)
        .expect("the session is listed");
    assert_eq!(entry.agent_display_name.as_deref(), Some("Night Shift"));
    let inspected = inspect_session(&mut rig.control, &session.id).await;
    assert_eq!(inspected.agent_display_name.as_deref(), Some("Night Shift"));
    std::fs::remove_file(rig.agents_dir.join("night.toml")).expect("delete night");
    let listed_after = rig.list().await;
    let cleared = listed_after
        .iter()
        .find(|info| info.id == session.id)
        .expect("the session is still listed");
    assert_eq!(cleared.agent_display_name, None);
    let inspected_after = inspect_session(&mut rig.control, &session.id).await;
    assert_eq!(inspected_after.agent_display_name, None);
    let unreadable = unreadable_label_events(&log_dir.path().join("daemon-logs"));
    assert_eq!(
        unreadable.len(),
        2,
        "the missing file logs once per list and inspect: {unreadable:?}"
    );
    for record in &unreadable {
        assert_eq!(
            record["fields"]["agent"], "night",
            "the log names the agent: {record:?}"
        );
    }
    rig.write_night("display_name = \"Night Shift\"\nbase = \"shell\n");
    let listed_broken = rig.list().await;
    assert_eq!(
        listed_broken
            .iter()
            .find(|info| info.id == session.id)
            .expect("the session is still listed")
            .agent_display_name,
        None
    );
    assert_eq!(
        unreadable_label_events(&log_dir.path().join("daemon-logs")).len(),
        2,
        "an unparsable file logs nothing more"
    );
    rig.finish().await;
}

/// The production JSON log records in `log_dir` whose `event` field is the
/// label enrichment's `agent_label.unreadable`.
fn unreadable_label_events(log_dir: &Path) -> Vec<Value> {
    let mut records = Vec::new();
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return records;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        records.extend(
            text.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|record| record["fields"]["event"] == "agent_label.unreadable"),
        );
    }
    records
}

/// The stored metadata line and the streamed session event never carry
/// `agent_display_name`: it is a response-only field.
#[tokio::test]
async fn stored_records_and_events_never_carry_the_label() {
    let mut rig = LabelRig::start("profile-label-stored").await;
    rig.rewrite_night("display_name = \"Night Shift\"\n");
    let mut subscriber = subscribe_events(&rig.socket.path).await;
    let session = rig.new_night_session().await;
    let streamed = wait_for_session_event(&mut subscriber, event::SESSION_CREATED).await;
    let frame = serde_json::to_value(&streamed).expect("serialize event");
    assert!(
        !frame.to_string().contains("agent_display_name"),
        "the event carries no label: {frame}"
    );
    let store_text = std::fs::read_to_string(&rig.store_path).expect("read the metadata line");
    assert!(
        !store_text.contains("agent_display_name"),
        "the stored record carries no label"
    );
    drop(subscriber);
    let listed = rig.list().await;
    assert_eq!(
        listed
            .iter()
            .find(|info| info.id == session.id)
            .expect("the session is listed")
            .agent_display_name
            .as_deref(),
        Some("Night Shift"),
        "the response still carries it"
    );
    rig.finish().await;
}
