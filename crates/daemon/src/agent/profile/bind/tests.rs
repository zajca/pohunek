//! Tests of the profile pin rewrite: the pure TOML edit, and the atomic
//! publish over a real agents directory and real package store.

// Rust guideline compliant 2026-10-05

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::Path;

use super::*;
use crate::agent::host::fixture::{install_pi_package, installed_pi_host, PI_SHAPED_PACKAGE_ID};
use crate::agent::host::RuntimeHost;

/// A value that must never reach any output.
const SENTINEL: &str = "sk-sentinel-secret-9f3a";

const DIGEST_OLD: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const DIGEST_NEW: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

/// Program of the first fixture version.
const FIRST_PROGRAM: &str = "/bin/sh";

/// Program of the second fixture version.
const SECOND_PROGRAM: &str = "/bin/true";

/// Program a profile of the replacement tests sets itself; a resolution that
/// fell back to a bare runtime would report another one.
const PROFILE_PROGRAM: &str = "profile-program";

/// Replacements one thread performs while the other resolves.
const REPLACEMENT_ROUNDS: usize = 200;

/// A profile with comments, a multi-line `[env]` secret, and no pin.
fn profile_text() -> String {
    format!(
        "# Work profile; keep in sync with the team doc.\n\
         base = \"acme-pi\"\n\
         # Extra launch flags.\n\
         args = [\"--mode\", \"rpc\"]   # trailing note\n\
         program = \"pi\"\n\
         \n\
         [env]\n\
         # Credentials for the gateway.\n\
         API_TOKEN = \"{SENTINEL}\"\n\
         MULTI = \"\"\"\n\
         line one {SENTINEL}\n\
         line two\n\
         \"\"\"\n"
    )
}

fn pin(package: &str, digest: &str) -> Pin {
    Pin {
        package: PackageId::parse(package).expect("package id"),
        digest: PackageDigest::parse(digest).expect("digest"),
    }
}

fn head(base: &str, pinned: Option<(&str, &str)>) -> ProfileHead {
    ProfileHead {
        base: RuntimeId::parse(base).expect("base"),
        package: pinned.map(|(package, _)| PackageId::parse(package).expect("package")),
        digest: pinned.map(|(_, digest)| PackageDigest::parse(digest).expect("digest")),
    }
}

// ----- parsing -------------------------------------------------------------

#[test]
fn head_reads_base_and_the_optional_pin() {
    let text = format!("base = \"acme-pi\"\npackage = \"acme.pi\"\ndigest = \"{DIGEST_OLD}\"\n");
    assert_eq!(
        parse_head(&text).expect("head"),
        head("acme-pi", Some(("acme.pi", DIGEST_OLD)))
    );
    assert_eq!(
        parse_head(&profile_text()).expect("head"),
        head("acme-pi", None)
    );
}

#[test]
fn head_diagnostics_name_keys_and_never_values() {
    let cases = [
        ("program = \"pi\"\n", "`base` is missing"),
        ("base = 7\n", "`base` must be a string"),
        ("base = \"Not Valid\"\n", "`base` is not a valid runtime id"),
        (
            "base = \"acme-pi\"\npackage = 7\n",
            "`package` must be a string",
        ),
        (
            "base = \"acme-pi\"\npackage = \"BAD ID\"\n",
            "`package` is not a valid package id",
        ),
        (
            "base = \"acme-pi\"\ndigest = \"sha256:abc\"\n",
            "`digest` is not a valid package digest",
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(parse_head(text).expect_err(text), expected);
    }
}

#[test]
fn toml_syntax_errors_report_the_line_and_never_the_source() {
    let text = format!("base = \"acme-pi\"\n[env]\nAPI_TOKEN = {SENTINEL}\n");
    let error = parse_head(&text).expect_err("unquoted value");
    assert!(error.starts_with("line 3:"), "{error}");
    assert!(!error.contains(SENTINEL), "{error}");
    let error = apply_pin(&text, &pin("acme.pi", DIGEST_NEW)).expect_err("unquoted value");
    assert!(!error.contains(SENTINEL), "{error}");
}

// ----- rewriting -----------------------------------------------------------

fn lines_without_pin(text: &str) -> String {
    let mut kept = String::new();
    for line in text.lines() {
        if !line.starts_with("package =") && !line.starts_with("digest =") {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    kept
}

#[test]
fn rewrite_inserts_the_pin_after_base_and_preserves_every_other_byte() {
    let original = profile_text();
    let rewritten = apply_pin(&original, &pin("acme.pi", DIGEST_NEW)).expect("rewrite");
    let expected = original.replacen(
        "base = \"acme-pi\"\n",
        &format!("base = \"acme-pi\"\npackage = \"acme.pi\"\ndigest = \"{DIGEST_NEW}\"\n"),
        1,
    );
    assert_eq!(rewritten, expected);
    assert_eq!(lines_without_pin(&rewritten), original);
    assert!(rewritten.contains(SENTINEL));
    assert_eq!(
        parse_head(&rewritten).expect("head").pin(),
        Some(pin("acme.pi", DIGEST_NEW))
    );
}

#[test]
fn rewrite_updates_existing_keys_in_place_and_keeps_their_comments() {
    let original = format!(
        "base = \"acme-pi\"\n\
         # which package\n\
         package = \"acme.old\"   # keep me\n\
         program = \"pi\"\n\
         digest = \"{DIGEST_OLD}\" # and me\n\
         \n\
         [env]\n\
         API_TOKEN = \"{SENTINEL}\"\n"
    );
    let rewritten = apply_pin(&original, &pin("acme.pi", DIGEST_NEW)).expect("rewrite");
    let expected = original
        .replace("\"acme.old\"", "\"acme.pi\"")
        .replace(DIGEST_OLD, DIGEST_NEW);
    assert_eq!(rewritten, expected);
}

#[test]
fn rewrite_completes_a_half_pin_directly_after_base() {
    let original = "base = \"acme-pi\"\nprogram = \"pi\"\npackage = \"acme.old\"\n";
    let rewritten = apply_pin(original, &pin("acme.pi", DIGEST_NEW)).expect("rewrite");
    let head = parse_head(&rewritten).expect("head");
    assert_eq!(head.pin(), Some(pin("acme.pi", DIGEST_NEW)));
    assert!(
        rewritten.starts_with("base = \"acme-pi\"\ndigest = "),
        "{rewritten}"
    );
    assert!(rewritten.contains("program = \"pi\"\n"));
}

#[test]
fn rewrite_handles_a_file_that_ends_right_after_base() {
    for original in ["base = \"acme-pi\"", "base = \"acme-pi\"\n"] {
        let rewritten = apply_pin(original, &pin("acme.pi", DIGEST_NEW)).expect("rewrite");
        assert_eq!(
            parse_head(&rewritten).expect("head").pin(),
            Some(pin("acme.pi", DIGEST_NEW)),
            "{original:?}"
        );
        assert_eq!(
            without_pin(&rewritten),
            without_pin(original),
            "{original:?}"
        );
    }
}

#[test]
fn rewrite_keeps_dotted_keys_and_tables_in_place() {
    let original = format!(
        "base = \"acme-pi\"\n\
         env.EXTRA = \"{SENTINEL}\"\n\
         program = \"pi\"\n\
         \n\
         [resume]\n\
         resumable = false\n\
         \n\
         [[rules]]\n\
         name = \"one\"\n"
    );
    let rewritten = apply_pin(&original, &pin("acme.pi", DIGEST_NEW)).expect("rewrite");
    assert_eq!(lines_without_pin(&rewritten), original);
    assert!(
        rewritten.starts_with("base = \"acme-pi\"\npackage = "),
        "{rewritten}"
    );
}

#[test]
fn rewrite_refuses_non_string_pin_keys() {
    for original in [
        "base = \"acme-pi\"\npackage = 5\n",
        "base = \"acme-pi\"\n[digest]\nx = 1\n",
    ] {
        assert!(
            apply_pin(original, &pin("acme.pi", DIGEST_NEW)).is_err(),
            "{original}"
        );
    }
}

// ----- publishing ----------------------------------------------------------

/// An agents directory next to a plugin root serving two versions of the
/// fixture package.
struct Fixture {
    _root: tempfile::TempDir,
    agents: PathBuf,
    host: RuntimeHost,
    first: PackageDigest,
    second: PackageDigest,
}

impl Fixture {
    fn new() -> Self {
        let root = pohunek_test_support::tempdir().expect("private test directory");
        let plugins = root.path().join("plugins");
        let agents = root.path().join("agents");
        fs::create_dir_all(&agents).expect("agents directory");
        fs::set_permissions(&agents, fs::Permissions::from_mode(0o700))
            .expect("owner-private agents directory");
        let (host, first) = installed_pi_host(&plugins, Path::new(FIRST_PROGRAM));
        let second = install_pi_package(&plugins, Path::new(SECOND_PROGRAM), "2.0.0", true);
        host.reload().expect("reload");
        Self {
            _root: root,
            agents,
            host,
            first,
            second,
        }
    }

    fn profiles(&self) -> ProfileRegistry {
        ProfileRegistry::with_runtimes(Some(self.agents.clone()), self.host.clone())
    }

    fn path(&self, name: &str) -> PathBuf {
        self.agents.join(format!("{name}.toml"))
    }

    /// A profile named `name` of base `pi` that sets its own program, pinned
    /// to `digest`, with a secret `[env]` value.
    fn write_pinned(&self, name: &str, digest: &PackageDigest) {
        let text = format!(
            "base = \"pi\"\npackage = \"{PI_SHAPED_PACKAGE_ID}\"\ndigest = \"{digest}\"\n\
             program = \"{PROFILE_PROGRAM}\"\nargs = [\"--flag\"]\n\n[env]\nTOKEN = \"{SENTINEL}\"\n"
        );
        self.write(name, &text);
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.path(name);
        fs::write(&path, text).expect("write profile");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("private profile");
    }

    fn pin_of(&self, name: &str) -> PackageDigest {
        let text = fs::read_to_string(self.path(name)).expect("read profile");
        parse_head(&text)
            .expect("head")
            .digest
            .expect("a pinned profile")
    }

    /// Names of the directory entries.
    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.agents)
            .expect("list agents")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }
}

/// Re-pins `name` to `digest` through the production read and publish path.
fn repin(
    dir: &BindDir,
    name: &str,
    package: &str,
    digest: &PackageDigest,
) -> Result<(), BindError> {
    let file = dir.read(name)?;
    let rewritten = apply_pin(
        file.text(),
        &Pin {
            package: PackageId::parse(package).expect("package id"),
            digest: digest.clone(),
        },
    )
    .expect("pin edit");
    dir.publish(name, &file, &rewritten)
}

#[test]
fn publish_replaces_the_pin_and_keeps_the_owner_private_mode() {
    let fixture = Fixture::new();
    fixture.write_pinned("work", &fixture.first);
    let dir = fixture.profiles().open_for_bind().expect("open");

    repin(&dir, "work", PI_SHAPED_PACKAGE_ID, &fixture.second).expect("publish");

    assert_eq!(fixture.pin_of("work"), fixture.second);
    let text = fs::read_to_string(fixture.path("work")).expect("read");
    assert!(text.contains(SENTINEL), "other bytes are kept");
    let mode = fs::metadata(fixture.path("work"))
        .expect("metadata")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(mode, 0o600);
    assert_eq!(fixture.entries(), ["work.toml"], "no temporary is left");
}

#[test]
fn publish_narrows_a_wider_mode_to_owner_private() {
    let fixture = Fixture::new();
    fixture.write_pinned("work", &fixture.first);
    fs::set_permissions(fixture.path("work"), fs::Permissions::from_mode(0o644)).expect("widen");
    let dir = fixture.profiles().open_for_bind().expect("open");

    repin(&dir, "work", PI_SHAPED_PACKAGE_ID, &fixture.second).expect("publish");

    let mode = fs::metadata(fixture.path("work"))
        .expect("metadata")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(mode, 0o600);
}

#[test]
fn publish_refuses_a_profile_replaced_after_it_was_read_and_leaves_it_alone() {
    let fixture = Fixture::new();
    fixture.write_pinned("work", &fixture.first);
    let dir = fixture.profiles().open_for_bind().expect("open");
    let file = dir.read("work").expect("read");
    let rewritten = apply_pin(
        file.text(),
        &pin(PI_SHAPED_PACKAGE_ID, fixture.second.as_str()),
    )
    .expect("pin edit");

    // An editor that saves by renaming a new file over the name.
    let concurrent = format!("base = \"shell\"\n# edited elsewhere {SENTINEL}\n");
    let staging = fixture.agents.join("editor.swap");
    fs::write(&staging, &concurrent).expect("stage the edit");
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o600)).expect("private");
    fs::rename(&staging, fixture.path("work")).expect("rename over");

    assert_eq!(
        dir.publish("work", &file, &rewritten),
        Err(BindError::Changed)
    );
    assert_eq!(
        fs::read_to_string(fixture.path("work")).expect("read"),
        concurrent
    );
    assert_eq!(fixture.entries(), ["work.toml"], "the temporary is removed");
}

#[test]
fn publish_refuses_an_in_place_edit_after_the_read() {
    let fixture = Fixture::new();
    fixture.write_pinned("work", &fixture.first);
    let dir = fixture.profiles().open_for_bind().expect("open");
    let file = dir.read("work").expect("read");
    let rewritten = apply_pin(
        file.text(),
        &pin(PI_SHAPED_PACKAGE_ID, fixture.second.as_str()),
    )
    .expect("pin edit");

    let mut edited = fs::read_to_string(fixture.path("work")).expect("read");
    edited.push_str("# appended in place\n");
    fs::write(fixture.path("work"), &edited).expect("edit in place");

    assert_eq!(
        dir.publish("work", &file, &rewritten),
        Err(BindError::Changed)
    );
    assert_eq!(
        fs::read_to_string(fixture.path("work")).expect("read"),
        edited
    );
}

#[test]
fn a_failure_before_the_rename_leaves_the_original_byte_identical() {
    let fixture = Fixture::new();
    fixture.write_pinned("work", &fixture.first);
    let before = fs::read(fixture.path("work")).expect("read");
    let dir = fixture.profiles().open_for_bind().expect("open");
    let file = dir.read("work").expect("read");
    let rewritten = apply_pin(
        file.text(),
        &pin(PI_SHAPED_PACKAGE_ID, fixture.second.as_str()),
    )
    .expect("pin edit");
    // A mode change after the read counts as a change of the profile.
    fs::set_permissions(fixture.path("work"), fs::Permissions::from_mode(0o660)).expect("widen");

    assert_eq!(
        dir.publish("work", &file, &rewritten),
        Err(BindError::Changed)
    );

    assert_eq!(fs::read(fixture.path("work")).expect("read"), before);
    assert_eq!(fixture.entries(), ["work.toml"]);
}

#[test]
fn read_refuses_links_special_files_group_writable_and_oversized_profiles() {
    let fixture = Fixture::new();
    let dir = fixture.profiles().open_for_bind().expect("open");

    assert_eq!(
        dir.read("absent").expect_err("missing"),
        BindError::NotFound
    );
    assert_eq!(
        dir.read("bad name").expect_err("invalid"),
        BindError::NotFound
    );

    fixture.write("target", "base = \"shell\"\n");
    symlink(fixture.path("target"), fixture.path("linked")).expect("symlink");
    assert!(matches!(dir.read("linked"), Err(BindError::Unusable(_))));

    fixture.write("hard", "base = \"shell\"\n");
    fs::hard_link(fixture.path("hard"), fixture.agents.join("hard.alias")).expect("hard link");
    assert!(matches!(dir.read("hard"), Err(BindError::Unusable(_))));

    fixture.write("writable", "base = \"shell\"\n");
    fs::set_permissions(fixture.path("writable"), fs::Permissions::from_mode(0o660))
        .expect("widen");
    assert!(matches!(dir.read("writable"), Err(BindError::Unusable(_))));

    let oversized = format!("base = \"shell\"\n# {}\n", "x".repeat(max_profile_bytes()));
    fixture.write("huge", &oversized);
    assert!(matches!(dir.read("huge"), Err(BindError::Unusable(_))));

    fs::create_dir(fixture.path("folder")).expect("directory named like a profile");
    assert!(matches!(dir.read("folder"), Err(BindError::Unusable(_))));

    fixture.write("binary", "base = \"shell\"\n");
    fs::write(fixture.path("binary"), [0xff_u8, 0xfe]).expect("non utf-8");
    assert!(matches!(dir.read("binary"), Err(BindError::Unusable(_))));
}

#[test]
fn a_group_writable_agents_directory_is_refused() {
    let fixture = Fixture::new();
    fixture.write("work", "base = \"shell\"\n");
    let profiles = fixture.profiles();
    fs::set_permissions(&fixture.agents, fs::Permissions::from_mode(0o770)).expect("widen");

    assert!(matches!(
        profiles.open_for_bind(),
        Err(BindError::Unusable(_))
    ));
}

#[test]
fn a_registry_without_an_agents_directory_has_no_profile() {
    let registry = ProfileRegistry::with_runtimes(None, RuntimeHost::default());
    assert_eq!(
        registry.open_for_bind().expect_err("no directory"),
        BindError::NotFound
    );
    let fixture = Fixture::new();
    let missing = ProfileRegistry::with_runtimes(
        Some(fixture.agents.join("never-created")),
        fixture.host.clone(),
    );
    assert_eq!(
        missing.open_for_bind().expect_err("missing"),
        BindError::NotFound
    );
}

// ----- recovery ------------------------------------------------------------

#[test]
fn a_temporary_left_by_an_interrupted_rewrite_is_inert_and_removed_by_the_next_bind() {
    let fixture = Fixture::new();
    fixture.write_pinned("work", &fixture.first);
    let stale = fixture.agents.join(format!(
        "{TEMPORARY_PREFIX}00112233445566778899aabbccddeeff"
    ));
    fs::write(
        &stale,
        format!("base = \"pi\"\n[env]\nTOKEN = \"{SENTINEL}\"\n"),
    )
    .expect("stale");
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o600)).expect("private");
    let profiles = fixture.profiles();

    // The temporary is never a profile: the original still resolves, the
    // enumeration lists one profile and retention sees only the real pin.
    let agent = profiles
        .resolve_agent("work")
        .expect("the profile resolves");
    assert_eq!(agent.program(), PROFILE_PROGRAM);
    assert_eq!(profiles.enumerate().len(), 1);
    let pinned = profiles.pinned_digests().expect("retention scan");
    assert!(pinned.contains(&fixture.first));
    assert_eq!(pinned.iter().count(), 1);

    let dir = profiles.open_for_bind().expect("open");
    dir.remove_stale_temporaries();
    repin(&dir, "work", PI_SHAPED_PACKAGE_ID, &fixture.second).expect("the next bind");

    assert_eq!(fixture.entries(), ["work.toml"]);
    assert_eq!(fixture.pin_of("work"), fixture.second);
}

#[test]
fn cleanup_leaves_profiles_and_unrelated_files_alone() {
    let fixture = Fixture::new();
    fixture.write_pinned("work", &fixture.first);
    fs::write(fixture.agents.join("notes.txt"), "keep").expect("unrelated file");
    let dir = fixture.profiles().open_for_bind().expect("open");

    dir.remove_stale_temporaries();

    assert_eq!(fixture.entries(), ["notes.txt", "work.toml"]);
}

// ----- no gap ---------------------------------------------------------------

#[test]
fn a_profile_named_like_its_runtime_always_resolves_while_it_is_replaced() {
    let fixture = Fixture::new();
    // The profile is named after the runtime id, so a missing file would
    // silently resolve the bare runtime and drop the profile's program.
    fixture.write_pinned("pi", &fixture.first);
    let profiles = fixture.profiles();
    let dir = profiles.open_for_bind().expect("open");
    let digests = [fixture.first.clone(), fixture.second.clone()];
    let done = std::sync::atomic::AtomicBool::new(false);

    std::thread::scope(|scope| {
        let resolver = scope.spawn(|| {
            let mut resolved = 0_usize;
            loop {
                let agent = profiles
                    .resolve_agent("pi")
                    .expect("the profile resolves at every instant");
                let profile = agent
                    .profile
                    .as_ref()
                    .expect("a profile, not the bare runtime");
                assert_eq!(profile.program, PROFILE_PROGRAM);
                assert_eq!(profile.args, ["--flag"]);
                assert!(profile
                    .env
                    .iter()
                    .any(|(key, value)| key == "TOKEN" && value == SENTINEL));
                resolved += 1;
                if done.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
            }
            resolved
        });
        for round in 0..REPLACEMENT_ROUNDS {
            let digest = &digests[(round + 1) % digests.len()];
            repin(&dir, "pi", PI_SHAPED_PACKAGE_ID, digest).expect("replace");
        }
        done.store(true, std::sync::atomic::Ordering::Release);
        assert!(resolver.join().expect("the resolver never failed") > 0);
    });

    assert_eq!(fixture.entries(), ["pi.toml"]);
}
