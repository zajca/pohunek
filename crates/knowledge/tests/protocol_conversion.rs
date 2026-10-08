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

#[test]
fn every_validated_concept_type_reaches_its_protocol_variant() {
    let directory = pohunek_test_support::tempdir_with_prefix("knowledge-protocol-types-")
        .expect("private bundle root");
    let names = [
        "Concept",
        "Guide",
        "Runbook",
        "Troubleshooting",
        "SafetyPolicy",
        "CliCommand",
        "ConfigReference",
        "ProtocolMethod",
        "ProtocolEvent",
        "SetupAsset",
        "PromptTemplate",
        "SourceMap",
        "SnapshotTemplate",
        "ReleaseNote",
    ];
    for name in names {
        std::fs::write(
            directory.path().join(format!("{name}.md")),
            format!(
                "---\ntype: {name}\nid: kind/{name}\ntitle: {name}\ndescription: Type contract fixture.\nsource_kind: manual\nsince: 0.3.3\n---\n\n# {name}\n"
            ),
        )
        .expect("write concept fixture");
    }

    let report = validate_bundle(directory.path()).expect("all concept types validate");
    assert_eq!(report.concepts.len(), names.len());
    for concept in report.concepts {
        let expected = concept.id.trim_start_matches("kind/").to_owned();
        let converted: protocol::ConceptMeta = ConceptMeta::from(concept).into();
        assert_eq!(protocol_type_name(converted.r#type), expected);
    }
}

fn protocol_type_name(value: ConceptType) -> &'static str {
    match value {
        ConceptType::Concept => "Concept",
        ConceptType::Guide => "Guide",
        ConceptType::Runbook => "Runbook",
        ConceptType::Troubleshooting => "Troubleshooting",
        ConceptType::SafetyPolicy => "SafetyPolicy",
        ConceptType::CliCommand => "CliCommand",
        ConceptType::ConfigReference => "ConfigReference",
        ConceptType::ProtocolMethod => "ProtocolMethod",
        ConceptType::ProtocolEvent => "ProtocolEvent",
        ConceptType::SetupAsset => "SetupAsset",
        ConceptType::PromptTemplate => "PromptTemplate",
        ConceptType::SourceMap => "SourceMap",
        ConceptType::SnapshotTemplate => "SnapshotTemplate",
        ConceptType::ReleaseNote => "ReleaseNote",
    }
}
