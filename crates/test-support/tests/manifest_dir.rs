//! Integration test: the helper reports the runtime package directory.

#[test]
fn manifest_dir_matches_the_runtime_environment() {
    let runtime =
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set at run time");
    assert_eq!(
        pohunek_test_support::manifest_dir(),
        std::path::PathBuf::from(runtime)
    );
    assert!(pohunek_test_support::manifest_dir().ends_with("crates/test-support"));
}
