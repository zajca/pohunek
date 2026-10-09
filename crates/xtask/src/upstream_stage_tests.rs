//! Scenarios for `compat stage-upstream` and `compat verify-stage`, run
//! through the functions the commands call. The registry is out of reach: a
//! shim `npm` copies a prebuilt `node_modules` tree into the project, so the
//! scenarios cover everything around the install (lock and lockfile
//! validation, the scrubbed environment, the pins, the banner probe, the
//! manifests and their verification). The real install is exercised by
//! running the command against the registry.

// Rust guideline compliant 2026-10-08

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use pohunek_test_support::fs::{write_executable, write_file};
use pohunek_test_support::process_env::ProcessEnv;
use serde_json::{json, Value};
use tempfile::TempDir;

use crate::upstream_stage::{
    host_key, read_lock, read_project, stage_upstream, verify_stage, Fault, Outcome, StageError,
    Tools, POSIX_VERIFY,
};
use crate::XtaskError;

const RUNTIME: &str = "fakert";
const PACKAGE: &str = "@fake/up";
const RELEASE: &str = "1.2.3";
const BINARY: &str = "up";
const BANNER: &str = "fake-cli 1.2.3";
const REGISTRY: &str = "https://registry.example.test/";

/// Tool search path of the shim and the probe: coreutils only.
const TOOL_PATH: &str = "/usr/bin:/bin";

fn sri(fill: char) -> String {
    format!("sha512-{}A==", fill.to_string().repeat(85))
}

fn banner_script(banner: &str) -> String {
    format!("#!/bin/sh\necho '{banner}'\n")
}

/// A throwaway repository root holding one fake runtime, the prebuilt
/// install tree the shim copies, and the shim itself.
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            dir: pohunek_test_support::tempdir().expect("fixture root"),
        };
        fixture.write_runtime(&banner_script(BANNER));
        fixture.write_shim("");
        fixture
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn out(&self) -> PathBuf {
        self.dir.path().join("out")
    }

    fn tree(&self) -> PathBuf {
        self.dir.path().join("install-tree")
    }

    fn npm_dir(&self) -> PathBuf {
        self.root().join("compat").join(RUNTIME).join("npm")
    }

    fn lock_path(&self) -> PathBuf {
        self.root()
            .join("compat")
            .join(RUNTIME)
            .join("compatibility-lock.json")
    }

    fn stage(&self) -> PathBuf {
        self.out().join(RUNTIME)
    }

    fn log(&self, name: &str) -> PathBuf {
        self.dir.path().join("log").join(name)
    }

    fn tools(&self) -> Tools {
        Tools {
            npm: self.dir.path().join("tools").join("npm"),
            path: TOOL_PATH.into(),
        }
    }

    fn lock(&self) -> Value {
        serde_json::from_slice(&fs::read(self.lock_path()).expect("read lock")).expect("lock json")
    }

    fn write_lock(&self, lock: &Value) {
        write_file(
            self.lock_path(),
            serde_json::to_vec_pretty(lock).expect("json"),
        )
        .expect("write lock");
    }

    fn edit_lock(&self, edit: impl FnOnce(&mut Value)) {
        let mut lock = self.lock();
        edit(&mut lock);
        self.write_lock(&lock);
    }

    fn lockfile(&self) -> Value {
        let bytes = fs::read(self.npm_dir().join("package-lock.json")).expect("read lockfile");
        serde_json::from_slice(&bytes).expect("lockfile json")
    }

    fn edit_lockfile(&self, edit: impl FnOnce(&mut Value)) {
        let mut lockfile = self.lockfile();
        edit(&mut lockfile);
        write_file(
            self.npm_dir().join("package-lock.json"),
            serde_json::to_vec_pretty(&lockfile).expect("json"),
        )
        .expect("write lockfile");
    }

    fn edit_manifest(&self, edit: impl FnOnce(&mut Value)) {
        let path = self.npm_dir().join("package.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).expect("read")).unwrap();
        edit(&mut manifest);
        write_file(&path, serde_json::to_vec_pretty(&manifest).expect("json")).expect("write");
    }

    fn write_runtime(&self, binary_script: &str) {
        let root = self.root();
        let npm_dir = self.npm_dir();
        fs::create_dir_all(&npm_dir).expect("npm dir");
        let lock = json!({
            "schema": 1,
            "package": "pohunek.runtime.fakert",
            "runtime": RUNTIME,
            "upstream": {
                "npm": PACKAGE,
                "release": RELEASE,
                "integrity": sri('A'),
                "binary": BINARY,
                "version_output": "fake-cli {release}"
            },
            "supported": {"min": "1.0.0", "below": "2.0.0"}
        });
        self.write_lock(&lock);
        write_file(
            npm_dir.join("package.json"),
            serde_json::to_vec_pretty(&json!({
                "name": "pohunek-upstream-fakert",
                "version": "0.0.0",
                "private": true,
                "dependencies": {PACKAGE: RELEASE}
            }))
            .expect("json"),
        )
        .expect("manifest");
        let lockfile = json!({
            "name": "pohunek-upstream-fakert",
            "version": "0.0.0",
            "lockfileVersion": 3,
            "requires": true,
            "packages": {
                "": {
                    "name": "pohunek-upstream-fakert",
                    "version": "0.0.0",
                    "dependencies": {PACKAGE: RELEASE}
                },
                "node_modules/@fake/up": {
                    "version": RELEASE,
                    "resolved": format!("{REGISTRY}@fake/up/-/up-{RELEASE}.tgz"),
                    "integrity": sri('A'),
                    "bin": {BINARY: "./bin/up.sh"}
                },
                "node_modules/dep": {
                    "version": "4.5.6",
                    "resolved": format!("{REGISTRY}dep/-/dep-4.5.6.tgz"),
                    "integrity": sri('B')
                }
            }
        });
        write_file(
            npm_dir.join("package-lock.json"),
            serde_json::to_vec_pretty(&lockfile).expect("json"),
        )
        .expect("lockfile");
        assert!(root.is_dir());

        if self.tree().exists() {
            fs::remove_dir_all(self.tree()).expect("reset the install tree");
        }
        let modules = self.tree().join("node_modules");
        let package = modules.join("@fake").join("up");
        fs::create_dir_all(package.join("bin")).expect("package dir");
        fs::create_dir_all(modules.join("dep")).expect("dep dir");
        fs::create_dir_all(modules.join(".bin")).expect("bin dir");
        write_file(
            package.join("package.json"),
            json!({"name": PACKAGE, "version": RELEASE}).to_string(),
        )
        .expect("package.json");
        write_executable(package.join("bin").join("up.sh"), binary_script).expect("binary");
        write_file(
            modules.join("dep").join("index.js"),
            "module.exports = 4;\n",
        )
        .expect("dep");
        symlink("../@fake/up/bin/up.sh", modules.join(".bin").join(BINARY)).expect("bin link");
        write_file(
            modules.join(".package-lock.json"),
            json!({"packages": {"node_modules/@fake/up": {
                "version": RELEASE, "integrity": sri('A')
            }}})
            .to_string(),
        )
        .expect("hidden lockfile");
    }

    /// Writes the `npm` shim: it records its arguments and the names (never
    /// the values) of its environment, then installs the prebuilt tree, or
    /// exits with `failure` code when one is given.
    fn write_shim(&self, failure: &str) {
        let tools = self.dir.path().join("tools");
        let log = self.dir.path().join("log");
        fs::create_dir_all(&tools).expect("tools dir");
        fs::create_dir_all(&log).expect("log dir");
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > '{log}/args'\n\
             env | sed 's/=.*//' | sort > '{log}/env-names'\n\
             printf '%s\\n' \"$npm_config_registry\" > '{log}/registry'\n\
             {failure}\n\
             cp -R '{tree}/node_modules' node_modules\n",
            log = log.display(),
            tree = self.tree().display(),
        );
        write_executable(tools.join("npm"), script).expect("shim");
    }

    fn run_stage(&self) -> Result<crate::upstream_stage::StageSummary, XtaskError> {
        stage_upstream(&self.root(), RUNTIME, &self.out(), &self.tools())
    }

    fn staged(&self) -> PathBuf {
        self.run_stage().expect("stage the fake runtime");
        self.stage()
    }

    fn verify(&self) -> Result<crate::upstream_stage::StageSummary, XtaskError> {
        verify_stage(&self.root(), RUNTIME, &self.out())
    }

    fn posix_verify(&self) -> bool {
        Command::new("/bin/sh")
            .args(["-c", POSIX_VERIFY, "sh"])
            .arg(self.stage())
            .env_clear()
            .env("PATH", TOOL_PATH)
            .status()
            .expect("run the POSIX verifier")
            .success()
    }
}

fn refusal(result: Result<impl std::fmt::Debug, XtaskError>) -> (String, Fault, usize) {
    match result {
        Err(XtaskError::UpstreamStage(StageError::Refused {
            subject,
            fault,
            more,
        })) => (subject, fault, more),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn refused(result: Result<impl std::fmt::Debug, XtaskError>, subject: &str, fault: Fault) {
    let (got_subject, got_fault, _more) = refusal(result);
    assert_eq!((got_subject.as_str(), got_fault), (subject, fault));
}

#[test]
fn an_untouched_stage_verifies_with_every_independent_checker() {
    let fixture = Fixture::new();
    let stage = fixture.staged();

    let summary = fixture.verify().expect("verify the untouched stage");
    assert_eq!(summary.release, RELEASE);
    assert_eq!(summary.links, 2, "bin/up and lib/node_modules/.bin/up");
    assert!(
        fixture.posix_verify(),
        "the documented POSIX verifier agrees"
    );
    let status = Command::new("sha256sum")
        .args(["-c", "STAGE.sha256"])
        .current_dir(&stage)
        .env_clear()
        .env("PATH", TOOL_PATH)
        .output()
        .expect("run sha256sum");
    assert!(status.status.success(), "sha256sum -c accepts STAGE.sha256");

    assert_eq!(
        fs::read_link(stage.join("bin").join(BINARY)).expect("bin link"),
        Path::new("../lib/node_modules/@fake/up/bin/up.sh"),
        "bin/<binary> is the relative link npm -g would create"
    );
    let links = fs::read_to_string(stage.join("STAGE.links")).expect("links manifest");
    assert!(links.contains("link\tbin/up\t../lib/node_modules/@fake/up/bin/up.sh\n"));
    assert!(links.contains("exec\tlib/node_modules/@fake/up/bin/up.sh\n"));
    assert!(
        !fixture.out().read_dir().unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".stage-")),
        "the scratch directory is removed"
    );
}

#[test]
fn npm_runs_with_a_scrubbed_environment_and_the_lockfile_registry() {
    let fixture = Fixture::new();
    let mut env = ProcessEnv::lock();
    env.set("NPM_TOKEN", "must-not-leak")
        .set("NODE_AUTH_TOKEN", "must-not-leak")
        .set("npm_config_registry", "https://evil.example.test/")
        .set("npm_config__authtoken", "must-not-leak");
    fixture.staged();
    drop(env);

    let names = fs::read_to_string(fixture.log("env-names")).expect("env names");
    let names: Vec<&str> = names.lines().collect();
    for leaked in ["NPM_TOKEN", "NODE_AUTH_TOKEN", "npm_config__authtoken"] {
        assert!(!names.contains(&leaked), "{leaked} reached npm");
    }
    for required in [
        "PATH",
        "HOME",
        "npm_config_userconfig",
        "npm_config_globalconfig",
    ] {
        assert!(names.contains(&required), "{required} is set for npm");
    }
    assert_eq!(
        fs::read_to_string(fixture.log("registry")).expect("registry"),
        format!("{REGISTRY}\n"),
        "the registry comes from the lockfile's resolved URLs"
    );
    let args = fs::read_to_string(fixture.log("args")).expect("args");
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        ["ci", "--ignore-scripts", "--no-audit", "--no-fund"]
    );
}

#[test]
fn install_scripts_run_only_when_the_lock_enables_them_for_the_locked_package() {
    let fixture = Fixture::new();
    fixture.edit_lock(|lock| lock["upstream"]["scripts"] = json!(true));
    let wanted = format!("{PACKAGE}@{RELEASE}");
    fixture.edit_manifest(|manifest| manifest["allowScripts"] = json!({ wanted.clone(): true }));
    fixture.edit_lockfile(|lockfile| {
        lockfile["packages"]["node_modules/@fake/up"]["hasInstallScript"] = json!(true);
    });
    fixture.staged();
    let args = fs::read_to_string(fixture.log("args")).expect("args");
    assert!(args.lines().any(|a| a == "--ignore-scripts=false"));

    // A dependency that gains an install script is refused before npm runs.
    let other = Fixture::new();
    other.edit_lock(|lock| lock["upstream"]["scripts"] = json!(true));
    other.edit_manifest(|manifest| manifest["allowScripts"] = json!({ wanted.clone(): true }));
    other.edit_lockfile(|lockfile| {
        lockfile["packages"]["node_modules/@fake/up"]["hasInstallScript"] = json!(true);
        lockfile["packages"]["node_modules/dep"]["hasInstallScript"] = json!(true);
    });
    refused(
        other.run_stage(),
        "npm/package-lock.json hasInstallScript",
        Fault::Mismatch,
    );
    assert!(!other.log("args").exists(), "npm never ran");
}

/// Every way a staged tree can drift is refused by `verify-stage` and by the
/// POSIX verifier, and the refusal names the path.
#[test]
fn drift_after_staging_is_refused_by_both_verifiers() {
    type Mutation = fn(&Path);
    let dep = "lib/node_modules/dep/index.js";
    let cases: [(&str, Mutation, &str, Fault); 7] = [
        (
            "modified file",
            |s| {
                write_file(
                    s.join("lib/node_modules/dep/index.js"),
                    "module.exports = 5;\n",
                )
                .unwrap();
            },
            dep,
            Fault::Modified,
        ),
        (
            "added file",
            |s| {
                write_file(s.join("lib/node_modules/dep/extra.js"), "x").unwrap();
            },
            "lib/node_modules/dep/extra.js",
            Fault::Added,
        ),
        (
            "removed file",
            |s| {
                fs::remove_file(s.join("lib/node_modules/dep/index.js")).unwrap();
            },
            dep,
            Fault::Removed,
        ),
        (
            "retargeted link",
            |s| {
                let link = s.join("bin/up");
                fs::remove_file(&link).unwrap();
                symlink("../lib/node_modules/dep/index.js", link).unwrap();
            },
            "bin/up",
            Fault::Retargeted,
        ),
        (
            "file replaced by an identical-content link",
            |s| {
                let file = s.join("lib/node_modules/dep/index.js");
                fs::rename(&file, s.join("lib/node_modules/dep/real.js")).unwrap();
                symlink("real.js", file).unwrap();
            },
            dep,
            Fault::TypeChanged,
        ),
        (
            "cleared executable bit",
            |s| {
                let binary = s.join("lib/node_modules/@fake/up/bin/up.sh");
                fs::set_permissions(binary, fs::Permissions::from_mode(0o644)).unwrap();
            },
            "lib/node_modules/@fake/up/bin/up.sh",
            Fault::ModeChanged,
        ),
        (
            "added executable bit",
            |s| {
                let file = s.join("lib/node_modules/dep/index.js");
                fs::set_permissions(file, fs::Permissions::from_mode(0o755)).unwrap();
            },
            dep,
            Fault::ModeChanged,
        ),
    ];

    for (name, mutate, subject, fault) in cases {
        let fixture = Fixture::new();
        let stage = fixture.staged();
        assert!(fixture.posix_verify(), "{name}: baseline");
        mutate(&stage);
        let (got_subject, got_fault, _more) = refusal(fixture.verify());
        assert_eq!(
            (got_subject.as_str(), got_fault),
            (subject, fault),
            "{name}"
        );
        assert!(
            !fixture.posix_verify(),
            "{name}: the POSIX verifier accepted it"
        );
    }
}

#[test]
fn a_refusal_never_echoes_file_content() {
    let fixture = Fixture::new();
    let stage = fixture.staged();
    write_file(
        stage.join("lib/node_modules/dep/index.js"),
        "TOP-SECRET-CONTENT",
    )
    .expect("modify");
    let error = fixture.verify().expect_err("modified file is refused");
    assert!(!error.to_string().contains("TOP-SECRET-CONTENT"));
}

#[test]
fn a_missing_or_corrupt_manifest_is_refused() {
    let fixture = Fixture::new();
    let stage = fixture.staged();
    let sha256 = fs::read_to_string(stage.join("STAGE.sha256")).expect("manifest");

    write_file(stage.join("STAGE.sha256"), "not a manifest\n").expect("corrupt");
    refused(fixture.verify(), "STAGE.sha256", Fault::Malformed);
    write_file(stage.join("STAGE.sha256"), &sha256).expect("restore");
    fixture.verify().expect("restored manifest verifies");

    fs::remove_file(stage.join("STAGE.sha256")).expect("remove manifest");
    refused(fixture.verify(), "STAGE.sha256", Fault::Missing);
    write_file(stage.join("STAGE.sha256"), &sha256).expect("restore");
    fs::remove_file(stage.join("STAGE.links")).expect("remove links");
    refused(fixture.verify(), "STAGE.links", Fault::Missing);

    fs::remove_dir_all(&stage).expect("remove stage");
    refused(fixture.verify(), RUNTIME, Fault::Missing);
}

#[test]
fn a_manifest_symlink_outside_the_stage_is_refused_by_both_verifiers() {
    for name in ["STAGE.sha256", "STAGE.links"] {
        let fixture = Fixture::new();
        let stage = fixture.staged();
        let manifest = stage.join(name);
        let outside = fixture.out().join(format!("outside-{name}"));
        fs::rename(&manifest, &outside).expect("move manifest outside the stage");
        symlink(&outside, &manifest).expect("link to the external manifest");

        refused(fixture.verify(), name, Fault::TypeChanged);
        assert!(
            !fixture.posix_verify(),
            "{name}: POSIX verifier accepted a link"
        );
    }
}

#[test]
fn a_directory_in_place_of_a_manifest_is_refused_by_both_verifiers() {
    for name in ["STAGE.sha256", "STAGE.links"] {
        let fixture = Fixture::new();
        let stage = fixture.staged();
        let manifest = stage.join(name);
        fs::remove_file(&manifest).expect("remove manifest");
        fs::create_dir(&manifest).expect("replace manifest with directory");

        refused(fixture.verify(), name, Fault::TypeChanged);
        assert!(
            !fixture.posix_verify(),
            "{name}: POSIX verifier accepted a directory"
        );
    }
}

#[test]
fn a_stage_built_from_another_npm_project_is_refused() {
    let fixture = Fixture::new();
    fixture.staged();
    let path = fixture.npm_dir().join("package-lock.json");
    let mut bytes = fs::read(&path).expect("lockfile");
    bytes.push(b'\n');
    write_file(&path, bytes).expect("edit committed lockfile");
    refused(fixture.verify(), "lib/package-lock.json", Fault::Mismatch);
}

#[test]
fn a_tree_that_cannot_be_recorded_faithfully_is_refused_at_staging() {
    let escape_cases: [(&str, &str); 2] = [("absolute", "/etc/passwd"), ("parent", "../../../x")];
    for (name, target) in escape_cases {
        let fixture = Fixture::new();
        symlink(target, fixture.tree().join("node_modules").join("evil")).expect("link");
        let (subject, fault, _more) = refusal(fixture.run_stage());
        assert_eq!(
            (subject.as_str(), fault),
            ("lib/node_modules/evil", Fault::EscapingLink),
            "{name}"
        );
        assert!(!fixture.stage().exists(), "{name}: nothing is left behind");
    }

    let fixture = Fixture::new();
    write_file(
        fixture
            .tree()
            .join("node_modules")
            .join("dep")
            .join("a\nb.js"),
        "x",
    )
    .expect("odd name");
    let (_subject, fault, _more) = refusal(fixture.run_stage());
    assert_eq!(fault, Fault::UnsafeName);
}

#[test]
fn a_banner_other_than_the_locked_release_fails_and_leaves_no_stage() {
    let fixture = Fixture::new();
    fixture.write_runtime(&banner_script("fake-cli 9.9.9"));
    match fixture.run_stage() {
        Err(XtaskError::UpstreamStage(StageError::Banner { expected, actual })) => {
            assert_eq!(expected, BANNER);
            assert!(actual.contains("9.9.9"));
        }
        other => panic!("expected a banner refusal, got {other:?}"),
    }
    assert!(!fixture.stage().exists());
    assert_eq!(
        fixture.out().read_dir().unwrap().count(),
        0,
        "scratch is removed"
    );
}

/// Regression: a runtime that writes into its own install while it prints
/// its banner would also break the isolated run, so staging refuses it.
#[test]
fn a_runtime_that_writes_into_its_install_during_the_probe_is_refused() {
    let fixture = Fixture::new();
    fixture.write_runtime(&format!(
        "#!/bin/sh\necho scribble > \"$(dirname \"$0\")/touched\"\necho '{BANNER}'\n"
    ));
    refused(fixture.run_stage(), "bin/touched", Fault::Added);
    assert!(!fixture.stage().exists());
}

#[test]
fn a_failing_install_reports_npm_and_leaves_nothing() {
    let fixture = Fixture::new();
    fixture.write_shim("exit 3");
    match fixture.run_stage() {
        Err(XtaskError::UpstreamStage(StageError::Command { step, outcome })) => {
            assert_eq!((step, outcome), ("npm ci", Outcome::Exit(Some(3))));
        }
        other => panic!("expected an npm failure, got {other:?}"),
    }
    assert_eq!(fixture.out().read_dir().unwrap().count(), 0);
}

#[test]
fn an_existing_stage_is_never_overwritten() {
    let fixture = Fixture::new();
    let stage = fixture.staged();
    fixture.verify().expect("first stage verifies");

    refused(fixture.run_stage(), RUNTIME, Fault::AlreadyExists);
    fixture.verify().expect("the existing stage is untouched");
    assert!(stage.join("STAGE.sha256").is_file());

    fs::remove_dir_all(&stage).expect("remove");
    fs::create_dir(&stage).expect("empty destination");
    fixture
        .run_stage()
        .expect("an empty destination is replaced");
    fixture.verify().expect("restaged tree verifies");
}

#[test]
fn a_native_digest_pin_is_checked_for_this_platform() {
    let key = host_key().expect("tests run on a platform with a pinned key");
    let script = banner_script(BANNER);
    let digest = {
        use sha2::{Digest as _, Sha256};
        format!("{:x}", Sha256::digest(script.as_bytes()))
    };

    let fixture = Fixture::new();
    fixture.edit_lock(|lock| lock["upstream"]["sha256"] = json!({ key: digest.clone() }));
    fixture.staged();
    fixture.verify().expect("the pinned binary verifies");

    let wrong = Fixture::new();
    wrong.edit_lock(|lock| lock["upstream"]["sha256"] = json!({ key: "0".repeat(64) }));
    refused(
        wrong.run_stage(),
        &format!("upstream.sha256.{key}"),
        Fault::Mismatch,
    );

    let missing = Fixture::new();
    missing.edit_lock(|lock| lock["upstream"]["sha256"] = json!({ "plan9-mips": digest.clone() }));
    refused(
        missing.run_stage(),
        &format!("upstream.sha256.{key}"),
        Fault::Unpinned,
    );
}

#[test]
fn a_release_other_than_the_locked_one_is_refused_after_install() {
    let fixture = Fixture::new();
    write_file(
        fixture.tree().join("node_modules/@fake/up/package.json"),
        json!({"name": PACKAGE, "version": "1.2.4"}).to_string(),
    )
    .expect("package.json");
    refused(fixture.run_stage(), "installed release", Fault::Mismatch);

    let fixture = Fixture::new();
    write_file(
        fixture.tree().join("node_modules/.package-lock.json"),
        json!({"packages": {"node_modules/@fake/up": {
            "version": RELEASE, "integrity": sri('C')
        }}})
        .to_string(),
    )
    .expect("hidden lockfile");
    refused(fixture.run_stage(), "installed integrity", Fault::Mismatch);
}

type Edit = fn(&Fixture);
type Case<'a> = (&'a str, Edit, &'a str, Fault);

/// Applies each edit to a fresh fixture and requires the refusal before npm
/// runs and before any stage exists.
fn assert_refused_before_npm(cases: Vec<Case<'_>>) {
    for (name, edit, subject, fault) in cases {
        let fixture = Fixture::new();
        edit(&fixture);
        let (got_subject, got_fault, _more) = refusal(fixture.run_stage());
        assert_eq!(
            (got_subject.as_str(), got_fault),
            (subject, fault),
            "{name}"
        );
        assert!(!fixture.log("args").exists(), "{name}: npm ran");
        assert!(!fixture.stage().exists(), "{name}: a stage was created");
    }
}

#[test]
fn a_lock_that_does_not_pin_an_npm_release_is_refused_before_npm_runs() {
    let cases: Vec<Case<'_>> = vec![
        (
            "no integrity",
            |f| {
                f.edit_lock(|l| {
                    l["upstream"].as_object_mut().unwrap().remove("integrity");
                });
            },
            "upstream.integrity",
            Fault::Missing,
        ),
        (
            "truncated integrity",
            |f| {
                f.edit_lock(|l| l["upstream"]["integrity"] = json!("sha512-AAAA"));
            },
            "upstream.integrity",
            Fault::Malformed,
        ),
        (
            "sha1 integrity",
            |f| {
                f.edit_lock(|l| {
                    l["upstream"]["integrity"] = json!("sha1-AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
                });
            },
            "upstream.integrity",
            Fault::Malformed,
        ),
        (
            "not an npm lock",
            |f| {
                f.edit_lock(|l| {
                    l["upstream"].as_object_mut().unwrap().remove("npm");
                });
            },
            "upstream.npm",
            Fault::Unsupported,
        ),
        (
            "other lock schema",
            |f| {
                f.edit_lock(|l| l["schema"] = json!(2));
            },
            "lock.schema",
            Fault::Unsupported,
        ),
        (
            "other runtime",
            |f| {
                f.edit_lock(|l| l["runtime"] = json!("other"));
            },
            "lock.runtime",
            Fault::Mismatch,
        ),
        (
            "no banner",
            |f| {
                f.edit_lock(|l| {
                    l["upstream"]
                        .as_object_mut()
                        .unwrap()
                        .remove("version_output");
                });
            },
            "upstream.version_output",
            Fault::Missing,
        ),
        (
            "banner without release",
            |f| {
                f.edit_lock(|l| l["upstream"]["version_output"] = json!("fake-cli"));
            },
            "upstream.version_output",
            Fault::Malformed,
        ),
    ];
    assert_refused_before_npm(cases);
}

#[test]
fn a_lockfile_that_does_not_pin_the_locked_release_is_refused_before_npm_runs() {
    let cases: Vec<Case<'_>> = vec![
        (
            "lockfile integrity differs",
            |f| {
                f.edit_lockfile(|l| {
                    l["packages"]["node_modules/@fake/up"]["integrity"] = json!(sri('D'));
                });
            },
            "upstream.integrity",
            Fault::Mismatch,
        ),
        (
            "lockfile release differs",
            |f| {
                f.edit_lockfile(|l| {
                    l["packages"]["node_modules/@fake/up"]["version"] = json!("1.2.4");
                });
            },
            "npm/package-lock.json release",
            Fault::Mismatch,
        ),
        (
            "dependency without integrity",
            |f| {
                f.edit_lockfile(|l| {
                    l["packages"]["node_modules/dep"]
                        .as_object_mut()
                        .unwrap()
                        .remove("integrity");
                });
            },
            "npm/package-lock.json node_modules/dep",
            Fault::Missing,
        ),
        (
            "dependency from another host",
            |f| {
                f.edit_lockfile(|l| {
                    l["packages"]["node_modules/dep"]["resolved"] =
                        json!("https://mirror.example.test/dep.tgz");
                });
            },
            "npm/package-lock.json resolved",
            Fault::Unsupported,
        ),
        (
            "dependency over http",
            |f| {
                f.edit_lockfile(|l| {
                    l["packages"]["node_modules/dep"]["resolved"] =
                        json!("http://registry.example.test/dep.tgz");
                });
            },
            "npm/package-lock.json node_modules/dep",
            Fault::Unsupported,
        ),
    ];
    assert_refused_before_npm(cases);
}

#[test]
fn an_npm_manifest_that_does_not_pin_exactly_is_refused_before_npm_runs() {
    let cases: Vec<Case<'_>> = vec![
        (
            "floating extra dependency",
            |f| {
                f.edit_manifest(|m| m["dependencies"]["extra"] = json!("^1.0.0"));
            },
            "npm/package.json",
            Fault::Mismatch,
        ),
        (
            "scripts allowed without the lock saying so",
            |f| {
                f.edit_manifest(|m| m["allowScripts"] = json!({"@fake/up@1.2.3": true}));
            },
            "npm/package.json allowScripts",
            Fault::Mismatch,
        ),
        (
            "old lockfile format",
            |f| {
                f.edit_lockfile(|l| l["lockfileVersion"] = json!(2));
            },
            "npm/package-lock.json lockfileVersion",
            Fault::Unsupported,
        ),
    ];
    assert_refused_before_npm(cases);
}

#[test]
fn the_hermes_lock_is_not_a_shape_this_command_stages() {
    let root = pohunek_test_support::workspace_root();
    let out = pohunek_test_support::tempdir().expect("out");
    let tools = Tools {
        npm: PathBuf::from("/nonexistent/npm"),
        path: TOOL_PATH.into(),
    };
    refused(
        stage_upstream(&root, "hermes", out.path(), &tools),
        "lock.schema",
        Fault::Unsupported,
    );
    refused(
        verify_stage(&root, "hermes", out.path()),
        "lock.schema",
        Fault::Unsupported,
    );
}

#[test]
fn a_runtime_name_cannot_leave_the_compat_directory() {
    let root = pohunek_test_support::workspace_root();
    for name in ["", "../x", "a/b", "Pi", "pi "] {
        refused(read_lock(&root, name), "runtime", Fault::Malformed);
    }
}

/// Every official package must pin its upstream release the way this command
/// stages it; a new `runtime-packages/<runtime>` without pins fails here.
#[test]
fn every_official_runtime_package_has_a_consistent_upstream_pin() {
    let root = pohunek_test_support::workspace_root();
    let mut checked = 0;
    for entry in fs::read_dir(root.join("runtime-packages")).expect("runtime-packages") {
        let entry = entry.expect("entry");
        if !entry.file_type().expect("type").is_dir() {
            continue;
        }
        let runtime = entry.file_name().into_string().expect("utf-8 name");
        let lock = read_lock(&root, &runtime).unwrap_or_else(|e| panic!("{runtime}: {e}"));
        read_project(&root, &runtime, &lock).unwrap_or_else(|e| panic!("{runtime}: {e}"));
        checked += 1;
    }
    assert!(checked >= 3, "the official packages were found");
}

#[test]
fn the_commands_parse_their_documented_arguments() {
    use clap::Parser as _;

    #[derive(Debug, clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        action: crate::attestation::CompatAction,
    }
    for args in [
        ["cli", "stage-upstream", "--runtime", "pi", "--out", "o"].as_slice(),
        ["cli", "verify-stage", "--runtime", "pi", "--dir", "o"].as_slice(),
    ] {
        Cli::try_parse_from(args).expect("parse");
    }
    Cli::try_parse_from(["cli", "stage-upstream", "--runtime", "pi"])
        .expect_err("--out is required");
}

#[test]
fn the_knowledge_page_quotes_the_posix_verifier_verbatim() {
    let page = fs::read_to_string(
        pohunek_test_support::workspace_root().join("docs/knowledge/concepts/upstream-staging.md"),
    )
    .expect("read the knowledge page");
    assert!(
        page.contains(POSIX_VERIFY),
        "docs/knowledge/concepts/upstream-staging.md must quote POSIX_VERIFY"
    );
}
