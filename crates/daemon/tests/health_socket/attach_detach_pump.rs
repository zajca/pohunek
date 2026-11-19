//! Attach, answer, detach, then output and input.
//!
//! After `attach` + `detach` the daemon keeps draining the session PTY
//! output and writing to the live PTY handle: the agent never blocks on
//! stdout, runs its hooks and processes input, and the daemon's screen and
//! `updated_at` keep moving.
//!
//! The fake dialog agent draws a trust dialog, blocks on a gate file, prints
//! more than 64 KiB, emits a handshake line, then echoes each stdin line
//! prefixed with `echo:`. The scenario attaches through the real attach
//! path, answers, detaches, releases the gate and records:
//! - whether output after detach reaches the daemon screen,
//! - whether the handshake arrives via `session.output`,
//! - whether input written after detach reaches the agent.
//!
//! The same steps without attach (answer injected via `session.input`)
//! run for contrast.

// Rust guideline compliant 2026-06-26

use super::*;

/// Fake dialog agent: draws a trust dialog, blocks on `$1` (the gate file),
/// prints more than 64 KiB, emits the handshake line, then echoes stdin.
const DIALOG_SCRIPT: &str = "printf 'DIALOG: trust this folder? [y/n]\\n'; while [ ! -e \"$1\" ]; do sleep 0.05; done; head -c 70000 /dev/zero | tr '\\0' A; printf '\\nHANDSHAKE-PUMP\\n'; while IFS= read -r line; do printf 'echo:%s\\n' \"$line\"; done";

/// The PTY text the session has printed so far, decoded from `session.output`.
async fn terminal_text(framed: &mut Framed<UnixStream, LinesCodec>, id: &SessionId) -> String {
    use base64::Engine as _;

    let params = protocol::SessionOutputParams::new(id.clone(), None, None, 262_144, None)
        .expect("valid output params");
    let request = Request::make(
        "attach-detach-output",
        method::SESSION_OUTPUT,
        serde_json::to_value(params).expect("output params"),
    );
    let output: protocol::SessionOutputResult =
        serde_json::from_value(ok_payload(exchange(framed, &request).await))
            .expect("output result");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(output.data_base64())
        .expect("output data is base64");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Reads retained output until `marker` appears or the wait guard fires.
async fn output_until(
    framed: &mut Framed<UnixStream, LinesCodec>,
    id: &SessionId,
    marker: &str,
    what: &str,
) {
    wait::guard(what, async {
        loop {
            if terminal_text(framed, id).await.contains(marker) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

async fn read_screen_text(framed: &mut Framed<UnixStream, LinesCodec>, id: &SessionId) -> String {
    let req = Request::make(
        "attach-detach-read",
        method::SESSION_READ,
        serde_json::to_value(
            protocol::SessionReadParams::new(
                id.clone(),
                Some(protocol::SessionReadSource::Visible),
                None,
                None,
            )
            .expect("valid read params"),
        )
        .expect("serialize read params"),
    );
    let result: protocol::SessionReadResult =
        serde_json::from_value(ok_payload(exchange(framed, &req).await)).expect("read result");
    result.text
}

fn dialog_config(gate: &std::path::Path) -> SessionRegistryConfig {
    let script = format!("gate=\"$1\"; {DIALOG_SCRIPT}");
    SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            [
                "-c".to_owned(),
                script,
                "dialog-agent".to_owned(),
                gate.to_str().expect("UTF-8 gate path").to_owned(),
            ],
        ),
        ..SessionRegistryConfig::default()
    }
}

#[tokio::test]
async fn attach_answer_detach_then_output_and_input() {
    let socket = temp_socket("attach-detach-pump");
    let gate = socket.dir.join("dialog.gate");
    let (shutdown, handle, _) =
        spawn_server_with_config(&socket, "0.0.0", dialog_config(&gate)).await;

    let mut control = connect(&socket).await;
    let created = create_session(&mut control, &socket).await;

    // Attach through the real attach path and read the dialog prompt.
    let attach = attach_session(&mut control, &created.id).await;
    let mut raw = open_attach_stream(&socket, &attach.stream_id).await;
    let _ = read_until_marker(&mut raw, b"DIALOG: trust this folder?").await;

    // Answer the dialog like an operator, then detach.
    raw.write_all(b"y\n").await.expect("answer the dialog");
    let detached = detach_stream(&mut control, &attach.stream_id).await;
    assert!(detached.detached, "detach should close the stream");
    assert_raw_stream_closes(&mut raw).await;

    // Release the agent: >64 KiB, handshake, echo loop.
    std::fs::File::create(&gate).expect("open the gate");
    output_until(
        &mut control,
        &created.id,
        "HANDSHAKE-PUMP",
        "the handshake after detach reaches retained output",
    )
    .await;
    let echoed = input_session(&mut control, &created.id, "hello-after-detach").await;
    assert!(echoed.accepted, "input after detach is accepted");
    output_until(
        &mut control,
        &created.id,
        "echo:hello-after-detach",
        "input after detach is echoed by the agent",
    )
    .await;

    let screen = read_screen_text(&mut control, &created.id).await;
    let inspected = inspect_session(&mut control, &created.id).await;
    eprintln!(
        "after detach: handshake_on_screen={} state={:?} activity={:?} updated_at={}",
        screen.contains("HANDSHAKE"),
        inspected.state,
        inspected.activity,
        inspected.updated_at
    );

    stop_session(&mut control, &created.id).await;
    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn no_attach_answer_via_input_for_contrast() {
    let socket = temp_socket("no-attach-pump");
    let gate = socket.dir.join("dialog.gate");
    let (shutdown, handle, _) =
        spawn_server_with_config(&socket, "0.0.0", dialog_config(&gate)).await;

    let mut control = connect(&socket).await;
    let created = create_session(&mut control, &socket).await;

    // Answer injected via `session.input`, never through an attach stream.
    let answered = input_session(&mut control, &created.id, "y").await;
    assert!(answered.accepted, "the contrast answer is accepted");
    std::fs::File::create(&gate).expect("open the gate");
    output_until(
        &mut control,
        &created.id,
        "HANDSHAKE-PUMP",
        "the contrast handshake reaches retained output",
    )
    .await;
    let echoed = input_session(&mut control, &created.id, "hello-no-attach").await;
    assert!(echoed.accepted, "the contrast input is accepted");
    output_until(
        &mut control,
        &created.id,
        "echo:hello-no-attach",
        "the contrast input is echoed by the agent",
    )
    .await;

    stop_session(&mut control, &created.id).await;
    let _ = shutdown.send(());
    let _ = handle.await;
}
