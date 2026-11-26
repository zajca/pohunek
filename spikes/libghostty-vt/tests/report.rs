//! End-to-end scenario: every committed fixture replays through every
//! available model and the report states each assertion outcome, the pairwise
//! differences and the repaint round trips.

// Rust guideline compliant 2026-06-26

use std::path::Path;

use pohunek_libghostty_spike::config::ModelConfig;
use pohunek_libghostty_spike::model::GridModel;
use pohunek_libghostty_spike::report::{build_report, load_fixtures, ModelSpec};
use pohunek_libghostty_spike::vt100_model::Vt100Model;

fn specs(config: &ModelConfig) -> Vec<ModelSpec> {
    let _ = config;
    #[allow(
        unused_mut,
        reason = "the libghostty model is added only with the feature"
    )]
    let mut specs = vec![ModelSpec {
        name: "vt100",
        make: Box::new(
            |rows, cols| Ok(Box::new(Vt100Model::new(rows, cols)) as Box<dyn GridModel>),
        ),
    }];
    #[cfg(feature = "libghostty")]
    {
        let config = config.clone();
        specs.push(ModelSpec {
            name: "libghostty",
            make: Box::new(move |rows, cols| {
                pohunek_libghostty_spike::ghostty_model::GhosttyModel::new(&config, rows, cols)
                    .map(|model| Box::new(model) as Box<dyn GridModel>)
            }),
        });
    }
    specs
}

#[test]
fn every_fixture_replays_through_every_model_into_a_deterministic_report() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let config = ModelConfig::load(&manifest.join("config/model.toml")).expect("config");
    let fixtures = load_fixtures(&manifest.join("fixtures"), &config).expect("fixtures");
    assert!(fixtures.len() >= 12, "fixtures: {}", fixtures.len());
    let models = specs(&config);

    let report = build_report(&fixtures, &models).expect("report");
    let again = build_report(&fixtures, &models).expect("report again");
    assert_eq!(report.to_json().unwrap(), again.to_json().unwrap());
    assert_eq!(report.to_markdown(), again.to_markdown());

    assert_eq!(report.traces.len(), fixtures.len());
    for (trace, fixture) in report.traces.iter().zip(&fixtures) {
        assert_eq!(trace.assertions.len(), fixture.expect.assertion.len());
        for row in &trace.assertions {
            for model in &report.models {
                assert!(row.outcome.contains_key(model), "{} / {model}", row.id);
            }
        }
        assert_eq!(trace.repaint.len(), models.len(), "{}", trace.trace);
    }

    // The harness evaluates assertions for real: the plain facts of the wide
    // character fixture pass on the shipped model, and an unobservable
    // property is a failure with a reason, not an error.
    let wide = report
        .traces
        .iter()
        .find(|t| t.trace == "wide-chars")
        .expect("wide-chars");
    let first = wide
        .assertions
        .iter()
        .find(|a| a.id == "wide-first-cell")
        .expect("assertion");
    assert_eq!(first.outcome["vt100"], "pass");
    let links = report
        .traces
        .iter()
        .find(|t| t.trace == "hyperlink-wrap")
        .expect("hyperlink-wrap");
    let link = links
        .assertions
        .iter()
        .find(|a| a.id == "link-first-cell")
        .expect("assertion");
    assert!(link.outcome["vt100"].starts_with("FAIL: the model cannot observe"));
}
