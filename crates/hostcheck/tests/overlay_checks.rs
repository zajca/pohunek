//! The overlay doctor checks through the public probe list, for overlays other
//! than the one the binaries ship with.
//!
//! The checks derive their names and verdicts from the overlay id and the
//! provider's diagnostic alone, so a provider added later gets its checks
//! without a change in `hostcheck`.

use std::net::SocketAddr;

use hostcheck::{linux_checks, overlay_checks, OverlayChecks, StandardCheckInputs, Supervision};
use overlay::{OverlayDiagnostic, OverlayError, OverlayId};
use pohunek_test_support::tempdir;
use protocol::{DoctorCheck, DoctorStatus};

fn id(value: &str) -> OverlayId {
    OverlayId::new(value).expect("overlay id")
}

fn ready(overlay: &str, port: u16, listener: &str) -> OverlayDiagnostic {
    OverlayDiagnostic {
        overlay: id(overlay),
        port,
        listener: Some(listener.parse::<SocketAddr>().expect("listener address")),
        error: None,
    }
}

fn failed(overlay: &str, port: u16, error: OverlayError) -> OverlayDiagnostic {
    OverlayDiagnostic {
        overlay: id(overlay),
        port,
        listener: None,
        error: Some(error),
    }
}

fn status_of<'a>(checks: &'a [DoctorCheck], name: &str) -> &'a DoctorCheck {
    checks
        .iter()
        .find(|check| check.name == name)
        .unwrap_or_else(|| panic!("check {name} in {checks:?}"))
}

/// Every configured overlay gets its own `<overlay>_cli` check in the probe
/// list, between the directory checks and `schema_version`.
#[test]
fn each_configured_overlay_gets_a_cli_check_in_the_probe_list() {
    let root = tempdir().expect("fixture root");
    let diagnostics = [
        ready("netbird", 18722, "100.64.0.7:18722"),
        failed(
            "tailscale",
            18722,
            OverlayError::CliMissing(id("tailscale")),
        ),
    ];

    let checks = linux_checks(StandardCheckInputs {
        socket_dir: &root.path().join("run"),
        state_dir: &root.path().join("data"),
        log_dir: &root.path().join("logs"),
        config_dir: &root.path().join("config"),
        home_dir: None,
        effective_uid: 0,
        worker: None,
        access_dirs: &[],
        supervision: Supervision::Unknown,
        overlays: OverlayChecks {
            diagnostics: &diagnostics,
            from_file: true,
        },
    });

    let names: Vec<&str> = checks.iter().map(|check| check.name.as_str()).collect();
    let first = names.iter().position(|name| *name == "netbird_cli");
    assert_eq!(
        first.map(|position| &names[position..=position + 2]),
        Some(&["netbird_cli", "tailscale_cli", "schema_version"][..]),
        "{names:?}"
    );
    assert_eq!(status_of(&checks, "netbird_cli").status, DoctorStatus::Ok);
    let tailscale = status_of(&checks, "tailscale_cli");
    assert_eq!(tailscale.status, DoctorStatus::Fail, "{tailscale:?}");
    assert!(tailscale.detail.contains("tailscale"), "{tailscale:?}");
    assert!(tailscale.detail.contains("overlays.toml"), "{tailscale:?}");
}

/// The same missing CLI only warns when the overlay is the default of an absent
/// file, and a provider without a local address warns either way.
#[test]
fn a_missing_cli_fails_only_for_an_overlay_the_file_lists() {
    let diagnostics = [
        failed(
            "tailscale",
            18722,
            OverlayError::CliMissing(id("tailscale")),
        ),
        failed(
            "headscale",
            18722,
            OverlayError::ListenerAddressMissing(id("headscale")),
        ),
    ];

    let listed = overlay_checks::cli_checks(OverlayChecks {
        diagnostics: &diagnostics,
        from_file: true,
    });
    let default = overlay_checks::cli_checks(OverlayChecks {
        diagnostics: &diagnostics,
        from_file: false,
    });

    assert_eq!(listed[0].status, DoctorStatus::Fail);
    assert_eq!(default[0].status, DoctorStatus::Warn);
    assert_eq!(listed[1].name, "headscale_cli");
    assert_eq!(listed[1].status, DoctorStatus::Warn);
    assert_eq!(default[1].status, DoctorStatus::Warn);
}

/// The listener checks carry the id and the port, and the CLI reads both back
/// from the check list, ignoring unrelated checks.
#[test]
fn listener_checks_round_trip_their_overlay_and_port() {
    let diagnostics = [
        ready("netbird", 9731, "100.64.0.7:9731"),
        failed(
            "tailscale",
            9732,
            OverlayError::ListenerAddressMissing(id("tailscale")),
        ),
    ];
    let mut checks = overlay_checks::listener_checks(OverlayChecks {
        diagnostics: &diagnostics,
        from_file: true,
    });
    checks.push(DoctorCheck::new("netbird_cli", DoctorStatus::Ok, "found"));

    assert_eq!(checks[0].name, "overlay_netbird_listener");
    assert_eq!(checks[0].status, DoctorStatus::Ok);
    assert_eq!(checks[1].status, DoctorStatus::Warn);
    let reported = overlay_checks::reported_overlays(&checks);
    assert_eq!(
        reported
            .iter()
            .map(|overlay| (overlay.id.as_str(), overlay.port))
            .collect::<Vec<_>>(),
        [("netbird", Some(9731)), ("tailscale", Some(9732))]
    );
}

/// A daemon that reports no overlay check, as one of an earlier release does,
/// yields nothing to compare.
#[test]
fn a_check_list_without_listener_checks_reports_no_overlay() {
    let checks = [
        DoctorCheck::new("bin:git", DoctorStatus::Ok, "found"),
        DoctorCheck::new("overlay__listener", DoctorStatus::Ok, "port=1; empty id"),
    ];

    assert!(overlay_checks::reported_overlays(&checks).is_empty());
}
