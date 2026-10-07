use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::PathBuf;

use protocol::{PackageIdentity, PackageOrigin, PackageVersion};

use super::*;

/// A value that must never reach any output.
const SENTINEL: &str = "sk-sentinel-secret-9f3a";

const DIGEST_OLD: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const DIGEST_NEW: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
const DIGEST_SIBLING: &str =
    "sha256:1111111111112222222222222222222222222222222222222222222222222222";

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

fn info(id: &str, digest: &str, runtime: Option<&str>) -> PackageInfo {
    PackageInfo {
        digest: PackageDigest::parse(digest).expect("digest"),
        package: PackageIdentity {
            id: PackageId::parse(id).expect("id"),
            version: PackageVersion::parse("1.0.0").expect("version"),
        },
        origin: PackageOrigin::ExplicitDigest,
        enabled: true,
        selected: true,
        installed_at_unix_seconds: 1,
        runtime_id: runtime.map(|value| RuntimeId::parse(value).expect("runtime")),
        fault: None,
        referenced: false,
        selection_blocked: None,
    }
}

fn head(base: &str, pinned: Option<(&str, &str)>) -> ProfileHead {
    ProfileHead {
        base: RuntimeId::parse(base).expect("base"),
        package: pinned.map(|(package, _)| PackageId::parse(package).expect("package")),
        digest: pinned.map(|(_, digest)| PackageDigest::parse(digest).expect("digest")),
    }
}

// ----- names ---------------------------------------------------------------

/// The daemon's `bad_names_each_reject_with_invalid_name` table plus the
/// accepted shapes of `valid_dotted_name_is_accepted`; both sides must agree.
#[test]
fn profile_names_follow_the_daemons_charset_rule() {
    for bad in [
        "../../../../etc/passwd",
        "a/b",
        "a\\b",
        "-leading",
        ".hidden",
        "..",
        "a..b",
        "",
        "a\u{7}b",
        "sp ace",
        "ünïcode",
    ] {
        assert!(!is_valid_profile_name(bad), "{bad:?}");
    }
    for good in ["work", "issue.v2-final", "a_b", "A1", "x.toml", "9"] {
        assert!(is_valid_profile_name(good), "{good:?}");
    }
    assert!(is_valid_profile_name(&"a".repeat(PROFILE_NAME_MAX_BYTES)));
    assert!(!is_valid_profile_name(
        &"a".repeat(PROFILE_NAME_MAX_BYTES + 1)
    ));
}

// ----- parsing -------------------------------------------------------------

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

// ----- classification ------------------------------------------------------

#[test]
fn state_classification_covers_every_case() {
    let packages = vec![
        info("acme.pi", DIGEST_OLD, Some("acme-pi")),
        info("acme.other", DIGEST_NEW, Some("other")),
    ];
    let cases = [
        (head("shell", None), ProfileState::Builtin),
        (head("acme-pi", None), ProfileState::NeedsMigration),
        (
            head("acme-pi", Some(("acme.pi", DIGEST_OLD))),
            ProfileState::Pinned,
        ),
        (
            head("acme-pi", Some(("acme.pi", DIGEST_SIBLING))),
            ProfileState::PinNotInstalled,
        ),
        // The pinned digest is installed but serves another runtime.
        (
            head("acme-pi", Some(("acme.other", DIGEST_NEW))),
            ProfileState::NeedsMigration,
        ),
        // The pinned digest is installed under another package id.
        (
            head("acme-pi", Some(("acme.renamed", DIGEST_OLD))),
            ProfileState::NeedsMigration,
        ),
    ];
    for (head, expected) in cases {
        assert_eq!(classify(&head, &packages).0, expected, "{head:?}");
    }
    let half = ProfileHead {
        package: Some(PackageId::parse("acme.pi").expect("id")),
        ..head("acme-pi", None)
    };
    let (state, detail) = classify(&half, &packages);
    assert_eq!(state, ProfileState::NeedsMigration);
    assert!(detail.is_some());
}

// ----- target resolution ---------------------------------------------------

#[test]
fn a_digest_prefix_without_or_with_several_matches_is_refused() {
    let packages = vec![
        info("acme.pi", DIGEST_OLD, Some("acme-pi")),
        info("acme.pi", DIGEST_SIBLING, Some("acme-pi")),
    ];
    let none: DigestSelector = "333333333333".parse().expect("selector");
    let error = resolve_prefix("work", &none, &packages).expect_err("no match");
    assert_eq!(error.code(), "profile_target_invalid");
    let shared: DigestSelector = "111111111111".parse().expect("selector");
    let error = resolve_prefix("work", &shared, &packages).expect_err("two matches");
    assert_eq!(error.code(), "profile_target_invalid");
    assert!(error.to_string().contains("longer --digest"), "{error}");
}

// ----- file policy ---------------------------------------------------------

#[test]
fn file_policy_refuses_links_special_files_foreign_owners_and_writable_modes() {
    let me = 1000;
    assert_eq!(check_file_policy(FileKind::Regular, me, me, 0o600), Ok(()));
    assert_eq!(check_file_policy(FileKind::Regular, me, me, 0o644), Ok(()));
    assert!(check_file_policy(FileKind::Symlink, me, me, 0o600).is_err());
    assert!(check_file_policy(FileKind::Other, me, me, 0o600).is_err());
    assert!(check_file_policy(FileKind::Regular, me + 1, me, 0o600).is_err());
    assert!(check_file_policy(FileKind::Regular, 0, me, 0o600).is_err());
    for mode in [0o620, 0o602, 0o660, 0o666] {
        assert!(
            check_file_policy(FileKind::Regular, me, me, mode).is_err(),
            "{mode:o}"
        );
    }
}

// ----- filesystem ----------------------------------------------------------

struct Agents {
    _root: tempfile::TempDir,
    dir: PathBuf,
}

impl Agents {
    /// An agents directory with the given mode under a private fixture root.
    fn new(dir_mode: u32) -> Self {
        let root = pohunek_test_support::tempdir().expect("fixture root");
        let dir = root.path().join("agents");
        fs::create_dir(&dir).expect("agents directory");
        fs::set_permissions(&dir, fs::Permissions::from_mode(dir_mode)).expect("directory mode");
        Self { _root: root, dir }
    }

    fn write(&self, name: &str, text: &str, mode: u32) -> PathBuf {
        let path = self.dir.join(format!("{name}.toml"));
        fs::write(&path, text).expect("write profile");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("profile mode");
        path
    }

    fn open(&self) -> TrustedDir {
        open_agents_dir(&self.dir)
            .expect("open agents directory")
            .expect("agents directory exists")
    }
}

#[test]
fn load_refuses_symlinks_special_files_and_group_writable_profiles() {
    let agents = Agents::new(0o700);
    let real = agents.write("real", &profile_text(), 0o600);
    symlink(&real, agents.dir.join("link.toml")).expect("symlink");
    agents.write("loose", &profile_text(), 0o660);
    agents.write("open", &profile_text(), 0o666);
    fs::create_dir(agents.dir.join("dir.toml")).expect("directory named like a profile");
    let dir = agents.open();

    for name in ["link", "loose", "open", "dir"] {
        match load_profile(&dir, name) {
            Err(LoadFault::Unsafe(_)) => {}
            other => panic!("{name}: expected an unsafe fault, got {:?}", other.err()),
        }
    }
    assert_eq!(load_profile(&dir, "absent").err(), Some(LoadFault::Missing));
    load_profile(&dir, "real").expect("a safe profile loads");
}

#[test]
fn load_refuses_oversized_and_non_utf8_profiles() {
    let agents = Agents::new(0o700);
    agents.write("big", &"#".repeat(MAX_PROFILE_BYTES + 1), 0o600);
    let path = agents.dir.join("binary.toml");
    fs::write(&path, [0xff_u8, 0xfe, 0x00]).expect("write binary");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
    let dir = agents.open();
    assert!(matches!(
        load_profile(&dir, "big"),
        Err(LoadFault::Unreadable(_))
    ));
    assert!(matches!(
        load_profile(&dir, "binary"),
        Err(LoadFault::Unreadable(_))
    ));
}

#[test]
fn listing_reports_each_state_and_never_leaks_file_content() {
    let agents = Agents::new(0o700);
    agents.write("builtin", "base = \"shell\"\n", 0o600);
    agents.write("fresh", &profile_text(), 0o600);
    agents.write(
        "pinned",
        &format!("base = \"acme-pi\"\npackage = \"acme.pi\"\ndigest = \"{DIGEST_OLD}\"\n"),
        0o600,
    );
    agents.write(
        "gone",
        &format!("base = \"acme-pi\"\npackage = \"acme.pi\"\ndigest = \"{DIGEST_SIBLING}\"\n"),
        0o600,
    );
    agents.write(
        "broken",
        &format!("base = \"acme-pi\"\n[env]\nTOKEN = {SENTINEL}\n"),
        0o600,
    );
    agents.write("loose", &profile_text(), 0o666);
    // Files that are not profiles are ignored.
    fs::write(agents.dir.join("notes.txt"), SENTINEL).expect("notes");
    fs::create_dir(agents.dir.join("manifests")).expect("manifests");
    let packages = vec![info("acme.pi", DIGEST_OLD, Some("acme-pi"))];

    let listing = list_profiles(Some(&agents.open()), &packages).expect("listing");

    let states: Vec<(&str, ProfileState)> = listing
        .profiles
        .iter()
        .map(|entry| (entry.name.as_str(), entry.state))
        .collect();
    assert_eq!(
        states,
        [
            ("broken", ProfileState::Unreadable),
            ("builtin", ProfileState::Builtin),
            ("fresh", ProfileState::NeedsMigration),
            ("gone", ProfileState::PinNotInstalled),
            ("loose", ProfileState::Unreadable),
            ("pinned", ProfileState::Pinned),
        ]
    );
    assert!(!listing.truncated);
    let table = render_listing(&listing);
    let json = serde_json::to_string(&listing).expect("json");
    for output in [&table, &json] {
        assert!(!output.contains(SENTINEL), "{output}");
        assert!(output.contains("needs_migration"), "{output}");
        assert!(output.contains("line 3"), "{output}");
    }
    // Tables abbreviate digests; JSON carries them whole.
    assert!(table.contains("sha256:111111111111 "), "{table}");
    assert!(!table.contains(DIGEST_OLD), "{table}");
    assert!(json.contains(DIGEST_OLD), "{json}");
}

#[test]
fn listing_names_invalid_profile_files_by_a_sanitized_name() {
    let agents = Agents::new(0o700);
    agents.write("ok", "base = \"shell\"\n", 0o600);
    let odd = agents.dir.join("bad\u{1b}[31mname.toml");
    fs::write(&odd, "base = \"shell\"\n").expect("odd file");
    let listing = list_profiles(Some(&agents.open()), &[]).expect("listing");
    assert_eq!(listing.profiles.len(), 2);
    let table = render_listing(&listing);
    assert!(!table.contains('\u{1b}'), "{table:?}");
    assert!(table.contains("not a valid profile name"), "{table}");
}

#[test]
fn profile_names_lists_valid_profile_files_only() {
    let agents = Agents::new(0o700);
    agents.write("beta", "base = \"shell\"\n", 0o600);
    agents.write("alpha", "base = \"shell\"\n", 0o600);
    fs::write(agents.dir.join("notes.txt"), "x").expect("notes");
    fs::write(agents.dir.join(".hidden.toml"), "x").expect("hidden");
    fs::write(agents.dir.join(".pohunek-profile-migrate-ab"), "x").expect("staging name");
    assert_eq!(profile_names(&agents.dir), ["alpha", "beta"]);
    assert!(profile_names(&agents.dir.join("missing")).is_empty());
}
