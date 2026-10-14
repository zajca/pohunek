//! Bounded capture of a lifecycle hook's stderr.
//!
//! A hook's stderr is a pipe whose read end one named thread owns for its whole
//! life:
//!
//! 1. While the hook runs, the thread keeps only the last `limit` bytes and
//!    counts the bytes it dropped, so a chatty hook never blocks on a full pipe.
//! 2. When the hook has exited or been killed, [`StderrCapture::finish`] asks
//!    for the tail. The thread reads only what is buffered in the pipe at that
//!    moment, so a descendant that keeps writing cannot stall the caller, and
//!    hands the tail over.
//! 3. The thread then reads and discards until EOF and exits; nobody joins it.
//!    A process the hook left running in the background keeps a stderr that
//!    neither fills up nor raises `SIGPIPE` while the daemon runs. Each such
//!    process keeps the thread and its descriptor until it closes stderr.
//!
//! The caller never waits for EOF, because such a descendant may hold the write
//! end open indefinitely. The read end lives in the daemon, so after a daemon
//! restart a remaining descendant's next stderr write raises `SIGPIPE`; a hook
//! that starts long-lived processes redirects their stderr itself. When the pipe
//! is still open a release grace after the hook ended, the thread logs a warning
//! naming the hook, so the owner learns which hook to fix.

// Rust guideline compliant 2026-10-14

use std::collections::VecDeque;
use std::io::{self, PipeReader, PipeWriter};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use tracing::warn;

/// Bytes read from the pipe in one call.
///
/// One page: the kernel moves pipe data in pages, so a larger buffer would not
/// return more per call under normal load. The tail limit is independent of it.
const READ_CHUNK_BYTES: usize = 4096;

/// UTF-8 continuation bytes are `0b10xx_xxxx`.
const UTF8_CONTINUATION_MASK: u8 = 0b1100_0000;
const UTF8_CONTINUATION_TAG: u8 = 0b1000_0000;

/// Longest run of continuation bytes that can follow a cut inside one UTF-8
/// character (a four-byte character cut after its first byte).
const MAX_UTF8_CONTINUATION_BYTES: usize = 3;

/// How a capture reads and when it reports a held pipe.
#[derive(Debug, Clone, Copy)]
pub(super) struct CaptureSettings {
    /// Bytes kept.
    pub limit: NonZeroUsize,
    /// Longest wait of the reader before it notices a request.
    pub wake_interval: Duration,
    /// How long after the hook ended the pipe may stay open before the reader
    /// warns that a background process still holds it.
    pub release_grace: Duration,
}

/// The hook a capture belongs to, named in its warning.
#[derive(Debug, Clone)]
pub(super) struct HookOrigin {
    /// Hook event token, such as `post-create`.
    pub event: &'static str,
    /// Hook script path.
    pub script: PathBuf,
}

/// A running capture of one hook's stderr.
pub(super) struct StderrCapture {
    /// Sending, or dropping, asks the reader for its tail.
    request: Sender<()>,
    /// Receives the tail once.
    tail: Receiver<StderrTail>,
}

impl StderrCapture {
    /// Creates the stderr pipe and starts its reader thread.
    ///
    /// Returns the capture and the write end the hook gets as its stderr. The
    /// caller must drop every copy of the write end it holds as soon as the hook
    /// is spawned, including the one inside its `Command`, or the reader never
    /// sees EOF and its discard phase never ends.
    ///
    /// The settings' wake interval bounds how long [`finish`](Self::finish)
    /// waits for the reader to notice the request. The reader logs through the
    /// caller's current tracing dispatcher.
    ///
    /// # Errors
    ///
    /// Returns the error of creating the pipe, making its read end non-blocking,
    /// converting the wake interval, or spawning the thread.
    pub(super) fn start(
        settings: CaptureSettings,
        origin: HookOrigin,
    ) -> io::Result<(Self, PipeWriter)> {
        let (pipe, writer) = io::pipe()?;
        rustix::io::ioctl_fionbio(&pipe, true)?;
        let wake = Timespec::try_from(settings.wake_interval).map_err(|_overflow| {
            io::Error::new(io::ErrorKind::InvalidInput, "wake interval too long")
        })?;
        let (request, requests) = mpsc::channel();
        let (tail_sender, tail) = mpsc::channel();
        let reader = Reader {
            pipe,
            settings,
            wake,
            origin,
            requests,
            tail_sender,
        };
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        thread::Builder::new()
            .name("hook-stderr".to_owned())
            .spawn(move || tracing::dispatcher::with_default(&dispatch, || reader.run()))?;
        Ok((Self { request, tail }, writer))
    }

    /// Returns the tail kept so far, plus what the pipe buffers right now.
    ///
    /// Call it once the hook has exited or been killed. It returns within about
    /// one wake interval. `None` means the reader thread is gone (it panicked).
    pub(super) fn finish(self) -> Option<StderrTail> {
        // A send fails only when the reader is gone, which `recv` reports below.
        let _ = self.request.send(());
        self.tail.recv().ok()
    }
}

/// The last bytes a hook wrote to stderr.
#[derive(Debug, Default)]
pub(super) struct StderrTail {
    bytes: Vec<u8>,
    /// Bytes written before `bytes` that the bound dropped.
    omitted: u64,
}

impl StderrTail {
    /// Renders the tail as text for a warning detail, or `None` when it is empty.
    ///
    /// The bytes are decoded as lossy UTF-8. Terminal escape sequences and
    /// control characters other than line feed and tab are removed, so the text
    /// cannot drive a terminal that prints it. A cut is marked with the number
    /// of bytes omitted before it.
    pub(super) fn render(&self) -> Option<String> {
        let mut start = 0;
        if self.omitted > 0 {
            // Skip the rest of a character the cut split, instead of rendering it
            // as a replacement character.
            while start < self.bytes.len().min(MAX_UTF8_CONTINUATION_BYTES)
                && self.bytes[start] & UTF8_CONTINUATION_MASK == UTF8_CONTINUATION_TAG
            {
                start += 1;
            }
        }
        let text = strip_terminal_controls(&String::from_utf8_lossy(&self.bytes[start..]));
        let text = text.trim_end();
        if text.is_empty() {
            return None;
        }
        let omitted = self.omitted + start as u64;
        Some(if omitted > 0 {
            format!("[{omitted} earlier bytes omitted]\n{text}")
        } else {
            text.to_owned()
        })
    }
}

/// Removes terminal escape sequences, every control character but line feed
/// and tab, and the invisible characters that reorder or hide text where it is
/// shown (bidirectional controls, zero-width characters, line and paragraph
/// separators, the byte order mark).
///
/// A CSI sequence (`ESC [` or C1 `CSI`) ends at its final byte (`@` to `~`); an
/// OSC, DCS, SOS, PM or APC string ends at `BEL` or the string terminator
/// (`ESC \` or C1 `ST`); any other escape covers the one character after `ESC`.
/// An unterminated sequence removes the rest of the text.
fn strip_terminal_controls(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        let introducer = match character {
            '\u{1b}' => chars.next(),
            '\u{9b}' => Some('['),
            '\u{90}' => Some('P'),
            '\u{9d}' => Some(']'),
            '\u{98}' => Some('X'),
            '\u{9e}' => Some('^'),
            '\u{9f}' => Some('_'),
            '\n' | '\t' => {
                kept.push(character);
                continue;
            }
            control if control.is_control() || is_invisible_format(control) => continue,
            printable => {
                kept.push(printable);
                continue;
            }
        };
        match introducer {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']' | 'P' | 'X' | '^' | '_') => {
                while let Some(next) = chars.next() {
                    match next {
                        '\u{7}' | '\u{9c}' => break,
                        '\u{1b}' if chars.peek() == Some(&'\\') => {
                            chars.next();
                            break;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    kept
}

/// Whether `character` is an invisible formatting character that can reorder or
/// hide the text around it.
fn is_invisible_format(character: char) -> bool {
    matches!(
        character,
        '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

/// The last `limit` bytes of a stream and how many came before them.
struct TailBuffer {
    limit: usize,
    bytes: VecDeque<u8>,
    omitted: u64,
}

impl TailBuffer {
    fn new(limit: NonZeroUsize) -> Self {
        Self {
            limit: limit.get(),
            bytes: VecDeque::new(),
            omitted: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        let excess = self.bytes.len().saturating_sub(self.limit);
        if excess > 0 {
            self.bytes.drain(..excess);
            self.omitted += excess as u64;
        }
    }

    fn into_tail(self) -> StderrTail {
        StderrTail {
            bytes: self.bytes.into(),
            omitted: self.omitted,
        }
    }
}

/// Whether the write end of the pipe is still open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pipe {
    Open,
    Closed,
}

/// The reader thread's state.
struct Reader {
    pipe: PipeReader,
    settings: CaptureSettings,
    wake: Timespec,
    origin: HookOrigin,
    requests: Receiver<()>,
    tail_sender: Sender<StderrTail>,
}

impl Reader {
    /// Captures, reads what is buffered at the request, then discards until EOF.
    fn run(self) {
        let reader = &self.pipe;
        let mut tail = TailBuffer::new(self.settings.limit);
        let mut chunk = [0_u8; READ_CHUNK_BYTES];
        let mut pipe = Pipe::Open;
        loop {
            match self.requests.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) => {}
            }
            if pipe == Pipe::Closed {
                // Everything was read; only the request is still to come. A
                // dropped capture also ends the wait.
                let _ = self.requests.recv();
                break;
            }
            match wait_readable(reader, Some(&self.wake)) {
                // One chunk per wake, so a writer that never pauses cannot keep
                // the loop from seeing the request.
                Readiness::Ready => {
                    pipe = read_into(reader, &mut chunk, READ_CHUNK_BYTES, &mut tail);
                }
                Readiness::Idle => {}
                // Capture stops, but the read end stays open for the discard
                // phase, so the hook never writes into a closed pipe.
                Readiness::Failed => {
                    let _ = self.requests.recv();
                    break;
                }
            }
        }
        if pipe == Pipe::Open {
            // Only what is buffered now: a writer that never pauses cannot keep
            // this read going. A failed query reads nothing more.
            let buffered = rustix::io::ioctl_fionread(reader)
                .ok()
                .and_then(|buffered| usize::try_from(buffered).ok())
                .unwrap_or(0);
            pipe = read_into(reader, &mut chunk, buffered, &mut tail);
        }
        // The caller may already be gone; the tail is then dropped.
        let _ = self.tail_sender.send(tail.into_tail());
        if pipe == Pipe::Open
            && !discard_until(
                reader,
                &mut chunk,
                Instant::now() + self.settings.release_grace,
            )
        {
            warn!(
                event = self.origin.event,
                script = %self.origin.script.display(),
                "a background process still holds the hook's stderr; redirect its stderr or set hooks.stderr_tail_bytes = 0, or it gets SIGPIPE after a daemon restart"
            );
            discard_until_eof(reader, &mut chunk);
        }
    }
}

/// Result of waiting for the pipe.
enum Readiness {
    /// Data or EOF is waiting.
    Ready,
    /// The timeout passed or a signal interrupted the wait.
    Idle,
    /// `poll` itself failed; no later wait can succeed.
    Failed,
}

/// Waits until `reader` is readable or closed, at most `timeout` when given.
fn wait_readable(reader: &PipeReader, timeout: Option<&Timespec>) -> Readiness {
    let mut fds = [PollFd::new(reader, PollFlags::IN)];
    match poll(&mut fds, timeout) {
        Ok(0) | Err(Errno::INTR) => Readiness::Idle,
        Ok(_) => Readiness::Ready,
        Err(_) => Readiness::Failed,
    }
}

/// Reads at most `budget` bytes without blocking, keeping them in `tail`.
///
/// Stops when the pipe has nothing more buffered. Returns
/// [`Pipe::Closed`] at EOF or on a read error, since no later read can succeed.
fn read_into(
    reader: &PipeReader,
    chunk: &mut [u8],
    mut budget: usize,
    tail: &mut TailBuffer,
) -> Pipe {
    while budget > 0 {
        let want = budget.min(chunk.len());
        match rustix::io::read(reader, &mut chunk[..want]) {
            Ok(0) => return Pipe::Closed,
            Ok(read) => {
                tail.push(&chunk[..read]);
                budget -= read;
            }
            Err(Errno::INTR) => {}
            Err(Errno::AGAIN) => return Pipe::Open,
            Err(_) => return Pipe::Closed,
        }
    }
    Pipe::Open
}

/// Reads and drops everything until EOF or `deadline`.
///
/// Returns `true` when the pipe ended (EOF or an error), `false` when it is
/// still open at the deadline.
fn discard_until(reader: &PipeReader, chunk: &mut [u8], deadline: Instant) -> bool {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let Ok(timeout) = Timespec::try_from(remaining) else {
            return false;
        };
        match wait_readable(reader, Some(&timeout)) {
            Readiness::Ready => {}
            Readiness::Idle => continue,
            Readiness::Failed => return false,
        }
        loop {
            match rustix::io::read(reader, &mut *chunk) {
                Ok(0) => return true,
                Ok(_) | Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => break,
                Err(_) => return true,
            }
        }
    }
}

/// Reads and drops everything until EOF or a read error.
fn discard_until_eof(reader: &PipeReader, chunk: &mut [u8]) {
    loop {
        match wait_readable(reader, None) {
            Readiness::Ready => {}
            Readiness::Idle => continue,
            // Without `poll`, blocking reads still drain the pipe.
            Readiness::Failed => {
                if rustix::io::ioctl_fionbio(reader, false).is_ok() {
                    discard_blocking(reader, chunk);
                }
                return;
            }
        }
        loop {
            match rustix::io::read(reader, &mut *chunk) {
                Ok(0) => return,
                Ok(_) | Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => break,
                Err(_) => return,
            }
        }
    }
}

/// Reads and drops everything with blocking reads until EOF or a read error.
fn discard_blocking(reader: &PipeReader, chunk: &mut [u8]) {
    loop {
        match rustix::io::read(reader, &mut *chunk) {
            Ok(0) => return,
            Ok(_) | Err(Errno::INTR) => {}
            Err(_) => return,
        }
    }
}
