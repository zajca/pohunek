#![cfg(feature = "protocol")]

use knowledge::{validate_bundle, ConceptMeta};
use pohunek_test_support::manifest_dir;
use protocol::{ConceptDeprecation, ConceptIntent, ConceptType};

#[test]
fn validated_bundle_metadata_reaches_protocol_shape() {
    let fixture = manifest_dir().join("tests/fixtures/good");
    let report = validate_bundle(fixture).expect("bundle fixture validates");
    let concepts: Vec<protocol::ConceptMeta> = report
        .concepts
        .into_iter()
        .map(ConceptMeta::from)
        .map(Into::into)
        .collect();

    assert_eq!(concepts.len(), 2);
    let runbook = concepts
        .iter()
        .find(|concept| concept.id == "runbook-setup")
        .expect("setup runbook reaches the protocol");
    assert_eq!(runbook.r#type, ConceptType::Runbook);
    assert_eq!(runbook.title, "Setup Runbook");
    assert_eq!(runbook.intents, Some(vec![ConceptIntent::Setup]));
    assert_eq!(runbook.changed_in, Some(vec!["0.3.4".to_owned()]));
    assert_eq!(
        runbook.deprecated,
        Some(ConceptDeprecation::Details {
            version: "0.4.0".to_owned(),
            successor: Some("runbook/new-setup".to_owned()),
        })
    );
}
