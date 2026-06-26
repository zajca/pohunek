#![cfg(feature = "protocol")]

use knowledge::{ConceptMeta, Deprecation, Intent};
use protocol::{ConceptDeprecation, ConceptIntent, ConceptType};

#[test]
fn concept_meta_converts_to_protocol_shape() {
    let meta = ConceptMeta {
        r#type: knowledge::ConceptType::Runbook,
        id: "runbook/update".to_owned(),
        title: "Update".to_owned(),
        description: "Update after release".to_owned(),
        intents: Some(vec![Intent::Update]),
        since: Some("0.3.3".to_owned()),
        changed_in: Some(vec!["0.3.4".to_owned()]),
        deprecated: Some(Deprecation::Details {
            version: "0.4.0".to_owned(),
            successor: Some("runbook/new-update".to_owned()),
        }),
    };

    let converted: protocol::ConceptMeta = meta.into();

    assert_eq!(converted.r#type, ConceptType::Runbook);
    assert_eq!(converted.id, "runbook/update");
    assert_eq!(converted.intents, Some(vec![ConceptIntent::Update]));
    assert_eq!(
        converted.deprecated,
        Some(ConceptDeprecation::Details {
            version: "0.4.0".to_owned(),
            successor: Some("runbook/new-update".to_owned()),
        })
    );
}
