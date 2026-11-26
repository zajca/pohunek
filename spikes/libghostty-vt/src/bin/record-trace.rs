//! Runs a command in a real PTY and writes its output as a trace.
//!
//! ```text
//! record-trace --out FILE --rows N --cols N --home DIR
//!              [--resize RxC]... [--on PATTERN=INPUT]... [--env KEY=VALUE]... [--cwd DIR]
//!              [--config FILE] -- COMMAND [ARG]...
//! ```
//!
//! The child gets an empty environment plus `HOME`, the XDG directories under
//! `--home`, `TERM`, `LANG`, `PATH` (from the config) and every `--env`. Each
//! PTY read is one `Output` record. A script that prints the config's resize
//! marker and then reads a line gets the next `--resize` applied (recorded as a
//! `Resize` record) and a newline written to its stdin, so resizes land at
//! fixed points of the output.

// Rust guideline compliant 2026-06-26

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use pohunek_libghostty_spike::config::ModelConfig;
use pohunek_libghostty_spike::trace::{Record, Trace};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// `EIO`, the error a PTY master reports once the child side is closed
/// (the same value on Linux and macOS).
const RAW_EIO: i32 = 5;

/// Usage text printed by `--help`.
const USAGE: &str = "usage: record-trace --out FILE --rows N --cols N --home DIR \
[--resize ROWSxCOLS]... [--on PATTERN=INPUT]... [--env KEY=VALUE]... [--cwd DIR] [--config FILE] -- COMMAND [ARG]...

  --out FILE        trace file to write
  --rows/--cols N   initial PTY size
  --home DIR        isolated HOME; XDG directories are created under it
  --resize RxC      size applied at the next resize marker the child prints (repeatable, in order)
  --on PAT=INPUT    write INPUT to the PTY when PAT appears in the output since the previous step
                    (repeatable, in order; INPUT understands \\r \\n \\t \\e \\\\)
  --env KEY=VALUE   extra environment variable of the child (repeatable)
  --cwd DIR         working directory of the child (default: --home)
  --config FILE     config file (default: config/model.toml)";

struct Args {
    out: PathBuf,
    rows: u16,
    cols: u16,
    home: PathBuf,
    resizes: Vec<(u16, u16)>,
    env: Vec<(String, String)>,
    cwd: Option<PathBuf>,
    config: PathBuf,
    /// Ordered `--on PATTERN=INPUT` steps: INPUT is written to the PTY once
    /// PATTERN appears in the output that follows the previous step.
    steps: Vec<(Vec<u8>, Vec<u8>)>,
    command: Vec<String>,
}

/// Decodes the `\r`, `\n`, `\t`, `\e` and `\\` escapes of a `--on` input.
fn unescape(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.extend_from_slice(c.to_string().as_bytes());
            continue;
        }
        match chars.next() {
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('e') => out.push(0x1b),
            Some(other) => out.extend_from_slice(other.to_string().as_bytes()),
            None => out.push(b'\\'),
        }
    }
    out
}

fn parse_args() -> Result<Args, String> {
    let mut argv = std::env::args().skip(1);
    let (mut out, mut rows, mut cols, mut home, mut cwd) = (None, None, None, None, None);
    let mut resizes = Vec::new();
    let mut env = Vec::new();
    let mut config = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/config/model.toml"));
    let mut steps = Vec::new();
    let mut command = Vec::new();
    while let Some(flag) = argv.next() {
        if flag == "--help" {
            return Err(USAGE.to_owned());
        }
        if flag == "--" {
            command = argv.by_ref().collect();
            break;
        }
        let mut value = || argv.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--out" => out = Some(PathBuf::from(value()?)),
            "--rows" => rows = Some(value()?.parse::<u16>().map_err(|e| e.to_string())?),
            "--cols" => cols = Some(value()?.parse::<u16>().map_err(|e| e.to_string())?),
            "--home" => home = Some(PathBuf::from(value()?)),
            "--cwd" => cwd = Some(PathBuf::from(value()?)),
            "--config" => config = PathBuf::from(value()?),
            "--resize" => {
                let text = value()?;
                let (r, c) = text.split_once('x').ok_or("--resize needs ROWSxCOLS")?;
                resizes.push((
                    r.parse::<u16>().map_err(|e| e.to_string())?,
                    c.parse::<u16>().map_err(|e| e.to_string())?,
                ));
            }
            "--on" => {
                let text = value()?;
                let (pattern, input) = text.split_once('=').ok_or("--on needs PATTERN=INPUT")?;
                if pattern.is_empty() {
                    return Err("--on needs a non-empty pattern".to_owned());
                }
                steps.push((unescape(pattern), unescape(input)));
            }
            "--env" => {
                let text = value()?;
                let (k, v) = text.split_once('=').ok_or("--env needs KEY=VALUE")?;
                env.push((k.to_owned(), v.to_owned()));
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if command.is_empty() {
        return Err("a command is required after --".to_owned());
    }
    Ok(Args {
        out: out.ok_or("--out is required")?,
        rows: rows.ok_or("--rows is required")?,
        cols: cols.ok_or("--cols is required")?,
        home: home.ok_or("--home is required")?,
        resizes,
        env,
        cwd,
        config,
        steps,
        command,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear recording sequence: validate, spawn, read loop, write"
)]
fn run(args: &Args, config: &ModelConfig) -> Result<(), String> {
    let limits = config.limits();
    let size = |rows, cols| PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut trace = Trace {
        rows: args.rows,
        cols: args.cols,
        records: Vec::new(),
    };
    // Validate the dimensions before spawning anything.
    trace.encode(&limits).map_err(|e| e.to_string())?;
    for (rows, cols) in &args.resizes {
        Trace {
            rows: *rows,
            cols: *cols,
            records: Vec::new(),
        }
        .encode(&limits)
        .map_err(|e| e.to_string())?;
    }

    let pair = native_pty_system()
        .openpty(size(args.rows, args.cols))
        .map_err(|e| e.to_string())?;
    let mut command = CommandBuilder::new(&args.command[0]);
    command.args(&args.command[1..]);
    command.env_clear();
    command.env("HOME", &args.home);
    for (name, dir) in [
        ("XDG_CONFIG_HOME", ".config"),
        ("XDG_DATA_HOME", ".local/share"),
        ("XDG_CACHE_HOME", ".cache"),
        ("XDG_STATE_HOME", ".local/state"),
    ] {
        let path = args.home.join(dir);
        std::fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        command.env(name, path);
    }
    command.env("TERM", &config.child_term);
    command.env("LANG", &config.child_lang);
    command.env("PATH", &config.child_path);
    for (key, value) in &args.env {
        command.env(key, value);
    }
    command.cwd(args.cwd.as_ref().unwrap_or(&args.home));
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|e| e.to_string())?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let mut writer = pair.master.take_writer().map_err(|e| e.to_string())?;

    // The watchdog kills a child that outlives the configured limit.
    let mut killer = child.clone_killer();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let limit = Duration::from_secs(config.record_timeout_seconds);
    let timed_out = Arc::new(AtomicBool::new(false));
    let watchdog_flag = Arc::clone(&timed_out);
    let watchdog = std::thread::spawn(move || {
        if done_rx.recv_timeout(limit) == Err(mpsc::RecvTimeoutError::Timeout) {
            watchdog_flag.store(true, Ordering::SeqCst);
            let _ = killer.kill();
        }
    });

    let marker = config.resize_marker.as_bytes();
    let mut pending = args.resizes.iter();
    let mut steps = args.steps.iter();
    let mut step = steps.next();
    // Output since the previous step, searched for the next step's pattern.
    let mut since_step: Vec<u8> = Vec::new();
    let mut tail: Vec<u8> = Vec::new();
    let mut buffer = vec![0_u8; config.read_chunk_bytes];
    let mut abort_handle = child.clone_killer();
    let outcome: Result<(), String> = (|| loop {
        // A PTY whose child has exited reports end of input or EIO.
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.raw_os_error() == Some(RAW_EIO) => return Ok(()),
            Err(error) => return Err(format!("reading the PTY: {error}")),
        };
        let chunk = &buffer[..read];
        trace.records.push(Record::Output(chunk.to_vec()));
        if let Some((pattern, input)) = step {
            since_step.extend_from_slice(chunk);
            if since_step
                .windows(pattern.len())
                .any(|w| w == pattern.as_slice())
            {
                writer.write_all(input).map_err(|e| e.to_string())?;
                writer.flush().map_err(|e| e.to_string())?;
                since_step.clear();
                step = steps.next();
            } else if since_step.len() > config.max_record_bytes {
                return Err("an --on pattern did not appear within the output bound".to_owned());
            }
        }
        tail.extend_from_slice(chunk);
        let mut hits = 0;
        while let Some(at) = tail.windows(marker.len()).position(|w| w == marker) {
            tail.drain(..at + marker.len());
            hits += 1;
        }
        let keep = marker.len().saturating_sub(1).min(tail.len());
        tail.drain(..tail.len() - keep);
        for _ in 0..hits {
            if let Some((rows, cols)) = pending.next() {
                pair.master
                    .resize(size(*rows, *cols))
                    .map_err(|e| e.to_string())?;
                trace.records.push(Record::Resize {
                    rows: *rows,
                    cols: *cols,
                });
            }
            writer.write_all(b"\n").map_err(|e| e.to_string())?;
            writer.flush().map_err(|e| e.to_string())?;
        }
    })();
    if outcome.is_err() {
        // A failed recording must not leave the child running.
        let _ = abort_handle.kill();
    }
    let _ = done_tx.send(());
    let status = child.wait().map_err(|e| e.to_string())?;
    let _ = watchdog.join();
    outcome?;
    if timed_out.load(Ordering::SeqCst) {
        return Err(format!(
            "the child outlived record_timeout_seconds ({}) and was killed",
            config.record_timeout_seconds
        ));
    }
    if !status.success() {
        return Err(format!("the child exited with {status:?}"));
    }
    if pending.next().is_some() {
        return Err("the script ended before every --resize was applied".to_owned());
    }
    if step.is_some() {
        return Err("the child ended before every --on pattern appeared".to_owned());
    }
    eprintln!("child exited: {status:?}; {} records", trace.records.len());
    let bytes = trace.encode(&limits).map_err(|e| e.to_string())?;
    std::fs::write(&args.out, bytes).map_err(|e| format!("{}: {e}", args.out.display()))
}

fn main() -> ExitCode {
    let result = parse_args().and_then(|args| {
        let config = ModelConfig::load(&args.config).map_err(|e| e.to_string())?;
        run(&args, &config)
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("record-trace: {message}");
            ExitCode::FAILURE
        }
    }
}
