use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::PathBuf;

use clap::Parser as _;
use protocol::{PackageFault, PackageIdentity, PackageOrigin};

use super::*;
use crate::commands::plugin::Action;
use crate::{Cli, Commands};

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

fn pin(package: &str, digest: &str) -> Pin {
    Pin {
        package: PackageId::parse(package).expect("package id"),
        digest: PackageDigest::parse(digest).expect("digest"),
    }
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

fn parse(args: &[&str]) -> Result<Action, clap::Error> {
    let mut words = vec!["pohunek", "plugin", "profile"];
    words.extend_from_slice(args);
    match Cli::try_parse_from(words)?.command {
        Commands::Plugin { action } => Ok(action),
        other => panic!("unexpected command {other:?}"),
    }
}

fn profile_action(args: &[&str]) -> ProfileAction {
    match parse(args).expect("parses") {
        Action::Profile { action } => action,
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn profile_subcommands_parse() {
    assert!(matches!(
        profile_action(&["list", "--json"]),
        ProfileAction::List { json: true }
    ));
    match profile_action(&[
        "migrate",
        "work",
        "--digest",
        "111111111111",
        "--yes",
        "--json",
    ]) {
        ProfileAction::Migrate(args) => {
            assert_eq!(args.name, "work");
            assert_eq!(
                args.digest,
                Some(DigestSelector::Prefix("111111111111".to_owned()))
            );
            assert!(args.yes && args.json);
        }
        other @ ProfileAction::List { .. } => panic!("unexpected {other:?}"),
    }
    match profile_action(&["migrate", "work"]) {
        ProfileAction::Migrate(args) => {
            assert!(args.digest.is_none() && !args.yes && !args.json);
        }
        other @ ProfileAction::List { .. } => panic!("unexpected {other:?}"),
    }
    assert!(parse(&["list", "--json"]).expect("list").wants_json());
}

#[test]
fn profile_usage_errors_are_reported_by_clap() {
    for args in [
        &["migrate"][..],
        &["migrate", "../x"],
        &["migrate", ".hidden"],
        &["migrate", "work", "--digest", "abc"],
        &["bogus"],
    ] {
        parse(args).expect_err("must not parse");
    }
}

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
fn rewrite_is_idempotent() {
    let once = apply_pin(&profile_text(), &pin("acme.pi", DIGEST_NEW)).expect("first");
    let twice = apply_pin(&once, &pin("acme.pi", DIGEST_NEW)).expect("second");
    assert_eq!(once, twice);
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
fn target_defaults_to_the_selected_enabled_package_serving_the_base() {
    let mut other_version = info("acme.pi", DIGEST_SIBLING, Some("acme-pi"));
    other_version.selected = false;
    let packages = vec![info("acme.pi", DIGEST_OLD, Some("acme-pi")), other_version];
    let base = RuntimeId::parse("acme-pi").expect("base");
    let chosen = resolve_target("work", &base, &packages, None).expect("selected");
    assert_eq!(chosen.digest.as_str(), DIGEST_OLD);
    let selector: DigestSelector = "111111111111222".parse().expect("selector");
    let chosen = resolve_target("work", &base, &packages, Some(&selector)).expect("by digest");
    assert_eq!(chosen.digest.as_str(), DIGEST_SIBLING);
}

#[test]
fn target_errors_name_the_reason() {
    let base = RuntimeId::parse("acme-pi").expect("base");
    let none_selected = {
        let mut package = info("acme.pi", DIGEST_OLD, Some("acme-pi"));
        package.selected = false;
        vec![package]
    };
    let code = |error: Error| error.code();
    assert_eq!(
        code(resolve_target("w", &base, &[], None).expect_err("builtin")),
        "profile_base_builtin"
    );
    assert_eq!(
        code(
            resolve_target(
                "w",
                &base,
                &[info("acme.x", DIGEST_OLD, Some("other"))],
                None
            )
            .expect_err("nothing serves the base")
        ),
        "profile_base_builtin"
    );
    assert_eq!(
        code(resolve_target("w", &base, &none_selected, None).expect_err("none selected")),
        "profile_target_invalid"
    );
    let two = vec![
        info("acme.pi", DIGEST_OLD, Some("acme-pi")),
        info("acme.pi", DIGEST_SIBLING, Some("acme-pi")),
    ];
    let error = resolve_target("w", &base, &two, None).expect_err("two selected");
    assert!(error.to_string().contains("several"), "{error}");
    let prefix: DigestSelector = "111111111111".parse().expect("selector");
    resolve_target("w", &base, &two, Some(&prefix)).expect_err("ambiguous prefix");
    let other: DigestSelector = "333333333333".parse().expect("selector");
    resolve_target("w", &base, &two, Some(&other)).expect_err("no match");
    // A digest of a package serving another runtime does not match.
    let mixed = vec![
        info("acme.pi", DIGEST_OLD, Some("acme-pi")),
        info("acme.x", DIGEST_NEW, Some("other")),
    ];
    let wrong: DigestSelector = "222222222222".parse().expect("selector");
    resolve_target("w", &base, &mixed, Some(&wrong)).expect_err("wrong runtime");
    let mut faulted = info("acme.pi", DIGEST_OLD, Some("acme-pi"));
    faulted.fault = Some(PackageFault::RootModified);
    let error = resolve_target("w", &base, &[faulted], None).expect_err("faulted");
    assert!(error.to_string().contains("faulted"), "{error}");
}

#[test]
fn already_pinned_requires_the_same_id_and_digest() {
    let target = info("acme.pi", DIGEST_OLD, Some("acme-pi"));
    assert!(already_pinned(
        &head("acme-pi", Some(("acme.pi", DIGEST_OLD))),
        &target
    ));
    assert!(!already_pinned(
        &head("acme-pi", Some(("acme.pi", DIGEST_NEW))),
        &target
    ));
    assert!(!already_pinned(
        &head("acme-pi", Some(("acme.renamed", DIGEST_OLD))),
        &target
    ));
    assert!(!already_pinned(&head("acme-pi", None), &target));
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

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").mode() & MODE_BITS
}

#[test]
fn write_pin_rewrites_atomically_and_keeps_the_owner_private_mode() {
    let agents = Agents::new(0o755);
    let path = agents.write("work", &profile_text(), 0o600);
    let dir = agents.open();
    let loaded = load_profile(&dir, "work").expect("load");
    write_pin(&dir, "work", &loaded, &pin("acme.pi", DIGEST_NEW)).expect("write");

    let after = fs::read_to_string(&path).expect("read back");
    assert_eq!(
        after,
        apply_pin(&profile_text(), &pin("acme.pi", DIGEST_NEW)).expect("expected")
    );
    assert_eq!(mode_of(&path), 0o600);
    // No staging or quarantine file remains beside the profile.
    let names: Vec<_> = fs::read_dir(&agents.dir)
        .expect("read directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(names, ["work.toml"]);
}

#[test]
fn write_pin_narrows_a_wider_mode_to_the_ceiling() {
    let agents = Agents::new(0o700);
    let path = agents.write("work", &profile_text(), 0o644);
    let dir = agents.open();
    let loaded = load_profile(&dir, "work").expect("load");
    write_pin(&dir, "work", &loaded, &pin("acme.pi", DIGEST_NEW)).expect("write");
    assert_eq!(mode_of(&path), 0o600);
}

#[test]
fn write_pin_detects_a_concurrent_edit_and_leaves_it_untouched() {
    let agents = Agents::new(0o700);
    let path = agents.write("work", &profile_text(), 0o600);
    let dir = agents.open();
    let loaded = load_profile(&dir, "work").expect("load");
    let edited = format!("{}# edited elsewhere\n", profile_text());
    fs::write(&path, &edited).expect("concurrent edit");

    let error =
        write_pin(&dir, "work", &loaded, &pin("acme.pi", DIGEST_NEW)).expect_err("stale rewrite");
    assert!(error.contains("changed while"), "{error}");
    assert_eq!(fs::read_to_string(&path).expect("read back"), edited);
}

#[test]
fn write_pin_failure_leaves_the_original_byte_identical() {
    if nix::unistd::Uid::effective().is_root() {
        // Root ignores directory permissions, so the failure cannot be forced.
        return;
    }
    let agents = Agents::new(0o700);
    let path = agents.write("work", &profile_text(), 0o600);
    let dir = agents.open();
    let loaded = load_profile(&dir, "work").expect("load");
    fs::set_permissions(&agents.dir, fs::Permissions::from_mode(0o500)).expect("read-only");

    let result = write_pin(&dir, "work", &loaded, &pin("acme.pi", DIGEST_NEW));

    fs::set_permissions(&agents.dir, fs::Permissions::from_mode(0o700)).expect("restore");
    let error = result.expect_err("the staging file cannot be created");
    assert!(!error.contains(SENTINEL), "{error}");
    assert_eq!(
        fs::read_to_string(&path).expect("read back"),
        profile_text()
    );
    assert_eq!(mode_of(&path), 0o600);
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
fn agents_directory_must_not_be_group_or_world_writable() {
    let agents = Agents::new(0o770);
    open_agents_dir(&agents.dir).expect_err("a group-writable directory is refused");
    let private = Agents::new(0o700);
    let missing = private.dir.join("nested");
    assert!(open_agents_dir(&missing)
        .expect("absent is not an error")
        .is_none());
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
fn listing_without_an_agents_directory_is_empty() {
    let listing = list_profiles(None, &[]).expect("listing");
    assert!(listing.profiles.is_empty());
    assert_eq!(render_listing(&listing), "No host agent profiles.\n");
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

#[test]
fn staging_names_are_random_and_never_look_like_profiles() {
    let first = temporary_name().expect("name");
    let second = temporary_name().expect("name");
    assert_ne!(first, second);
    assert!(first.starts_with(TEMPORARY_PREFIX));
    assert!(!first.ends_with(PROFILE_EXTENSION));
    assert!(first.len() < 255);
}

#[test]
fn review_and_result_text_name_the_change_without_content() {
    let target = info("acme.pi", DIGEST_NEW, Some("acme-pi"));
    let review = render_review("work", &head("acme-pi", None), &target);
    for needle in [
        "work",
        "acme-pi",
        "acme.pi 1.0.0",
        DIGEST_NEW,
        "Current pin: none",
    ] {
        assert!(review.contains(needle), "{needle}: {review}");
    }
    let review = render_review(
        "work",
        &head("acme-pi", Some(("acme.old", DIGEST_OLD))),
        &target,
    );
    assert!(
        review.contains(&format!("acme.old {DIGEST_OLD}")),
        "{review}"
    );
}
