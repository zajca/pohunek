//! Replays every fixture through the chosen models and writes
//! `report.json` and `report.md`.
//!
//! ```text
//! spike-report --models vt100[,libghostty] [--fixtures DIR] [--out DIR] [--config FILE]
//! ```

// Rust guideline compliant 2026-06-26

use std::path::PathBuf;
use std::process::ExitCode;

use pohunek_libghostty_spike::config::ModelConfig;
use pohunek_libghostty_spike::model::GridModel;
use pohunek_libghostty_spike::report::{build_report, load_fixtures, ModelSpec};
use pohunek_libghostty_spike::vt100_model::Vt100Model;

fn spec(name: &str, config: &ModelConfig) -> Result<ModelSpec, String> {
    let _ = config;
    match name {
        "vt100" => Ok(ModelSpec {
            name: "vt100",
            make: Box::new(|rows, cols| {
                Ok(Box::new(Vt100Model::new(rows, cols)) as Box<dyn GridModel>)
            }),
        }),
        #[cfg(feature = "libghostty")]
        "libghostty" => {
            let config = config.clone();
            Ok(ModelSpec {
                name: "libghostty",
                make: Box::new(move |rows, cols| {
                    pohunek_libghostty_spike::ghostty_model::GhosttyModel::new(&config, rows, cols)
                        .map(|model| Box::new(model) as Box<dyn GridModel>)
                }),
            })
        }
        other => Err(format!("unknown or disabled model {other}")),
    }
}

fn run() -> Result<(), String> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let mut models = "vt100".to_owned();
    let mut fixtures = PathBuf::from(format!("{manifest}/fixtures"));
    let mut out = PathBuf::from(".");
    let mut config = PathBuf::from(format!("{manifest}/config/model.toml"));
    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        let value = argv.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--models" => models = value,
            "--fixtures" => fixtures = PathBuf::from(value),
            "--out" => out = PathBuf::from(value),
            "--config" => config = PathBuf::from(value),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let config = ModelConfig::load(&config).map_err(|e| e.to_string())?;
    let specs = models
        .split(',')
        .map(|name| spec(name, &config))
        .collect::<Result<Vec<_>, _>>()?;
    let fixtures = load_fixtures(&fixtures, &config).map_err(|e| e.to_string())?;
    let report = build_report(&fixtures, &specs).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    std::fs::write(
        out.join("report.json"),
        report.to_json().map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(out.join("report.md"), report.to_markdown()).map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("spike-report: {message}");
            ExitCode::FAILURE
        }
    }
}
