//! Wire shapes of package setup entries, `package.setup_resolve` and the
//! `program` of `package.create_profile`.

// Rust guideline compliant 2026-11-23

use protocol::{
    CommandErrorKind, CommandName, PackageCreateProfileParams, PackageDigest, PackageErrorKind,
    PackageId, PackageIdentity, PackageInstallResult, PackageVersion, RuntimeId, SetupInfo,
    SetupResolveParams, SetupResolveResult,
};
use serde_json::json;

fn digest() -> PackageDigest {
    PackageDigest::parse(&format!("sha256:{}", "a".repeat(64))).expect("digest")
}

#[test]
fn a_setup_entry_serializes_its_four_fields_and_nothing_else() {
    let entry = SetupInfo {
        name: CommandName::parse("accounts").expect("name"),
        program: "bin/tool".to_owned(),
        args: vec!["fixed".to_owned()],
        description: "Manage the account list".to_owned(),
    };
    let value = serde_json::to_value(&entry).expect("serialize");
    assert_eq!(
        value,
        json!({
            "name": "accounts",
            "program": "bin/tool",
            "args": ["fixed"],
            "description": "Manage the account list"
        })
    );
    let back: SetupInfo = serde_json::from_value(value).expect("deserialize");
    assert_eq!(back, entry);
}

#[test]
fn setup_resolve_params_refuse_an_argument_or_an_environment_field() {
    let params: SetupResolveParams =
        serde_json::from_value(json!({ "package": "acme.runtime.pi", "setup": "accounts" }))
            .expect("the two fields parse");
    assert_eq!(params.setup.as_str(), "accounts");

    for smuggled in [
        json!({ "package": "acme.runtime.pi", "setup": "accounts", "args": ["x"] }),
        json!({ "package": "acme.runtime.pi", "setup": "accounts", "env": {} }),
    ] {
        assert!(
            serde_json::from_value::<SetupResolveParams>(smuggled).is_err(),
            "a request cannot carry a field the daemon never reads"
        );
    }
}

#[test]
fn setup_resolve_result_carries_the_verified_program_and_fixed_args() {
    let result = SetupResolveResult {
        package: PackageIdentity {
            id: PackageId::parse("acme.runtime.pi").expect("id"),
            version: PackageVersion::parse("1.0.0").expect("version"),
        },
        setup: CommandName::parse("accounts").expect("name"),
        digest: digest(),
        root: "/store/files".to_owned(),
        program: "/store/files/bin/tool".to_owned(),
        args: vec!["fixed".to_owned()],
    };
    let value = serde_json::to_value(&result).expect("serialize");
    assert_eq!(value["program"], "/store/files/bin/tool");
    assert_eq!(value["args"], json!(["fixed"]));
    let back: SetupResolveResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(back, result);
}

#[test]
fn an_install_result_without_setup_entries_omits_the_field() {
    let value = json!({
        "status": "installed",
        "package": {
            "digest": digest(),
            "package": { "id": "acme.runtime.pi", "version": "1.0.0" },
            "origin": "explicit_digest",
            "enabled": true,
            "selected": true,
            "installed_at_unix_seconds": 1,
            "referenced": false,
            "revoked": false
        },
        "reloaded": true
    });
    let parsed: PackageInstallResult = serde_json::from_value(value).expect("a pre-setup result");
    assert!(
        parsed.setup.is_empty(),
        "a result without the key has no entries"
    );
    let encoded = serde_json::to_value(&parsed).expect("serialize");
    assert!(
        encoded.get("setup").is_none(),
        "an empty setup list is not written"
    );
}

#[test]
fn create_profile_program_is_optional_and_written_only_when_present() {
    let without: PackageCreateProfileParams = serde_json::from_value(json!({
        "profile": "work",
        "base": "shell",
        "dry_run": false
    }))
    .expect("a request without program parses");
    assert_eq!(without.program, None);
    assert!(serde_json::to_value(&without)
        .expect("serialize")
        .get("program")
        .is_none());

    let with = PackageCreateProfileParams {
        program: Some("/usr/local/bin/tool".to_owned()),
        ..without
    };
    let value = serde_json::to_value(&with).expect("serialize");
    assert_eq!(value["program"], "/usr/local/bin/tool");
    let base: RuntimeId = RuntimeId::parse("shell").expect("runtime id");
    assert_eq!(with.base, base);
}

#[test]
fn the_setup_and_program_refusals_have_their_wire_codes() {
    assert_eq!(
        CommandErrorKind::SetupNotDeclared.code(),
        "command_setup_not_declared"
    );
    assert_eq!(
        CommandErrorKind::SetupNotLocal.code(),
        "command_setup_not_local"
    );
    assert_eq!(
        PackageErrorKind::ProfileProgramInvalid.code(),
        "package_profile_program_invalid"
    );
}
