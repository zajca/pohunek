//! The models the binaries can build, by name.

// Rust guideline compliant 2026-06-26

use crate::config::ModelConfig;
use crate::model::GridModel;
use crate::report::ModelSpec;
use crate::vt100_model::Vt100Model;

/// Names of the models linked into this build, in report order.
#[must_use]
pub fn linked_models() -> Vec<&'static str> {
    let mut names = vec!["vt100", "vt100-scrollback"];
    if cfg!(feature = "libghostty") {
        names.push("libghostty");
    }
    names
}

/// The factory of the model called `name`.
///
/// # Errors
///
/// Returns a message when `name` is unknown or needs a feature this build
/// lacks.
pub fn model_spec(name: &str, config: &ModelConfig) -> Result<ModelSpec, String> {
    let scrollback_rows = config.vt100_scrollback_rows;
    match name {
        "vt100" => Ok(ModelSpec {
            name: "vt100",
            make: Box::new(|rows, cols| {
                Ok(Box::new(Vt100Model::new(rows, cols)) as Box<dyn GridModel>)
            }),
        }),
        "vt100-scrollback" => Ok(ModelSpec {
            name: "vt100-scrollback",
            make: Box::new(move |rows, cols| {
                Ok(
                    Box::new(Vt100Model::with_scrollback(rows, cols, scrollback_rows))
                        as Box<dyn GridModel>,
                )
            }),
        }),
        #[cfg(feature = "libghostty")]
        "libghostty" => {
            let config = config.clone();
            Ok(ModelSpec {
                name: "libghostty",
                make: Box::new(move |rows, cols| {
                    crate::ghostty_model::GhosttyModel::new(&config, rows, cols)
                        .map(|model| Box::new(model) as Box<dyn GridModel>)
                }),
            })
        }
        other => Err(format!("unknown or disabled model {other}")),
    }
}
