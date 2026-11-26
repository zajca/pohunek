//! A safe, headless libghostty-vt terminal.
//!
//! PTY output is hostile input, so the terminal is configured to have no
//! effect outside itself:
//!
//! - No reply callback is installed (`WRITE_PTY`, device attributes,
//!   enquiry, XTVERSION, title report, size report). The model never answers a
//!   query; Pohunek forwards terminal queries to the attached client.
//! - `CLIPBOARD_READ` stays NULL: an OSC 52 `?` read is ignored and an OSC
//!   5522 read is refused.
//! - `CLIPBOARD_WRITE` is a capture callback that records the decoded byte
//!   length of each write and answers "denied". No clipboard is ever touched.
//! - Kitty graphics are compiled out of the library; [`HeadlessTerminal::new`]
//!   checks the build info and that setting a graphics option is unsupported.
//! - Every size bound (scrollback, APC, unknown and continuation sequences,
//!   clipboard transactions) comes from `config/model.toml`.
//!
//! Ownership: the terminal handle and its callback state are freed on drop.
//! Threading: the handle is not thread-safe, so the type is neither `Send` nor
//! `Sync` (it holds raw pointers). A grid reference is used only inside one
//! method call, before anything can mutate the terminal.

// Rust guideline compliant 2026-06-26

#![allow(
    unsafe_code,
    reason = "this module is the safe wrapper around the libghostty-vt C API"
)]

use std::ffi::{c_int, c_void, CStr};
use std::ptr;

use crate::config::ModelConfig;
use crate::ffi;

/// A libghostty call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("libghostty {operation} failed: {name} ({code})")]
pub struct GhosttyError {
    /// The call that failed.
    pub operation: &'static str,
    /// Raw `GhosttyResult`.
    pub code: i32,
    /// The code's symbolic name.
    pub name: &'static str,
}

fn check(operation: &'static str, code: ffi::GhosttyResult) -> Result<(), GhosttyError> {
    if code == ffi::SUCCESS {
        return Ok(());
    }
    Err(GhosttyError {
        operation,
        code,
        name: code_name(code),
    })
}

fn code_name(code: ffi::GhosttyResult) -> &'static str {
    match code {
        ffi::OUT_OF_MEMORY => "OUT_OF_MEMORY",
        ffi::INVALID_VALUE => "INVALID_VALUE",
        ffi::OUT_OF_SPACE => "OUT_OF_SPACE",
        ffi::NO_VALUE => "NO_VALUE",
        ffi::IO_ERROR => "IO_ERROR",
        ffi::LIMIT_EXCEEDED => "LIMIT_EXCEEDED",
        ffi::REJECTED => "REJECTED",
        ffi::UNSUPPORTED => "UNSUPPORTED",
        _ => "UNKNOWN",
    }
}

/// One cell as the model reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellInfo {
    /// Grapheme text; empty for a blank cell.
    pub text: String,
    /// First half of a wide cell.
    pub wide: bool,
    /// Trailing half of a wide cell.
    pub spacer_tail: bool,
    /// OSC 8 target; empty for none.
    pub hyperlink: String,
}

/// State the C callbacks write into; heap allocated so its address is stable.
#[derive(Debug, Default)]
struct CallbackState {
    clipboard_writes: Vec<usize>,
    progress: Option<String>,
}

/// A libghostty-vt terminal with the headless settings above.
pub struct HeadlessTerminal {
    handle: ffi::Terminal,
    state: Box<CallbackState>,
    cell_px: u32,
}

impl std::fmt::Debug for HeadlessTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeadlessTerminal").finish_non_exhaustive()
    }
}

unsafe extern "C" fn on_clipboard_write(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
    write: *const ffi::ClipboardWrite,
) {
    // SAFETY: libghostty passes the userdata pointer set in `new` (a live
    // `CallbackState` owned by the terminal) and a valid request that lives
    // for the duration of this call.
    let (state, write) = unsafe { (&mut *userdata.cast::<CallbackState>(), &*write) };
    let mut total = 0;
    for index in 0..write.contents_len {
        // SAFETY: `contents` points to `contents_len` entries, valid during the call.
        total += unsafe { (*write.contents.add(index)).data.len };
    }
    state.clipboard_writes.push(total);
    let reply = ffi::ClipboardWriteReply {
        size: std::mem::size_of::<ffi::ClipboardWriteReply>(),
        result: ffi::CLIPBOARD_WRITE_DENIED,
        remember: false,
    };
    // SAFETY: the request's `reply` function must be called with the request
    // and a sized reply before the callback returns; both are valid here.
    unsafe { (write.reply)(write, &raw const reply) };
}

unsafe extern "C" fn on_progress(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
    report: *const ffi::ProgressReport,
) {
    // SAFETY: same contract as `on_clipboard_write`.
    let (state, report) = unsafe { (&mut *userdata.cast::<CallbackState>(), &*report) };
    state.progress = Some(if report.progress >= 0 {
        format!("4;{};{}", report.state, report.progress)
    } else {
        format!("4;{}", report.state)
    });
}

fn set_size_option(
    terminal: ffi::Terminal,
    option: c_int,
    value: usize,
    name: &'static str,
) -> Result<(), GhosttyError> {
    // SAFETY: `terminal` is live; the option takes a `size_t *` and copies it.
    check(name, unsafe {
        ffi::ghostty_terminal_set(terminal, option, ptr::from_ref(&value).cast())
    })
}

impl HeadlessTerminal {
    /// Creates a terminal of `rows` x `cols` cells and applies the headless
    /// settings from `config`.
    ///
    /// # Errors
    ///
    /// Returns a [`GhosttyError`] when libghostty rejects a call, or when the
    /// linked library still has Kitty graphics compiled in.
    pub fn new(config: &ModelConfig, rows: u16, cols: u16) -> Result<Self, GhosttyError> {
        let mut graphics = true;
        // SAFETY: the build-info key takes a `bool *`.
        check("build_info", unsafe {
            ffi::ghostty_build_info(
                ffi::BUILD_INFO_KITTY_GRAPHICS,
                ptr::from_mut(&mut graphics).cast(),
            )
        })?;
        if graphics {
            return Err(GhosttyError {
                operation: "build_info(kitty graphics)",
                code: ffi::REJECTED,
                name: "the library was built with Kitty graphics",
            });
        }
        let mut handle: ffi::Terminal = ptr::null_mut();
        // SAFETY: a NULL allocator selects the default; `handle` receives the terminal.
        check("terminal_new", unsafe {
            ffi::ghostty_terminal_new(ptr::null(), &raw mut handle, cols, rows)
        })?;
        let mut terminal = Self {
            handle,
            state: Box::default(),
            cell_px: config.ghostty_cell_px,
        };
        terminal.configure(config)?;
        Ok(terminal)
    }

    fn configure(&mut self, config: &ModelConfig) -> Result<(), GhosttyError> {
        let state: *mut CallbackState = &raw mut *self.state;
        // SAFETY: `handle` is live; USERDATA stores the pointer, which stays
        // valid because `state` is boxed and outlives the terminal (see `Drop`).
        check("set userdata", unsafe {
            ffi::ghostty_terminal_set(self.handle, ffi::OPT_USERDATA, state.cast())
        })?;
        // SAFETY: the option takes a callback of exactly this type.
        check("set clipboard write", unsafe {
            ffi::ghostty_terminal_set(
                self.handle,
                ffi::OPT_CLIPBOARD_WRITE,
                on_clipboard_write as ffi::ClipboardWriteFn as *const c_void,
            )
        })?;
        // SAFETY: the option takes a callback of exactly this type.
        check("set progress report", unsafe {
            ffi::ghostty_terminal_set(
                self.handle,
                ffi::OPT_PROGRESS_REPORT,
                on_progress as ffi::ProgressReportFn as *const c_void,
            )
        })?;
        for (option, value, name) in [
            (
                ffi::OPT_SCROLLBACK_MAX_BYTES,
                config.ghostty_scrollback_max_bytes,
                "set scrollback bytes",
            ),
            (
                ffi::OPT_APC_MAX_BYTES,
                config.ghostty_apc_max_bytes,
                "set apc max bytes",
            ),
            (
                ffi::OPT_UNKNOWN_MAX_BYTES,
                config.ghostty_unknown_max_bytes,
                "set unknown max bytes",
            ),
            (
                ffi::OPT_CONTINUATION_MAX_BYTES,
                config.ghostty_continuation_max_bytes,
                "set continuation max bytes",
            ),
            (
                ffi::OPT_CLIPBOARD_WRITE_MAX_BYTES,
                config.ghostty_clipboard_write_max_bytes,
                "set clipboard write max bytes",
            ),
        ] {
            set_size_option(self.handle, option, value, name)?;
        }
        // Positive evidence that Kitty graphics are gone: the option is unsupported.
        let limit = 0_usize;
        // SAFETY: `handle` is live; the option takes a `size_t *`.
        let code = unsafe {
            ffi::ghostty_terminal_set(
                self.handle,
                ffi::OPT_KITTY_IMAGE_STORAGE_LIMIT,
                ptr::from_ref(&limit).cast(),
            )
        };
        if code != ffi::UNSUPPORTED {
            return Err(GhosttyError {
                operation: "set kitty image storage limit (expected UNSUPPORTED)",
                code,
                name: code_name(code),
            });
        }
        Ok(())
    }

    /// Applies the next PTY output chunk.
    pub fn feed(&mut self, bytes: &[u8]) {
        // SAFETY: `handle` is live and `bytes` is a valid slice for the call.
        unsafe { ffi::ghostty_terminal_vt_write(self.handle, bytes.as_ptr(), bytes.len()) };
    }

    /// Resizes the terminal.
    ///
    /// # Errors
    ///
    /// Returns a [`GhosttyError`] when libghostty rejects the size.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), GhosttyError> {
        // SAFETY: `handle` is live.
        check("terminal_resize", unsafe {
            ffi::ghostty_terminal_resize(self.handle, cols, rows, self.cell_px, self.cell_px)
        })
    }

    fn get<T: Default>(&self, key: c_int, name: &'static str) -> Result<T, GhosttyError> {
        let mut out = T::default();
        // SAFETY: `handle` is live; every key used here documents an output
        // of exactly `T` (u16, bool, u8, usize or c_int).
        check(name, unsafe {
            ffi::ghostty_terminal_get(self.handle, key, ptr::from_mut(&mut out).cast())
        })?;
        Ok(out)
    }

    /// Rows and columns.
    ///
    /// # Errors
    ///
    /// Returns a [`GhosttyError`] when a read fails.
    pub fn size(&self) -> Result<(u16, u16), GhosttyError> {
        Ok((
            self.get(ffi::DATA_ROWS, "get rows")?,
            self.get(ffi::DATA_COLS, "get cols")?,
        ))
    }

    /// Cursor row and column, zero based.
    ///
    /// # Errors
    ///
    /// Returns a [`GhosttyError`] when a read fails.
    pub fn cursor(&self) -> Result<(u16, u16), GhosttyError> {
        Ok((
            self.get(ffi::DATA_CURSOR_Y, "get cursor y")?,
            self.get(ffi::DATA_CURSOR_X, "get cursor x")?,
        ))
    }

    /// Whether the cursor is visible.
    ///
    /// # Errors
    ///
    /// Returns a [`GhosttyError`] when the read fails.
    pub fn cursor_visible(&self) -> Result<bool, GhosttyError> {
        self.get(ffi::DATA_CURSOR_VISIBLE, "get cursor visible")
    }

    /// Whether the alternate screen is active.
    ///
    /// # Errors
    ///
    /// Returns a [`GhosttyError`] when the read fails.
    pub fn alternate_screen(&self) -> Result<bool, GhosttyError> {
        let screen: c_int = self.get(ffi::DATA_ACTIVE_SCREEN, "get active screen")?;
        Ok(screen == ffi::SCREEN_ALTERNATE)
    }

    /// The Kitty keyboard flag bits.
    ///
    /// # Errors
    ///
    /// Returns a [`GhosttyError`] when the read fails.
    pub fn kitty_keyboard_flags(&self) -> Result<u8, GhosttyError> {
        self.get(ffi::DATA_KITTY_KEYBOARD_FLAGS, "get kitty keyboard flags")
    }

    /// Rows scrolled off the primary screen.
    ///
    /// # Errors
    ///
    /// Returns a [`GhosttyError`] when the read fails.
    pub fn scrollback_rows(&self) -> Result<usize, GhosttyError> {
        self.get(ffi::DATA_SCROLLBACK_ROWS, "get scrollback rows")
    }

    /// The window title, when one was set.
    ///
    /// # Errors
    ///
    /// Returns a [`GhosttyError`] when the read fails.
    pub fn title(&self) -> Result<Option<String>, GhosttyError> {
        let mut title = ffi::GhosttyString {
            ptr: ptr::null(),
            len: 0,
        };
        // SAFETY: `handle` is live; the key outputs a `GhosttyString`, which
        // borrows terminal memory valid until the next mutation (copied below).
        check("get title", unsafe {
            ffi::ghostty_terminal_get(
                self.handle,
                ffi::DATA_TITLE,
                ptr::from_mut(&mut title).cast(),
            )
        })?;
        if title.len == 0 {
            return Ok(None);
        }
        // SAFETY: `ptr` and `len` describe a live byte range per the header.
        let bytes = unsafe { std::slice::from_raw_parts(title.ptr, title.len) };
        Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
    }

    /// The latest progress report in OSC 9;4 payload form.
    #[must_use]
    pub fn progress(&self) -> Option<&str> {
        self.state.progress.as_deref()
    }

    /// Decoded byte length of every clipboard write the program made.
    #[must_use]
    pub fn clipboard_writes(&self) -> &[usize] {
        &self.state.clipboard_writes
    }

    fn grid_ref(&self, row: u16, col: u16) -> Result<ffi::GridRef, GhosttyError> {
        let point = ffi::Point {
            tag: ffi::POINT_TAG_ACTIVE,
            value: ffi::PointValue {
                coordinate: ffi::PointCoordinate {
                    x: col,
                    y: u32::from(row),
                },
            },
        };
        let mut grid_ref = ffi::GridRef {
            node: ptr::null_mut(),
            x: 0,
            y: 0,
        };
        // SAFETY: `handle` is live; `grid_ref` receives an untracked reference
        // that the callers use before the next mutating call.
        check("grid_ref", unsafe {
            ffi::ghostty_terminal_grid_ref(self.handle, point, &raw mut grid_ref)
        })?;
        Ok(grid_ref)
    }

    /// Whether `row` of the active screen soft-wraps into the next one.
    ///
    /// # Errors
    ///
    /// Returns a [`GhosttyError`] when libghostty rejects the row.
    pub fn row_wrapped(&self, row: u16) -> Result<bool, GhosttyError> {
        let grid_ref = self.grid_ref(row, 0)?;
        let mut packed: ffi::Packed = 0;
        // SAFETY: `grid_ref` was just obtained and the terminal is unchanged.
        check("grid_ref_row", unsafe {
            ffi::ghostty_grid_ref_row(&raw const grid_ref, &raw mut packed)
        })?;
        let mut wrapped = false;
        // SAFETY: the key outputs a `bool`.
        check("row_get wrap", unsafe {
            ffi::ghostty_row_get(
                packed,
                ffi::ROW_DATA_WRAP,
                ptr::from_mut(&mut wrapped).cast(),
            )
        })?;
        Ok(wrapped)
    }

    /// Reads the cell at `row`, `col` of the active screen.
    ///
    /// # Errors
    ///
    /// Returns a [`GhosttyError`] when libghostty rejects the position or a read.
    pub fn cell(&self, row: u16, col: u16) -> Result<CellInfo, GhosttyError> {
        let grid_ref = self.grid_ref(row, col)?;
        let mut packed: ffi::Packed = 0;
        // SAFETY: `grid_ref` was just obtained and the terminal is unchanged.
        check("grid_ref_cell", unsafe {
            ffi::ghostty_grid_ref_cell(&raw const grid_ref, &raw mut packed)
        })?;
        let mut wide: c_int = 0;
        // SAFETY: the key outputs a `GhosttyCellWide` (an int).
        check("cell_get wide", unsafe {
            ffi::ghostty_cell_get(packed, ffi::CELL_DATA_WIDE, ptr::from_mut(&mut wide).cast())
        })?;
        Ok(CellInfo {
            text: Self::graphemes(&grid_ref)?,
            wide: wide == ffi::CELL_WIDE_WIDE,
            spacer_tail: wide == ffi::CELL_WIDE_SPACER_TAIL,
            hyperlink: Self::hyperlink(&grid_ref)?,
        })
    }

    fn graphemes(grid_ref: &ffi::GridRef) -> Result<String, GhosttyError> {
        let mut codepoints = [0_u32; 32];
        let mut written = 0_usize;
        // SAFETY: `codepoints` holds `len` entries; `grid_ref` is current.
        let mut code = unsafe {
            ffi::ghostty_grid_ref_graphemes(
                grid_ref,
                codepoints.as_mut_ptr(),
                codepoints.len(),
                &raw mut written,
            )
        };
        let mut heap;
        let slice: &[u32] = if code == ffi::OUT_OF_SPACE {
            heap = vec![0_u32; written];
            // SAFETY: `heap` holds `written` entries as the call asked for.
            code = unsafe {
                ffi::ghostty_grid_ref_graphemes(
                    grid_ref,
                    heap.as_mut_ptr(),
                    heap.len(),
                    &raw mut written,
                )
            };
            &heap
        } else {
            &codepoints
        };
        check("grid_ref_graphemes", code)?;
        Ok(slice[..written]
            .iter()
            .map(|cp| char::from_u32(*cp).unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect())
    }

    fn hyperlink(grid_ref: &ffi::GridRef) -> Result<String, GhosttyError> {
        let mut needed = 0_usize;
        // SAFETY: a NULL buffer of length 0 asks only for the size.
        let code = unsafe {
            ffi::ghostty_grid_ref_hyperlink_uri(grid_ref, ptr::null_mut(), 0, &raw mut needed)
        };
        if code == ffi::SUCCESS {
            return Ok(String::new());
        }
        if code != ffi::OUT_OF_SPACE {
            return Err(GhosttyError {
                operation: "grid_ref_hyperlink_uri",
                code,
                name: code_name(code),
            });
        }
        let mut buf = vec![0_u8; needed];
        // SAFETY: `buf` holds `needed` bytes as the call asked for.
        check("grid_ref_hyperlink_uri", unsafe {
            ffi::ghostty_grid_ref_hyperlink_uri(
                grid_ref,
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut needed,
            )
        })?;
        Ok(String::from_utf8_lossy(&buf[..needed]).into_owned())
    }

    /// The visible text, without styling.
    ///
    /// # Errors
    ///
    /// Returns a [`GhosttyError`] when formatting fails.
    pub fn format_plain(&self) -> Result<Vec<u8>, GhosttyError> {
        self.format(ffi::FORMATTER_FORMAT_PLAIN, 0)
    }

    /// A byte stream that recreates the screen, with cursor, modes, styles,
    /// hyperlinks and keyboard state.
    ///
    /// # Errors
    ///
    /// Returns a [`GhosttyError`] when formatting fails.
    pub fn format_vt(&self) -> Result<Vec<u8>, GhosttyError> {
        self.format(ffi::FORMATTER_FORMAT_VT, ffi::FORMATTER_EXTRAS_ALL)
    }

    fn format(&self, format: c_int, extras: u32) -> Result<Vec<u8>, GhosttyError> {
        let mut formatter: ffi::Formatter = ptr::null_mut();
        // SAFETY: `handle` is live; a NULL allocator selects the default.
        check("formatter_new", unsafe {
            ffi::ghostty_formatter_terminal_new(ptr::null(), &raw mut formatter, self.handle)
        })?;
        let result = (|| {
            // SAFETY: `formatter` is live; EMIT takes the format enum, EXTRAS a `uint32_t`.
            check("formatter_set emit", unsafe {
                ffi::ghostty_formatter_set(
                    formatter,
                    ffi::FORMATTER_OPT_EMIT,
                    ptr::from_ref(&format).cast(),
                )
            })?;
            // SAFETY: as above.
            check("formatter_set extras", unsafe {
                ffi::ghostty_formatter_set(
                    formatter,
                    ffi::FORMATTER_OPT_EXTRAS,
                    ptr::from_ref(&extras).cast(),
                )
            })?;
            let mut out: *mut u8 = ptr::null_mut();
            let mut len = 0_usize;
            // SAFETY: the buffer is allocated by libghostty with the default allocator.
            check("formatter_format_alloc", unsafe {
                ffi::ghostty_formatter_format_alloc(
                    formatter,
                    ptr::null(),
                    &raw mut out,
                    &raw mut len,
                )
            })?;
            let bytes = if out.is_null() {
                Vec::new()
            } else {
                // SAFETY: `out` holds `len` bytes until freed below.
                unsafe { std::slice::from_raw_parts(out, len) }.to_vec()
            };
            // SAFETY: `out` came from `format_alloc` with the default allocator and length `len`.
            unsafe { ffi::ghostty_free(ptr::null(), out, len) };
            Ok(bytes)
        })();
        // SAFETY: `formatter` is live and not used afterwards.
        unsafe { ffi::ghostty_formatter_free(formatter) };
        result
    }
}

impl Drop for HeadlessTerminal {
    fn drop(&mut self) {
        // SAFETY: `handle` is live and owned; `state` is dropped after it, so
        // no callback can run on a freed state.
        unsafe { ffi::ghostty_terminal_free(self.handle) };
    }
}

/// The library version string reported at run time.
#[must_use]
pub fn version() -> String {
    // SAFETY: `ghostty_version` returns a static NUL-terminated string.
    unsafe { CStr::from_ptr(ffi::ghostty_version()) }
        .to_string_lossy()
        .into_owned()
}
