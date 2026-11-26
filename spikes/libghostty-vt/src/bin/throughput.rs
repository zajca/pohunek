//! Feeds one parser a workload several times and prints one JSON line with the
//! run times, their median and the parser's resident memory.
//!
//! ```text
//! throughput --model NAME --workload agent|flood [--fixtures DIR] [--config FILE]
//! ```
//!
//! Subjects (`--model`):
//! - `vt100`: `TerminalTracker`, what the session worker runs today.
//! - `vt100-parser`: a bare `vt100::Parser` without scrollback, the tracker
//!   minus its OSC scanner.
//! - `vt100-scrollback`: a bare `vt100::Parser` with the configured scrollback.
//! - `libghostty`: the headless libghostty-vt terminal (feature `libghostty`).
//!
//! Workloads:
//! - `agent`: the recorded agent trace replayed back to back.
//! - `flood`: a deterministic synthetic stream of coloured text lines with
//!   wide characters, cursor moves and line erases.
//!
//! The run count, flood size, chunk size and seed come from the config file.
//! Numbers are indicative: they depend on the machine the binary runs on.

// Rust guideline compliant 2026-06-26

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use pohunek_libghostty_spike::config::ModelConfig;
use pohunek_libghostty_spike::report::load_fixtures;
use pohunek_libghostty_spike::trace::{Record, Trace};

const MIB: f64 = 1_048_576.0;

/// What one run feeds: only the parsing path, with no cell dump or diff.
enum Subject {
    /// `TerminalTracker`, as the session worker runs it.
    Tracker(pohunek_terminal::TerminalTracker),
    /// A bare `vt100` parser, with or without scrollback.
    Parser(vt100::Parser),
    /// libghostty-vt with the production settings of the spike.
    #[cfg(feature = "libghostty")]
    Ghostty(Box<pohunek_libghostty_spike::ghostty::HeadlessTerminal>),
}

impl Subject {
    fn new(name: &str, config: &ModelConfig, rows: u16, cols: u16) -> Result<Self, String> {
        match name {
            "vt100" => Ok(Self::Tracker(pohunek_terminal::TerminalTracker::new(
                rows, cols,
            ))),
            "vt100-parser" => Ok(Self::Parser(vt100::Parser::new(rows, cols, 0))),
            "vt100-scrollback" => Ok(Self::Parser(vt100::Parser::new(
                rows,
                cols,
                config.vt100_scrollback_rows,
            ))),
            #[cfg(feature = "libghostty")]
            "libghostty" => {
                pohunek_libghostty_spike::ghostty::HeadlessTerminal::new(config, rows, cols)
                    .map(|terminal| Self::Ghostty(Box::new(terminal)))
                    .map_err(|e| e.to_string())
            }
            other => Err(format!("unknown or disabled model {other}")),
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        match self {
            Self::Tracker(tracker) => tracker.feed(bytes),
            Self::Parser(parser) => parser.process(bytes),
            #[cfg(feature = "libghostty")]
            Self::Ghostty(terminal) => terminal.feed(bytes),
        }
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "the libghostty variant can fail, and the build without it keeps one signature"
    )]
    fn resize(&mut self, rows: u16, cols: u16) -> Result<(), String> {
        match self {
            Self::Tracker(tracker) => tracker.resize(rows, cols),
            Self::Parser(parser) => parser.screen_mut().set_size(rows, cols),
            #[cfg(feature = "libghostty")]
            Self::Ghostty(terminal) => terminal.resize(rows, cols).map_err(|e| e.to_string())?,
        }
        Ok(())
    }

    /// The visible text, to check that every run ends on the same screen.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "the libghostty variant can fail, and the build without it keeps one signature"
    )]
    fn visible_text(&self) -> Result<String, String> {
        match self {
            Self::Tracker(tracker) => Ok(tracker.snapshot(0).visible_text),
            Self::Parser(parser) => Ok(parser.screen().contents()),
            #[cfg(feature = "libghostty")]
            Self::Ghostty(terminal) => terminal
                .format_plain()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .map_err(|e| e.to_string()),
        }
    }
}

/// Words of the flood; one is wide so the flood exercises two-cell glyphs.
const WORDS: [&str; 10] = [
    "alpha",
    "build",
    "compile",
    "diff",
    "error",
    "\u{4e16}\u{754c}",
    "fn",
    "log",
    "warning",
    "src/lib.rs:42",
];
/// SGR sequences the flood switches between.
const STYLES: [&str; 5] = [
    "\u{1b}[0m",
    "\u{1b}[1;31m",
    "\u{1b}[38;5;208m",
    "\u{1b}[48;2;10;20;30m",
    "\u{1b}[4;32m",
];

struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % bound as u64).expect("below a usize bound")
    }
}

fn flood(bytes: usize, seed: u64) -> Vec<u8> {
    let mut rng = Xorshift(seed);
    let mut out = Vec::with_capacity(bytes + 256);
    while out.len() < bytes {
        let words = 3 + rng.below(20);
        for _ in 0..words {
            if rng.below(4) == 0 {
                out.extend_from_slice(STYLES[rng.below(STYLES.len())].as_bytes());
            }
            out.extend_from_slice(WORDS[rng.below(WORDS.len())].as_bytes());
            out.push(b' ');
        }
        match rng.below(8) {
            0 => out.extend_from_slice(b"\x1b[2K\r"),
            1 => out.extend_from_slice(b"\x1b[3A\x1b[10C"),
            _ => {}
        }
        out.extend_from_slice(b"\x1b[0m\r\n");
    }
    out.truncate(bytes);
    out
}

/// Resident set size in KiB; `None` where `/proc/self/status` does not exist.
fn resident_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.trim().strip_suffix("kB"))
        .and_then(|number| number.trim().parse().ok())
}

enum Workload {
    Agent { trace: Trace, replays: usize },
    Flood { chunks: Vec<Vec<u8>> },
}

impl Workload {
    fn bytes(&self) -> usize {
        match self {
            Self::Agent { trace, replays } => {
                let once: usize = trace
                    .records
                    .iter()
                    .map(|record| match record {
                        Record::Output(bytes) => bytes.len(),
                        Record::Resize { .. } => 0,
                    })
                    .sum();
                once * replays
            }
            Self::Flood { chunks } => chunks.iter().map(Vec::len).sum(),
        }
    }

    fn feed(&self, model: &mut Subject) -> Result<(), String> {
        match self {
            Self::Agent { trace, replays } => {
                for _ in 0..*replays {
                    for record in &trace.records {
                        match record {
                            Record::Output(bytes) => model.feed(bytes),
                            Record::Resize { rows, cols } => model.resize(*rows, *cols)?,
                        }
                    }
                }
            }
            Self::Flood { chunks } => {
                for chunk in chunks {
                    model.feed(chunk);
                }
            }
        }
        Ok(())
    }
}

fn median(sorted: &[f64]) -> f64 {
    sorted[sorted.len() / 2]
}

fn run() -> Result<(), String> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let mut model_name = None;
    let mut workload_name = None;
    let mut fixtures = PathBuf::from(format!("{manifest}/fixtures"));
    let mut config = PathBuf::from(format!("{manifest}/config/model.toml"));
    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        let value = argv.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--model" => model_name = Some(value),
            "--workload" => workload_name = Some(value),
            "--fixtures" => fixtures = PathBuf::from(value),
            "--config" => config = PathBuf::from(value),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let model_name = model_name.ok_or("--model is required")?;
    let workload_name = workload_name.ok_or("--workload is required")?;
    let config = ModelConfig::load(&config).map_err(|e| e.to_string())?;
    let agent = load_fixtures(&fixtures, &config)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|fixture| fixture.name == config.throughput_agent_trace)
        .ok_or_else(|| format!("no fixture {}", config.throughput_agent_trace))?
        .trace;
    let (rows, cols) = (agent.rows, agent.cols);
    let workload = match workload_name.as_str() {
        "agent" => Workload::Agent {
            trace: agent,
            replays: config.throughput_agent_replays,
        },
        "flood" => Workload::Flood {
            chunks: flood(config.throughput_flood_bytes, config.throughput_flood_seed)
                .chunks(config.throughput_flood_chunk_bytes)
                .map(<[u8]>::to_vec)
                .collect(),
        },
        other => return Err(format!("unknown workload {other}")),
    };
    let bytes = workload.bytes();
    let mut seconds = Vec::new();
    let mut resident_delta = None;
    let mut visible = None;
    for run in 0..config.throughput_runs {
        let before = resident_kib();
        let mut model = Subject::new(&model_name, &config, rows, cols)?;
        let start = Instant::now();
        workload.feed(&mut model)?;
        seconds.push(start.elapsed().as_secs_f64());
        if run == 0 {
            // The first run of a fresh process: nothing freed earlier is reused.
            resident_delta = before.zip(resident_kib()).map(|(b, a)| a.saturating_sub(b));
        }
        let text = model.visible_text()?;
        match &visible {
            None => visible = Some(text),
            Some(first) if *first != text => {
                return Err("two runs ended on different screens".to_owned());
            }
            Some(_) => {}
        }
    }
    let mut sorted = seconds.clone();
    sorted.sort_by(f64::total_cmp);
    let median_seconds = median(&sorted);
    let mib = f64::from(u32::try_from(bytes).map_err(|_| "the workload is above 4 GiB")?) / MIB;
    let line = serde_json::json!({
        "model": model_name,
        "workload": workload_name,
        "bytes": bytes,
        "runs_seconds": seconds,
        "median_seconds": median_seconds,
        "median_mib_per_second": mib / median_seconds,
        "resident_delta_kib": resident_delta,
    });
    println!("{line}");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("throughput: {message}");
            ExitCode::FAILURE
        }
    }
}
