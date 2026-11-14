//! Process-sourced activity never acknowledges a waited input on a session
//! whose manifest declares a program status source (#2140).
//!
//! The agent is a `/bin/sh` loop run under the Claude profile: the terminal
//! echo and its `got:` replies are real PTY output that the detector reads as
//! `Working/Process`. The clock is paused after the input is on the wire, so
//! the deadline that must expire is a virtual timer and the output that
//! precedes it is ordered by the events the daemon publishes.

// Rust guideline compliant 2026-11-11

use super::*;

const AGENT_PROFILE: &str = "process-evidence-claude";

/// Any non-zero Claude submit delay selects the delayed-submit framing, which
/// writes Enter on its own after the body.
const DELAYED_SUBMIT: Duration = Duration::from_millis(20);

/// The `session.new` acknowledgement window of the initial-input scenario.
const ACK_WINDOW_MS: u32 = 5_000;

/// A Claude profile over a shell that echoes every line it reads, with
/// `submit_delay` between the body and Enter; `Duration::ZERO` makes the
/// input one atomic plan.
fn process_evidence_registry(tag: &str, submit_delay: Duration) -> SessionRegistry {
    let agents_dir = temp_agents_dir_with(
        tag,
        AGENT_PROFILE,
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", 'while IFS= read -r line; do echo got:$line; done']\n",
    );
    SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        agents_dir: Some(agents_dir),
        submit_delay_overrides: submit_delay_override("claude", submit_delay),
        ..SessionRegistryConfig::default()
    })
}

async fn create_session(registry: &SessionRegistry) -> SessionId {
    let mut create = params();
    create.agent = AGENT_PROFILE.to_owned();
    let id = registry
        .create(create)
        .await
        .expect("create the Claude profile session")
        .id;
    let declares_status = {
        let sessions = registry.inner.sessions.lock().await;
        let update = sessions[&id].detector_config.borrow().clone();
        crate::session::program_status_app(&update.config).is_some()
    };
    assert!(
        declares_status,
        "the Claude manifest declares a status source"
    );
    id
}

/// Waits until the detector published a process-sourced `working`.
async fn process_working_published(
    events: &mut tokio::sync::broadcast::Receiver<Arc<protocol::Event>>,
) {
    pohunek_test_support::wait::guard("the detector to publish Working/Process", async {
        loop {
            match events.recv().await {
                Ok(event)
                    if event.event() == protocol::event::AGENT_STATE
                        && event.payload()["activity"] == "working"
                        && event.payload()["source"] == "process" =>
                {
                    return;
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("event channel closed")
                }
            }
        }
    })
    .await;
}

/// Runs a waited input for `target` to its deadline and returns its outcome.
/// `released_by` runs once Enter was sent, before the clock is paused.
async fn expire_waited_input<F: std::future::Future<Output = ()>>(
    registry: &SessionRegistry,
    id: &SessionId,
    target: AgentActivity,
    released_by: F,
) -> Result<protocol::SessionInputResult, protocol::ProtocolError> {
    let sent = Arc::new(tokio::sync::Barrier::new(2));
    registry.hold_next_input_sent(Arc::clone(&sent));
    let call = spawn_waited_input(
        registry,
        id,
        "m",
        vec![target],
        protocol::MAX_SESSION_WAIT_MS,
    );
    reach_boundary(&sent).await;
    released_by.await;
    // Enter is sent and the output it caused is on record: from here the
    // deadline is a virtual timer that only `advance` moves.
    let inhibitor = AutoAdvanceInhibitor::new();
    tokio::time::pause();
    reach_boundary(&sent).await;
    tokio::time::advance(
        Duration::from_millis(u64::from(protocol::MAX_SESSION_WAIT_MS)) + TIMER_TICK,
    )
    .await;
    let outcome = call.await.expect("waited input task joins");
    tokio::time::resume();
    inhibitor.release().await;
    outcome
}

#[tokio::test]
async fn terminal_output_after_the_write_does_not_acknowledge_a_working_wait() {
    let registry = process_evidence_registry("process-evidence-output", Duration::ZERO);
    let id = create_session(&registry).await;
    let mut events = registry.subscribe();

    let outcome = expire_waited_input(&registry, &id, AgentActivity::Working, async {
        process_working_published(&mut events).await;
    })
    .await;
    let error = outcome.expect_err("process output is not a turn");
    assert_eq!(error.code, "session_input_timeout");
    registry.stop(&id).await.expect("stop the session");
}

/// Guards the revision boundary: `Working/Process` already on record when the
/// next input is written is not evidence of that input, whatever its source.
#[tokio::test]
async fn process_activity_recorded_before_the_write_does_not_acknowledge_a_working_wait() {
    let registry = process_evidence_registry("process-evidence-before", Duration::ZERO);
    let id = create_session(&registry).await;
    let mut events = registry.subscribe();
    registry
        .input(protocol::SessionInputParams {
            session_id: id.clone(),
            text: "a".to_owned(),
            wait: None,
        })
        .await
        .expect("type the first input");
    process_working_published(&mut events).await;

    let outcome = expire_waited_input(&registry, &id, AgentActivity::Working, async {}).await;
    let error = outcome.expect_err("earlier activity is not evidence of this input");
    assert_eq!(error.code, "session_input_timeout");
    registry.stop(&id).await.expect("stop the session");
}

/// Output followed by an idle record is not a turn: on delayed-submit framing
/// an `idle` target needs `working` evidence that is not process output.
#[tokio::test]
async fn terminal_output_then_an_idle_record_does_not_acknowledge_an_idle_wait() {
    let registry = process_evidence_registry("process-evidence-idle", DELAYED_SUBMIT);
    let id = create_session(&registry).await;
    let record = |activity, source| ActivityTransition {
        activity,
        source,
        blocked_kind: None,
        fallback: false,
    };

    // A program status `working` followed by idle is a turn.
    let sent = Arc::new(tokio::sync::Barrier::new(2));
    registry.hold_next_input_sent(Arc::clone(&sent));
    let call = spawn_waited_input(
        &registry,
        &id,
        "m",
        vec![AgentActivity::Idle],
        protocol::MAX_SESSION_WAIT_MS,
    );
    reach_boundary(&sent).await;
    registry
        .record_activity(
            &id,
            record(AgentActivity::Working, StateSource::OscProgress),
        )
        .await;
    registry
        .record_activity(&id, record(AgentActivity::Idle, StateSource::OscProgress))
        .await;
    reach_boundary(&sent).await;
    let result = call
        .await
        .expect("waited input task joins")
        .expect("a program status turn answers the idle target");
    assert_eq!(result.activity, Some(AgentActivity::Idle));
    assert_eq!(result.activity_source, Some(StateSource::OscProgress));

    // Process output followed by idle is not.
    let outcome = expire_waited_input(&registry, &id, AgentActivity::Idle, async {
        registry
            .record_activity(&id, record(AgentActivity::Working, StateSource::Process))
            .await;
        registry
            .record_activity(&id, record(AgentActivity::Idle, StateSource::Screen))
            .await;
    })
    .await;
    let error = outcome.expect_err("process output followed by idle is not a turn");
    assert_eq!(error.code, "session_input_timeout");
    registry.stop(&id).await.expect("stop the session");
}

/// `session.new` with an acknowledgement window counts only evidence of the
/// turn: process output after the initial input leaves `prompt_delivered`
/// false and keeps the session.
#[tokio::test]
async fn terminal_output_after_the_initial_input_does_not_deliver_the_prompt() {
    let registry = process_evidence_registry("process-evidence-new", Duration::ZERO);
    let mut events = registry.subscribe();
    let sent = Arc::new(tokio::sync::Barrier::new(2));
    registry.hold_next_input_sent(Arc::clone(&sent));
    arm_ready_mark(&registry);
    let mut create = params();
    create.agent = AGENT_PROFILE.to_owned();
    create.input = Some("m".to_owned());
    create.input_ack_timeout_ms = Some(ACK_WINDOW_MS);
    let create = tokio::spawn({
        let registry = registry.clone();
        async move { create_acknowledged(&registry, create).await }
    });

    reach_boundary(&sent).await;
    let inhibitor = AutoAdvanceInhibitor::new();
    tokio::time::pause();
    process_working_published(&mut events).await;
    reach_boundary(&sent).await;
    // The window opens once the write returned; advance a window at a time
    // until it closed, for at most `HANG_GUARD` of virtual time.
    let step = Duration::from_millis(u64::from(ACK_WINDOW_MS)) + TIMER_TICK;
    let max_steps = pohunek_test_support::wait::HANG_GUARD.as_millis() / step.as_millis();
    let mut steps = 0;
    while !create.is_finished() {
        assert!(
            steps < max_steps,
            "the acknowledgement window closes within the hang guard"
        );
        tokio::time::advance(step).await;
        tokio::task::yield_now().await;
        steps += 1;
    }
    let (info, prompt_delivered) = create
        .await
        .expect("create task joins")
        .expect("an unacknowledged prompt keeps the session");
    tokio::time::resume();
    inhibitor.release().await;
    assert_eq!(prompt_delivered, Some(false));
    assert!(
        registry
            .list()
            .await
            .iter()
            .any(|listed| listed.id == info.id),
        "the session is kept"
    );
    registry.stop(&info.id).await.expect("stop the session");
}
