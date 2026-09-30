//! The one place where the outcome of settling a quarantined entry is decided.
//!
//! Removing or moving back a quarantined entry can end in more states than
//! success: the platform primitives keep an entry whose inode changed, report
//! where an entry really is after a failed move, or fail after the rename
//! committed. Every caller routes through these helpers, so no such outcome is
//! folded into "done". The primitives' outcome enums are `#[non_exhaustive]`,
//! so each match ends in a wildcard arm that fails closed (the entry is treated
//! as still in quarantine) and a new variant can never read as success.

use std::path::{Path, PathBuf};

use pohunek_platform::filesystem::{
    FsError, MoveOutcome, RemoveOutcome, StageOutcome, StagedEntry, TrustedDir,
};

/// How an attempt to settle one quarantined entry ended.
#[derive(Debug)]
pub(super) enum Settled {
    /// The entry is gone (removed) or back at its name.
    Done,
    /// The entry was removed, but the directory synchronization that makes the
    /// removal durable did not complete; no data remains in quarantine.
    Unconfirmed(String),
    /// The entry, or the data it holds, remains at `at`.
    Left {
        /// Where the entry really is now.
        at: PathBuf,
        /// Why it could not be settled.
        why: String,
    },
}

/// Where an entry a failed primitive was moving really is.
fn true_location(error: &FsError, staged: &StagedEntry) -> PathBuf {
    error
        .recovery_path()
        .map_or_else(|| staged.path(), Path::to_path_buf)
}

/// Deletes a quarantined entry.
pub(super) fn discard(staged: StagedEntry) -> Settled {
    let quarantine = staged.path();
    match staged.remove() {
        Ok(RemoveOutcome::Removed | RemoveOutcome::Missing) => Settled::Done,
        Ok(RemoveOutcome::IdentityChanged) => Settled::Left {
            at: quarantine,
            why: "the quarantined entry changed and was kept".to_owned(),
        },
        Err(error @ FsError::CommittedDurabilityUncertain { .. }) => {
            Settled::Unconfirmed(error.to_string())
        }
        Err(error) => Settled::Left {
            at: error.recovery_path().map_or(quarantine, Path::to_path_buf),
            why: error.to_string(),
        },
        Ok(_) => Settled::Left {
            at: quarantine,
            why: "an unrecognized removal outcome; the entry is treated as kept".to_owned(),
        },
    }
}

/// Moves a quarantined entry back to `name` without replacing anything.
pub(super) fn put_back(staged: &StagedEntry, name: &str) -> Settled {
    match staged.restore(name) {
        Ok(MoveOutcome::Moved) => Settled::Done,
        Ok(MoveOutcome::DestinationExists) => Settled::Left {
            at: staged.path(),
            why: "the destination was recreated".to_owned(),
        },
        Err(error) => Settled::Left {
            at: true_location(&error, staged),
            why: error.to_string(),
        },
        Ok(_) => Settled::Left {
            at: staged.path(),
            why: "an unrecognized restore outcome; the entry is treated as kept".to_owned(),
        },
    }
}

/// The result of moving an entry aside.
pub(super) enum Staging {
    /// The entry is quarantined and bound to its inode.
    Moved(StagedEntry),
    /// There was nothing to move.
    Missing,
    /// The entry changed or the quarantine name was taken; nothing moved.
    Refused,
}

/// Moves the entry `name` aside if it is still the inode `identity`.
///
/// # Errors
///
/// The primitive's error. When it carries a location (`recovery_path()`), the
/// entry stays there and the caller must report it.
pub(super) fn stage(
    dir: &TrustedDir,
    name: &str,
    prefix: &str,
    identity: pohunek_platform::filesystem::EntryIdentity,
) -> Result<Staging, FsError> {
    Ok(match dir.stage_random(name, prefix, identity)? {
        StageOutcome::Staged(staged) => Staging::Moved(staged),
        StageOutcome::Missing => Staging::Missing,
        // `IdentityChanged` and `DestinationExists` move nothing, and any variant
        // added later also refuses, so the caller aborts before mutating.
        _ => Staging::Refused,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use pohunek_platform::filesystem::EntryKind;

    use super::{discard, put_back, stage, Settled, Staging};
    use pohunek_platform::filesystem::TrustedDir;

    const FILE_MODE: u32 = 0o600;
    const DIR_MODE: u32 = 0o700;

    fn fixture() -> (tempfile::TempDir, TrustedDir) {
        let root = pohunek_test_support::tempdir().expect("fixture root");
        let dir = TrustedDir::open_absolute(root.path(), DIR_MODE).expect("open trusted root");
        (root, dir)
    }

    fn staged_file(dir: &TrustedDir) -> pohunek_platform::filesystem::StagedEntry {
        dir.create_file("entry", b"original", FILE_MODE)
            .expect("create");
        let identity = dir
            .entry_identity("entry", EntryKind::RegularFile)
            .expect("inspect")
            .expect("exists");
        match stage(dir, "entry", ".pohunek-move-fixture-", identity).expect("stage") {
            Staging::Moved(staged) => staged,
            _ => panic!("the entry must be quarantined"),
        }
    }

    #[test]
    fn discard_reports_a_kept_entry_and_where_it_is() {
        let (root, dir) = fixture();
        let staged = staged_file(&dir);
        let quarantine = staged.path();
        // The quarantined inode changes after it was staged, so the primitive
        // keeps it instead of deleting a different generation.
        fs::write(&quarantine, b"changed by someone else").expect("change in place");

        match discard(staged) {
            Settled::Left { at, .. } => assert_eq!(at, quarantine),
            other => panic!("a kept entry must be reported, got {other:?}"),
        }
        assert_eq!(
            fs::read(&quarantine).expect("the entry is kept"),
            b"changed by someone else"
        );
        drop(root);
    }

    #[test]
    fn put_back_reports_a_recreated_destination_and_where_the_entry_is() {
        let (root, dir) = fixture();
        let staged = staged_file(&dir);
        fs::write(root.path().join("entry"), b"recreated").expect("recreate the name");

        match put_back(&staged, "entry") {
            Settled::Left { at, .. } => assert_eq!(fs::read(&at).expect("entry"), b"original"),
            other => panic!("a collision must be reported, got {other:?}"),
        }
        assert_eq!(
            fs::read(root.path().join("entry")).expect("read"),
            b"recreated"
        );
    }

    #[test]
    fn a_settled_entry_is_done_and_a_changed_identity_refuses_to_stage() {
        let (root, dir) = fixture();
        let staged = staged_file(&dir);
        assert!(matches!(put_back(&staged, "entry"), Settled::Done));
        assert_eq!(
            fs::read(root.path().join("entry")).expect("restored"),
            b"original"
        );

        let identity = dir
            .entry_identity("entry", EntryKind::RegularFile)
            .expect("inspect")
            .expect("exists");
        // The replacement is created while the original still exists, so it
        // is guaranteed a different inode; deleting first would let the
        // filesystem hand the freed inode number straight back.
        fs::write(root.path().join("entry.swap"), b"another inode").expect("write replacement");
        fs::set_permissions(
            root.path().join("entry.swap"),
            std::os::unix::fs::PermissionsExt::from_mode(FILE_MODE),
        )
        .expect("chmod");
        fs::rename(root.path().join("entry.swap"), root.path().join("entry")).expect("swap in");
        assert!(matches!(
            stage(&dir, "entry", ".pohunek-move-fixture-", identity).expect("stage"),
            Staging::Refused
        ));
        assert_eq!(
            fs::read(root.path().join("entry")).expect("untouched"),
            b"another inode"
        );
    }
}
