//! The reporter scripts' report identity and retry of a refused admission.
//!
//! Every scenario runs the real `pohunek-agent-notify.sh` or
//! `pohunek-agent-state.sh` against a scripted daemon socket that records each
//! request and answers as the scenario says. The scripts get no `claude` or
//! `codex` executable and synthetic hook input; retry policy values reach them
//! through the launch environment, as the daemon sets them for a session.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pohunek_test_support::wait::guard;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

const SESSION: &str = "s-7001";
const OTHER_SESSION: &str = "s-7002";
const AGENTS: [&str; 2] = ["claude", "codex"];
const RETRYABLE: &str = "hook_schema_unavailable";

/// Retry policy a scenario launches the script with: a short deadline keeps
/// the exhausted case brief without the test waiting on a clock of its own.
/// All values are milliseconds.
pub(crate) struct Policy {
    pub(crate) deadline: u32,
    pub(crate) backoff: u32,
    pub(crate) backoff_max: u32,
}

const POLICY: Policy = Policy {
    deadline: 400,
    backoff: 10,
    backoff_max: 40,
};

/// What the scripted daemon answers to one request.
#[derive(Clone)]
enum Answer {
    Ok,
    Err(&'static str),
    Silent,
}

/// Scripted daemon: answers the n-th request with the n-th answer, and the
/// last answer to every later one.
struct ScriptedDaemon {
    requests: Arc<Mutex<Vec<Value>>>,
    socket: PathBuf,
    _dir: crate::test_support::ScopedDir,
}

impl ScriptedDaemon {
    fn start(answers: Vec<Answer>) -> Self {
        let dir = crate::test_support::scoped_dir("ph-reporter-retry-");
        let socket = dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind the scripted daemon");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        tokio::spawn(async move {
            while let Ok((stream, _addr)) = listener.accept().await {
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                if BufReader::new(read).read_line(&mut line).await.is_err() {
                    continue;
                }
                let Ok(request) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let index = {
                    let mut seen = seen.lock().expect("requests lock");
                    seen.push(request.clone());
                    seen.len() - 1
                };
                let answer = answers
                    .get(index)
                    .or_else(|| answers.last())
                    .cloned()
                    .unwrap_or(Answer::Ok);
                let body = match answer {
                    Answer::Silent => continue,
                    Answer::Ok => json!({"ok": {"recorded": true}}),
                    Answer::Err(code) => {
                        json!({"err": {"class": "daemon", "code": code, "msg": "scripted"}})
                    }
                };
                let mut reply = json!({"v": protocol::PROTOCOL_VERSION.get(), "id": request["id"]});
                reply
                    .as_object_mut()
                    .expect("reply object")
                    .extend(body.as_object().expect("body object").clone());
                let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
            }
        });
        Self {
            requests,
            socket,
            _dir: dir,
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("requests lock").clone()
    }

    fn requests_of(&self, method: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|request| request["method"] == method)
            .collect()
    }
}

/// One script run: which script, with which environment and stdin.
pub(crate) struct Run<'a> {
    agent: &'a str,
    script: &'a str,
    pub(crate) args: &'a [&'a str],
    stdin: &'a str,
    pub(crate) session: &'a str,
    pub(crate) policy: Option<&'a Policy>,
    extra_env: &'a [(&'a str, String)],
}

impl<'a> Run<'a> {
    pub(crate) fn notify(agent: &'a str, stdin: &'a str) -> Self {
        let args: &[&str] = if agent == "claude" {
            &["notification", "auth_success"]
        } else {
            &["permission_request"]
        };
        Self {
            agent,
            script: "pohunek-agent-notify.sh",
            args,
            stdin,
            session: SESSION,
            policy: None,
            extra_env: &[],
        }
    }

    fn state(agent: &'a str) -> Self {
        Self {
            agent,
            script: "pohunek-agent-state.sh",
            args: &["session"],
            stdin: "{\"session_id\":\"native-1\"}",
            session: SESSION,
            policy: None,
            extra_env: &[],
        }
    }

    /// Runs the script against the scripted daemon and returns the hook trace
    /// it wrote.
    async fn run(&self, daemon: &ScriptedDaemon) -> String {
        self.run_at(&daemon.socket).await
    }

    /// Runs the script against the daemon socket at `socket` and returns the
    /// hook trace it wrote.
    pub(crate) async fn run_at(&self, socket: &std::path::Path) -> String {
        let dir = crate::test_support::scoped_dir("ph-reporter-retry-run-");
        let trace = dir.join("hook-trace.log");
        let script = super::runnable_script(self.agent, self.script);
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg(&script)
            .args(self.args)
            .env_clear()
            .env("PATH", super::tests::inherited_path())
            .env("TMPDIR", &*dir)
            .env("POHUNEK_ENV", "1")
            .env("POHUNEK_SESSION_ID", self.session)
            .env("POHUNEK_SOCKET_PATH", socket)
            .env(
                "POHUNEK_PROTOCOL_VERSION",
                protocol::PROTOCOL_VERSION.get().to_string(),
            )
            .env("POHUNEK_HOOK_TRACE_PATH", &trace)
            .env("POHUNEK_HOOK_TRACE_MAX_BYTES", "65536")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(policy) = self.policy {
            command
                .env("POHUNEK_HOOK_ADMISSION_RETRY", "1")
                .env(
                    "POHUNEK_HOOK_RETRY_DEADLINE_MS",
                    policy.deadline.to_string(),
                )
                .env("POHUNEK_HOOK_RETRY_BACKOFF_MS", policy.backoff.to_string())
                .env(
                    "POHUNEK_HOOK_RETRY_BACKOFF_MAX_MS",
                    policy.backoff_max.to_string(),
                );
        }
        for (name, value) in self.extra_env {
            command.env(name, value);
        }
        let mut child = command.spawn().expect("spawn the reporter script");
        let mut stdin = child.stdin.take().expect("script stdin");
        stdin
            .write_all(self.stdin.as_bytes())
            .await
            .expect("write the hook input");
        drop(stdin);
        let status = guard("the reporter script", child.wait())
            .await
            .expect("wait for the reporter script");
        assert!(status.success(), "a hook always exits 0: {status:?}");
        std::fs::read_to_string(&trace).unwrap_or_default()
    }
}

fn source_id(request: &Value) -> &str {
    request["params"]["source_id"]
        .as_str()
        .expect("the notification carries a source id")
}

#[tokio::test(flavor = "multi_thread")]
async fn two_sessions_firing_the_same_event_send_different_source_ids() {
    for agent in AGENTS {
        let daemon = ScriptedDaemon::start(vec![Answer::Ok]);
        let mut first = Run::notify(agent, "{}");
        first.session = SESSION;
        first.run(&daemon).await;
        let mut second = Run::notify(agent, "{}");
        second.session = OTHER_SESSION;
        second.run(&daemon).await;

        let requests = daemon.requests_of("notification.create");
        assert_eq!(requests.len(), 2, "{agent}: {requests:?}");
        let (one, two) = (source_id(&requests[0]), source_id(&requests[1]));
        assert_ne!(one, two, "{agent}: the sessions never share a key");
        assert!(
            one.starts_with(&format!("hook:{agent}:{SESSION}:")),
            "{agent}: {one}"
        );
        assert!(
            two.starts_with(&format!("hook:{agent}:{OTHER_SESSION}:")),
            "{agent}: {two}"
        );
        for request in &requests {
            assert_eq!(
                request["params"]["source"]["host_local_source_id"], request["params"]["source_id"],
                "{agent}: the host-local id is the source id"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn one_session_firing_the_same_event_twice_gets_a_fresh_random_id_each_time() {
    for agent in AGENTS {
        let daemon = ScriptedDaemon::start(vec![Answer::Ok]);
        Run::notify(agent, "{}").run(&daemon).await;
        Run::notify(agent, "{}").run(&daemon).await;
        let requests = daemon.requests_of("notification.create");
        assert_ne!(
            source_id(&requests[0]),
            source_id(&requests[1]),
            "{agent}: an invocation without a payload id never repeats another's key"
        );
        let id = requests[0]["params"]["metadata"]["hook_event_id"]
            .as_str()
            .expect("the event id");
        assert_eq!(id.len(), 32, "{agent}: a 128-bit random id: {id}");
        assert!(
            id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{agent}: {id}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_payload_event_id_is_kept_in_the_session_scoped_key() {
    for agent in AGENTS {
        let daemon = ScriptedDaemon::start(vec![Answer::Ok]);
        Run::notify(agent, "{\"hook_event_id\":\"evt-77\"}")
            .run(&daemon)
            .await;
        let requests = daemon.requests_of("notification.create");
        assert_eq!(requests.len(), 1, "{agent}");
        let key = source_id(&requests[0]);
        assert!(
            key.starts_with(&format!("hook:{agent}:{SESSION}:")) && key.ends_with(":evt-77"),
            "{agent}: {key}"
        );
        assert_eq!(requests[0]["params"]["metadata"]["hook_event_id"], "evt-77");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_longer_than_the_daemon_keeps_is_replaced_by_a_digest() {
    for agent in AGENTS {
        let daemon = ScriptedDaemon::start(vec![Answer::Ok]);
        let long_session = format!("s-{}", "a".repeat(300));
        let mut run = Run::notify(agent, "{}");
        run.session = &long_session;
        run.run(&daemon).await;
        let requests = daemon.requests_of("notification.create");
        let key = source_id(&requests[0]);
        assert!(
            key.len() <= 256 && key.starts_with(&format!("hook:{agent}:sha256:")),
            "{agent}: {key}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retried_notification_keeps_the_random_id_of_its_first_attempt() {
    for agent in AGENTS {
        let daemon = ScriptedDaemon::start(vec![Answer::Err(RETRYABLE), Answer::Ok]);
        let mut run = Run::notify(agent, "{}");
        run.policy = Some(&POLICY);
        run.run(&daemon).await;
        let requests = daemon.requests_of("notification.create");
        assert_eq!(requests.len(), 2, "{agent}: {requests:?}");
        assert_eq!(
            source_id(&requests[0]),
            source_id(&requests[1]),
            "{agent}: the retry names the same event"
        );
        for request in &requests {
            assert_eq!(
                request["params"]["retryable_admission"],
                json!(true),
                "{agent}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_report_retried_past_its_deadline_gives_up_and_exits() {
    for agent in AGENTS {
        let daemon = ScriptedDaemon::start(vec![Answer::Err(RETRYABLE)]);
        let mut run = Run::notify(agent, "{}");
        run.policy = Some(&POLICY);
        run.run(&daemon).await;
        let attempts = daemon.requests_of("notification.create");
        assert!(
            attempts.len() >= 2,
            "{agent}: it retried: {}",
            attempts.len()
        );
        assert!(
            attempts
                .windows(2)
                .all(|pair| source_id(&pair[0]) == source_id(&pair[1])),
            "{agent}: every attempt names one event"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_state_report_gives_up_at_the_deadline_and_traces_it() {
    for agent in AGENTS {
        let daemon = ScriptedDaemon::start(vec![Answer::Err(RETRYABLE)]);
        let mut run = Run::state(agent);
        run.policy = Some(&POLICY);
        let trace = run.run(&daemon).await;
        assert!(
            daemon.requests_of("session.report_agent").len() >= 2,
            "{agent}: it retried"
        );
        assert!(trace.contains("daemon_retry"), "{agent}: {trace}");
        assert!(trace.contains("daemon_retry_exhausted"), "{agent}: {trace}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retried_state_report_keeps_its_identifiers() {
    for agent in AGENTS {
        let daemon = ScriptedDaemon::start(vec![Answer::Err(RETRYABLE), Answer::Ok]);
        let mut run = Run::state(agent);
        run.policy = Some(&POLICY);
        run.run(&daemon).await;
        let reports = daemon.requests_of("session.report_agent");
        assert_eq!(reports.len(), 2, "{agent}: {reports:?}");
        for key in [
            "seq",
            "monotonic_sequence",
            "agent_session_id",
            "session_id",
        ] {
            assert_eq!(
                reports[0]["params"][key], reports[1]["params"][key],
                "{agent}: {key} survives the retry"
            );
        }
        for report in &reports {
            assert_eq!(
                report["params"]["retryable_admission"],
                json!(true),
                "{agent}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_native_id_report_is_retried_with_the_agent_report_before_it() {
    for agent in AGENTS {
        // The agent report is admitted; the native-id report is not, then both
        // go out again and the second native-id report is admitted.
        let daemon = ScriptedDaemon::start(vec![
            Answer::Ok,
            Answer::Err(RETRYABLE),
            Answer::Ok,
            Answer::Ok,
        ]);
        let mut run = Run::state(agent);
        run.policy = Some(&POLICY);
        let worker_instance = [("POHUNEK_WORKER_INSTANCE_ID", "instance-1".to_owned())];
        run.extra_env = &worker_instance;
        run.run(&daemon).await;
        let methods: Vec<String> = daemon
            .requests()
            .iter()
            .map(|request| request["method"].as_str().expect("method").to_owned())
            .collect();
        assert_eq!(
            methods,
            [
                "session.report_agent",
                "session.report_native_id",
                "session.report_agent",
                "session.report_native_id"
            ],
            "{agent}"
        );
        let natives = daemon.requests_of("session.report_native_id");
        assert_eq!(
            natives[0]["params"]["sequence"],
            natives[1]["params"]["sequence"]
        );
        assert_eq!(natives[1]["params"]["retryable_admission"], json!(true));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retried_report_keeps_the_sequence_it_offered_the_worker_first() {
    for agent in ["claude", "codex"] {
        let daemon = ScriptedDaemon::start(vec![Answer::Err(RETRYABLE), Answer::Ok]);
        let worker = ScriptedDaemon::start(vec![Answer::Err("worker_refused")]);
        let mut run = Run::state(agent);
        run.policy = Some(&POLICY);
        let env = [
            ("POHUNEK_WORKER_INSTANCE_ID", "instance-1".to_owned()),
            (
                "POHUNEK_WORKER_SOCKET_PATH",
                worker.socket.display().to_string(),
            ),
            ("POHUNEK_AGENT_PID", std::process::id().to_string()),
        ];
        run.extra_env = &env;
        run.run(&daemon).await;
        let sequences: Vec<Value> = worker
            .requests()
            .iter()
            .map(|request| request["sequence"].clone())
            .collect();
        assert!(
            sequences.len() >= 2,
            "{agent}: the worker is offered every attempt"
        );
        assert!(
            sequences.windows(2).all(|pair| pair[0] == pair[1]),
            "{agent}: {sequences:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_release_is_retried_like_a_report() {
    let daemon = ScriptedDaemon::start(vec![Answer::Err(RETRYABLE), Answer::Ok]);
    let mut run = Run::state("claude");
    run.args = &["release"];
    run.policy = Some(&POLICY);
    run.run(&daemon).await;
    let releases = daemon.requests_of("session.release_agent");
    assert_eq!(releases.len(), 2, "{releases:?}");
    assert_eq!(releases[0]["params"]["seq"], releases[1]["params"]["seq"]);
    assert_eq!(releases[1]["params"]["retryable_admission"], json!(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_schema_denial_is_final_even_with_the_opt_in() {
    for agent in AGENTS {
        let daemon = ScriptedDaemon::start(vec![Answer::Err("hook_not_admitted"), Answer::Ok]);
        let mut run = Run::notify(agent, "{}");
        run.policy = Some(&POLICY);
        run.run(&daemon).await;
        assert_eq!(
            daemon.requests_of("notification.create").len(),
            1,
            "{agent}: a denial is not retried"
        );

        let daemon = ScriptedDaemon::start(vec![Answer::Err("hook_not_admitted"), Answer::Ok]);
        let mut run = Run::state(agent);
        run.policy = Some(&POLICY);
        let trace = run.run(&daemon).await;
        assert_eq!(
            daemon.requests_of("session.report_agent").len(),
            1,
            "{agent}"
        );
        assert!(trace.contains("daemon_refused"), "{agent}: {trace}");
        assert!(!trace.contains("daemon_retry"), "{agent}: {trace}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_launch_flag_no_opt_in_field_is_sent_and_nothing_is_retried() {
    for agent in AGENTS {
        // A policy with a missing value counts as no policy at all.
        let incomplete: &[(&str, String)] = &[("POHUNEK_HOOK_ADMISSION_RETRY", "1".to_owned())];
        for extra in [&[][..], incomplete] {
            let daemon = ScriptedDaemon::start(vec![Answer::Err(RETRYABLE)]);
            let mut run = Run::notify(agent, "{}");
            run.extra_env = extra;
            run.run(&daemon).await;
            let requests = daemon.requests_of("notification.create");
            assert_eq!(requests.len(), 1, "{agent}: {requests:?}");
            assert!(
                requests[0]["params"].get("retryable_admission").is_none(),
                "{agent}: {}",
                requests[0]
            );

            let daemon = ScriptedDaemon::start(vec![Answer::Err(RETRYABLE)]);
            let mut run = Run::state(agent);
            run.extra_env = extra;
            run.run(&daemon).await;
            let reports = daemon.requests_of("session.report_agent");
            assert_eq!(reports.len(), 1, "{agent}");
            assert!(
                reports[0]["params"].get("retryable_admission").is_none(),
                "{agent}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_daemon_ends_the_hook_without_a_retry() {
    for agent in AGENTS {
        let daemon = ScriptedDaemon::start(vec![Answer::Silent]);
        let mut run = Run::notify(agent, "{}");
        run.policy = Some(&POLICY);
        run.run(&daemon).await;
        assert_eq!(
            daemon.requests_of("notification.create").len(),
            1,
            "{agent}"
        );
    }
}

/// Seconds in the reporters' socket timeout, read from the installed scripts.
fn socket_timeout_ms(agent: &str, script: &str) -> u64 {
    let text = std::fs::read_to_string(super::runnable_script(agent, script)).expect("read script");
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("SOCKET_TIMEOUT_SECS = "))
        .expect("the script names its socket timeout");
    // Seconds as a decimal literal, scaled without floating point.
    let (whole, fraction) = value.trim().split_once('.').unwrap_or((value.trim(), "0"));
    let whole: u64 = whole.parse().expect("whole seconds");
    let fraction: u64 = format!("{fraction:0<3}")[..3]
        .parse()
        .expect("milliseconds");
    whole * 1_000 + fraction
}

/// Longest run of exchanges one delivery attempt makes: the worker route, the
/// agent report and the native-id report.
const EXCHANGES_PER_ATTEMPT: u64 = 3;

#[test]
fn the_admission_wait_and_the_retry_deadline_fit_the_reporter_and_the_hook_timeout() {
    let wait = u64::try_from(pohunek_service_config::DEFAULT_HOOK_ADMISSION_WAIT.as_millis())
        .expect("the admission wait fits in u64");
    let deadline = u64::try_from(super::hook_retry_deadline().as_millis())
        .expect("the retry deadline fits in u64");
    let hook_timeout = super::HOOK_TIMEOUT_SECS * 1_000;
    for agent in AGENTS {
        for script in ["pohunek-agent-notify.sh", "pohunek-agent-state.sh"] {
            let socket = socket_timeout_ms(agent, script);
            assert!(
                wait < socket,
                "{agent} {script}: the typed refusal arrives before the reporter's socket timeout"
            );
            assert!(
                deadline + EXCHANGES_PER_ATTEMPT * socket < hook_timeout,
                "{agent} {script}: a last attempt started at the deadline ends inside the hook timeout"
            );
        }
    }
    assert!(
        pohunek_service_config::HOOK_RETRY_BACKOFF
            <= pohunek_service_config::HOOK_RETRY_BACKOFF_MAX
    );
}
