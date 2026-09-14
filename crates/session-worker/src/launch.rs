//! Owns deferred launch claims until verification or bounded rejection.

// Rust guideline compliant 2026-09-14

use serde::Serialize;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::journal::{JournalRecord, PendingLaunchClaim, RuntimePhase};
use crate::WorkerError;

/// Launch-specific disposition, separate from acceptance of active identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LaunchClaimStatus {
    Accepted,
    Pending,
    Rejected,
    NotApplicable,
}

/// Records ownership before the caller durably commits and acknowledges it.
pub(crate) fn submit(
    journal: &mut JournalRecord,
    claim: PendingLaunchClaim,
    capacity: usize,
    mut verify: impl FnMut(&PendingLaunchClaim) -> Result<bool, WorkerError>,
) -> Result<LaunchClaimStatus, WorkerError> {
    if let Some(existing) = journal.launch_identity.as_ref() {
        return Ok(if existing == &claim.identity {
            LaunchClaimStatus::Accepted
        } else {
            LaunchClaimStatus::Rejected
        });
    }
    if let Some(existing) = journal.pending_launch_claims.iter().find(|existing| {
        existing.identity.process == claim.identity.process
            && existing.identity.provider == claim.identity.provider
    }) {
        // A later active report cannot replace or extend the first launch claim.
        return Ok(
            if existing.retry_pending && existing.identity == claim.identity {
                LaunchClaimStatus::Pending
            } else {
                LaunchClaimStatus::Rejected
            },
        );
    }
    match verify(&claim) {
        Ok(true) => {
            journal.launch_identity = Some(claim.identity);
            journal.pending_launch_claims.clear();
            Ok(LaunchClaimStatus::Accepted)
        }
        Ok(false) => Ok(LaunchClaimStatus::Rejected),
        Err(_) => {
            if journal.pending_launch_claims.len() >= capacity {
                return Err(WorkerError::Protocol(
                    "pending launch claim capacity exhausted".to_owned(),
                ));
            }
            journal.pending_launch_claims.push(claim);
            Ok(LaunchClaimStatus::Pending)
        }
    }
}

/// Retries only original, unexpired claims against their complete generations.
/// Returns whether durable state changed; observation errors retain ownership.
pub(crate) fn retry(
    journal: &mut JournalRecord,
    now: OffsetDateTime,
    mut verify: impl FnMut(&PendingLaunchClaim) -> Result<bool, WorkerError>,
) -> bool {
    let mut changed = false;
    let mut retained = Vec::with_capacity(journal.pending_launch_claims.len());
    for mut claim in std::mem::take(&mut journal.pending_launch_claims) {
        if !claim.retry_pending {
            retained.push(claim);
            continue;
        }
        let current = journal.phase == RuntimePhase::Live
            && journal.runtime_id.as_ref() == Some(&claim.runtime_id)
            && journal.child.as_ref() == Some(&claim.root)
            && OffsetDateTime::parse(&claim.expires_at, &Rfc3339).is_ok_and(|expiry| expiry > now);
        if !current || journal.launch_identity.is_some() {
            claim.retry_pending = false;
            retained.push(claim);
            changed = true;
            continue;
        }
        match verify(&claim) {
            Ok(true) => {
                journal.launch_identity = Some(claim.identity);
                changed = true;
            }
            Ok(false) => {
                claim.retry_pending = false;
                retained.push(claim);
                changed = true;
            }
            Err(_) => retained.push(claim),
        }
    }
    if journal.launch_identity.is_some() {
        retained.clear();
    }
    journal.pending_launch_claims = retained;
    changed
}

#[cfg(test)]
mod tests {
    use super::{retry, submit, LaunchClaimStatus};
    use crate::journal::{
        ChildIdentity, JournalRecord, LaunchIdentity, PendingLaunchClaim, RuntimePhase,
    };
    use crate::WorkerError;
    use time::{format_description::well_known::Rfc3339, OffsetDateTime};

    fn fixture() -> (JournalRecord, PendingLaunchClaim, OffsetDateTime) {
        let now = OffsetDateTime::UNIX_EPOCH;
        let root = ChildIdentity {
            pid: 10,
            process_group: 10,
            start_identity: "100".into(),
        };
        let mut journal = JournalRecord::bootstrap(
            "s-1".into(),
            "worker-1".into(),
            1,
            "1".into(),
            (1, 1),
            "now".into(),
        );
        journal.phase = RuntimePhase::Live;
        journal.runtime_id = Some("runtime-1".into());
        journal.child = Some(root.clone());
        let claim = PendingLaunchClaim {
            retry_pending: true,
            runtime_id: "runtime-1".into(),
            root: root.clone(),
            identity: LaunchIdentity {
                provider: "claude".into(),
                process: root,
                reference_kind: "id".into(),
                native_reference: "first-reference".into(),
            },
            expires_at: (now + time::Duration::seconds(60))
                .format(&Rfc3339)
                .unwrap(),
        };
        (journal, claim, now)
    }

    fn unavailable(_: &PendingLaunchClaim) -> Result<bool, WorkerError> {
        Err(WorkerError::Protocol("transient inspection failure".into()))
    }

    #[test]
    fn transient_error_retains_original_claim_until_later_success() {
        let (mut journal, claim, now) = fixture();
        assert_eq!(
            submit(&mut journal, claim.clone(), 1, unavailable).unwrap(),
            LaunchClaimStatus::Pending
        );
        assert!(journal.launch_identity.is_none());
        // The durable representation contains the full retry input, not an active-report pointer.
        let mut journal: JournalRecord =
            serde_json::from_slice(&serde_json::to_vec(&journal).unwrap()).unwrap();
        assert!(!retry(&mut journal, now, unavailable));
        let mut later = claim.clone();
        later.identity.native_reference = "later-reference".into();
        assert_eq!(
            submit(&mut journal, later.clone(), 1, |_| panic!(
                "must not replace pending claim"
            ))
            .unwrap(),
            LaunchClaimStatus::Rejected
        );
        assert!(retry(&mut journal, now, |pending| {
            assert_eq!(pending, &claim);
            Ok(true)
        }));
        assert_eq!(journal.launch_identity, Some(claim.identity));
        assert!(journal.pending_launch_claims.is_empty());
        assert_eq!(
            submit(&mut journal, later, 1, |_| panic!(
                "immutable launch must not be reprobed"
            ))
            .unwrap(),
            LaunchClaimStatus::Rejected
        );
    }

    #[test]
    fn expiration_runtime_and_root_changes_reject_without_inspection() {
        for mutation in 0..4 {
            let (mut journal, claim, now) = fixture();
            submit(&mut journal, claim, 1, unavailable).unwrap();
            match mutation {
                0 => journal.pending_launch_claims[0].expires_at = now.format(&Rfc3339).unwrap(),
                1 => journal.runtime_id = Some("replacement".into()),
                2 => journal.child.as_mut().unwrap().start_identity = "101".into(),
                3 => journal.phase = RuntimePhase::Terminal,
                _ => unreachable!(),
            }
            assert!(retry(&mut journal, now, |_| panic!(
                "stale claim must not be inspected"
            )));
            assert!(journal.launch_identity.is_none());
            assert!(!journal.pending_launch_claims[0].retry_pending);
        }
    }

    #[test]
    fn rejected_generation_or_authorization_cannot_become_launch() {
        let (mut journal, claim, now) = fixture();
        submit(&mut journal, claim.clone(), 1, unavailable).unwrap();
        assert!(retry(&mut journal, now, |_| Ok(false)));
        assert!(journal.launch_identity.is_none());
        assert!(!journal.pending_launch_claims[0].retry_pending);
        let mut replacement = claim;
        replacement.identity.native_reference = "replacement".into();
        assert_eq!(
            submit(&mut journal, replacement, 1, |_| panic!(
                "rejection fence must remain final"
            ))
            .unwrap(),
            LaunchClaimStatus::Rejected
        );
    }

    #[test]
    fn duplicate_does_not_extend_expiry_and_capacity_fails_before_acknowledgment() {
        let (mut journal, claim, _) = fixture();
        submit(&mut journal, claim.clone(), 1, unavailable).unwrap();
        let mut duplicate = claim.clone();
        duplicate.expires_at = "later".into();
        assert_eq!(
            submit(&mut journal, duplicate, 1, |_| panic!("already owned")).unwrap(),
            LaunchClaimStatus::Pending
        );
        assert_eq!(journal.pending_launch_claims, vec![claim.clone()]);
        let mut other = claim;
        other.identity.process.pid += 1;
        let error = submit(&mut journal, other, 1, unavailable).unwrap_err();
        assert!(error.to_string().contains("capacity exhausted"));
        assert_eq!(journal.pending_launch_claims.len(), 1);
    }
}
