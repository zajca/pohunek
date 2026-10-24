//! Process-level scenarios of the Tailscale listener of `pohunekd`.
//!
//! A real daemon runs with the bootstrap environment of a service job and a
//! fake `tailscale` on `PATH`. A `bind()` interposer records the address the
//! daemon asks the OS to bind and moves the socket to loopback, so a scenario
//! learns what the daemon bound without a Tailscale interface. A scenario
//! that expects no bind waits for the fake CLI to have served the failing
//! document, switches to a usable one, and requires the capture to name only
//! the usable address.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use pohunek_platform::supervisor::BOOTSTRAP_ENV_ALLOWLIST;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::poll_until;
use pohunek_test_support::{bin_exe, bind_redirect, worker_binary};

#[path = "support/connect_redirect.rs"]
mod connect_redirect;
#[path = "support/tailscale_status.rs"]
mod tailscale_status;

use tailscale_status::{FakeTailscale, Node, Status};

/// The port `overlays.toml` configures. The daemon's bind is moved to an
/// ephemeral loopback port, so the number never occupies a real socket.
const PORT: u16 = 9741;

/// The address the usable status reports for this machine.
const SELF_ADDRESS: &str = "100.64.0.7";

/// A daemon process, killed and reaped on drop.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One hermetic daemon environment.
struct Host {
    env: TestEnv,
    fake: FakeTailscale,
    bind_library: PathBuf,
    bind_capture: PathBuf,
    log: PathBuf,
}

impl Host {
    /// A home whose `overlays.toml` (when `configured`) selects Tailscale.
    fn new(configured: bool) -> Self {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let fake = FakeTailscale::new(env.root());
        fs::create_dir_all(fake.bin_dir()).expect("create the bin directory");
        if configured {
            let dir = env.config_home().join("pohunek");
            fs::create_dir_all(&dir).expect("create the config directory");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
                .expect("make the config directory private");
            let file = dir.join("overlays.toml");
            fs::write(
                &file,
                format!("providers = [\"tailscale\"]\nport = {PORT}\n"),
            )
            .expect("write overlays.toml");
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
                .expect("make overlays.toml private");
        }
        let (bind_library, bind_capture) = bind_redirect::build(env.root());
        let log = env.root().join("daemon.out");
        Self {
            env,
            fake,
            bind_library,
            bind_capture,
            log,
        }
    }

    /// Starts the daemon with the environment of the service job: the
    /// bootstrap allowlist and the executable search path, nothing else.
    fn start(&self) -> Daemon {
        let mut daemon = Command::new(bin_exe("pohunek").with_file_name("pohunekd"));
        daemon.env_clear();
        for key in BOOTSTRAP_ENV_ALLOWLIST {
            if let Some(value) = self.env.environment().get(std::ffi::OsStr::new(key)) {
                daemon.env(key, value);
            }
        }
        let log = fs::File::create(&self.log).expect("create the daemon log");
        let child = daemon
            .env("PATH", self.fake.bin_dir())
            .env("LD_PRELOAD", &self.bind_library)
            .env(bind_redirect::CAPTURE_ENV, &self.bind_capture)
            .env("SHELL", "/bin/sh")
            .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
            .env("POHUNEK_WORKER_BIN", worker_binary())
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("duplicate the log handle"))
            .stderr(log)
            .spawn()
            .expect("spawn pohunekd");
        Daemon(child)
    }

    /// The bind lines captured so far, one `<ip>:<port> -> 127.0.0.1:<port>`
    /// per bind.
    fn binds(&self) -> Vec<String> {
        fs::read_to_string(&self.bind_capture)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Waits until the capture holds `count` binds.
    fn wait_for_binds(&self, count: usize) -> Vec<String> {
        poll_until(&format!("the daemon to bind {count} time(s)"), || {
            Some(self.binds()).filter(|binds| binds.len() >= count)
        })
    }

    /// The detail of the daemon's `overlay_tailscale_listener` check, once the
    /// daemon answers `daemon.doctor` with its overlay report.
    fn listener_check_detail(&self) -> String {
        poll_until("the daemon to report its listener check", || {
            let output = self
                .env
                .command(bin_exe("pohunek"))
                .env("PATH", self.fake.bin_dir())
                .args(["doctor", "--json"])
                .output()
                .expect("run doctor");
            let document: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
            document["ok"]["checks"]
                .as_array()?
                .iter()
                .find(|check| check["name"] == "overlay_tailscale_listener")
                .and_then(|check| check["detail"].as_str().map(str::to_owned))
        })
    }

    fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Waits until the daemon log carries `fragment`.
    fn wait_for_log(&self, fragment: &str) {
        poll_until(&format!("the daemon log to carry {fragment:?}"), || {
            self.log_text().contains(fragment).then_some(())
        });
    }
}

/// The local port a captured bind line was redirected to.
fn redirected_port(line: &str, original: &str) -> u16 {
    line.strip_prefix(&format!("{original} -> 127.0.0.1:"))
        .unwrap_or_else(|| panic!("{line:?} binds {original}"))
        .parse()
        .expect("the redirected loopback port")
}

/// A usable-looking address the unsafe-state documents carry, so a bind in an
/// unsafe state is told apart from the bind of the usable document.
const UNSAFE_STATE_ADDRESS: &str = "100.64.0.66";

fn unsafe_state() -> Status {
    Status::running(UNSAFE_STATE_ADDRESS, vec![])
}

fn usable() -> Status {
    Status::running(SELF_ADDRESS, vec![])
}

/// The daemon binds this node's IPv4 address on the configured port, and a
/// client reaches it through an IPv4 and through an IPv6 peer route.
#[test]
fn the_daemon_binds_the_ipv4_address_and_serves_peers_on_both_families() {
    let host = Host::new(true);
    host.fake.install(&Status::running(
        SELF_ADDRESS,
        vec![
            Node::peer("remote4", "nREMOT4001CNTRL", &["100.64.0.8"]),
            Node::peer("remote6", "nREMOT6001CNTRL", &["fd7a:115c:a1e0::9"]),
        ],
    ));
    let _daemon = host.start();
    let binds = host.wait_for_binds(1);
    assert_eq!(binds.len(), 1, "one listener per overlay: {binds:?}");
    let local_port = redirected_port(&binds[0], &format!("{SELF_ADDRESS}:{PORT}"));
    assert_ne!(local_port, 0, "the redirected bind succeeded: {binds:?}");

    let (connect_library, connect_capture) = connect_redirect::build(host.env.root());
    poll_until("the redirected listener to accept connections", || {
        std::net::TcpStream::connect(("127.0.0.1", local_port))
            .ok()
            .map(drop)
    });
    for (selector, dialed) in [
        ("remote4", format!("100.64.0.8:{PORT}")),
        ("remote6", format!("[fd7a:115c:a1e0::9]:{PORT}")),
    ] {
        let _ = fs::remove_file(&connect_capture);
        let output = host
            .env
            .command(bin_exe("pohunek"))
            .env("PATH", host.fake.bin_dir())
            .env("LD_PRELOAD", &connect_library)
            .env("POHUNEK_TEST_CONNECT_CAPTURE", &connect_capture)
            .env("POHUNEK_TEST_CONNECT_REDIRECT_PORT", local_port.to_string())
            .args(["--host", selector, "session", "list", "--json"])
            .output()
            .expect("run the remote CLI");
        let capture = fs::read_to_string(&connect_capture).unwrap_or_default();
        assert_eq!(
            capture.lines().collect::<Vec<_>>(),
            [dialed.as_str()],
            "{selector}"
        );
        assert!(
            output.status.success(),
            "{selector}: the daemon answers: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(host.binds().len(), 1, "no second listener appeared");
}

/// A status that offers no safe listener address never leads to a bind, and
/// the daemon binds as soon as a usable status follows.
#[test]
fn an_unsafe_listener_address_is_never_bound() {
    let ipv6_only = Node::peer("self-box", "nSELF00001CNTRL", &["fd7a:115c:a1e0::7"]);
    let cases: Vec<(&str, Status)> = vec![
        ("NeedsLogin", unsafe_state().with_state("NeedsLogin")),
        (
            "InUseOtherUser",
            unsafe_state().with_state("InUseOtherUser"),
        ),
        (
            "NeedsMachineAuth",
            unsafe_state().with_state("NeedsMachineAuth"),
        ),
        ("Stopped", unsafe_state().with_state("Stopped")),
        ("userspace networking", unsafe_state().userspace()),
        ("no IPv4 address", usable().with_this(ipv6_only)),
        (
            "ChromeOS range address",
            Status::running("100.115.92.5", vec![]),
        ),
        (
            "service address",
            Status::running("100.100.100.100", vec![]),
        ),
        (
            "address outside the range",
            Status::running("10.1.2.3", vec![]),
        ),
        ("loopback address", Status::running("127.0.0.1", vec![])),
    ];
    std::thread::scope(|scope| {
        for (label, unsafe_status) in &cases {
            scope.spawn(move || {
                let host = Host::new(true);
                host.fake.install(unsafe_status);
                let _daemon = host.start();
                // The first document is served: the daemon has seen the unsafe
                // state and must have declined to bind it.
                poll_until("the fake CLI to serve the unsafe document", || {
                    (host.fake.invocations() >= 1).then_some(())
                });
                host.fake.set_status(&usable());
                let binds = host.wait_for_binds(1);
                assert_eq!(
                    binds.len(),
                    1,
                    "{label}: only the usable address is bound: {binds:?}"
                );
                assert!(
                    binds[0].starts_with(&format!("{SELF_ADDRESS}:{PORT} -> ")),
                    "{label}: {binds:?}"
                );
            });
        }
    });
}

/// The daemon's listener check names why a node has no address, and nothing is
/// bound, for each state that cannot carry overlay traffic.
#[test]
fn the_listener_check_names_why_the_node_has_no_address() {
    let ipv6_only = Node::peer("self-box", "nSELF00001CNTRL", &["fd7a:115c:a1e0::7"]);
    let cases: Vec<(&str, Status, &str)> = vec![
        (
            "userspace networking",
            unsafe_state().userspace(),
            "userspace networking",
        ),
        (
            "a stopped node",
            unsafe_state().with_state("Stopped"),
            "not running (state: Stopped)",
        ),
        (
            "no IPv4 address",
            usable().with_this(ipv6_only),
            "no IPv4 node address",
        ),
    ];
    for (label, status, reason) in cases {
        let host = Host::new(true);
        host.fake.install(&status);
        let _daemon = host.start();
        let detail = host.listener_check_detail();
        assert!(
            detail.contains(&format!("port={PORT};")) && detail.contains(reason),
            "{label}: {detail}"
        );
        assert!(host.binds().is_empty(), "{label}: nothing is bound");
    }
}

/// An overlay the owner listed keeps retrying while its CLI is missing, binds
/// once the CLI appears, and rebinds when the node's address changes.
#[test]
fn a_listed_overlay_retries_for_its_cli_and_follows_an_address_change() {
    let host = Host::new(true);
    let _daemon = host.start();
    host.wait_for_log("overlay CLI missing; serving local-only and retrying");
    assert!(host.binds().is_empty(), "nothing to bind without the CLI");

    host.fake.install(&usable());
    let binds = host.wait_for_binds(1);
    redirected_port(&binds[0], &format!("{SELF_ADDRESS}:{PORT}"));

    let moved = Status::running("100.64.0.9", vec![]);
    host.fake.set_status(&moved);
    let binds = host.wait_for_binds(2);
    assert_eq!(binds.len(), 2, "{binds:?}");
    redirected_port(&binds[1], &format!("100.64.0.9:{PORT}"));
}

/// With no `overlays.toml` the `NetBird` default stays disabled while its CLI
/// is missing.
#[test]
fn the_unconfigured_default_stays_disabled_without_its_cli() {
    let host = Host::new(false);
    let _daemon = host.start();
    host.wait_for_log("overlay CLI missing; listener disabled");
    assert!(
        !host.log_text().contains("retrying"),
        "the default does not retry: {}",
        host.log_text()
    );
    assert!(host.binds().is_empty());
}
