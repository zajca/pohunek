//! The release consumer suite: one runtime driven out of process through the
//! exact release binaries.
//!
//! `the_release_binaries_serve_the_runtime_out_of_process` copies the release
//! `pohunek`, `pohunekd` and `pohunek-sessiond` into an installed layout,
//! starts the daemon as a subprocess in a hermetic environment, installs the
//! runtime's package archive through the CLI from a signed catalog, and runs a
//! real upstream agent against a loopback model stub: ready, one turn, stop,
//! resume. Only after every check passed does it write the consumer report
//! that the release attestation consumes. The contract (inputs, modes, how a
//! release row or the archive smoke calls it, how to add a driver) is in
//! `docs/development.md`, "Release consumer suite".
//!
//! The suite skips only when no `POHUNEK_CONSUMER_*` variable is set, which is
//! a plain `cargo test`. Once any is set, a missing prerequisite is a failure.
//! The remaining tests are hermetic regression scenarios of the harness
//! guards; they need no upstream and no model, only this build's binaries.

#![cfg(target_os = "linux")]

// Rust guideline compliant 2026-10-08

use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::poll_until;

#[path = "support/catalog_fixture.rs"]
mod catalog_fixture;
#[path = "support/messages_stub.rs"]
mod messages_stub;
#[path = "support/model_stub.rs"]
mod model_stub;
#[path = "support/process_guard.rs"]
mod process_guard;
#[path = "support/release_bounded.rs"]
mod release_bounded;
#[path = "support/release_catalog.rs"]
mod release_catalog;
#[path = "support/release_drivers.rs"]
mod release_drivers;
#[path = "support/release_env.rs"]
mod release_env;
#[path = "support/release_host.rs"]
mod release_host;
#[path = "support/release_layout.rs"]
mod release_layout;
#[path = "support/release_report.rs"]
mod release_report;
#[path = "support/release_scenario.rs"]
mod release_scenario;
#[path = "support/responses_stub.rs"]
mod responses_stub;

use process_guard::ProcessGuard;
use release_catalog::{read_package, ThrowawayRoot};
use release_drivers::driver_for;
use release_env::{Inputs, Mode, REPORT_VAR};
use release_host::Host;
use release_layout::{AnchorSource, Layout};
use release_scenario::{install_official, locate_smoke_upstream, locate_upstream, run_scenario};

/// Prepares a run from `lookup`: refuses a report path that aliases an input,
/// removes the stale report, then validates the inputs.
///
/// The stale report goes before the validation so that a run which fails
/// early never leaves the report of an earlier run behind; the overlap check
/// goes first so that removal can never touch an input. `Ok(None)` means
/// nothing was requested.
fn begin(lookup: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Result<Option<Inputs>, String> {
    release_env::check_report_overlap(lookup).map_err(|error| error.to_string())?;
    release_report::remove_stale_if_absolute(lookup(REPORT_VAR));
    release_env::read_inputs(lookup).map_err(|error| error.to_string())
}

#[test]
fn the_release_binaries_serve_the_runtime_out_of_process() {
    let inputs = match begin(&|name| std::env::var_os(name)) {
        Ok(Some(inputs)) => inputs,
        Ok(None) => {
            eprintln!("skipped: no POHUNEK_CONSUMER_* variable is set");
            return;
        }
        Err(message) => panic!("{message}"),
    };
    consume(&inputs);
}

/// Runs the whole suite for `inputs` and writes the report.
fn consume(inputs: &Inputs) {
    let driver = driver_for(&inputs.runtime).unwrap_or_else(|message| panic!("{message}"));
    let facts = read_package(&inputs.package).unwrap_or_else(|message| panic!("{message}"));
    assert_eq!(
        facts.runtime_id.as_str(),
        inputs.runtime,
        "the package archive serves another runtime than {}",
        release_env::RUNTIME_VAR
    );
    let path = std::env::var_os("PATH");
    let upstream = match inputs.mode() {
        Mode::Row => locate_upstream(&facts.program, path.as_deref()),
        Mode::Smoke => locate_smoke_upstream(
            &facts.program,
            path.as_deref(),
            std::env::var_os("POHUNEK_CONSUMER_STAGE_BIN").as_deref(),
        ),
    }
    .unwrap_or_else(|message| panic!("{message}"));

    let env = TestEnv::new().expect("create the hermetic environment");
    let throwaway = ThrowawayRoot::new();
    let anchor_file = inputs.bin_dir.join(package::ANCHOR_FILE_NAME);
    let anchor = match inputs.mode() {
        Mode::Smoke => AnchorSource::File(&anchor_file),
        Mode::Row => AnchorSource::Bytes(&throwaway.anchor),
    };
    let layout = Layout::install(&inputs.bin_dir, &env.root().join("prefix"), &anchor)
        .unwrap_or_else(|error| panic!("{error}"));
    let catalog = inputs.catalog.clone().unwrap_or_else(|| {
        let path = env.root().join("runtime-catalog.json");
        std::fs::write(&path, throwaway.catalog(&facts, &layout.version))
            .expect("write the throwaway catalog");
        path
    });

    let mut host = Host::start(env, layout, upstream.parent());
    // Declared after the host so it drops first and ends whatever an upstream
    // detached before the environment is removed.
    let processes = ProcessGuard::new(host.env.root());

    install_official(&host, &inputs.package, &catalog, &facts);
    let prepared = (driver.prepare)(&host.env);
    let outcome = run_scenario(&host, &facts, driver, &prepared, &upstream);

    host.layout
        .verify_files()
        .unwrap_or_else(|error| panic!("{error}"));
    host.shutdown();
    let scope = host.env.root().to_path_buf();
    poll_until("every pohunek process of the run to end", || {
        host.layout
            .pohunek_processes(&scope)
            .unwrap_or_else(|error| panic!("{error}"))
            .is_empty()
            .then_some(())
    });
    processes.reap();
    host.layout
        .verify_files()
        .unwrap_or_else(|error| panic!("{error}"));

    let report = release_report::document(
        &inputs.runtime,
        facts.digest.as_str(),
        &outcome.upstream_version,
        &host.layout,
    );
    release_report::write(&inputs.report, &report);
}

/// Hermetic regression scenarios of the harness guards.
mod guards {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt as _};
    use std::path::{Path, PathBuf};
    use std::process::Stdio;

    use package::{build_archive, ArchiveEntry, Limits, ANCHOR_FILE_NAME};
    use pohunek_test_support::env::TestEnv;
    use pohunek_test_support::fs::write_executable;
    use pohunek_test_support::{bin_exe, worker_binary};

    use super::begin;
    use super::model_stub::ModelStub;
    use super::process_guard::ChildGuard;
    use super::release_bounded::{run as run_bounded, PROBE_TIMEOUT};
    use super::release_catalog::{read_package, PackageFacts, ThrowawayRoot};
    use super::release_drivers::Stub;
    use super::release_drivers::{driver_for, DRIVERS};
    use super::release_env::{
        check_report_overlap, read_inputs, EnvError, BINARIES, BIN_DIR_VAR, CATALOG_VAR,
        PACKAGE_VAR, REPORT_VAR, RUNTIME_VAR,
    };
    use super::release_host::Host;
    use super::release_layout::{sha256_file, AnchorSource, Layout, LayoutError};
    use super::release_report;
    use super::release_scenario::{
        history_restored, install_official, install_with_catalog, locate_smoke_upstream,
        locate_upstream, probe_upstream, probe_upstream_within,
    };

    /// Longest a child test process may run: the child's own waits end at the
    /// 120 s hang guard of `pohunek_test_support::wait`.
    const CHILD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

    /// Runs `command` bounded and requires success, printing its output on
    /// failure.
    fn assert_child_succeeds(command: std::process::Command, label: &str) {
        let output = run_bounded(command, label, CHILD_TIMEOUT)
            .unwrap_or_else(|message| panic!("{message}"));
        assert!(
            output.status.success(),
            "{label} failed ({}):\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Package id of the synthetic archive; it serves a non-reserved runtime.
    const PACKAGE_ID: &str = "acme.runtime.pi";

    /// Program the synthetic runtime launches; it only has to exist.
    const PROGRAM: &str = "/bin/sh";

    const DETECT_MANIFEST: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]
"#;

    /// A descriptor with a version probe for `[1.0.0, 1.1.0)`.
    fn runtime_document(version: &str) -> String {
        format!(
            r#"schema = 1
id = "{PACKAGE_ID}"
version = "{version}"
runtime_api = 1

[runtime]
id = "pi"
name = "Pi"
program = "{PROGRAM}"
args = []
detect_manifest = "detect.toml"
prompt_arg = true
version_probe = {{ parser = "semver-v1", args = ["--version"], min = "1.0.0", below = "1.1.0" }}

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = true
reference_kind = "id"
args = ["--session", "{{reference}}"]

[fork]
supported = false

[native_reference]
strategy = "assigned"
launch_args = ["--session-id", "{{reference}}"]

[native_reference.existence]
check = "none"
"#
        )
    }

    /// Writes the synthetic package archive of `version` to `dir`.
    fn write_package(dir: &Path, version: &str) -> PathBuf {
        let entries = [
            ArchiveEntry {
                path: "runtime.toml".to_owned(),
                contents: runtime_document(version).into_bytes(),
                executable: false,
            },
            ArchiveEntry {
                path: "detect.toml".to_owned(),
                contents: DETECT_MANIFEST.as_bytes().to_vec(),
                executable: false,
            },
        ];
        let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the archive builds");
        let path = dir.join(format!("package-{version}.tar.zst"));
        fs::write(&path, bytes).expect("write the archive");
        path
    }

    /// A bin dir made of this build's binaries. The guard scenarios are the
    /// only place the suite looks at the build directory: they prove harness
    /// behavior, not release bytes.
    fn target_bin_dir(parent: &Path) -> PathBuf {
        let dir = parent.join("bins");
        fs::create_dir_all(&dir).expect("create the bin dir");
        let cli = bin_exe("pohunek");
        let daemon = cli.with_file_name("pohunekd");
        assert!(
            daemon.is_file(),
            "{} is missing; the guard scenarios run this build's binaries, so build them first \
             with `cargo build -p pohunek-daemon --bin pohunekd` and \
             `cargo build -p pohunek-session-worker --bin pohunek-sessiond` (CI builds every \
             binary before the tests run)",
            daemon.display()
        );
        // `worker_binary` fails with its own build command when the worker is absent.
        for (name, source) in [
            ("pohunek", cli.clone()),
            ("pohunekd", daemon),
            ("pohunek-sessiond", worker_binary()),
        ] {
            assert!(source.is_file(), "{} is missing", source.display());
            write_executable(
                dir.join(name),
                fs::read(&source).expect("read a build binary"),
            )
            .expect("copy a build binary");
        }
        dir
    }

    /// A started daemon from this build's binaries with the synthetic package
    /// archive authorized by a throwaway catalog.
    struct Guarded {
        host: Host,
        catalog: PathBuf,
        archive: PathBuf,
        facts: PackageFacts,
    }

    fn guarded() -> Guarded {
        let env = TestEnv::new().expect("create the hermetic environment");
        let bins = target_bin_dir(env.root());
        let archive = write_package(env.root(), "1.0.0");
        let facts = read_package(&archive).expect("read the synthetic package");
        let root = ThrowawayRoot::new();
        let layout = Layout::install(
            &bins,
            &env.root().join("prefix"),
            &AnchorSource::Bytes(&root.anchor),
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let catalog = env.root().join("runtime-catalog.json");
        fs::write(&catalog, root.catalog(&facts, &layout.version)).expect("write the catalog");
        let host = Host::start(env, layout, None);
        Guarded {
            host,
            catalog,
            archive,
            facts,
        }
    }

    // ---- input contract -------------------------------------------------

    /// A complete, valid set of inputs below `dir`.
    fn valid_inputs(dir: &Path) -> HashMap<&'static str, OsString> {
        let bins = dir.join("bins");
        fs::create_dir_all(&bins).expect("bin dir");
        for name in BINARIES {
            fs::write(bins.join(name), b"binary").expect("binary");
        }
        fs::write(dir.join("package.tar.zst"), b"archive").expect("package");
        let reports = dir.join("out");
        fs::create_dir_all(&reports).expect("report dir");
        HashMap::from([
            (BIN_DIR_VAR, bins.into_os_string()),
            (RUNTIME_VAR, OsString::from("codex")),
            (PACKAGE_VAR, dir.join("package.tar.zst").into_os_string()),
            (REPORT_VAR, reports.join("report.json").into_os_string()),
        ])
    }

    fn read(values: &HashMap<&'static str, OsString>) -> Result<Option<super::Inputs>, EnvError> {
        read_inputs(&|name| values.get(name).cloned())
    }

    #[test]
    fn an_entirely_unset_environment_skips_and_any_set_variable_makes_every_prerequisite_mandatory()
    {
        assert_eq!(read(&HashMap::new()), Ok(None));

        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let complete = valid_inputs(dir.path());
        let inputs = read(&complete).expect("valid").expect("inputs");
        assert_eq!(inputs.runtime, "codex");
        assert_eq!(inputs.mode(), super::Mode::Row);

        for var in [BIN_DIR_VAR, RUNTIME_VAR, PACKAGE_VAR, REPORT_VAR] {
            let mut values = complete.clone();
            values.remove(var);
            assert_eq!(
                read(&values),
                Err(EnvError::Missing { var }),
                "{var} is mandatory"
            );
            let message = read(&values).unwrap_err().to_string();
            assert!(message.contains(var), "{message}");
        }

        // Only the catalog set: the other four are still required.
        let only_catalog = HashMap::from([(CATALOG_VAR, OsString::from("/abs/catalog.json"))]);
        assert!(matches!(
            read(&only_catalog),
            Err(EnvError::Missing { var: BIN_DIR_VAR })
        ));
    }

    #[test]
    fn a_relative_empty_or_non_regular_input_is_refused_naming_its_variable() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let complete = valid_inputs(dir.path());
        let with = |var: &'static str, value: OsString| {
            let mut values = complete.clone();
            values.insert(var, value);
            values
        };

        assert!(matches!(
            read(&with(BIN_DIR_VAR, OsString::from("relative/bins"))),
            Err(EnvError::NotAbsolute {
                var: BIN_DIR_VAR,
                ..
            })
        ));
        assert!(matches!(
            read(&with(PACKAGE_VAR, OsString::new())),
            Err(EnvError::Empty { var: PACKAGE_VAR })
        ));
        assert!(matches!(
            read(&with(RUNTIME_VAR, OsString::from("Not A Runtime"))),
            Err(EnvError::InvalidRuntime { .. })
        ));
        assert!(matches!(
            read(&with(PACKAGE_VAR, dir.path().join("bins").into_os_string())),
            Err(EnvError::NotRegularFile {
                var: PACKAGE_VAR,
                ..
            })
        ));
        assert!(matches!(
            read(&with(
                REPORT_VAR,
                dir.path().join("missing/report.json").into_os_string()
            )),
            Err(EnvError::UnwritableReport { .. })
        ));

        // A symbolic link is not a regular file: the bytes it names could change.
        let linked = dir.path().join("linked");
        fs::create_dir_all(&linked).expect("linked dir");
        for name in BINARIES {
            symlink(dir.path().join("bins").join(name), linked.join(name)).expect("link");
        }
        assert!(matches!(
            read(&with(BIN_DIR_VAR, linked.into_os_string())),
            Err(EnvError::NotRegularFile {
                var: BIN_DIR_VAR,
                ..
            })
        ));

        // A missing binary names the bin dir variable and the file.
        fs::remove_file(dir.path().join("bins/pohunekd")).expect("remove");
        let error = read(&complete).unwrap_err();
        assert!(matches!(
            error,
            EnvError::NotRegularFile {
                var: BIN_DIR_VAR,
                ..
            }
        ));
        assert!(error.to_string().contains("pohunekd"), "{error}");
    }

    #[test]
    fn smoke_mode_requires_the_catalog_and_the_archives_own_anchor() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let mut values = valid_inputs(dir.path());
        let catalog = dir.path().join("catalog.json");
        fs::write(&catalog, b"{}").expect("catalog");
        values.insert(CATALOG_VAR, catalog.clone().into_os_string());

        // No anchor beside the binaries: refused, naming the bin dir variable.
        let error = read(&values).unwrap_err();
        assert!(
            matches!(
                error,
                EnvError::NotRegularFile {
                    var: BIN_DIR_VAR,
                    ..
                }
            ),
            "{error}"
        );
        assert!(error.to_string().contains(ANCHOR_FILE_NAME), "{error}");

        fs::write(dir.path().join("bins").join(ANCHOR_FILE_NAME), b"{}").expect("anchor");
        let inputs = read(&values).expect("valid").expect("inputs");
        assert_eq!(inputs.mode(), super::Mode::Smoke);

        values.insert(CATALOG_VAR, OsString::from("catalog.json"));
        assert!(matches!(
            read(&values),
            Err(EnvError::NotAbsolute {
                var: CATALOG_VAR,
                ..
            })
        ));
    }

    // ---- the report path against the inputs -------------------------------

    #[test]
    fn a_report_path_that_aliases_an_input_is_refused_and_destroys_nothing() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let mut complete = valid_inputs(dir.path());
        let catalog = dir.path().join("catalog.json");
        fs::write(&catalog, b"catalog").expect("catalog");
        complete.insert(CATALOG_VAR, catalog.clone().into_os_string());
        fs::write(dir.path().join("bins").join(ANCHOR_FILE_NAME), b"anchor").expect("anchor");
        let package = dir.path().join("package.tar.zst");

        let snapshot = |dir: &Path| -> Vec<(PathBuf, Vec<u8>)> {
            let mut files: Vec<_> = fs::read_dir(dir.join("bins"))
                .expect("bins")
                .flatten()
                .map(|entry| (entry.path(), fs::read(entry.path()).expect("read")))
                .collect();
            files.push((package.clone(), fs::read(&package).expect("package")));
            files.push((catalog.clone(), fs::read(&catalog).expect("catalog")));
            files.sort();
            files
        };
        let before = snapshot(dir.path());

        let alias = dir.path().join("alias-dir");
        symlink(dir.path(), &alias).expect("directory alias");
        let hard = dir.path().join("out/hard-link");
        fs::hard_link(&package, &hard).expect("hard link");
        let soft = dir.path().join("out/soft-link");
        symlink(&catalog, &soft).expect("soft link");

        let cases: Vec<(&str, PathBuf, &str)> = vec![
            ("the package itself", package.clone(), PACKAGE_VAR),
            ("the catalog itself", catalog.clone(), CATALOG_VAR),
            (
                "a release binary",
                dir.path().join("bins/pohunekd"),
                BIN_DIR_VAR,
            ),
            (
                "the anchor",
                dir.path().join("bins").join(ANCHOR_FILE_NAME),
                BIN_DIR_VAR,
            ),
            (
                "the package through a parent symlink",
                alias.join("package.tar.zst"),
                PACKAGE_VAR,
            ),
            ("a hard link of the package", hard, PACKAGE_VAR),
            ("a symlink to the catalog", soft, CATALOG_VAR),
        ];
        for (what, report, variable) in &cases {
            let mut values = complete.clone();
            values.insert(REPORT_VAR, report.clone().into_os_string());
            let error = check_report_overlap(&|name| values.get(name).cloned()).expect_err(what);
            assert!(
                matches!(&error, EnvError::ReportOverlaps { input, .. } if input == variable),
                "{what}: {error}"
            );
            assert!(error.to_string().contains(REPORT_VAR), "{what}: {error}");
        }

        // The temporary sibling of the report is an input.
        let mut sibling = complete.clone();
        sibling.insert(
            PACKAGE_VAR,
            dir.path().join("report.json.partial").into_os_string(),
        );
        sibling.insert(REPORT_VAR, dir.path().join("report.json").into_os_string());
        assert!(matches!(
            check_report_overlap(&|name| sibling.get(name).cloned()),
            Err(EnvError::ReportOverlaps {
                input: PACKAGE_VAR,
                ..
            })
        ));

        // A report of its own is fine, and nothing above changed any input.
        check_report_overlap(&|name| complete.get(name).cloned()).expect("distinct report");
        assert_eq!(snapshot(dir.path()), before);
    }

    // ---- directory modes under a permissive umask --------------------------

    /// Set in the child process of the umask scenario.
    const UMASK_CHILD_VAR: &str = "RELEASE_CONSUMER_UMASK_CHILD";

    #[test]
    fn the_layout_and_its_official_install_do_not_depend_on_the_umask() {
        // A child process owns the umask, so this multithreaded test binary
        // never changes its own.
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", "umask 0002; exec \"$0\" \"$@\""])
            .arg(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "guards::layout_under_umask_0002_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(UMASK_CHILD_VAR, "1");
        assert_child_succeeds(command, "the child under umask 0002");
    }

    #[test]
    fn layout_under_umask_0002_child() {
        use std::os::unix::fs::MetadataExt as _;

        if std::env::var_os(UMASK_CHILD_VAR).is_none() {
            return;
        }
        let guarded = guarded();
        let layout = &guarded.host.layout;
        let mut dir = layout.dir.clone();
        while dir.starts_with(guarded.host.env.root().join("prefix")) {
            let mode = fs::metadata(&dir).expect("metadata").mode() & 0o7777;
            assert_eq!(mode, 0o755, "{}", dir.display());
            dir.pop();
        }
        install_official(
            &guarded.host,
            &guarded.archive,
            &guarded.catalog,
            &guarded.facts,
        );
    }

    // ---- the resume evidence ------------------------------------------------

    /// Sends one chat-completions request to the Pi stub and waits until the
    /// stub has recorded it.
    fn send_to_stub(stub: &ModelStub, body: &str, token: &str) {
        use std::io::Write as _;

        let address = stub
            .base_url()
            .trim_start_matches("http://")
            .trim_end_matches("/v1")
            .to_owned();
        let mut stream = std::net::TcpStream::connect(&address).expect("connect to the stub");
        write!(
            stream,
            "POST /v1/chat/completions HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .expect("send the request");
        stream.flush().expect("flush");
        pohunek_test_support::wait::poll_until("the stub to record the request", || {
            (!stub.bodies_containing(token).is_empty()).then_some(())
        });
    }

    #[test]
    fn a_resume_that_opened_a_fresh_conversation_fails_the_history_check() {
        let reply = "working done";

        // Fresh conversation: the second turn's request lacks the first turn.
        let stub = ModelStub::start();
        send_to_stub(
            &stub,
            r#"{"messages":[{"role":"user","content":"second-turn-9c1d"}]}"#,
            "second-turn-9c1d",
        );
        let fresh = Stub::Pi(stub);
        let error = history_restored(&fresh, "first-turn-7f3a", reply, "second-turn-9c1d")
            .expect_err("a fresh conversation");
        assert!(error.contains("not restored"), "{error}");

        // Restored conversation: the request carries the first prompt and reply.
        let stub = ModelStub::start();
        send_to_stub(
            &stub,
            r#"{"messages":[{"role":"user","content":"say hi first-turn-7f3a"},{"role":"assistant","content":"working done"},{"role":"user","content":"second-turn-9c1d"}]}"#,
            "second-turn-9c1d",
        );
        history_restored(
            &Stub::Pi(stub),
            "first-turn-7f3a",
            reply,
            "second-turn-9c1d",
        )
        .expect("a restored conversation");

        // No request for the second turn at all.
        let none = Stub::Pi(ModelStub::start());
        assert!(history_restored(&none, "first-turn-7f3a", reply, "second-turn-9c1d").is_err());
    }

    // ---- ordering of the report cleanup and the validation -----------------

    /// Set in the child process of the relative-path scenario.
    const RELATIVE_CHILD_VAR: &str = "RELEASE_CONSUMER_RELATIVE_CHILD";

    #[test]
    fn a_relative_input_that_resolves_to_the_report_is_never_deleted() {
        // The child runs with a private fixture directory as its working
        // directory, so nothing outside the fixture is read or written.
        let fixture = pohunek_test_support::tempdir().expect("tempdir");
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "guards::relative_input_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .current_dir(fixture.path())
            .env(RELATIVE_CHILD_VAR, "1");
        assert_child_succeeds(command, "the relative-path child");
    }

    #[test]
    fn relative_input_child() {
        if std::env::var_os(RELATIVE_CHILD_VAR).is_none() {
            return;
        }
        let cwd = std::env::current_dir().expect("cwd");
        let complete = valid_inputs(&cwd);
        let victim = cwd.join("relative-victim.bin");
        fs::write(&victim, b"precious").expect("victim");
        let mut relative = complete.clone();
        relative.insert(PACKAGE_VAR, OsString::from("relative-victim.bin"));
        relative.insert(REPORT_VAR, victim.clone().into_os_string());
        let error = begin(&|name| relative.get(name).cloned()).unwrap_err();
        assert!(error.contains(PACKAGE_VAR), "{error}");
        assert_eq!(fs::read(&victim).expect("victim survives"), b"precious");
    }

    #[test]
    fn invalid_or_relative_inputs_leave_every_named_file_untouched_and_no_stale_report_survives() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let complete = valid_inputs(dir.path());
        let package = dir.path().join("package.tar.zst");
        let report = dir.path().join("out/report.json");

        // Invalid inputs fail, never touch the inputs, and remove the stale
        // report so a failed run cannot be mistaken for the earlier one.
        for (what, var, value) in [
            ("a relative bin dir", BIN_DIR_VAR, OsString::from("bins")),
            ("a bad runtime", RUNTIME_VAR, OsString::from("No Good")),
            ("an empty package", PACKAGE_VAR, OsString::new()),
        ] {
            fs::write(&report, b"stale").expect("stale report");
            let before = fs::read(&package).expect("package");
            let mut values = complete.clone();
            values.insert(var, value);
            let error = begin(&|name| values.get(name).cloned()).unwrap_err();
            assert!(error.contains(var), "{what}: {error}");
            assert!(!report.exists(), "{what}: a stale report survived");
            assert_eq!(fs::read(&package).expect("package"), before, "{what}");
        }

        // A relative report path is rejected and nothing is removed.
        let mut values = complete.clone();
        values.insert(REPORT_VAR, OsString::from("report.json"));
        begin(&|name| values.get(name).cloned()).unwrap_err();

        // Valid inputs: the stale report is gone before the run starts.
        fs::write(&report, b"stale").expect("stale report");
        let inputs = begin(&|name| complete.get(name).cloned()).expect("valid");
        assert!(inputs.is_some());
        assert!(!report.exists());
    }

    // ---- concurrent staging --------------------------------------------------

    #[test]
    fn executables_staged_and_run_from_many_threads_never_hit_a_busy_text_file() {
        const THREADS: usize = 8;
        const ROUNDS: usize = 4;

        let workers: Vec<_> = (0..THREADS)
            .map(|thread| {
                std::thread::spawn(move || {
                    for round in 0..ROUNDS {
                        let env = TestEnv::new().expect("create the hermetic environment");
                        let bins = env.root().join("bins");
                        fs::create_dir_all(&bins).expect("bin dir");
                        for name in BINARIES {
                            let script = format!("#!/bin/sh\necho \"{name} 9.9.{thread}\"\n");
                            write_executable(bins.join(name), script).expect("write");
                        }
                        // Install copies the files and executes the staged
                        // `pohunekd` and `pohunek`.
                        let layout =
                            Layout::install(&bins, &env.root().join("prefix"), &AnchorSource::None)
                                .unwrap_or_else(|error| {
                                    panic!("thread {thread} round {round}: {error}")
                                });
                        assert_eq!(layout.version, format!("9.9.{thread}"));
                        let mut command = std::process::Command::new(layout.path("pohunekd"));
                        command.arg("--version");
                        assert_child_succeeds(command, "the installed copy");
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("worker thread");
        }
    }

    // ---- locating the upstream ------------------------------------------------

    #[test]
    fn the_upstream_is_located_like_the_daemon_does() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let broken = dir.path().join("broken");
        let valid = dir.path().join("valid");
        fs::create_dir_all(&broken).expect("broken dir");
        fs::create_dir_all(&valid).expect("valid dir");
        // A file that is not executable earlier on PATH must not shadow the
        // installed upstream.
        fs::write(broken.join("pi"), b"#!/bin/sh\nexit 0\n").expect("plain file");
        fs::set_permissions(broken.join("pi"), fs::Permissions::from_mode(0o644)).expect("mode");
        write_executable(valid.join("pi"), "#!/bin/sh\nexit 0\n").expect("executable");
        // A relative entry holding an executable `pi` is never searched.
        let relative_dir = dir.path().join("rel");
        fs::create_dir_all(&relative_dir).expect("relative dir");
        write_executable(relative_dir.join("pi"), "#!/bin/sh\nexit 0\n").expect("executable");

        let path = std::env::join_paths([broken.as_path(), valid.as_path()]).expect("join");
        assert_eq!(
            locate_upstream("pi", Some(&path)).expect("found"),
            valid.join("pi")
        );

        let only_broken = std::env::join_paths([broken.as_path()]).expect("join");
        let message = locate_upstream("pi", Some(&only_broken)).unwrap_err();
        assert!(message.contains("`pi`"), "{message}");

        // The relative entry would resolve against the working directory of
        // this process if it were searched.
        let cwd = std::env::current_dir().expect("cwd");
        let relative = pathdiff(&relative_dir, &cwd);
        let relative_only = std::env::join_paths([relative.as_path()]).expect("join");
        locate_upstream("pi", Some(&relative_only)).unwrap_err();
        locate_upstream("pi", None).unwrap_err();
    }

    #[test]
    fn archive_smoke_refuses_host_fallback_and_external_staged_links() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let stage_bin = dir.path().join("stage/pi/bin");
        let host_bin = dir.path().join("host/bin");
        fs::create_dir_all(&stage_bin).expect("stage bin");
        fs::create_dir_all(&host_bin).expect("host bin");
        write_executable(host_bin.join("pi"), "#!/bin/sh\nexit 0\n").expect("host executable");
        let path = std::env::join_paths([stage_bin.as_path(), host_bin.as_path()]).expect("PATH");
        assert_eq!(
            locate_upstream("pi", Some(&path)).expect("host fallback exists"),
            host_bin.join("pi").canonicalize().expect("canonical host")
        );
        let refused = locate_smoke_upstream("pi", Some(&path), Some(stage_bin.as_os_str()))
            .expect_err("missing staged entry must be refused");
        assert!(refused.contains("staged upstream `pi`"), "{refused}");

        fs::write(stage_bin.join("pi"), "#!/bin/sh\nexit 0\n").expect("staged entry");
        fs::set_permissions(stage_bin.join("pi"), fs::Permissions::from_mode(0o644))
            .expect("non-executable mode");
        let refused = locate_smoke_upstream("pi", Some(&path), Some(stage_bin.as_os_str()))
            .expect_err("unusable staged entry must not use host fallback");
        assert!(refused.contains("verified stage"), "{refused}");

        fs::remove_file(stage_bin.join("pi")).expect("remove staged entry");
        symlink(host_bin.join("pi"), stage_bin.join("pi")).expect("external stage link");
        let refused = locate_smoke_upstream("pi", Some(&path), Some(stage_bin.as_os_str()))
            .expect_err("external staged link must be refused");
        assert!(refused.contains("verified stage"), "{refused}");

        fs::remove_file(stage_bin.join("pi")).expect("remove link");
        write_executable(stage_bin.join("pi"), "#!/bin/sh\nexit 0\n").expect("staged executable");
        assert_eq!(
            locate_smoke_upstream("pi", Some(&path), Some(stage_bin.as_os_str()))
                .expect("valid stage"),
            stage_bin
                .join("pi")
                .canonicalize()
                .expect("canonical staged entry")
        );
    }

    /// `target` relative to `base`, both absolute.
    fn pathdiff(target: &Path, base: &Path) -> PathBuf {
        let mut up = PathBuf::new();
        let mut ancestor = base;
        while !target.starts_with(ancestor) {
            up.push("..");
            ancestor = ancestor.parent().expect("a common root");
        }
        up.join(target.strip_prefix(ancestor).expect("below the ancestor"))
    }

    // ---- bounded subprocesses -------------------------------------------------

    /// Seconds the fake upstreams sleep; the number is unique to these tests
    /// so a leftover is recognizable by its exact argument vector.
    const FAKE_SLEEP_SECONDS: &str = "7351";

    /// Whether a live process runs exactly `sleep 7351`.
    fn fake_sleep_alive() -> bool {
        let wanted = format!("sleep\0{FAKE_SLEEP_SECONDS}\0").into_bytes();
        fs::read_dir("/proc")
            .expect("/proc")
            .flatten()
            .any(|entry| fs::read(entry.path().join("cmdline")).is_ok_and(|argv| argv == wanted))
    }

    #[test]
    fn a_hung_upstream_probe_fails_within_the_bound_and_leaves_no_process() {
        let env = TestEnv::new().expect("create the hermetic environment");
        let archive = write_package(env.root(), "1.0.0");
        let facts = read_package(&archive).expect("read the synthetic package");
        let hung = env.root().join("hung-upstream");
        write_executable(
            &hung,
            format!("#!/bin/sh\nsleep {FAKE_SLEEP_SECONDS} &\nwait\n"),
        )
        .expect("write the hung upstream");

        let mut command = env.command(&hung);
        command.args(facts.probe.args());
        let started = std::time::Instant::now();
        let error = run_bounded(
            command,
            "hung-upstream",
            std::time::Duration::from_millis(500),
        )
        .unwrap_err();
        assert!(error.contains("hung-upstream"), "{error}");
        assert!(error.contains("did not exit"), "{error}");
        assert!(started.elapsed() < PROBE_TIMEOUT, "bounded by the timeout");
        // The `sleep` child of the script shares its process group and is killed.
        pohunek_test_support::wait::poll_until("the hung group to be gone", || {
            (!fake_sleep_alive()).then_some(())
        });
    }

    #[test]
    #[should_panic(expected = "hung-probe")]
    fn the_upstream_probe_fails_naming_the_program_when_it_hangs() {
        let env = TestEnv::new().expect("create the hermetic environment");
        let archive = write_package(env.root(), "1.0.0");
        let facts = read_package(&archive).expect("read the synthetic package");
        let hung = env.root().join("hung-probe");
        write_executable(&hung, "#!/bin/sh\nwhile :; do :; done\n").expect("write");
        probe_upstream_within(
            &env,
            &hung,
            &facts,
            &[],
            std::time::Duration::from_millis(300),
        );
    }

    #[test]
    fn a_background_child_holding_the_pipes_does_not_block_collection() {
        let env = TestEnv::new().expect("create the hermetic environment");
        let script = env.root().join("leaky-upstream");
        // The background process keeps stdout and stderr open after the
        // script has printed its version and exited.
        write_executable(
            &script,
            format!("#!/bin/sh\necho 1.0.7\nsleep {FAKE_SLEEP_SECONDS} &\nexit 0\n"),
        )
        .expect("write the leaky upstream");
        let started = std::time::Instant::now();
        let output = run_bounded(env.command(&script), "leaky-upstream", PROBE_TIMEOUT)
            .expect("collected within the bound");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1.0.7");
        assert!(
            started.elapsed() < PROBE_TIMEOUT,
            "collection ended by the drain grace, not the probe timeout"
        );
        pohunek_test_support::wait::poll_until("the leaked child to be killed", || {
            (!fake_sleep_alive()).then_some(())
        });
    }

    // ---- drivers ---------------------------------------------------------

    #[test]
    fn a_runtime_without_a_driver_fails_and_names_where_to_add_one() {
        let message = driver_for("opencode").err().expect("no driver");
        assert!(message.contains("add an entry to DRIVERS"), "{message}");
        assert!(message.contains("release_drivers.rs"), "{message}");
        for driver in &DRIVERS {
            assert!(driver_for(driver.runtime).is_ok(), "{}", driver.runtime);
        }
        let mut ids: Vec<&str> = DRIVERS.iter().map(|driver| driver.runtime).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), DRIVERS.len(), "one driver per runtime id");
    }

    // ---- the layout ------------------------------------------------------

    #[test]
    fn the_layout_holds_byte_identical_copies_and_refuses_a_swapped_binary() {
        let env = TestEnv::new().expect("create the hermetic environment");
        let bins = target_bin_dir(env.root());
        let layout = Layout::install(&bins, &env.root().join("prefix"), &AnchorSource::None)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(layout
            .dir
            .ends_with(format!("libexec/pohunek/{}", layout.version)));
        layout.verify_files().expect("an untouched layout verifies");
        for name in BINARIES {
            assert_eq!(
                fs::read(layout.path(name)).expect("installed"),
                fs::read(bins.join(name)).expect("source"),
                "{name}"
            );
        }

        // Swapping the executed copy after the install is refused.
        let installed = layout.path("pohunekd").to_path_buf();
        let original = fs::read(&installed).expect("read");
        fs::remove_file(&installed).expect("unlink");
        fs::write(&installed, b"swapped").expect("write");
        let error = layout.verify_files().unwrap_err();
        assert!(
            matches!(&error, LayoutError::Changed { file, .. } if file.ends_with("pohunekd")),
            "{error}"
        );
        assert!(error.to_string().contains("not byte-identical"), "{error}");
        fs::write(&installed, &original).expect("restore");
        layout.verify_files().expect("the restored copy verifies");

        // Changing the bin dir after the copy is refused as well: the report
        // must not name bytes the given directory no longer holds.
        fs::write(bins.join("pohunek-sessiond"), b"another worker").expect("rewrite the source");
        let error = layout.verify_files().unwrap_err();
        assert!(
            matches!(
                &error,
                LayoutError::Changed {
                    against: "the copy that runs",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_running_pohunek_process_outside_the_layout_is_refused() {
        let guarded = guarded();
        let host = &guarded.host;
        let scope = host.env.root().to_path_buf();

        // The running daemon is the layout's copy and is accepted.
        host.layout
            .verify_process(host.daemon_pid, "pohunekd")
            .unwrap_or_else(|error| panic!("{error}"));
        let running = host
            .layout
            .pohunek_processes(&scope)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(
            running
                .iter()
                .any(|(pid, name)| *pid == host.daemon_pid && name == "pohunekd"),
            "{running:?}"
        );

        // A process named like a pohunek binary that runs from elsewhere is not.
        let outside = scope.join("elsewhere");
        fs::create_dir_all(&outside).expect("outside dir");
        let impostor = outside.join("pohunekd");
        write_executable(&impostor, fs::read("/bin/sh").expect("read /bin/sh"))
            .expect("copy the shell");
        let child = host
            .env
            .command(&impostor)
            .args(["-c", "read line"])
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn the impostor");
        let child = ChildGuard::new(child);

        let error = host.layout.pohunek_processes(&scope).unwrap_err();
        assert!(
            matches!(&error, LayoutError::OutsideLayout { pid, .. } if *pid == child.id()),
            "{error}"
        );
        assert!(error.to_string().contains("outside the layout"), "{error}");
        let error = host
            .layout
            .verify_process(child.id(), "pohunekd")
            .unwrap_err();
        assert!(
            matches!(error, LayoutError::OutsideLayout { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_worker_of_another_release_makes_the_directory_unusable() {
        let env = TestEnv::new().expect("create the hermetic environment");
        let bins = env.root().join("bins");
        fs::create_dir_all(&bins).expect("bin dir");
        for (name, version) in [
            ("pohunek", "1.2.3"),
            ("pohunekd", "1.2.3"),
            ("pohunek-sessiond", "1.2.4"),
        ] {
            write_executable(
                bins.join(name),
                format!("#!/bin/sh\necho \"{name} {version}\"\n"),
            )
            .expect("write");
        }
        let error =
            Layout::install(&bins, &env.root().join("prefix"), &AnchorSource::None).unwrap_err();
        assert!(
            matches!(&error, LayoutError::Version { detail } if detail.contains("1.2.4") && detail.contains("pohunek-sessiond")),
            "{error}"
        );

        // The same directory with a matching worker installs.
        write_executable(
            bins.join("pohunek-sessiond"),
            "#!/bin/sh\necho \"pohunek-sessiond 1.2.3\"\n",
        )
        .expect("write");
        let layout = Layout::install(&bins, &env.root().join("prefix2"), &AnchorSource::None)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(layout.version, "1.2.3");
    }

    #[test]
    fn a_daemon_that_ignores_sigterm_fails_the_stop_and_is_still_reaped() {
        let env = TestEnv::new().expect("create the hermetic environment");
        let bins = target_bin_dir(env.root());
        let layout = Layout::install(&bins, &env.root().join("prefix"), &AnchorSource::None)
            .unwrap_or_else(|error| panic!("{error}"));
        // A long-lived stand-in that ignores SIGTERM.
        let ready = env.root().join("stand-in-ready");
        let child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("trap '' TERM; : > \"$0\"; while :; do read line; done")
            .arg(&ready)
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn the stand-in");
        // The signal must arrive after the shell ignores it.
        pohunek_test_support::wait::poll_until("the stand-in to ignore SIGTERM", || {
            ready.exists().then_some(())
        });
        let pid = child.id();
        let mut host = Host::adopt(env, layout, child);
        std::fs::write(host.env.root().join("daemon.out"), b"").expect("log");

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            host.stop_daemon(std::time::Duration::from_millis(300));
        }));
        let message = *outcome
            .expect_err("the stop times out")
            .downcast::<String>()
            .expect("message");
        assert!(message.contains("did not exit within"), "{message}");
        // The handle is still owned, so dropping the host kills and reaps it.
        assert_eq!(host.daemon_pid, pid);
        drop(host);
        pohunek_test_support::wait::poll_until("the stand-in to be gone", || {
            (!Path::new(&format!("/proc/{pid}")).exists()).then_some(())
        });
    }

    // ---- the catalog install ----------------------------------------------

    #[test]
    fn smoke_mode_installs_from_the_supplied_catalog_with_the_archives_own_anchor() {
        let env = TestEnv::new().expect("create the hermetic environment");
        let bins = target_bin_dir(env.root());
        let archive = write_package(env.root(), "1.0.0");
        let facts = read_package(&archive).expect("read the synthetic package");

        // The anchor and the signed catalog come from outside, as the final
        // bundle ships them; the harness generates nothing.
        let root = ThrowawayRoot::new();
        let anchor = bins.join(ANCHOR_FILE_NAME);
        fs::write(&anchor, &root.anchor).expect("write the archive anchor");
        let layout = Layout::install(
            &bins,
            &env.root().join("prefix"),
            &AnchorSource::File(&anchor),
        )
        .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            fs::read(layout.dir.join(ANCHOR_FILE_NAME)).expect("installed anchor"),
            root.anchor,
            "the anchor beside the daemon is the archive's own file"
        );
        let catalog = env.root().join("official-catalog.json");
        fs::write(&catalog, root.catalog(&facts, &layout.version)).expect("write the catalog");

        let host = Host::start(env, layout, None);
        install_official(&host, &archive, &catalog, &facts);

        // The anchor is part of what ran: replacing it is detected.
        fs::write(host.layout.dir.join(ANCHOR_FILE_NAME), b"{}").expect("replace the anchor");
        assert!(matches!(
            host.layout.verify_files(),
            Err(LayoutError::Changed { .. })
        ));
    }

    #[test]
    fn a_catalog_signed_by_a_key_the_archives_anchor_does_not_trust_is_refused() {
        let env = TestEnv::new().expect("create the hermetic environment");
        let bins = target_bin_dir(env.root());
        let archive = write_package(env.root(), "1.0.0");
        let facts = read_package(&archive).expect("read the synthetic package");
        let trusted = ThrowawayRoot::new();
        let stranger = ThrowawayRoot::new();
        let layout = Layout::install(
            &bins,
            &env.root().join("prefix"),
            &AnchorSource::Bytes(&trusted.anchor),
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let catalog = env.root().join("foreign-catalog.json");
        fs::write(&catalog, stranger.catalog(&facts, &layout.version)).expect("write");
        let host = Host::start(env, layout, None);
        let (code, refused) = install_with_catalog(&host, &archive, &catalog);
        assert_eq!(code, 1, "{refused}");
        assert_eq!(refused["err"]["code"], "package_untrusted", "{refused}");
    }

    #[test]
    fn an_official_install_of_an_archive_the_catalog_does_not_name_is_refused_with_a_typed_error() {
        let guarded = guarded();
        let host = &guarded.host;

        // The authorized archive installs as official.
        let other = write_package(host.env.root(), "1.0.1");
        let (code, refused) = install_with_catalog(host, &other, &guarded.catalog);
        assert_eq!(code, 1, "{refused}");
        assert_eq!(refused["err"]["code"], "package_untrusted", "{refused}");
        let listed = host.ok(&["plugin", "list"]);
        assert_eq!(
            listed["packages"].as_array().map(Vec::len),
            Some(0),
            "a refused install records nothing: {listed}"
        );

        install_official(host, &guarded.archive, &guarded.catalog, &guarded.facts);
    }

    #[test]
    #[should_panic(expected = "official install failed")]
    fn the_harness_fails_the_run_when_the_official_install_is_refused() {
        let guarded = guarded();
        let other = write_package(guarded.host.env.root(), "1.0.1");
        let other_facts = read_package(&other).expect("read");
        install_official(&guarded.host, &other, &guarded.catalog, &other_facts);
    }

    #[test]
    fn a_stale_partial_link_is_never_followed_when_the_report_is_written() {
        let env = TestEnv::new().expect("create the hermetic environment");
        let bins = target_bin_dir(env.root());
        let layout = Layout::install(&bins, &env.root().join("prefix"), &AnchorSource::None)
            .unwrap_or_else(|error| panic!("{error}"));
        let path = env.root().join("report.json");

        // Unrelated files reachable through the names a naive writer would use.
        let victim = env.root().join("victim.txt");
        fs::write(&victim, b"precious").expect("victim");
        let hard_victim = env.root().join("hard-victim.txt");
        fs::write(&hard_victim, b"also precious").expect("hard victim");
        symlink(&victim, env.root().join("report.json.partial")).expect("symlink");
        fs::hard_link(&hard_victim, env.root().join(".report.json.partial")).expect("hard link");

        let digest = format!("sha256:{}", "cd".repeat(32));
        let document = release_report::document("pi", &digest, "1.0.2", &layout);
        release_report::write(&path, &document);

        assert_eq!(fs::read(&victim).expect("victim"), b"precious");
        assert_eq!(
            fs::read(&hard_victim).expect("hard victim"),
            b"also precious"
        );
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read")).expect("JSON");
        assert_eq!(written["runtime"], "pi");
        assert_eq!(written["package_digest"], digest.as_str());
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &fs::metadata(&path).expect("metadata").permissions(),
        );
        assert_eq!(mode & 0o777, 0o644);
        // Only the report and the files the scenario made are left: no
        // temporary file of the writer survives.
        let leftovers: Vec<String> = fs::read_dir(env.root())
            .expect("root")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".report.json.") && name != ".report.json.partial")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    // ---- the upstream version ----------------------------------------------

    fn fake_upstream(env: &TestEnv, banner: &str) -> PathBuf {
        let path = env.root().join("fake-upstream");
        write_executable(&path, format!("#!/bin/sh\necho {banner}\n").into_bytes())
            .expect("write the fake upstream");
        path
    }

    #[test]
    fn the_probed_version_is_read_with_the_package_grammar_and_must_be_in_range() {
        let env = TestEnv::new().expect("create the hermetic environment");
        let archive = write_package(env.root(), "1.0.0");
        let facts = read_package(&archive).expect("read the synthetic package");

        let accepted = probe_upstream(&env, &fake_upstream(&env, "1.0.7"), &facts, &[]);
        assert_eq!(accepted, "1.0.7");
    }

    #[test]
    #[should_panic(expected = "outside the supported range")]
    fn an_upstream_outside_the_supported_range_fails_the_run() {
        let env = TestEnv::new().expect("create the hermetic environment");
        let archive = write_package(env.root(), "1.0.0");
        let facts = read_package(&archive).expect("read the synthetic package");
        probe_upstream(&env, &fake_upstream(&env, "1.1.0"), &facts, &[]);
    }

    // ---- the report --------------------------------------------------------

    #[test]
    fn the_report_names_the_installed_digests_and_a_stale_one_is_removed() {
        let env = TestEnv::new().expect("create the hermetic environment");
        let bins = target_bin_dir(env.root());
        let layout = Layout::install(&bins, &env.root().join("prefix"), &AnchorSource::None)
            .unwrap_or_else(|error| panic!("{error}"));
        let path = env.root().join("report.json");

        fs::write(&path, b"stale").expect("stale report");
        release_report::remove_stale(&path);
        assert!(!path.exists());

        let digest = format!("sha256:{}", "ab".repeat(32));
        let document = release_report::document("codex", &digest, "0.160.0", &layout);
        release_report::write(&path, &document);
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read")).expect("JSON");
        assert_eq!(written["schema"], 1);
        assert_eq!(written["runtime"], "codex");
        assert_eq!(written["package_digest"], digest.as_str());
        assert_eq!(written["upstream_version"], "0.160.0");
        let matrix: serde_json::Value = serde_json::from_slice(
            &fs::read(pohunek_test_support::workspace_root().join("compat/matrix.json"))
                .expect("read the matrix"),
        )
        .expect("matrix JSON");
        assert_eq!(written["suite_version"], matrix["suite_version"]);
        for name in BINARIES {
            assert_eq!(
                written["executables"][name].as_str(),
                Some(sha256_file(&bins.join(name)).expect("hash").as_str()),
                "{name}"
            );
        }
        assert_eq!(
            written["executables"].as_object().map(serde_json::Map::len),
            Some(3)
        );
        assert!(!env.root().join("report.json.partial").exists());
    }
}
