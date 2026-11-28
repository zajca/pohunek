//! Hand-written `extern "C"` declarations of the libghostty-vt 1.0.2 functions
//! the spike uses, and the `repr(C)` mirrors of the structs they take.
//!
//! No binding generator is used and nothing is declared "for later". Each
//! signature follows `include/ghostty/vt/*.h` of the pinned source tarball.
//! Every mirrored struct is checked at compile time against the layout the
//! build script recorded from a C probe compiled for the build target
//! (`abi-layout.txt`).
//!
//! The functions are `unsafe` to call; [`crate::ghostty`] is the only caller
//! and owns the safety arguments.

// Rust guideline compliant 2026-06-26

#![allow(
    unsafe_code,
    reason = "this module declares the C ABI of libghostty-vt"
)]
#![allow(
    missing_debug_implementations,
    reason = "the repr(C) mirrors hold raw pointers and are never printed"
)]
#![allow(
    unreachable_pub,
    reason = "the declarations are public for the wrapper module; nothing outside the crate is meant to call them"
)]

use std::ffi::{c_char, c_int, c_void};
// `size_of` and `align_of` are in the prelude of this toolchain; `offset_of`
// is not.
use std::mem::offset_of;

#[allow(
    non_upper_case_globals,
    reason = "constants generated from the C probe"
)]
mod layout {
    include!(concat!(env!("OUT_DIR"), "/abi_layout.rs"));
}

/// `GhosttyResult`: 0 on success, negative on failure.
pub type GhosttyResult = c_int;
/// `GhosttyTerminal`: opaque handle.
pub type Terminal = *mut c_void;
/// `GhosttyFormatter`: opaque handle.
pub type Formatter = *mut c_void;
/// `GhosttyCell` and `GhosttyRow`: packed 64-bit values.
pub type Packed = u64;

pub const SUCCESS: GhosttyResult = 0;
pub const OUT_OF_MEMORY: GhosttyResult = -1;
pub const INVALID_VALUE: GhosttyResult = -2;
pub const OUT_OF_SPACE: GhosttyResult = -3;
pub const NO_VALUE: GhosttyResult = -4;
pub const IO_ERROR: GhosttyResult = -5;
pub const LIMIT_EXCEEDED: GhosttyResult = -6;
pub const REJECTED: GhosttyResult = -7;
pub const UNSUPPORTED: GhosttyResult = -8;

/// `GhosttyPointCoordinate`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PointCoordinate {
    pub x: u16,
    pub y: u32,
}

/// `GhosttyPointValue`.
#[repr(C)]
#[derive(Clone, Copy)]
pub union PointValue {
    pub coordinate: PointCoordinate,
    pub padding: [u64; 2],
}

/// `GhosttyPoint`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Point {
    pub tag: c_int,
    pub value: PointValue,
}

/// `GHOSTTY_POINT_TAG_ACTIVE`: coordinates in the active screen area.
pub const POINT_TAG_ACTIVE: c_int = 0;

/// `GhosttyGridRef`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GridRef {
    pub node: *mut c_void,
    pub x: u16,
    pub y: u16,
}

/// `GhosttyString`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyString {
    pub ptr: *const u8,
    pub len: usize,
}

/// `GhosttyClipboardContent`.
#[repr(C)]
pub struct ClipboardContent {
    pub mime: GhosttyString,
    pub data: GhosttyString,
}

/// `GhosttyClipboardWriteReply`.
#[repr(C)]
pub struct ClipboardWriteReply {
    pub size: usize,
    pub result: c_int,
    pub remember: bool,
}

/// `GhosttyClipboardWrite`.
#[repr(C)]
pub struct ClipboardWrite {
    pub location: c_int,
    pub contents: *const ClipboardContent,
    pub contents_len: usize,
    pub name: GhosttyString,
    pub granted: bool,
    pub can_remember: bool,
    pub ctx: *const c_void,
    pub reply: unsafe extern "C" fn(*const ClipboardWrite, *const ClipboardWriteReply),
}

/// `GhosttyTerminalProgressReport`.
#[repr(C)]
pub struct ProgressReport {
    pub state: c_int,
    pub progress: i8,
}

/// `GHOSTTY_CLIPBOARD_WRITE_RESULT_DENIED`.
pub const CLIPBOARD_WRITE_DENIED: c_int = 1;

pub type ClipboardWriteFn = unsafe extern "C" fn(Terminal, *mut c_void, *const ClipboardWrite);
pub type ProgressReportFn = unsafe extern "C" fn(Terminal, *mut c_void, *const ProgressReport);

// `GhosttyTerminalOption`.
pub const OPT_USERDATA: c_int = 1;
pub const OPT_APC_MAX_BYTES: c_int = 20;
pub const OPT_CLIPBOARD_WRITE: c_int = 27;
pub const OPT_SCROLLBACK_MAX_BYTES: c_int = 28;
pub const OPT_PROGRESS_REPORT: c_int = 31;
pub const OPT_CONTINUATION_MAX_BYTES: c_int = 32;
pub const OPT_UNKNOWN_MAX_BYTES: c_int = 37;
pub const OPT_CLIPBOARD_WRITE_MAX_BYTES: c_int = 40;
/// Compiled out of this build; setting it must return [`UNSUPPORTED`].
pub const OPT_KITTY_IMAGE_STORAGE_LIMIT: c_int = 16;

// `GhosttyTerminalData`.
pub const DATA_COLS: c_int = 1;
pub const DATA_ROWS: c_int = 2;
pub const DATA_CURSOR_X: c_int = 3;
pub const DATA_CURSOR_Y: c_int = 4;
pub const DATA_ACTIVE_SCREEN: c_int = 6;
pub const DATA_CURSOR_VISIBLE: c_int = 7;
pub const DATA_KITTY_KEYBOARD_FLAGS: c_int = 8;
pub const DATA_TITLE: c_int = 12;
pub const DATA_SCROLLBACK_ROWS: c_int = 15;

/// `GHOSTTY_TERMINAL_SCREEN_ALTERNATE`.
pub const SCREEN_ALTERNATE: c_int = 1;

// `GhosttyCellData` and `GhosttyRowData`.
pub const CELL_DATA_WIDE: c_int = 3;
pub const ROW_DATA_WRAP: c_int = 1;

// `GhosttyCellWide`.
pub const CELL_WIDE_WIDE: c_int = 1;
pub const CELL_WIDE_SPACER_TAIL: c_int = 2;

// `GhosttyBuildInfo`.
pub const BUILD_INFO_KITTY_GRAPHICS: c_int = 2;

// `GhosttyFormatterOption`, `GhosttyFormatterFormat` and the extras bits
// (`GhosttyFormatterExtras` of include/ghostty/vt/formatter.h, pinned 1.0.2).
pub const FORMATTER_OPT_EMIT: c_int = 1;
pub const FORMATTER_OPT_UNWRAP: c_int = 2;
pub const FORMATTER_OPT_TRIM: c_int = 3;
pub const FORMATTER_OPT_EXTRAS: c_int = 5;
pub const FORMATTER_FORMAT_PLAIN: c_int = 0;
pub const FORMATTER_FORMAT_VT: c_int = 1;
/// Restore the color palette with OSC 4.
pub const FORMATTER_EXTRA_PALETTE: u32 = 1 << 0;
/// Restore terminal modes that differ from their defaults with CSI h/l.
pub const FORMATTER_EXTRA_MODES: u32 = 1 << 1;
/// Restore the scrolling region with DECSTBM and DECSLRM.
pub const FORMATTER_EXTRA_SCROLLING_REGION: u32 = 1 << 2;
/// Restore tab stops.
pub const FORMATTER_EXTRA_TABSTOPS: u32 = 1 << 3;
/// Restore the working directory with OSC 7.
pub const FORMATTER_EXTRA_PWD: u32 = 1 << 4;
/// Restore keyboard modes such as modifyOtherKeys.
pub const FORMATTER_EXTRA_KEYBOARD: u32 = 1 << 5;
/// Restore the cursor position with CUP.
pub const FORMATTER_EXTRA_CURSOR: u32 = 1 << 6;
/// Restore the style the cursor writes new text with, using SGR.
pub const FORMATTER_EXTRA_STYLE: u32 = 1 << 7;
/// Restore the hyperlink the cursor writes new text with, using OSC 8.
pub const FORMATTER_EXTRA_HYPERLINK: u32 = 1 << 8;
/// Restore character protection with DECSCA.
pub const FORMATTER_EXTRA_PROTECTION: u32 = 1 << 9;
/// Restore the Kitty keyboard protocol flags.
pub const FORMATTER_EXTRA_KITTY_KEYBOARD: u32 = 1 << 10;
/// Restore the character sets, such as the line drawing set.
pub const FORMATTER_EXTRA_CHARSETS: u32 = 1 << 11;
pub const FORMATTER_EXTRAS_ALL: u32 = 0xFFF;
/// The extras the attach repaint emits: everything except the palette. A
/// repaint with the palette bit would send 256 OSC 4 sets on every attach and
/// leave the user's palette changed after detach (issue #2538 M14).
pub const FORMATTER_EXTRAS_ATTACH: u32 = FORMATTER_EXTRAS_ALL & !FORMATTER_EXTRA_PALETTE;

extern "C" {
    pub fn ghostty_version() -> *const c_char;
    pub fn ghostty_build_info(data: c_int, out: *mut c_void) -> GhosttyResult;
    pub fn ghostty_free(allocator: *const c_void, ptr: *mut u8, len: usize);

    pub fn ghostty_terminal_new(
        allocator: *const c_void,
        terminal: *mut Terminal,
        cols: u16,
        rows: u16,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_free(terminal: Terminal);
    pub fn ghostty_terminal_vt_write(terminal: Terminal, data: *const u8, len: usize);
    pub fn ghostty_terminal_resize(
        terminal: Terminal,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_set(
        terminal: Terminal,
        option: c_int,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_get(terminal: Terminal, data: c_int, out: *mut c_void)
        -> GhosttyResult;
    pub fn ghostty_terminal_grid_ref(
        terminal: Terminal,
        point: Point,
        out_ref: *mut GridRef,
    ) -> GhosttyResult;

    pub fn ghostty_grid_ref_cell(grid_ref: *const GridRef, out: *mut Packed) -> GhosttyResult;
    pub fn ghostty_grid_ref_row(grid_ref: *const GridRef, out: *mut Packed) -> GhosttyResult;
    pub fn ghostty_grid_ref_graphemes(
        grid_ref: *const GridRef,
        buf: *mut u32,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;
    pub fn ghostty_grid_ref_hyperlink_uri(
        grid_ref: *const GridRef,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;
    pub fn ghostty_cell_get(cell: Packed, data: c_int, out: *mut c_void) -> GhosttyResult;
    pub fn ghostty_row_get(row: Packed, data: c_int, out: *mut c_void) -> GhosttyResult;

    pub fn ghostty_formatter_terminal_new(
        allocator: *const c_void,
        formatter: *mut Formatter,
        terminal: Terminal,
    ) -> GhosttyResult;
    pub fn ghostty_formatter_set(
        formatter: Formatter,
        option: c_int,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_formatter_format_alloc(
        formatter: Formatter,
        allocator: *const c_void,
        out_ptr: *mut *mut u8,
        out_len: *mut usize,
    ) -> GhosttyResult;
    pub fn ghostty_formatter_free(formatter: Formatter);
}

// The mirrors above match the layout the C compiler computed for the target.
const _: () = {
    assert!(size_of::<Point>() == layout::POINT_SIZE);
    assert!(align_of::<Point>() == layout::POINT_ALIGN);
    assert!(offset_of!(Point, value) == layout::POINT_VALUE_OFFSET);
    assert!(size_of::<GridRef>() == layout::GRID_REF_SIZE);
    assert!(align_of::<GridRef>() == layout::GRID_REF_ALIGN);
    assert!(size_of::<GhosttyString>() == layout::STRING_SIZE);
    assert!(align_of::<GhosttyString>() == layout::STRING_ALIGN);
    assert!(size_of::<ClipboardContent>() == layout::CLIPBOARD_CONTENT_SIZE);
    assert!(align_of::<ClipboardContent>() == layout::CLIPBOARD_CONTENT_ALIGN);
    assert!(size_of::<ClipboardWrite>() == layout::CLIPBOARD_WRITE_SIZE);
    assert!(align_of::<ClipboardWrite>() == layout::CLIPBOARD_WRITE_ALIGN);
    assert!(offset_of!(ClipboardWrite, contents) == layout::CLIPBOARD_WRITE_CONTENTS_OFFSET);
    assert!(
        offset_of!(ClipboardWrite, contents_len) == layout::CLIPBOARD_WRITE_CONTENTS_LEN_OFFSET
    );
    assert!(offset_of!(ClipboardWrite, name) == layout::CLIPBOARD_WRITE_NAME_OFFSET);
    assert!(offset_of!(ClipboardWrite, granted) == layout::CLIPBOARD_WRITE_GRANTED_OFFSET);
    assert!(
        offset_of!(ClipboardWrite, can_remember) == layout::CLIPBOARD_WRITE_CAN_REMEMBER_OFFSET
    );
    assert!(offset_of!(ClipboardWrite, ctx) == layout::CLIPBOARD_WRITE_CTX_OFFSET);
    assert!(offset_of!(ClipboardWrite, reply) == layout::CLIPBOARD_WRITE_REPLY_OFFSET);
    assert!(size_of::<ClipboardWriteReply>() == layout::CLIPBOARD_WRITE_REPLY_SIZE);
    assert!(align_of::<ClipboardWriteReply>() == layout::CLIPBOARD_WRITE_REPLY_ALIGN);
    assert!(offset_of!(ClipboardWriteReply, result) == layout::CLIPBOARD_WRITE_REPLY_RESULT_OFFSET);
    assert!(
        offset_of!(ClipboardWriteReply, remember) == layout::CLIPBOARD_WRITE_REPLY_REMEMBER_OFFSET
    );
    assert!(size_of::<ProgressReport>() == layout::PROGRESS_REPORT_SIZE);
    assert!(align_of::<ProgressReport>() == layout::PROGRESS_REPORT_ALIGN);
    assert!(offset_of!(ProgressReport, progress) == layout::PROGRESS_REPORT_PROGRESS_OFFSET);
    assert!(size_of::<c_int>() == layout::ENUM_SIZE);
};
