//! Links the whole model surface (feed, resize, dump, repaint) of every model
//! the build contains and prints one checksum line per model.
//!
//! Built with and without the `libghostty` feature, the size difference of the
//! stripped release binary is the cost of linking libghostty-vt into a Rust
//! program. The checksum keeps the optimizer from dropping the calls.

// Rust guideline compliant 2026-06-26

use std::path::PathBuf;
use std::process::ExitCode;

use pohunek_libghostty_spike::config::ModelConfig;
use pohunek_libghostty_spike::model::{GridDump, GridModel};
use pohunek_libghostty_spike::registry::{linked_models, model_spec};

/// Bytes every model is fed; plain text, colour, a wide character, a title and
/// an alternate screen round trip.
const INPUT: &[u8] = "\u{1b}]2;probe\u{7}plain \u{1b}[1;31mred\u{1b}[0m \u{4e16}\u{754c}\r\n\u{1b}[?1049hmenu\u{1b}[?1049l\r\nback\r\n".as_bytes();
const ROWS: u16 = 24;
const COLS: u16 = 80;

fn checksum(dump: &GridDump, repaint: &[u8]) -> u64 {
    let mut sum = u64::from(dump.cursor_row) * 31 + u64::from(dump.cursor_col);
    for row in &dump.cells {
        for cell in row {
            for byte in cell.text.bytes() {
                sum = sum
                    .wrapping_mul(1_099_511_628_211)
                    .wrapping_add(u64::from(byte));
            }
        }
    }
    for byte in repaint {
        sum = sum
            .wrapping_mul(1_099_511_628_211)
            .wrapping_add(u64::from(*byte));
    }
    sum
}

fn run() -> Result<(), String> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let mut config = PathBuf::from(format!("{manifest}/config/model.toml"));
    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--config" => config = PathBuf::from(argv.next().ok_or("--config needs a value")?),
            other => {
                return Err(format!(
                    "usage: size-probe [--config FILE]; unknown {other}"
                ))
            }
        }
    }
    let config = ModelConfig::load(&config).map_err(|e| e.to_string())?;
    for name in linked_models() {
        let spec = model_spec(name, &config)?;
        let mut model: Box<dyn GridModel> = (spec.make)(ROWS, COLS).map_err(|e| e.to_string())?;
        model.feed(INPUT);
        model.resize(ROWS + 1, COLS - 1);
        let dump = model.dump().map_err(|e| e.to_string())?;
        let repaint = model
            .repaint()
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        println!("{name} {:016x}", checksum(&dump, &repaint));
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("size-probe: {message}");
            ExitCode::FAILURE
        }
    }
}
