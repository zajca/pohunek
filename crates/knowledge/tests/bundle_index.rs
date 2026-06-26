use knowledge::{bundle_index, ConceptType, Intent};

#[test]
fn bundle_index_returns_docs_knowledge_concepts() {
    let index = bundle_index().expect("embedded bundle index should parse");

    assert!(index.len() >= 17);
    assert!(index.iter().any(|concept| {
        concept.id == "concept/architecture"
            && concept.r#type == ConceptType::Concept
            && concept.title == "Universal assistant architecture"
            && concept
                .intents
                .as_ref()
                .is_some_and(|intents| intents.contains(&Intent::Debug))
    }));
    assert!(index.iter().any(|concept| {
        concept.id == "runbook/update-after-release"
            && concept.r#type == ConceptType::Runbook
            && concept.since.as_deref() == Some("0.3.3")
    }));
}

#[test]
fn bundle_index_exposes_only_allowlisted_frontmatter_fields() {
    let index = bundle_index().expect("embedded bundle index should parse");
    let architecture = index
        .iter()
        .find(|concept| concept.id == "concept/architecture")
        .expect("architecture concept exists");

    let serialized = serde_yaml::to_value(architecture).expect("serialize concept meta");
    let keys = serialized
        .as_mapping()
        .expect("concept meta serializes to mapping")
        .keys()
        .filter_map(serde_yaml::Value::as_str)
        .collect::<Vec<_>>();

    assert!(keys.contains(&"type"));
    assert!(keys.contains(&"id"));
    assert!(keys.contains(&"title"));
    assert!(keys.contains(&"description"));
    assert!(keys.contains(&"intents"));
    assert!(!keys.contains(&"source_kind"));
    assert!(!keys.contains(&"tags"));
    assert!(!keys.contains(&"generated_from"));
    assert!(!keys.contains(&"citations"));
}
