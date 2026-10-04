//! Tests of host profiles bound to an installed package: the `package` and
//! `digest` keys, resolution from exactly that digest, revisions, retention
//! and `host.inspect`, over real archives installed into a real package store.

// Rust guideline compliant 2026-10-04

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use package::registry::{Registry, RetainedDigests};
use package::{Limits, PackageDigest};
use protocol::RuntimeId;

use super::ProfileRegistry;
use crate::agent::host::fixture::{install_pi_package, installed_pi_host, PI_SHAPED_PACKAGE_ID};
use crate::agent::host::{LaunchSource, RuntimeHost};

/// Program of the first fixture version.
const FIRST_PROGRAM: &str = "/bin/sh";

/// Program of the second fixture version; a different program makes the two
/// versions distinguishable through the capability inventory.
const SECOND_PROGRAM: &str = "/bin/true";

/// A plugin root with the fixture package installed, an agents directory and
/// a host state directory, over one runtime host.
struct Fixture {
    _root: tempfile::TempDir,
    plugins: PathBuf,
    agents: PathBuf,
    state: PathBuf,
    host: RuntimeHost,
    digest: PackageDigest,
}

impl Fixture {
    fn new() -> Self {
        let root = pohunek_test_support::tempdir().expect("private test directory");
        let plugins = root.path().join("plugins");
        let agents = root.path().join("agents");
        let state = root.path().join("state");
        fs::create_dir_all(&agents).expect("agents directory");
        fs::create_dir_all(&state).expect("state directory");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700))
            .expect("owner-private state directory");
        let (host, digest) = installed_pi_host(&plugins, Path::new(FIRST_PROGRAM));
        Self {
            _root: root,
            plugins,
            agents,
            state,
            host,
            digest,
        }
    }

    fn profiles(&self) -> ProfileRegistry {
        ProfileRegistry::with_runtimes(Some(self.agents.clone()), self.host.clone())
            .with_revision_state_dir(Some(self.state.clone()))
    }

    fn registry(&self) -> Registry {
        Registry::open_at(&self.plugins, Limits::DEFAULT).expect("registry")
    }

    /// The package keys of a profile pinned to `digest`.
    fn pin(package: &str, digest: &PackageDigest) -> String {
        format!("package = \"{package}\"\ndigest = \"{digest}\"\n")
    }

    /// A profile of base `pi` pinned to `digest`.
    fn write_pinned(&self, name: &str, digest: &PackageDigest) {
        let body = format!("base = \"pi\"\n{}", Self::pin(PI_SHAPED_PACKAGE_ID, digest));
        self.write(name, &body);
    }

    fn write(&self, name: &str, body: &str) {
        fs::write(self.agents.join(format!("{name}.toml")), body).expect("write profile");
    }

    /// Installs version 2.0.0 and selects it, then reloads the host.
    fn update_to_second_version(&self) -> PackageDigest {
        let digest = install_pi_package(&self.plugins, Path::new(SECOND_PROGRAM), "2.0.0", true);
        self.host.reload().expect("reload");
        digest
    }

    /// Rewrites the installed descriptor with other bytes of the same length.
    fn tamper(&self) {
        let hex = self
            .digest
            .as_str()
            .strip_prefix("sha256:")
            .expect("digest prefix");
        let descriptor = self
            .plugins
            .join("packages")
            .join(hex)
            .join("files")
            .join("runtime.toml");
        let mut bytes = fs::read(&descriptor).expect("read descriptor");
        let at = bytes
            .windows(b"fast".len())
            .position(|window| window == b"fast")
            .expect("model argument");
        bytes[at..at + 4].copy_from_slice(b"slow");
        fs::write(&descriptor, bytes).expect("write descriptor");
    }
}

fn code(result: Result<crate::agent::ResolvedAgent, protocol::ProtocolError>) -> String {
    result.expect_err("must be refused").code
}

fn digest_of(agent: &crate::agent::ResolvedAgent) -> PackageDigest {
    match &agent.definition.binding().provenance {
        protocol::BindingProvenance::Package { package_digest, .. } => package_digest.clone(),
        protocol::BindingProvenance::Builtin { .. } => panic!("a package definition"),
    }
}

#[test]
fn a_builtin_base_without_package_keys_resolves() {
    let fixture = Fixture::new();
    fixture.write("wrapped", "base = \"shell\"\nargs = [\"-l\"]\n");

    let agent = fixture
        .profiles()
        .resolve_agent("wrapped")
        .expect("resolves");

    assert!(agent.profile.is_some());
    assert_eq!(agent.base, RuntimeId::shell());
}

#[test]
fn package_keys_on_a_builtin_base_are_rejected() {
    let fixture = Fixture::new();
    fixture.write(
        "wrapped",
        &format!(
            "base = \"shell\"\n{}",
            Fixture::pin(PI_SHAPED_PACKAGE_ID, &fixture.digest)
        ),
    );

    let error = fixture
        .profiles()
        .resolve_agent("wrapped")
        .expect_err("a built-in base takes no package binding");

    assert_eq!(error.code, "invalid_profile");
}

#[test]
fn one_package_key_without_the_other_is_rejected() {
    let fixture = Fixture::new();
    for body in [
        format!("base = \"pi\"\npackage = \"{PI_SHAPED_PACKAGE_ID}\"\n"),
        format!("base = \"pi\"\ndigest = \"{}\"\n", fixture.digest),
        "base = \"shell\"\ndigest = \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"\n"
            .to_owned(),
    ] {
        fixture.write("half", &body);
        assert_eq!(
            code(fixture.profiles().resolve_agent("half")),
            "invalid_profile",
            "{body}"
        );
    }
}

#[test]
fn malformed_package_keys_are_rejected() {
    let fixture = Fixture::new();
    for (package, digest) in [
        ("Not A Package", fixture.digest.to_string()),
        (PI_SHAPED_PACKAGE_ID, "sha256:abc".to_owned()),
        (
            PI_SHAPED_PACKAGE_ID,
            fixture.digest.to_string().to_uppercase(),
        ),
    ] {
        fixture.write(
            "bad",
            &format!("base = \"pi\"\npackage = \"{package}\"\ndigest = \"{digest}\"\n"),
        );
        assert_eq!(
            code(fixture.profiles().resolve_agent("bad")),
            "invalid_profile",
            "{package} {digest}"
        );
    }
}

#[test]
fn a_package_served_base_without_keys_names_the_migration_command() {
    let fixture = Fixture::new();
    fixture.write("migrate-me", "base = \"pi\"\n");

    let error = fixture
        .profiles()
        .resolve_agent("migrate-me")
        .expect_err("an unbound package-served base is refused");

    assert_eq!(error.code, "invalid_profile");
    assert!(
        error
            .msg
            .contains("pohunek plugin profile migrate migrate-me"),
        "{}",
        error.msg
    );
}

#[test]
fn a_pinned_profile_resolves_from_its_digest() {
    let fixture = Fixture::new();
    fixture.write_pinned("pinned", &fixture.digest);

    let agent = fixture
        .profiles()
        .resolve_agent("pinned")
        .expect("resolves");

    assert_eq!(agent.base.as_str(), "pi");
    assert_eq!(digest_of(&agent), fixture.digest);
    assert!(agent.profile.is_some());
}

#[test]
fn a_digest_of_another_package_is_rejected() {
    let fixture = Fixture::new();
    fixture.write(
        "wrong",
        &format!(
            "base = \"pi\"\n{}",
            Fixture::pin("other.runtime.pi", &fixture.digest)
        ),
    );

    assert_eq!(
        code(fixture.profiles().resolve_agent("wrong")),
        "runtime_incompatible"
    );
}

#[test]
fn an_unknown_digest_is_not_installed() {
    let fixture = Fixture::new();
    let unknown = PackageDigest::parse(
        "sha256:7777777777777777777777777777777777777777777777777777777777777777",
    )
    .expect("digest");
    fixture.write_pinned("ghost", &unknown);

    assert_eq!(
        code(fixture.profiles().resolve_agent("ghost")),
        "runtime_not_installed"
    );
}

#[test]
fn a_modified_package_root_is_incompatible() {
    let fixture = Fixture::new();
    fixture.write_pinned("pinned", &fixture.digest);
    fixture.tamper();

    assert_eq!(
        code(fixture.profiles().resolve_agent("pinned")),
        "runtime_incompatible"
    );
}

#[test]
fn an_update_leaves_a_pinned_profile_on_its_digest_while_a_bare_name_follows_the_selection() {
    let fixture = Fixture::new();
    fixture.write_pinned("pinned", &fixture.digest);
    let second = fixture.update_to_second_version();
    let profiles = fixture.profiles();

    let pinned = profiles.resolve_agent("pinned").expect("pinned resolves");
    let bare = profiles.resolve_agent("pi").expect("bare name resolves");

    assert_eq!(digest_of(&pinned), fixture.digest);
    assert_eq!(digest_of(&bare), second);
    assert_ne!(fixture.digest, second);
}

#[test]
fn selecting_another_version_does_not_change_a_pinned_profile() {
    let fixture = Fixture::new();
    fixture.write_pinned("pinned", &fixture.digest);
    let second = fixture.update_to_second_version();
    fixture
        .registry()
        .select(&fixture.digest)
        .expect("select back");
    fixture.host.reload().expect("reload");
    fixture.write_pinned("on-second", &second);
    let profiles = fixture.profiles();

    assert_eq!(
        digest_of(&profiles.resolve_agent("pinned").expect("resolves")),
        fixture.digest
    );
    assert_eq!(
        digest_of(&profiles.resolve_agent("on-second").expect("resolves")),
        second
    );
}

#[test]
fn an_update_keeps_a_relay_approved_revision_while_a_migration_makes_it_stale() {
    let fixture = Fixture::new();
    fixture.write_pinned("pinned", &fixture.digest);
    let profiles = fixture.profiles();
    let agent = profiles.resolve_agent("pinned").expect("resolves");
    let approved = profiles
        .revision_of(&agent)
        .expect("key")
        .expect("a profile has a revision");
    let relay = LaunchSource::RelayProfile {
        profile: "pinned".to_owned(),
        revision: approved.clone(),
    };

    let second = fixture.update_to_second_version();
    relay
        .resolve(&profiles)
        .expect("an update of the package keeps the approved revision");

    fixture.write_pinned("pinned", &second);
    let migrated = relay
        .resolve(&profiles)
        .expect_err("a migration changes the digest and the revision");
    assert_eq!(migrated.code, "agent_profile_revision_stale");
    let migrated_agent = profiles.resolve_agent("pinned").expect("resolves");
    assert_ne!(
        profiles
            .revision_of(&migrated_agent)
            .expect("key")
            .expect("revision"),
        approved
    );
}

#[test]
fn a_disabled_pinned_package_still_resolves_for_existing_sessions() {
    let fixture = Fixture::new();
    fixture.write_pinned("pinned", &fixture.digest);
    fixture
        .registry()
        .set_enabled(&fixture.digest, false)
        .expect("disable");
    fixture.host.reload().expect("reload");
    let profiles = fixture.profiles();

    let agent = profiles.resolve_agent("pinned").expect("resolves");

    assert_eq!(digest_of(&agent), fixture.digest);
    assert_eq!(
        profiles
            .runtimes()
            .verify_launchable(&agent.definition)
            .expect_err("a fresh launch is refused")
            .code,
        "runtime_not_installed"
    );
}

fn pinned_set(profiles: &ProfileRegistry) -> RetainedDigests {
    profiles
        .pinned_digests()
        .expect("the agents directory scans")
}

#[test]
fn pinned_digests_lists_the_digests_profiles_pin() {
    let fixture = Fixture::new();
    fixture.write_pinned("pinned", &fixture.digest);
    fixture.write("wrapped", "base = \"shell\"\n");

    let pinned = pinned_set(&fixture.profiles());

    assert_eq!(pinned.iter().collect::<Vec<_>>(), [&fixture.digest]);
}

#[test]
fn pinned_digests_counts_a_profile_that_no_longer_resolves() {
    let fixture = Fixture::new();
    fixture.write_pinned("pinned", &fixture.digest);
    fixture.tamper();

    assert!(pinned_set(&fixture.profiles()).contains(&fixture.digest));
}

#[test]
fn pinned_digests_ignores_a_group_writable_profile() {
    let fixture = Fixture::new();
    fixture.write_pinned("pinned", &fixture.digest);
    fs::set_permissions(
        fixture.agents.join("pinned.toml"),
        fs::Permissions::from_mode(0o664),
    )
    .expect("loosen the profile");

    assert!(pinned_set(&fixture.profiles()).iter().next().is_none());
}

#[test]
fn pinned_digests_ignores_a_symlink_that_escapes_the_agents_directory() {
    let fixture = Fixture::new();
    let outside = pohunek_test_support::tempdir().expect("outside directory");
    fs::write(
        outside.path().join("evil.toml"),
        format!(
            "base = \"pi\"\n{}",
            Fixture::pin(PI_SHAPED_PACKAGE_ID, &fixture.digest)
        ),
    )
    .expect("outside profile");
    std::os::unix::fs::symlink(
        outside.path().join("evil.toml"),
        fixture.agents.join("evil.toml"),
    )
    .expect("symlink into the agents directory");

    assert!(pinned_set(&fixture.profiles()).iter().next().is_none());
}

#[test]
fn pinned_digests_skips_an_unparsable_profile_and_keeps_the_others() {
    let fixture = Fixture::new();
    fixture.write("broken", "base = [unterminated\n");
    fixture.write_pinned("pinned", &fixture.digest);
    fixture.write(
        "bad-digest",
        "base = \"pi\"\npackage = \"acme.runtime.pi\"\ndigest = \"sha256:zz\"\n",
    );

    let pinned = pinned_set(&fixture.profiles());

    assert_eq!(pinned.iter().collect::<Vec<_>>(), [&fixture.digest]);
}

#[test]
fn pinned_digests_is_empty_without_an_agents_directory() {
    let profiles = ProfileRegistry::new(None);
    assert!(pinned_set(&profiles).iter().next().is_none());
}

fn runtime_of<'a>(
    capabilities: &'a protocol::HostCapabilities,
    agent: &str,
) -> &'a protocol::AgentRuntime {
    capabilities
        .runtimes
        .iter()
        .find(|runtime| runtime.agent == agent)
        .unwrap_or_else(|| panic!("no runtime entry for {agent}"))
}

#[test]
fn host_inspect_describes_a_pinned_profile_from_its_pinned_definition() {
    let fixture = Fixture::new();
    fixture.write_pinned("pinned", &fixture.digest);
    fixture.update_to_second_version();

    let capabilities = crate::capabilities::host_capabilities("0.0.0", &fixture.profiles());

    let pinned = runtime_of(&capabilities, "pinned");
    let bare = runtime_of(&capabilities, "pi");
    assert!(pinned.available);
    assert!(
        pinned
            .path
            .as_deref()
            .is_some_and(|path| path.ends_with("sh")),
        "{:?}",
        pinned.path
    );
    assert!(
        bare.path
            .as_deref()
            .is_some_and(|path| path.ends_with("true")),
        "{:?}",
        bare.path
    );
}

#[test]
fn host_inspect_reports_a_pinned_profile_unavailable_when_its_package_is_disabled() {
    let fixture = Fixture::new();
    fixture.write_pinned("pinned", &fixture.digest);
    fixture
        .registry()
        .set_enabled(&fixture.digest, false)
        .expect("disable");
    fixture.host.reload().expect("reload");

    let capabilities = crate::capabilities::host_capabilities("0.0.0", &fixture.profiles());

    assert!(!runtime_of(&capabilities, "pinned").available);
    assert!(capabilities
        .supported_agents
        .iter()
        .any(|agent| agent == "pinned"));
}

#[test]
fn host_inspect_reports_a_pinned_profile_unavailable_when_its_root_fails_verification() {
    let fixture = Fixture::new();
    fixture.write_pinned("pinned", &fixture.digest);
    fixture.tamper();

    let capabilities = crate::capabilities::host_capabilities("0.0.0", &fixture.profiles());

    let pinned = runtime_of(&capabilities, "pinned");
    assert!(!pinned.available);
    assert_eq!(
        pinned
            .agent_base
            .as_ref()
            .and_then(|base| base.id())
            .map(RuntimeId::as_str),
        Some("pi")
    );
}

#[test]
fn host_inspect_reports_a_pinned_profile_unavailable_when_its_package_is_uninstalled() {
    let fixture = Fixture::new();
    fixture.write_pinned("pinned", &fixture.digest);
    fixture
        .registry()
        .uninstall(&fixture.digest, &RetainedDigests::new())
        .expect("uninstall");
    fixture.host.reload().expect("reload");

    let capabilities = crate::capabilities::host_capabilities("0.0.0", &fixture.profiles());

    assert!(!runtime_of(&capabilities, "pinned").available);
}

#[test]
fn the_profile_reader_never_blocks_on_a_fifo() {
    let fixture = Fixture::new();
    let fifo = fixture.agents.join("fifo.toml");
    nix::unistd::mkfifo(
        &fifo,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("create a fifo");
    // The listing already skips a fifo; the reader itself must also refuse one
    // that replaces a listed regular file, without waiting for a writer.
    super::read_profile_text(&fixture.agents, "fifo", &fifo).expect_err("refused");
}

#[test]
fn launch_and_retention_agree_on_a_contained_symlink_profile() {
    let fixture = Fixture::new();
    let definitions = fixture.agents.join("definitions");
    fs::create_dir(&definitions).expect("definitions directory");
    fs::write(
        definitions.join("work.txt"),
        format!(
            "base = \"pi\"\n{}",
            Fixture::pin(PI_SHAPED_PACKAGE_ID, &fixture.digest)
        ),
    )
    .expect("the pinned profile");
    std::os::unix::fs::symlink(
        definitions.join("work.txt"),
        fixture.agents.join("work.toml"),
    )
    .expect("a contained symlink");
    let profiles = fixture.profiles();

    // The launch path loads it, so retention must hold its digest.
    let agent = profiles.resolve_agent("work").expect("the profile loads");
    assert!(agent.profile.is_some());
    assert_eq!(
        pinned_set(&profiles).iter().collect::<Vec<_>>(),
        [&fixture.digest]
    );
}

#[test]
fn launch_and_retention_agree_on_an_oversized_profile() {
    let fixture = Fixture::new();
    fixture.write_pinned("big", &fixture.digest);
    let path = fixture.agents.join("big.toml");
    let file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open the profile");
    file.set_len(super::MAX_PROFILE_BYTES + 1).expect("grow");
    let profiles = fixture.profiles();

    // Neither accepts it, and the digest it would pin is not silently lost: the
    // profile does not load, so nothing launches from it.
    profiles
        .resolve_agent("big")
        .expect_err("too large to load");
    super::read_profile_text(&fixture.agents, "big", &path).expect_err("refused");
}

#[test]
fn the_retention_scan_refuses_a_directory_with_too_many_entries() {
    let fixture = Fixture::new();
    for index in 0..=super::MAX_SCANNED_PROFILES {
        fs::write(fixture.agents.join(format!("n{index}.txt")), b"").expect("write");
    }
    fixture
        .profiles()
        .pinned_digests()
        .expect_err("an oversized directory is not assumed to pin nothing");
}
