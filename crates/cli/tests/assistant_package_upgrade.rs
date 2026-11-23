//! The assistant package step of `pohunek service upgrade`: after a committed
//! upgrade it installs the version the signed plugin index lists for the new
//! core, keeps what the owner set, and reports every outcome without failing
//! the upgrade.
//!
//! A real `pohunekd` that holds an index trust anchor serves the installation,
//! the real `pohunek` binary installs the package that is selected before the
//! upgrade, and a loopback server serves the signed schema 2 index and the
//! archive. The step is driven through the seam `upgrade_assistant::reconcile`,
//! which is the CLI action minus the engine (the engine cannot stage an older
//! to newer core change in-process).

// Rust guideline compliant 2026-11-21

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use package::{build_archive, read_archive, ArchiveEntry, IndexChannel, Limits};
use pohunek_cli::service::report::{AcceptanceGroups, AssistantPackageStatus, UpgradeReport};
use pohunek_cli::service::VERSION;
use pohunek_cli::upgrade_assistant::{self, Options};
use pohunek_paths::{BasePaths, PathEnv, Platform};
use pohunek_test_support::catalog::{anchor_for, key_id, test_key};
use pohunek_test_support::env::TestEnv;
use serde_json::Value;

use super::plugin_catalog_install::{Anchor, Channel, Install};

#[expect(
    dead_code,
    reason = "the module is shared with scenarios that use more of its helpers"
)]
#[path = "support/assistant_index.rs"]
mod assistant_index;

use assistant_index::{Archive, Keys, Server, ASSISTANT_PACKAGE, INDEX_PATH};

/// A version older than every core this build can be.
const OLDER_VERSION: &str = "0.0.1";

/// A version no real core has reached.
const FUTURE_VERSION: &str = "99.0.0";

/// Writes an assistant context package archive of `version` below `dir` and
/// returns its path and bare SHA-256 hex.
fn context_archive(dir: &Path, version: &str) -> (PathBuf, String) {
    let manifest = format!(
        "schema = 1\nid = \"{ASSISTANT_PACKAGE}\"\nversion = \"{version}\"\nruntime_api = 1\nrequires = [\"package-context-v1\"]\n\n[context]\ndir = \"bundle\"\n"
    );
    let entries = [
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: manifest.into_bytes(),
            executable: false,
        },
        ArchiveEntry {
            path: "bundle/index.md".to_owned(),
            contents: b"# Index\n".to_vec(),
            executable: false,
        },
    ];
    let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the package archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the archive reads")
        .digest()
        .clone();
    let path = dir.join(format!("assistant-{version}.tar.zst"));
    fs::write(&path, bytes).expect("write the context archive");
    (
        path,
        digest.as_str().trim_start_matches("sha256:").to_owned(),
    )
}

/// An upgrade report that moved the core from `from` to `to`.
fn upgrade_report(from: &str, to: &str) -> UpgradeReport {
    UpgradeReport {
        warnings: Vec::new(),
        from_version: from.to_owned(),
        to_version: to.to_owned(),
        unchanged: false,
        resumed: false,
        rolled_back: None,
        removed_versions: Vec::new(),
        kept_versions: Vec::new(),
        gc_error: None,
        preflight: None,
        accepted_runtime_loss: false,
        acceptance: AcceptanceGroups::default(),
        config_schema_migration: None,
        runtimes: None,
        integrations: Vec::new(),
        repair: None,
        refused_entries: Vec::new(),
        assistant: None,
    }
}

/// One installed version of the assistant package as the daemon lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Listed {
    version: String,
    digest: String,
    selected: bool,
    enabled: bool,
}

/// A daemon with an index trust anchor, the signing keys and a loopback
/// server that serves the signed index.
struct Rig {
    install: Install,
    server: Server,
    keys: Keys,
}

impl Rig {
    async fn start() -> Self {
        let root = test_key();
        let anchor = Anchor::File {
            bytes: anchor_for(&root),
            mode: 0o644,
        };
        let channel = IndexChannel::new(key_id(&root))
            .to_bytes()
            .expect("encode the index channel");
        let install = Install::start_with_channel(
            &anchor,
            Some(Channel {
                bytes: &channel,
                mode: 0o644,
            }),
        )
        .await;
        Self {
            install,
            server: Server::start(),
            keys: Keys::with_root(root),
        }
    }

    /// The paths of the installation the daemon serves.
    fn base(&self) -> BasePaths {
        base_paths(&self.install.env)
    }

    fn url(&self) -> String {
        self.server.url(INDEX_PATH)
    }

    /// Serves a signed index that lists `version` of the package, and its
    /// archive.
    fn publish(&self, version: &str) -> Archive {
        let archive = assistant_index::assistant_archive(version, version);
        let path = assistant_index::archive_path(version);
        let entry = assistant_index::assistant_entry(&archive, version, self.server.url(&path));
        self.server.set(INDEX_PATH, self.keys.signed(vec![entry]));
        self.server.set(&path, archive.bytes.clone());
        archive
    }

    /// Serves a signed index that lists nothing.
    fn publish_nothing(&self) {
        self.server.set(INDEX_PATH, self.keys.signed(Vec::new()));
    }

    /// Installs `version` of the package by digest, as the owner would, and
    /// selects it.
    async fn install_by_digest(&self, version: &str) -> String {
        let dir = self.install.env.root().to_path_buf();
        let (path, digest) = context_archive(&dir, version);
        let (code, document) = self
            .install
            .json(&[
                "plugin",
                "install",
                path.to_str().expect("UTF-8 path"),
                "--sha256",
                &digest,
                "--yes",
            ])
            .await;
        assert_eq!(code, 0, "{document}");
        format!("sha256:{digest}")
    }

    /// The assistant package versions the daemon lists.
    async fn listed(&self) -> Vec<Listed> {
        let mut listed: Vec<Listed> = self
            .install
            .packages()
            .await
            .iter()
            .filter(|info| info["package"]["id"] == ASSISTANT_PACKAGE)
            .map(|info| Listed {
                version: info["package"]["version"]
                    .as_str()
                    .expect("version")
                    .to_owned(),
                digest: info["digest"].as_str().expect("digest").to_owned(),
                selected: info["selected"].as_bool().expect("selected"),
                enabled: info["enabled"].as_bool().expect("enabled"),
            })
            .collect();
        listed.sort_by(|left, right| left.version.cmp(&right.version));
        listed
    }

    /// The selected assistant package, when one is selected.
    async fn selected(&self) -> Option<Listed> {
        self.listed().await.into_iter().find(|entry| entry.selected)
    }

    /// The upgrade step for a core that moved from `from` to `to`.
    async fn reconcile(&self, from: &str, to: &str, opt_out: bool, repair: bool) -> UpgradeReport {
        let url = self.url();
        let options = Options {
            opt_out,
            repair,
            index_url: Some(&url),
        };
        upgrade_assistant::reconcile(upgrade_report(from, to), &self.base(), &options).await
    }

    /// The upgrade step of the usual run: the core moves from the older
    /// version to this build.
    async fn upgrade(&self) -> UpgradeReport {
        self.reconcile(OLDER_VERSION, VERSION, false, false).await
    }
}

/// The paths of the installation of `env`.
fn base_paths(env: &TestEnv) -> BasePaths {
    BasePaths::resolve_for(
        Platform::current().expect("platform"),
        nix::unistd::Uid::effective().as_raw(),
        &PathEnv {
            xdg_runtime_dir: Some(env.runtime_dir().into()),
            xdg_data_home: Some(env.data_home().into()),
            xdg_state_home: Some(env.state_home().into()),
            xdg_cache_home: Some(env.cache_home().into()),
            xdg_config_home: Some(env.config_home().into()),
            home: Some(env.home().into()),
        },
    )
    .expect("resolve the installation paths")
}

/// The report as the `--json` document the upgrade prints.
fn as_json(report: &UpgradeReport) -> Value {
    serde_json::to_value(report).expect("the upgrade report serializes")
}

/// The `assistant.install` object of `document`; panics when it is absent.
fn install_of(document: &Value) -> &Value {
    let install = &document["assistant"]["install"];
    assert!(install.is_object(), "{document}");
    install
}

/// Asserts that the install step failed and its detail contains `needle`.
fn assert_failed(document: &Value, needle: &str) {
    let install = install_of(document);
    assert_eq!(install["result"], "failed", "{document}");
    let detail = install["detail"]
        .as_str()
        .expect("the failure has a detail");
    assert!(detail.contains(needle), "{needle}: {detail}");
}

/// Whether the daemon lists one version of the package, selected.
fn only_selected(listed: &[Listed], version: &str) -> bool {
    listed.len() == 1 && listed[0].selected && listed[0].version == version
}

/// The upgrade installs the version the index lists for the new core when the
/// selected one is older, selects it in one step, keeps the old version
/// installed and reports both digests. A second upgrade finds it current and
/// asks nobody.
#[tokio::test]
async fn the_upgrade_installs_the_listed_package_over_an_older_selection() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let rig = Rig::start().await;
    let old_digest = rig.install_by_digest(OLDER_VERSION).await;
    let archive = rig.publish(VERSION);
    assert!(only_selected(&rig.listed().await, OLDER_VERSION));

    let report = rig.upgrade().await;
    let document = as_json(&report);
    let assistant = &document["assistant"];
    assert_eq!(assistant["status"], "current", "{document}");
    assert_eq!(assistant["version"], VERSION, "{document}");
    let install = install_of(&document);
    assert_eq!(install["result"], "installed", "{document}");
    assert_eq!(install["version"], VERSION, "{document}");
    assert_eq!(install["digest"], archive.digest.as_str(), "{document}");
    assert_eq!(install["replaced_version"], OLDER_VERSION, "{document}");
    assert_eq!(install["replaced_digest"], old_digest, "{document}");
    let text = upgrade_assistant::render_report(&report);
    assert!(
        text.contains(&format!(
            "{OLDER_VERSION} -> {VERSION} installed from the plugin index"
        )),
        "{text}"
    );

    let listed = rig.listed().await;
    assert_eq!(
        listed.len(),
        2,
        "the old digest stays installed: {listed:?}"
    );
    let selected = rig.selected().await.expect("a version is selected");
    assert_eq!(selected.version, VERSION);
    assert_eq!(selected.digest, archive.digest.as_str());
    assert!(selected.enabled);

    let requests = rig.server.requests().len();
    assert!(requests > 0);
    let again = document_of_upgrade(&rig).await;
    assert_eq!(again["assistant"]["status"], "current", "{again}");
    assert!(again["assistant"].get("install").is_none(), "{again}");
    assert_eq!(
        rig.server.requests().len(),
        requests,
        "a current package asks no index"
    );
}

async fn document_of_upgrade(rig: &Rig) -> Value {
    as_json(&rig.upgrade().await)
}

/// An absent package is installed as `pohunek setup` installs it; an index
/// that lists nothing, or nothing newer than the selection, leaves the old
/// package and says the package is not published.
#[tokio::test]
async fn an_unpublished_package_is_reported_and_the_old_one_stays() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let rig = Rig::start().await;

    // Nothing installed and nothing listed.
    rig.publish_nothing();
    let report = rig.upgrade().await;
    let document = as_json(&report);
    assert_eq!(document["assistant"]["status"], "absent", "{document}");
    let install = install_of(&document);
    assert_eq!(install["result"], "not_published", "{document}");
    assert_eq!(install["command"], "pohunek setup", "{document}");
    assert!(rig.listed().await.is_empty());
    let text = upgrade_assistant::render_report(&report);
    assert!(
        text.contains("pohunek setup") && text.contains(VERSION),
        "{text}"
    );

    // An older package is selected and the index lists nothing for the core.
    rig.install_by_digest(OLDER_VERSION).await;
    let report = rig.upgrade().await;
    let document = as_json(&report);
    assert_eq!(document["assistant"]["status"], "outdated", "{document}");
    assert_eq!(
        install_of(&document)["result"],
        "not_published",
        "{document}"
    );
    assert!(install_of(&document)["detail"]
        .as_str()
        .expect("detail")
        .contains(OLDER_VERSION));
    assert!(only_selected(&rig.listed().await, OLDER_VERSION));

    // The index lists only the version that is selected already.
    rig.publish(OLDER_VERSION);
    let document = document_of_upgrade(&rig).await;
    assert_eq!(
        install_of(&document)["result"],
        "not_published",
        "{document}"
    );
    assert!(only_selected(&rig.listed().await, OLDER_VERSION));

    // The first listing for the core installs it, as the setup does.
    let fresh = Rig::start().await;
    let archive = fresh.publish(VERSION);
    let document = document_of_upgrade(&fresh).await;
    let install = install_of(&document);
    assert_eq!(install["result"], "installed", "{document}");
    assert_eq!(install["replaced_version"], Value::Null, "{document}");
    let selected = fresh.selected().await.expect("selected");
    assert_eq!(selected.digest, archive.digest.as_str());
}

/// An index that lists the package only for a core this build is not in, or
/// only for another platform, is "not published" for this host, not a failure.
#[tokio::test]
async fn an_entry_for_another_core_or_platform_is_not_published() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let rig = Rig::start().await;
    let archive = assistant_index::assistant_archive(VERSION, VERSION);
    let path = assistant_index::archive_path(VERSION);

    // The only entry needs a core far above this build.
    let mut entry = assistant_index::assistant_entry(&archive, VERSION, rig.server.url(&path));
    entry.core = format!(">={FUTURE_VERSION}");
    rig.server.set(INDEX_PATH, rig.keys.signed(vec![entry]));
    let document = document_of_upgrade(&rig).await;
    assert_eq!(
        install_of(&document)["result"],
        "not_published",
        "{document}"
    );
    assert!(rig.listed().await.is_empty());

    // The only entry is for a platform this host is not.
    let mut entry = assistant_index::assistant_entry(&archive, VERSION, rig.server.url(&path));
    entry.platforms = vec!["unlisted-os-unlisted-arch".to_owned()];
    rig.server.set(INDEX_PATH, rig.keys.signed(vec![entry]));
    let document = document_of_upgrade(&rig).await;
    assert_eq!(
        install_of(&document)["result"],
        "not_published",
        "{document}"
    );
    assert!(rig.listed().await.is_empty());
}

/// An index or an archive that cannot be fetched, a signature the host does
/// not trust and an archive whose bytes are not the listed digest are refused
/// without a change, and a later good index installs: a refused install does
/// not leave the daemon's index mark above a good document.
#[tokio::test]
async fn a_refused_install_changes_nothing_and_a_later_good_index_installs() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let rig = Rig::start().await;
    let old_digest = rig.install_by_digest(OLDER_VERSION).await;
    let before = rig.listed().await;
    let archive = rig.publish(VERSION);
    let archive_path = assistant_index::archive_path(VERSION);

    // The archive is gone.
    rig.server.remove(&archive_path);
    let document = document_of_upgrade(&rig).await;
    assert_failed(&document, "package archive cannot be fetched");
    assert_eq!(
        install_of(&document)["command"],
        "pohunek setup",
        "{document}"
    );
    assert_eq!(document["assistant"]["status"], "outdated", "{document}");
    assert_eq!(rig.listed().await, before);

    // The index is gone.
    rig.server.remove(INDEX_PATH);
    let document = document_of_upgrade(&rig).await;
    assert_failed(&document, "plugin index cannot be fetched");
    assert_eq!(rig.listed().await, before);

    // The archive bytes are not the listed digest.
    let other = assistant_index::assistant_archive(VERSION, "other bytes");
    let entry = assistant_index::assistant_entry(&archive, VERSION, rig.server.url(&archive_path));
    rig.server.set(INDEX_PATH, rig.keys.signed(vec![entry]));
    rig.server.set(&archive_path, other.bytes);
    let document = document_of_upgrade(&rig).await;
    assert_failed(&document, "the index lists");
    assert_eq!(rig.listed().await, before);

    // The index is signed by a key the release does not endorse, with a
    // sequence above the good document's: the daemon must not record it.
    let entry = assistant_index::assistant_entry(&archive, VERSION, rig.server.url(&archive_path));
    let stranger = test_key();
    rig.server.set(
        INDEX_PATH,
        rig.keys.document(&stranger, now() + 1000, vec![entry]),
    );
    rig.server.set(&archive_path, archive.bytes.clone());
    let document = document_of_upgrade(&rig).await;
    assert_failed(&document, "package_untrusted");
    assert_eq!(rig.listed().await, before);
    let selected = rig.selected().await.expect("selected");
    assert_eq!(selected.digest, old_digest);

    // A good index installs afterwards.
    rig.publish(VERSION);
    let document = document_of_upgrade(&rig).await;
    assert_eq!(install_of(&document)["result"], "installed", "{document}");
    assert_eq!(
        rig.selected().await.expect("selected").digest,
        archive.digest.as_str()
    );
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_secs()
}

/// A package store the daemon cannot write to fails the install without a
/// half-installed package and without a selection change.
#[tokio::test]
async fn a_store_that_cannot_be_written_leaves_the_selection_unchanged() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    if nix::unistd::Uid::effective().is_root() {
        // A directory mode does not stop root, so the row has nothing to prove.
        return;
    }
    let rig = Rig::start().await;
    rig.install_by_digest(OLDER_VERSION).await;
    let before = rig.listed().await;
    rig.publish(VERSION);
    let packages = rig
        .install
        .env
        .state_home()
        .join("pohunek")
        .join("plugins")
        .join("packages");
    let mode = fs::metadata(&packages)
        .expect("the package store")
        .permissions()
        .mode();
    fs::set_permissions(&packages, fs::Permissions::from_mode(0o500)).expect("lock the store");
    let document = document_of_upgrade(&rig).await;
    fs::set_permissions(&packages, fs::Permissions::from_mode(mode)).expect("unlock the store");
    assert_failed(&document, "package_registry_failed");
    assert_eq!(rig.listed().await, before);
    let leftovers: Vec<_> = fs::read_dir(&packages)
        .expect("read the package store")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(leftovers.len(), before.len(), "{leftovers:?}");
}

/// An index the release does not endorse that names an installed version
/// changes no selection and reports no `installed`: the owner's older choice
/// stays.
#[tokio::test]
async fn an_unendorsed_index_naming_an_installed_version_changes_no_selection() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let rig = Rig::start().await;
    let old_digest = rig.install_by_digest(OLDER_VERSION).await;
    let archive = rig.publish(VERSION);
    let first = document_of_upgrade(&rig).await;
    assert_eq!(install_of(&first)["result"], "installed", "{first}");
    let (code, selected) = rig
        .install
        .json(&[
            "plugin",
            "select",
            ASSISTANT_PACKAGE,
            "--digest",
            &old_digest,
        ])
        .await;
    assert_eq!(code, 0, "{selected}");

    let archive_path = assistant_index::archive_path(VERSION);
    let entry = assistant_index::assistant_entry(&archive, VERSION, rig.server.url(&archive_path));
    let stranger = test_key();
    rig.server.set(
        INDEX_PATH,
        rig.keys.document(&stranger, now() + 1000, vec![entry]),
    );
    let before = rig.listed().await;
    let document = document_of_upgrade(&rig).await;
    assert_ne!(install_of(&document)["result"], "installed", "{document}");
    assert_eq!(rig.listed().await, before);
    assert_eq!(
        rig.selected().await.expect("selected").digest,
        old_digest,
        "the selection did not change"
    );
}

/// The rollback flow: the listed version is installed and enabled, the owner
/// selected an older one, and a later upgrade holds: it changes no selection
/// and names `plugin select` (a daemon install of an installed digest keeps the
/// recorded state, as after `pohunek setup`). A disabled listed version is held
/// the same way.
#[tokio::test]
async fn an_installed_listed_version_is_held_after_a_rollback() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let rig = Rig::start().await;
    let old_digest = rig.install_by_digest(OLDER_VERSION).await;
    let archive = rig.publish(VERSION);
    let first = document_of_upgrade(&rig).await;
    assert_eq!(install_of(&first)["result"], "installed", "{first}");

    let (code, selected) = rig
        .install
        .json(&[
            "plugin",
            "select",
            ASSISTANT_PACKAGE,
            "--digest",
            &old_digest,
        ])
        .await;
    assert_eq!(code, 0, "{selected}");
    assert_eq!(
        rig.selected().await.expect("selected").version,
        OLDER_VERSION
    );
    let before = rig.listed().await;

    let report = rig.upgrade().await;
    let document = as_json(&report);
    let install = install_of(&document);
    assert_eq!(install["result"], "held", "{document}");
    assert!(install["digest"].is_null(), "{document}");
    assert!(
        install["detail"]
            .as_str()
            .expect("detail")
            .contains("installed but not selected"),
        "{document}"
    );
    let command = install["command"].as_str().expect("command");
    assert_eq!(
        command,
        format!(
            "pohunek plugin select {ASSISTANT_PACKAGE} --digest {}",
            archive.digest.as_str()
        ),
        "{document}"
    );
    assert_eq!(document["assistant"]["status"], "outdated", "{document}");
    assert_eq!(rig.listed().await, before, "no selection changed");
    assert_eq!(
        rig.selected().await.expect("selected").digest,
        old_digest,
        "the owner's selection stays"
    );
    let text = upgrade_assistant::render_report(&report);
    assert!(text.contains("plugin select"), "{text}");

    let (code, disabled) = rig
        .install
        .json(&[
            "plugin",
            "disable",
            ASSISTANT_PACKAGE,
            "--digest",
            archive.digest.as_str(),
        ])
        .await;
    assert_eq!(code, 0, "{disabled}");
    let held = document_of_upgrade(&rig).await;
    assert_eq!(install_of(&held)["result"], "held", "{held}");
    assert!(
        install_of(&held)["detail"]
            .as_str()
            .expect("detail")
            .contains("installed but disabled"),
        "{held}"
    );
    assert_eq!(
        rig.selected().await.expect("selected").version,
        OLDER_VERSION,
        "a disabled listed version is not selected"
    );
}

/// A package the owner disabled stays as set: nothing is asked of the index
/// and nothing changes.
#[tokio::test]
async fn a_package_the_owner_disabled_is_left_as_set() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let rig = Rig::start().await;
    rig.install_by_digest(OLDER_VERSION).await;
    rig.publish(VERSION);

    let (code, disabled) = rig
        .install
        .json(&["plugin", "disable", ASSISTANT_PACKAGE])
        .await;
    assert_eq!(code, 0, "{disabled}");
    let before = rig.listed().await;
    let requests = rig.server.requests().len();
    let report = rig.upgrade().await;
    let document = as_json(&report);
    let install = install_of(&document);
    assert_eq!(install["result"], "held", "{document}");
    assert!(
        install["command"]
            .as_str()
            .expect("command")
            .contains("plugin enable"),
        "{document}"
    );
    assert_eq!(rig.listed().await, before);
    assert_eq!(
        rig.server.requests().len(),
        requests,
        "a held package asks no index"
    );
    let text = upgrade_assistant::render_report(&report);
    assert!(text.contains("plugin enable"), "{text}");
}

/// `--no-assistant-package` asks no index and installs nothing; the report
/// still tells that the package is older than the core.
#[tokio::test]
async fn the_opt_out_asks_no_index_and_installs_nothing() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let rig = Rig::start().await;
    rig.install_by_digest(OLDER_VERSION).await;
    rig.publish(VERSION);
    let before = rig.listed().await;

    let report = rig.reconcile(OLDER_VERSION, VERSION, true, false).await;
    let document = as_json(&report);
    assert_eq!(document["assistant"]["status"], "outdated", "{document}");
    assert_eq!(install_of(&document)["result"], "skipped", "{document}");
    assert!(
        rig.server.requests().is_empty(),
        "{:?}",
        rig.server.requests()
    );
    assert_eq!(rig.listed().await, before);
    let text = upgrade_assistant::render_report(&report);
    assert!(
        text.contains(OLDER_VERSION) && text.contains("pohunek setup"),
        "{text}"
    );
}

/// A run that is not a forward move to this build installs nothing: a
/// rollback or downgrade, an unchanged version, a repair and a core that is
/// not this build's keep the selected package and ask no index.
#[tokio::test]
async fn a_run_that_does_not_move_the_core_forward_installs_nothing() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let rig = Rig::start().await;
    rig.install_by_digest(OLDER_VERSION).await;
    rig.publish(VERSION);
    let before = rig.listed().await;

    let mut unchanged = upgrade_report(VERSION, VERSION);
    unchanged.unchanged = true;
    let url = rig.url();
    let options = Options {
        opt_out: false,
        repair: false,
        index_url: Some(&url),
    };
    let runs = [
        // A rollback to the previous release: the target is older.
        upgrade_assistant::reconcile(
            upgrade_report(FUTURE_VERSION, VERSION),
            &rig.base(),
            &options,
        )
        .await,
        // The same version again.
        upgrade_assistant::reconcile(unchanged, &rig.base(), &options).await,
        // `--repair` of the same version.
        rig.reconcile(VERSION, VERSION, false, true).await,
        // A forward move to a core that is not this build's.
        rig.reconcile(OLDER_VERSION, "0.0.2", false, false).await,
    ];
    for report in &runs {
        let document = as_json(report);
        assert_eq!(document["assistant"]["status"], "outdated", "{document}");
        assert!(document["assistant"].get("install").is_none(), "{document}");
    }
    assert!(
        rig.server.requests().is_empty(),
        "{:?}",
        rig.server.requests()
    );
    assert_eq!(rig.listed().await, before);
}

/// A daemon that does not answer after the commit leaves the package
/// unchecked, and nothing is installed.
#[tokio::test]
async fn an_unanswered_daemon_leaves_the_package_unchecked() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let env = TestEnv::new().expect("create the hermetic environment");
    let server = Server::start();
    let url = server.url(INDEX_PATH);
    let options = Options {
        opt_out: false,
        repair: false,
        index_url: Some(&url),
    };
    let report = upgrade_assistant::reconcile(
        upgrade_report(OLDER_VERSION, VERSION),
        &base_paths(&env),
        &options,
    )
    .await;
    let document = as_json(&report);
    assert_eq!(document["assistant"]["status"], "unchecked", "{document}");
    assert!(document["assistant"].get("install").is_none(), "{document}");
    assert!(server.requests().is_empty());
    let text = upgrade_assistant::render_report(&report);
    assert!(text.contains("was not checked"), "{text}");
    assert_eq!(
        report.assistant.expect("the state").status,
        AssistantPackageStatus::Unchecked
    );
}

/// An upgrade that fails before it commits never reaches the step: the exit
/// is non-zero, the report has no assistant object and the index is not
/// asked.
#[tokio::test]
async fn a_failed_upgrade_never_reaches_the_assistant_step() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let rig = Rig::start().await;
    rig.install_by_digest(OLDER_VERSION).await;
    rig.publish(VERSION);
    let missing = rig.install.env.root().join("no-such-release");
    let output = rig
        .install
        .run_with_env(
            &[
                "service",
                "upgrade",
                "--from",
                missing.to_str().expect("UTF-8 path"),
                "--json",
            ],
            &[("POHUNEK_TEST_OFFICIAL_INDEX_URL_V2", &rig.url())],
        )
        .await;
    assert!(!output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("\"assistant\""), "{stdout}");
    assert!(
        rig.server.requests().is_empty(),
        "{:?}",
        rig.server.requests()
    );
    assert!(only_selected(&rig.listed().await, OLDER_VERSION));
}
