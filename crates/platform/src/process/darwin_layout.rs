//! Decodes Darwin kernel process layouts without calling the operating system.
//!
//! The Darwin backend reads fixed C structures and one packed `KERN_PROCARGS2`
//! buffer. Keeping the decoding here makes region separation, bounds, and
//! integer conversions testable on every host, not only on macOS.

// Rust guideline compliant 2026-09-24

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use super::{OwnershipMarkers, Pid, StartIdentity};

/// Allowlisted daemon ownership marker.
pub(super) const ENV_DAEMON_ID: &str = "POHUNEK_DAEMON_ID";
/// Allowlisted session ownership marker.
pub(super) const ENV_SESSION_ID: &str = "POHUNEK_SESSION_ID";
/// Allowlisted worker runtime-generation ownership marker.
pub(super) const ENV_RUNTIME_ID: &str = "POHUNEK_RUNTIME_ID";

/// Width of the leading `KERN_PROCARGS2` argument count, written as a C `int`.
const ARGUMENT_COUNT_BYTES: usize = 4;

/// Caps the argument vector allocated from one kernel buffer.
///
/// Darwin enforces `kern.argmax` (1 MiB by default) on the whole buffer, so a
/// larger count can only come from a malformed region rather than a real exec.
const MAX_ARGUMENT_COUNT: usize = 4_096;

/// Caps the environment entries scanned for ownership markers.
///
/// The scan stops at the first entry beyond this bound instead of walking an
/// unbounded region that a malformed buffer could describe.
const MAX_ENVIRONMENT_ENTRIES: usize = 8_192;

/// Caps a retained ownership-marker value.
///
/// Pohunek markers are ULIDs and short opaque ids; a longer value means the
/// environment region was misparsed and must not become session metadata.
const MAX_MARKER_VALUE_BYTES: usize = 256;

/// Microsecond scale of the kernel process start timestamp.
const MICROSECONDS_PER_SECOND: u64 = 1_000_000;

/// Kernel sentinel in `proc_bsdinfo::e_tdev` for no controlling terminal.
const NO_CONTROLLING_DEVICE: u32 = u32::MAX;

/// Caps ancestry traversal depth so a corrupted parent graph cannot stall a scan.
pub(super) const MAX_DESCENDANT_DEPTH: usize = 64;

/// Caps the descendants collected from one traversal.
pub(super) const MAX_DESCENDANT_COUNT: usize = 4_096;

/// Failure of a kernel layout the operating system is expected to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LayoutError {
    /// Region boundaries, counts, or signs contradict the documented layout.
    Malformed,
    /// A native value cannot be represented by the shared contract.
    OutOfRange,
}

/// Distinct regions decoded from one `KERN_PROCARGS2` buffer.
///
/// The argument vector and the environment are separate regions of the same
/// buffer and are never merged: an environment value naming an agent is not
/// argument evidence, and an argument that looks like `KEY=VALUE` is not an
/// ownership marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NativeArguments {
    /// Executable path region, preserved as raw bytes.
    pub(super) executable: PathBuf,
    /// Argument region, exactly the kernel-reported argument count.
    pub(super) argv: Vec<String>,
    /// Allowlisted markers found only in the environment region.
    pub(super) markers: OwnershipMarkers,
}

/// Decodes one `KERN_PROCARGS2` buffer into its distinct regions.
///
/// # Errors
///
/// Returns [`LayoutError::Malformed`] when a region is truncated, the argument
/// count is negative or oversized, or a marker value exceeds its bound.
pub(super) fn parse_process_arguments(buffer: &[u8]) -> Result<NativeArguments, LayoutError> {
    let Some((count_region, rest)) = buffer.split_at_checked(ARGUMENT_COUNT_BYTES) else {
        return Err(LayoutError::Malformed);
    };
    let count_bytes: [u8; ARGUMENT_COUNT_BYTES] = count_region
        .try_into()
        .map_err(|_layout| LayoutError::Malformed)?;
    let argument_count =
        usize::try_from(i32::from_ne_bytes(count_bytes)).map_err(|_sign| LayoutError::Malformed)?;
    if argument_count > MAX_ARGUMENT_COUNT {
        return Err(LayoutError::Malformed);
    }

    let (executable, rest) = split_c_string(rest).ok_or(LayoutError::Malformed)?;
    if executable.is_empty() {
        return Err(LayoutError::Malformed);
    }

    // The kernel pads the executable path up to the saved stack alignment.
    let mut cursor = skip_nul_padding(rest);
    let mut argv = Vec::with_capacity(argument_count);
    for _ in 0..argument_count {
        let (argument, rest) = split_c_string(cursor).ok_or(LayoutError::Malformed)?;
        argv.push(String::from_utf8_lossy(argument).into_owned());
        cursor = rest;
    }

    Ok(NativeArguments {
        executable: PathBuf::from(OsString::from_vec(executable.to_vec())),
        argv,
        markers: collect_ownership_markers(skip_nul_padding(cursor))?,
    })
}

/// Collects allowlisted Pohunek markers from an environment region.
///
/// The first entry for a key wins, matching what `getenv` reports to the
/// process itself.
fn collect_ownership_markers(mut region: &[u8]) -> Result<OwnershipMarkers, LayoutError> {
    let mut markers = OwnershipMarkers::default();
    for _ in 0..MAX_ENVIRONMENT_ENTRIES {
        let Some((entry, rest)) = split_c_string(region) else {
            break;
        };
        region = rest;
        if entry.is_empty() {
            break;
        }
        let Some(separator) = entry.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let (key, value) = entry.split_at(separator);
        let slot = if key == ENV_DAEMON_ID.as_bytes() {
            &mut markers.daemon_id
        } else if key == ENV_SESSION_ID.as_bytes() {
            &mut markers.session_id
        } else if key == ENV_RUNTIME_ID.as_bytes() {
            &mut markers.runtime_id
        } else {
            continue;
        };
        let value = &value[1..];
        if value.len() > MAX_MARKER_VALUE_BYTES {
            return Err(LayoutError::Malformed);
        }
        if slot.is_none() {
            *slot = Some(String::from_utf8_lossy(value).into_owned());
        }
        if markers.daemon_id.is_some()
            && markers.session_id.is_some()
            && markers.runtime_id.is_some()
        {
            break;
        }
    }
    Ok(markers)
}

/// Splits one NUL-terminated string from the front of a region.
fn split_c_string(region: &[u8]) -> Option<(&[u8], &[u8])> {
    let terminator = region.iter().position(|byte| *byte == 0)?;
    Some((&region[..terminator], &region[terminator + 1..]))
}

/// Skips kernel alignment padding between two regions.
fn skip_nul_padding(region: &[u8]) -> &[u8] {
    let start = region
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(region.len());
    &region[start..]
}

/// Encodes a kernel process start time as an opaque same-boot identity.
///
/// # Errors
///
/// Returns [`LayoutError::Malformed`] for an out-of-scale microsecond field and
/// [`LayoutError::OutOfRange`] when the microsecond encoding overflows.
pub(super) fn encode_start_identity(
    seconds: u64,
    microseconds: u64,
) -> Result<StartIdentity, LayoutError> {
    if microseconds >= MICROSECONDS_PER_SECOND {
        return Err(LayoutError::Malformed);
    }
    seconds
        .checked_mul(MICROSECONDS_PER_SECOND)
        .and_then(|scaled| scaled.checked_add(microseconds))
        .map(StartIdentity::new)
        .ok_or(LayoutError::OutOfRange)
}

/// Encodes `kern.boottime` as an opaque boot identity value.
///
/// # Errors
///
/// Returns [`LayoutError::Malformed`] for a negative or out-of-scale timestamp.
pub(super) fn encode_boot_identity(seconds: i64, microseconds: i32) -> Result<String, LayoutError> {
    let seconds = u64::try_from(seconds).map_err(|_sign| LayoutError::Malformed)?;
    let microseconds = u64::try_from(microseconds).map_err(|_sign| LayoutError::Malformed)?;
    if microseconds >= MICROSECONDS_PER_SECOND {
        return Err(LayoutError::Malformed);
    }
    Ok(format!("{seconds}.{microseconds:06}"))
}

/// Resolves the controlling-terminal foreground group of one process.
///
/// A process without a controlling terminal reports the kernel `NODEV`
/// sentinel; the foreground group is independent of the observed process id.
pub(super) fn controlling_terminal_group(device: u32, foreground_group: u32) -> Option<Pid> {
    if device == NO_CONTROLLING_DEVICE || foreground_group == 0 {
        None
    } else {
        Some(foreground_group)
    }
}

/// Decodes a fixed-width kernel command-name field.
pub(super) fn decode_command_name(field: &[u8]) -> String {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Decodes a fixed-width kernel path field, preserving non-UTF-8 bytes.
///
/// # Errors
///
/// Returns [`LayoutError::Malformed`] when the field is unterminated or empty.
pub(super) fn decode_kernel_path(field: &[u8]) -> Result<PathBuf, LayoutError> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(LayoutError::Malformed)?;
    if end == 0 {
        return Err(LayoutError::Malformed);
    }
    Ok(PathBuf::from(OsString::from_vec(field[..end].to_vec())))
}

/// Walks descendants breadth-first under fixed depth and result bounds.
///
/// `children_of` is queried once per reached process. Processes that exit
/// during the walk simply contribute no children.
///
/// # Errors
///
/// Propagates the first `children_of` failure unchanged.
pub(super) fn bounded_descendants<E>(
    root: Pid,
    mut children_of: impl FnMut(Pid) -> Result<Vec<Pid>, E>,
) -> Result<Vec<Pid>, E> {
    let mut found = Vec::new();
    let mut seen = HashSet::from([root]);
    let mut queue = VecDeque::from([(root, 0_usize)]);
    while let Some((parent, depth)) = queue.pop_front() {
        if depth >= MAX_DESCENDANT_DEPTH {
            continue;
        }
        for child in children_of(parent)? {
            if found.len() >= MAX_DESCENDANT_COUNT {
                return Ok(found);
            }
            if seen.insert(child) {
                found.push(child);
                queue.push_back((child, depth + 1));
            }
        }
    }
    Ok(found)
}

/// Indexes a same-user process table by parent process id.
pub(super) fn children_by_parent(table: &[(Pid, Pid)]) -> HashMap<Pid, Vec<Pid>> {
    let mut index: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for &(pid, parent) in table {
        if pid != parent {
            index.entry(parent).or_default().push(pid);
        }
    }
    index
}

#[cfg(test)]
mod tests {
    use super::{
        bounded_descendants, children_by_parent, controlling_terminal_group, decode_command_name,
        decode_kernel_path, encode_boot_identity, encode_start_identity, parse_process_arguments,
        LayoutError, MAX_ARGUMENT_COUNT, MAX_DESCENDANT_COUNT, MAX_DESCENDANT_DEPTH,
        MAX_MARKER_VALUE_BYTES, NO_CONTROLLING_DEVICE,
    };
    use crate::process::Pid;
    use std::convert::Infallible;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    /// Builds a `KERN_PROCARGS2` buffer with the kernel's region order.
    fn procargs(
        argument_count: i32,
        executable: &[u8],
        padding: usize,
        argv: &[&[u8]],
        environment: &[&[u8]],
    ) -> Vec<u8> {
        let mut buffer = argument_count.to_ne_bytes().to_vec();
        buffer.extend_from_slice(executable);
        buffer.push(0);
        buffer.extend(std::iter::repeat_n(0_u8, padding));
        for argument in argv {
            buffer.extend_from_slice(argument);
            buffer.push(0);
        }
        for entry in environment {
            buffer.extend_from_slice(entry);
            buffer.push(0);
        }
        buffer.push(0);
        buffer
    }

    #[test]
    fn argument_and_environment_regions_stay_separate() {
        let buffer = procargs(
            2,
            b"/opt/homebrew/bin/node",
            7,
            &[b"node", b"POHUNEK_SESSION_ID=argv-not-a-marker"],
            &[
                b"PATH=/usr/bin",
                b"POHUNEK_SESSION_ID=01J0SESSION",
                b"EDITOR=claude",
            ],
        );

        let parsed = parse_process_arguments(&buffer).expect("kernel layout");

        assert_eq!(parsed.executable, PathBuf::from("/opt/homebrew/bin/node"));
        assert_eq!(
            parsed.argv,
            vec![
                "node".to_owned(),
                "POHUNEK_SESSION_ID=argv-not-a-marker".to_owned(),
            ]
        );
        assert_eq!(parsed.markers.session_id.as_deref(), Some("01J0SESSION"));
        assert_eq!(parsed.markers.daemon_id, None);
    }

    #[test]
    fn wrapper_argv_keeps_every_reported_argument() {
        let buffer = procargs(
            4,
            b"/opt/homebrew/bin/bun",
            3,
            &[b"bun", b"x", b"@anthropic-ai/claude-code", b"--resume"],
            &[b"POHUNEK_DAEMON_ID=01J0DAEMON"],
        );

        let parsed = parse_process_arguments(&buffer).expect("kernel layout");

        assert_eq!(parsed.argv.len(), 4);
        assert_eq!(parsed.argv[2], "@anthropic-ai/claude-code");
        assert_eq!(parsed.markers.daemon_id.as_deref(), Some("01J0DAEMON"));
        assert_eq!(parsed.markers.session_id, None);
    }

    #[test]
    fn login_shell_argument_zero_is_preserved() {
        let buffer = procargs(1, b"/bin/zsh", 1, &[b"-zsh"], &[]);

        let parsed = parse_process_arguments(&buffer).expect("kernel layout");

        assert_eq!(parsed.argv, vec!["-zsh".to_owned()]);
        assert!(!parsed.markers.is_marked());
    }

    #[test]
    fn non_utf8_executable_path_survives_decoding() {
        let raw = b"/Users/owner/\xff\xfe/agent";
        let buffer = procargs(1, raw, 0, &[b"agent"], &[]);

        let parsed = parse_process_arguments(&buffer).expect("kernel layout");

        assert_eq!(parsed.executable.as_os_str(), OsStr::from_bytes(raw));
    }

    #[test]
    fn empty_and_truncated_buffers_are_malformed() {
        for buffer in [
            Vec::new(),
            vec![0_u8; 3],
            // An argument count without any executable region.
            1_i32.to_ne_bytes().to_vec(),
            // A count that promises more arguments than the region holds.
            procargs(3, b"/bin/sh", 1, &[b"sh"], &[]),
        ] {
            assert_eq!(
                parse_process_arguments(&buffer),
                Err(LayoutError::Malformed),
                "buffer of {} bytes must be rejected",
                buffer.len()
            );
        }
    }

    #[test]
    fn negative_and_oversized_argument_counts_are_malformed() {
        let negative = procargs(-1, b"/bin/sh", 1, &[], &[]);
        assert_eq!(
            parse_process_arguments(&negative),
            Err(LayoutError::Malformed)
        );

        let oversized = procargs(
            i32::try_from(MAX_ARGUMENT_COUNT).expect("bound fits a C int") + 1,
            b"/bin/sh",
            1,
            &[],
            &[],
        );
        assert_eq!(
            parse_process_arguments(&oversized),
            Err(LayoutError::Malformed)
        );
    }

    #[test]
    fn an_empty_executable_region_is_malformed() {
        let buffer = procargs(0, b"", 0, &[], &[]);

        assert_eq!(
            parse_process_arguments(&buffer),
            Err(LayoutError::Malformed)
        );
    }

    #[test]
    fn an_oversized_marker_value_is_malformed() {
        let mut entry = b"POHUNEK_SESSION_ID=".to_vec();
        entry.extend(std::iter::repeat_n(b'a', MAX_MARKER_VALUE_BYTES + 1));
        let buffer = procargs(1, b"/bin/sh", 1, &[b"sh"], &[&entry]);

        assert_eq!(
            parse_process_arguments(&buffer),
            Err(LayoutError::Malformed)
        );
    }

    #[test]
    fn the_runtime_marker_is_read_from_the_environment_region_only() {
        let buffer = procargs(
            2,
            b"/bin/sh",
            1,
            &[b"sh", b"POHUNEK_RUNTIME_ID=argv-not-a-marker"],
            &[
                b"POHUNEK_RUNTIME_IDX=near-miss",
                b"POHUNEK_RUNTIME_ID=runtime-a",
                b"POHUNEK_RUNTIME_ID=runtime-b",
            ],
        );

        let parsed = parse_process_arguments(&buffer).expect("kernel layout");

        assert_eq!(parsed.markers.runtime_id.as_deref(), Some("runtime-a"));
        assert_eq!(parsed.markers.session_id, None);
        assert_eq!(parsed.markers.daemon_id, None);
        assert!(parsed.markers.is_marked());
    }

    #[test]
    fn an_oversized_runtime_marker_is_malformed() {
        let mut entry = b"POHUNEK_RUNTIME_ID=".to_vec();
        entry.extend(std::iter::repeat_n(b'r', MAX_MARKER_VALUE_BYTES + 1));
        let buffer = procargs(1, b"/bin/sh", 1, &[b"sh"], &[&entry]);

        assert_eq!(
            parse_process_arguments(&buffer),
            Err(LayoutError::Malformed)
        );
    }

    #[test]
    fn unrelated_environment_entries_are_not_retained() {
        let buffer = procargs(
            1,
            b"/bin/sh",
            1,
            &[b"sh"],
            &[b"AWS_SECRET_ACCESS_KEY=super-secret", b"HOME=/Users/owner"],
        );

        let parsed = parse_process_arguments(&buffer).expect("kernel layout");

        assert_eq!(parsed.markers, super::OwnershipMarkers::default());
        assert!(!format!("{parsed:?}").contains("super-secret"));
    }

    #[test]
    fn start_identity_encoding_is_checked() {
        assert_eq!(
            encode_start_identity(1_700_000_000, 123_456).expect("in range"),
            super::StartIdentity::new(1_700_000_000_123_456)
        );
        assert_eq!(
            encode_start_identity(1, 1_000_000),
            Err(LayoutError::Malformed)
        );
        assert_eq!(
            encode_start_identity(u64::MAX, 0),
            Err(LayoutError::OutOfRange)
        );
    }

    #[test]
    fn identical_start_times_compare_equal_across_callers() {
        let daemon_view = encode_start_identity(1_700_000_000, 5).expect("in range");
        let worker_view = encode_start_identity(1_700_000_000, 5).expect("in range");

        assert_eq!(daemon_view, worker_view);
        assert_eq!(daemon_view.to_string(), worker_view.to_string());
        assert_ne!(
            daemon_view,
            encode_start_identity(1_700_000_000, 6).expect("in range")
        );
    }

    #[test]
    fn boot_identity_encoding_rejects_negative_timestamps() {
        assert_eq!(
            encode_boot_identity(1_700_000_000, 42).expect("in range"),
            "1700000000.000042"
        );
        assert_eq!(encode_boot_identity(-1, 0), Err(LayoutError::Malformed));
        assert_eq!(
            encode_boot_identity(1, 1_000_000),
            Err(LayoutError::Malformed)
        );
    }

    #[test]
    fn foreground_group_is_independent_of_the_observed_process() {
        assert_eq!(controlling_terminal_group(0x0100_0002, 4_242), Some(4_242));
        assert_eq!(
            controlling_terminal_group(NO_CONTROLLING_DEVICE, 4_242),
            None
        );
        assert_eq!(controlling_terminal_group(0x0100_0002, 0), None);
    }

    #[test]
    fn kernel_string_fields_stop_at_the_terminator() {
        assert_eq!(decode_command_name(b"claude\0\0\0\0"), "claude");
        assert_eq!(decode_command_name(b"unterminated"), "unterminated");
        assert_eq!(decode_command_name(b"\0rest"), "");

        let mut path = b"/Users/owner/project".to_vec();
        path.resize(64, 0);
        assert_eq!(
            decode_kernel_path(&path).expect("terminated path"),
            PathBuf::from("/Users/owner/project")
        );
        assert_eq!(
            decode_kernel_path(b"/no/terminator"),
            Err(LayoutError::Malformed)
        );
        assert_eq!(decode_kernel_path(b"\0"), Err(LayoutError::Malformed));
    }

    #[test]
    fn non_utf8_kernel_path_survives_decoding() {
        // Darwin filesystems reject an invalid UTF-8 filename with `EILSEQ`, so
        // this is the only place the working-directory decoder can be held to a
        // byte sequence the kernel could still hand over.
        let raw = b"/Users/owner/agent-\xC3\x28";
        let mut field = raw.to_vec();
        field.resize(64, 0);

        let decoded = decode_kernel_path(&field).expect("terminated path");

        assert_eq!(decoded.as_os_str(), OsStr::from_bytes(raw));
    }

    #[test]
    fn descendant_traversal_bounds_depth_and_cycles() {
        let deep: Vec<(Pid, Pid)> = (1..=200).map(|pid| (pid + 1, pid)).collect();
        let index = children_by_parent(&deep);

        let found = bounded_descendants(1, |pid| {
            Ok::<_, Infallible>(index.get(&pid).cloned().unwrap_or_default())
        })
        .expect("infallible traversal");

        assert_eq!(found.len(), MAX_DESCENDANT_DEPTH);
        let deepest = Pid::try_from(MAX_DESCENDANT_DEPTH).expect("depth bound fits a pid");
        assert_eq!(found.last().copied(), Some(deepest + 1));
    }

    #[test]
    fn descendant_traversal_bounds_the_result_count() {
        let widest = Pid::try_from(MAX_DESCENDANT_COUNT).expect("count bound fits a pid");
        let wide: Vec<(Pid, Pid)> = (2..=(widest + 1_000)).map(|pid| (pid, 1)).collect();
        let index = children_by_parent(&wide);

        let found = bounded_descendants(1, |pid| {
            Ok::<_, Infallible>(index.get(&pid).cloned().unwrap_or_default())
        })
        .expect("infallible traversal");

        assert_eq!(found.len(), MAX_DESCENDANT_COUNT);
    }

    #[test]
    fn self_parented_and_repeated_entries_do_not_loop() {
        let table = [(1, 1), (2, 1), (3, 2), (2, 3)];
        let index = children_by_parent(&table);

        let found = bounded_descendants(1, |pid| {
            Ok::<_, Infallible>(index.get(&pid).cloned().unwrap_or_default())
        })
        .expect("infallible traversal");

        assert_eq!(found, vec![2, 3]);
    }

    #[test]
    fn traversal_failures_are_propagated_unchanged() {
        let result = bounded_descendants(1, |_pid| Err("permission denied"));

        assert_eq!(result, Err("permission denied"));
    }
}
