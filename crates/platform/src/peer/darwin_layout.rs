//! Decodes the Darwin `xucred` peer record without touching the kernel.
//!
//! The record is mirrored rather than consumed through `libc` so its bounds,
//! version, and group-count checks compile and run on every host. The Darwin
//! backend pins the mirror against the real kernel type at build time.

// Rust guideline compliant 2026-09-22

/// Width of the kernel `cr_groups` array, `XU_NGROUPS` in `<sys/ucred.h>`.
const GROUP_SLOTS: usize = 16;

/// Only layout version the kernel defines, `XUCRED_VERSION` in `<sys/ucred.h>`.
///
/// `cru2x` stamps every record with it, so an answer carrying anything else did
/// not come from the interface this decoder understands.
const SUPPORTED_VERSION: u32 = 0;

/// `struct xucred` from `<sys/ucred.h>`, mirrored for host-neutral decoding.
///
/// The declaration mirrors the kernel field order and widths so the compiler
/// computes the offsets; it is 76 bytes with `cr_groups` at offset 12.
#[repr(C)]
#[derive(Clone, Copy)]
#[expect(
    clippy::struct_field_names,
    reason = "the kernel field names carry the layout and must stay transcribed exactly"
)]
pub(super) struct XuCred {
    /// Structure layout version.
    pub(super) cr_version: u32,
    /// Effective user id of the peer.
    pub(super) cr_uid: u32,
    /// Number of valid entries in `cr_groups`.
    pub(super) cr_ngroups: i16,
    /// Group ids, the first of which is the effective group.
    pub(super) cr_groups: [u32; GROUP_SLOTS],
}

impl XuCred {
    /// Returns an all-zero record for the kernel to fill.
    pub(super) const fn zeroed() -> Self {
        Self {
            cr_version: 0,
            cr_uid: 0,
            cr_ngroups: 0,
            cr_groups: [0; GROUP_SLOTS],
        }
    }
}

/// Owner identity decoded from one kernel peer record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Owner {
    /// Effective user id of the peer.
    pub(super) uid: u32,
    /// Effective group id of the peer.
    pub(super) gid: u32,
}

/// Rejected kernel peer record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LayoutError {
    /// The kernel wrote a different number of bytes than the record needs.
    Truncated,
    /// The record carries a layout version this decoder does not understand.
    UnsupportedVersion,
    /// The group count is zero or beyond the fixed array.
    InvalidGroupCount,
}

/// Decodes one kernel peer record into the owner identity it attests.
///
/// `written` is the byte count the kernel reported through `optlen`.
///
/// # Errors
///
/// Returns [`LayoutError`] when the record is short, carries an unknown
/// version, or reports a group count outside the fixed array. A record without
/// a group is rejected rather than answered with a fabricated group id.
pub(super) fn decode(record: &XuCred, written: usize) -> Result<Owner, LayoutError> {
    if written != size_of::<XuCred>() {
        return Err(LayoutError::Truncated);
    }
    if record.cr_version != SUPPORTED_VERSION {
        return Err(LayoutError::UnsupportedVersion);
    }
    let groups =
        usize::try_from(record.cr_ngroups).map_err(|_sign| LayoutError::InvalidGroupCount)?;
    if groups == 0 || groups > GROUP_SLOTS {
        return Err(LayoutError::InvalidGroupCount);
    }
    Ok(Owner {
        uid: record.cr_uid,
        gid: record.cr_groups[0],
    })
}

#[cfg(test)]
mod tests {
    use super::{decode, LayoutError, Owner, XuCred, GROUP_SLOTS};

    fn record(version: u32, uid: u32, ngroups: i16, first_group: u32) -> XuCred {
        let mut record = XuCred::zeroed();
        record.cr_version = version;
        record.cr_uid = uid;
        record.cr_ngroups = ngroups;
        record.cr_groups[0] = first_group;
        record
    }

    #[test]
    fn mirrored_record_matches_the_documented_kernel_layout() {
        assert_eq!(size_of::<XuCred>(), 76);
        assert_eq!(std::mem::offset_of!(XuCred, cr_groups), 12);
    }

    #[test]
    fn complete_record_yields_the_effective_owner() {
        let decoded = decode(&record(0, 501, 3, 20), size_of::<XuCred>()).expect("decode record");
        assert_eq!(decoded, Owner { uid: 501, gid: 20 });
    }

    #[test]
    fn short_answer_is_rejected() {
        let error = decode(&record(0, 501, 1, 20), size_of::<XuCred>() - 1)
            .expect_err("short record must be rejected");
        assert_eq!(error, LayoutError::Truncated);
    }

    #[test]
    fn unknown_version_is_rejected() {
        let error =
            decode(&record(1, 501, 1, 20), size_of::<XuCred>()).expect_err("version must match");
        assert_eq!(error, LayoutError::UnsupportedVersion);
    }

    #[test]
    fn group_count_outside_the_array_is_rejected() {
        for count in [0, -1, i16::MIN, 17, i16::MAX] {
            let error = decode(&record(0, 501, count, 20), size_of::<XuCred>())
                .expect_err("group count must be inside the array");
            assert_eq!(error, LayoutError::InvalidGroupCount, "count {count}");
        }
    }

    #[test]
    fn supplementary_groups_beyond_the_count_are_never_read() {
        let mut filled = record(0, 501, 1, 20);
        for slot in 1..GROUP_SLOTS {
            filled.cr_groups[slot] = 0xDEAD_BEEF;
        }
        let decoded = decode(&filled, size_of::<XuCred>()).expect("decode record");
        assert_eq!(decoded.gid, 20);
    }
}
