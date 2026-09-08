use super::*;
use relay_protocol::{CredentialRecord, PrincipalId};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    },
};
use uuid::Uuid;

fn credential(id: u128) -> DeviceCredential {
    DeviceCredential {
        credential_id: CredentialId::from_uuid(Uuid::from_u128(id)),
        secret: Secret::new("sentinel-private-credential".to_owned()),
        expires_at: time::OffsetDateTime::now_utc() + time::Duration::hours(1),
    }
}

fn duplicate(value: &DeviceCredential) -> DeviceCredential {
    DeviceCredential {
        credential_id: value.credential_id,
        secret: Secret::new(value.secret.expose().to_owned()),
        expires_at: value.expires_at,
    }
}

#[derive(Default)]
struct MemoryStore {
    credential: Mutex<Option<DeviceCredential>>,
    fail_save: bool,
    pending: Mutex<Option<PendingRotation>>,
}

impl Store for MemoryStore {
    async fn pending(&self, _origin: &Origin) -> Result<Option<PendingRotation>, Error> {
        Ok(self.pending.lock().expect("pending rotation").clone())
    }
    async fn save_pending(&self, _origin: &Origin, pending: &PendingRotation) -> Result<(), Error> {
        *self.pending.lock().expect("pending rotation") = Some(pending.clone());
        Ok(())
    }
    async fn clear_pending(&self, _origin: &Origin) -> Result<(), Error> {
        *self.pending.lock().expect("pending rotation") = None;
        Ok(())
    }
    async fn load(&self, _origin: &Origin) -> Result<Option<DeviceCredential>, Error> {
        Ok(self
            .credential
            .lock()
            .expect("stored credential")
            .as_ref()
            .map(duplicate))
    }
    async fn save(&self, _origin: &Origin, credential: &DeviceCredential) -> Result<(), Error> {
        if self.fail_save {
            return Err(Error::Keyring);
        }
        *self.credential.lock().expect("stored credential") = Some(duplicate(credential));
        Ok(())
    }
    async fn remove(&self, _origin: &Origin) -> Result<(), Error> {
        *self.credential.lock().expect("stored credential") = None;
        Ok(())
    }
}

struct Relay {
    origin: Origin,
    polls: Mutex<VecDeque<DevicePollResult>>,
    poll_times: Mutex<Vec<Instant>>,
    revocations: Mutex<Vec<CredentialId>>,
    fail_revoke: AtomicBool,
    login_lifetime: time::Duration,
    rotation: Mutex<Option<CredentialMutation>>,
    rotation_targets: Mutex<Vec<CredentialId>>,
    account_kind: PrincipalKind,
    account_state: PrincipalState,
    rotation_requests: Mutex<Vec<RotateCredentialRequest>>,
    transport_failures: AtomicUsize,
    invalid_account: Option<CredentialId>,
    rotation_error: Mutex<Option<pohunek_relay_client::Error>>,
}

impl Relay {
    fn new(polls: Vec<DevicePollResult>) -> Self {
        Self {
            origin: Origin::parse("https://relay.example").expect("origin"),
            polls: Mutex::new(polls.into()),
            poll_times: Mutex::new(vec![]),
            revocations: Mutex::new(vec![]),
            fail_revoke: AtomicBool::new(false),
            login_lifetime: time::Duration::minutes(5),
            rotation: Mutex::new(None),
            rotation_targets: Mutex::new(vec![]),
            account_kind: PrincipalKind::Human,
            account_state: PrincipalState::Active,
            rotation_requests: Mutex::new(vec![]),
            transport_failures: AtomicUsize::new(0),
            invalid_account: None,
            rotation_error: Mutex::new(None),
        }
    }
}

impl Api for Relay {
    async fn revoke_owned(
        &self,
        _credential: &DeviceCredential,
        target: CredentialId,
        _request: &RevokeCredentialRequest,
    ) -> Result<(), pohunek_relay_client::Error> {
        self.revocations.lock().expect("revocations").push(target);
        Ok(())
    }
    fn origin(&self) -> &Origin {
        &self.origin
    }
    async fn start(&self) -> Result<DeviceLoginStart, pohunek_relay_client::Error> {
        Ok(DeviceLoginStart {
            login_id: LoginId::from_uuid(Uuid::nil()),
            verification_uri: "https://issuer.example/device".to_owned(),
            verification_uri_complete: None,
            user_code: "ABCD-EFGH".to_owned(),
            expires_at: time::OffsetDateTime::now_utc() + self.login_lifetime,
            interval_seconds: 1,
            poll_secret: Secret::new("sentinel-poll-secret".to_owned()),
        })
    }
    async fn poll(
        &self,
        _id: &LoginId,
        _secret: &Secret,
    ) -> Result<DevicePollResult, pohunek_relay_client::Error> {
        self.poll_times
            .lock()
            .expect("poll times")
            .push(Instant::now());
        self.polls
            .lock()
            .expect("polls")
            .pop_front()
            .ok_or(pohunek_relay_client::Error::Malformed)
    }
    async fn account(
        &self,
        credential: &DeviceCredential,
    ) -> Result<AccountRecord, pohunek_relay_client::Error> {
        if self.invalid_account == Some(credential.credential_id) {
            return Err(pohunek_relay_client::Error::Unauthenticated);
        }
        Ok(AccountRecord {
            principal_id: PrincipalId::from_uuid(Uuid::nil()),
            kind: self.account_kind.clone(),
            state: self.account_state.clone(),
            identities: vec![],
        })
    }
    async fn revoke(
        &self,
        credential: &DeviceCredential,
        _request: &RevokeCredentialRequest,
    ) -> Result<(), pohunek_relay_client::Error> {
        self.revocations
            .lock()
            .expect("revocations")
            .push(credential.credential_id);
        if self.fail_revoke.load(Ordering::SeqCst) {
            Err(pohunek_relay_client::Error::Transport)
        } else {
            Ok(())
        }
    }
    async fn rotate(
        &self,
        _credential: &DeviceCredential,
        target: CredentialId,
        request: &RotateCredentialRequest,
    ) -> Result<CredentialMutation, pohunek_relay_client::Error> {
        self.rotation_targets
            .lock()
            .expect("rotation targets")
            .push(target);
        self.rotation_requests
            .lock()
            .expect("rotation requests")
            .push(request.clone());
        if matches!(
            self.transport_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| remaining
                    .checked_sub(1)),
            Ok(_previous)
        ) {
            return Err(pohunek_relay_client::Error::Transport);
        }
        if let Some(error) = self.rotation_error.lock().expect("rotation error").take() {
            return Err(error);
        }
        self.rotation
            .lock()
            .expect("rotation result")
            .take()
            .ok_or(pohunek_relay_client::Error::Remote(409))
    }
}

fn rotation_metadata(id: u128, plan: &super::super::RotationPlan) -> CredentialRecord {
    CredentialRecord {
        credential_id: CredentialId::from_uuid(Uuid::from_u128(id)),
        principal_id: PrincipalId::from_uuid(Uuid::nil()),
        kind: CredentialKind::Human,
        issued_at: time::OffsetDateTime::now_utc(),
        expires_at: plan.request.expires_at,
        rotation_overlap_ends_at: None,
        last_used_at: None,
        revoked_at: None,
    }
}

#[tokio::test]
async fn lost_rotation_response_retries_exact_request_and_revokes_undelivered_secret() {
    for failures in [1, 2] {
        let api = Relay::new(vec![]);
        api.transport_failures.store(failures, Ordering::SeqCst);
        let request = super::super::rotation_request(3600, 60).expect("rotation request");
        *api.rotation.lock().expect("rotation") = Some(CredentialMutation {
            record: rotation_metadata(2, &request),
            credential: None,
        });
        let store = MemoryStore {
            credential: Mutex::new(Some(credential(1))),
            ..MemoryStore::default()
        };
        let first = rotate(&api, &store, &request).await;
        if failures == 2 {
            assert!(matches!(first, Err(Error::RotationPending)));
            assert_eq!(
                store
                    .pending(api.origin())
                    .await
                    .expect("pending")
                    .expect("journal retained")
                    .request,
                request.request
            );
            let another = super::super::rotation_request(7200, 60).expect("later command");
            assert!(matches!(
                rotate(&api, &store, &another).await,
                Err(Error::RotationPending)
            ));
            assert!(matches!(
                resume_rotation(&api, &store).await,
                Err(Error::DeliveryConsumed)
            ));
        } else {
            assert!(matches!(first, Err(Error::DeliveryConsumed)));
        }
        assert!(api
            .rotation_requests
            .lock()
            .expect("requests")
            .iter()
            .all(|sent| sent == &request.request));
        assert_eq!(
            *api.revocations.lock().expect("revocations"),
            [CredentialId::from_uuid(Uuid::from_u128(2))]
        );
        assert!(store
            .pending(api.origin())
            .await
            .expect("pending")
            .is_none());
        assert_eq!(
            store
                .load(api.origin())
                .await
                .expect("store")
                .expect("old credential retained")
                .credential_id,
            CredentialId::from_uuid(Uuid::from_u128(1))
        );
    }
}

#[tokio::test]
async fn rotation_restart_keeps_a_replacement_already_persisted_before_journal_cleanup() {
    let api = Relay::new(vec![]);
    let request = super::super::rotation_request(3600, 60).expect("rotation request");
    *api.rotation.lock().expect("rotation") = Some(CredentialMutation {
        record: rotation_metadata(2, &request),
        credential: None,
    });
    let store = MemoryStore {
        credential: Mutex::new(Some(credential(2))),
        pending: Mutex::new(Some(PendingRotation {
            created_at: request.created_at,
            principal_id: PrincipalId::from_uuid(Uuid::nil()),
            credential_id: CredentialId::from_uuid(Uuid::from_u128(1)),
            request: request.request.clone(),
        })),
        ..MemoryStore::default()
    };
    resume_rotation(&api, &store)
        .await
        .expect("resume completed persistence");
    assert!(api.revocations.lock().expect("revocations").is_empty());
    assert!(store
        .pending(api.origin())
        .await
        .expect("pending")
        .is_none());
}

#[tokio::test]
async fn unusable_replacement_is_revoked_without_overwriting_the_stored_credential() {
    let mut api = Relay::new(vec![]);
    let request = super::super::rotation_request(3600, 60).expect("rotation request");
    let replacement = credential(2);
    api.invalid_account = Some(replacement.credential_id);
    *api.rotation.lock().expect("rotation") = Some(CredentialMutation {
        record: rotation_metadata(2, &request),
        credential: Some(replacement),
    });
    let store = MemoryStore {
        credential: Mutex::new(Some(credential(1))),
        ..MemoryStore::default()
    };
    assert!(matches!(
        rotate(&api, &store, &request).await,
        Err(Error::AccountKind)
    ));
    assert_eq!(
        *api.revocations.lock().expect("revocations"),
        [CredentialId::from_uuid(Uuid::from_u128(2))]
    );
    assert_eq!(
        store
            .load(api.origin())
            .await
            .expect("store")
            .expect("retained credential")
            .credential_id,
        CredentialId::from_uuid(Uuid::from_u128(1))
    );
}

#[tokio::test]
async fn keyring_wait_cannot_consume_the_recovery_window_before_the_first_post() {
    let api = Relay::new(vec![]);
    let plan = super::super::rotation_request(3600, 60).expect("plan");
    let mut old = credential(1);
    old.expires_at = plan.created_at + config::MIN_REMAINING + time::Duration::seconds(1);
    let store = MemoryStore {
        credential: Mutex::new(Some(old)),
        ..MemoryStore::default()
    };
    let checks = AtomicUsize::new(0);
    let result = rotate_with_clock(&api, &store, &plan, || {
        plan.created_at
            + time::Duration::seconds(if checks.fetch_add(1, Ordering::SeqCst) == 0 {
                0
            } else {
                2
            })
    })
    .await;
    assert!(matches!(result, Err(Error::Lifetime)));
    assert!(api.rotation_requests.lock().expect("requests").is_empty());
    assert!(store
        .pending(api.origin())
        .await
        .expect("journal")
        .is_none());
}

#[tokio::test]
async fn client_clock_expiry_never_discards_an_unreconciled_rotation() {
    let api = Relay::new(vec![]);
    let mut plan = super::super::rotation_request(3600, 60).expect("plan");
    plan.created_at -= time::Duration::hours(2);
    plan.request.expires_at -= time::Duration::hours(2);
    *api.rotation.lock().expect("rotation") = Some(CredentialMutation {
        record: rotation_metadata(2, &plan),
        credential: None,
    });
    let store = MemoryStore {
        credential: Mutex::new(Some(credential(1))),
        pending: Mutex::new(Some(PendingRotation {
            created_at: plan.created_at,
            principal_id: PrincipalId::from_uuid(Uuid::nil()),
            credential_id: CredentialId::from_uuid(Uuid::from_u128(1)),
            request: plan.request.clone(),
        })),
        ..MemoryStore::default()
    };
    assert!(matches!(
        resume_rotation(&api, &store).await,
        Err(Error::DeliveryConsumed)
    ));
    assert_eq!(
        *api.revocations.lock().expect("revocations"),
        [CredentialId::from_uuid(Uuid::from_u128(2))]
    );
    assert!(store
        .pending(api.origin())
        .await
        .expect("pending")
        .is_none());
}

#[tokio::test]
async fn only_authoritative_no_commit_outcomes_clear_the_journal() {
    for (remote, cleared) in [
        (pohunek_relay_client::Error::RotationExpired, true),
        (pohunek_relay_client::Error::RotationRejected, true),
        (pohunek_relay_client::Error::Remote(410), false),
        (pohunek_relay_client::Error::Remote(400), false),
    ] {
        let api = Relay::new(vec![]);
        *api.rotation_error.lock().expect("rotation error") = Some(remote);
        let plan = super::super::rotation_request(3600, 120).expect("policy");
        let store = MemoryStore {
            credential: Mutex::new(Some(credential(1))),
            ..MemoryStore::default()
        };
        rotate(&api, &store, &plan)
            .await
            .expect_err("rotation rejected");
        assert_eq!(
            store
                .pending(api.origin())
                .await
                .expect("journal")
                .is_none(),
            cleared
        );
        assert!(api.revocations.lock().expect("revocations").is_empty());
        if cleared {
            let next = super::super::rotation_request(3600, 60).expect("shorter policy");
            *api.rotation.lock().expect("rotation") = Some(CredentialMutation {
                record: rotation_metadata(2, &next),
                credential: Some(credential(2)),
            });
            rotate(&api, &store, &next)
                .await
                .expect("new valid policy immediately usable");
        }
    }
}

#[tokio::test]
async fn failed_keyring_save_revokes_only_the_newly_issued_credential() {
    let api = Relay::new(vec![]);
    let store = MemoryStore {
        credential: Mutex::new(Some(credential(2))),
        fail_save: true,
        ..MemoryStore::default()
    };
    let issued = credential(1);
    assert!(matches!(
        persist(&api, &store, &issued).await,
        Err(Error::StorageRevoked)
    ));
    assert_eq!(
        *api.revocations.lock().expect("revocations"),
        [issued.credential_id]
    );
    assert_eq!(
        store
            .load(api.origin())
            .await
            .expect("store")
            .expect("old credential retained")
            .credential_id,
        CredentialId::from_uuid(Uuid::from_u128(2))
    );
}

#[tokio::test]
async fn failed_storage_and_failed_compensation_are_reported_without_secrets() {
    let api = Relay::new(vec![]);
    api.fail_revoke.store(true, Ordering::SeqCst);
    let store = MemoryStore {
        credential: Mutex::new(None),
        fail_save: true,
        ..MemoryStore::default()
    };
    let error = persist(&api, &store, &credential(1))
        .await
        .expect_err("both operations failed");
    assert!(matches!(error, Error::StorageUnrevoked));
    assert!(!format!("{error:?} {error}").contains("sentinel-private-credential"));
}

#[tokio::test]
async fn logout_retains_retry_credential_until_remote_revocation_succeeds() {
    let api = Relay::new(vec![]);
    api.fail_revoke.store(true, Ordering::SeqCst);
    let store = MemoryStore {
        credential: Mutex::new(Some(credential(1))),
        fail_save: false,
        ..MemoryStore::default()
    };
    logout(&api, &store).await.expect_err("offline relay");
    assert!(store.load(api.origin()).await.expect("store").is_some());
    api.fail_revoke.store(false, Ordering::SeqCst);
    logout(&api, &store).await.expect("retry succeeds");
    assert!(store.load(api.origin()).await.expect("store").is_none());
}

#[tokio::test(start_paused = true)]
async fn login_honors_initial_pending_and_slow_down_intervals() {
    let api = Relay::new(vec![
        DevicePollResult::Pending {
            retry_after_seconds: 2,
        },
        DevicePollResult::SlowDown {
            retry_after_seconds: 5,
        },
        DevicePollResult::Complete {
            credential: credential(1),
        },
    ]);
    let store = MemoryStore::default();
    let started = Instant::now();
    login(&api, &store, |uri, code| {
        assert_eq!(uri, "https://issuer.example/device");
        assert_eq!(code, "ABCD-EFGH");
        Ok(())
    })
    .await
    .expect("device login");
    let elapsed: Vec<_> = api
        .poll_times
        .lock()
        .expect("times")
        .iter()
        .map(|time| time.duration_since(started))
        .collect();
    assert_eq!(
        elapsed,
        [
            Duration::from_secs(1),
            Duration::from_secs(3),
            Duration::from_secs(8)
        ]
    );
    assert!(store.load(api.origin()).await.expect("store").is_some());
}

#[tokio::test(start_paused = true)]
async fn login_stops_at_its_monotonic_deadline_instead_of_sleeping_past_expiry() {
    let mut api = Relay::new(vec![DevicePollResult::Pending {
        retry_after_seconds: 60,
    }]);
    api.login_lifetime = time::Duration::seconds(3);
    let started = Instant::now();
    assert!(matches!(
        login(&api, &MemoryStore::default(), |_uri, _code| Ok(())).await,
        Err(Error::Expired)
    ));
    assert!(started.elapsed() <= Duration::from_secs(3));
    assert_eq!(api.poll_times.lock().expect("times").len(), 1);
}

#[tokio::test]
async fn login_cannot_overwrite_an_existing_authenticated_credential() {
    let api = Relay::new(vec![]);
    let store = MemoryStore {
        credential: Mutex::new(Some(credential(1))),
        fail_save: false,
        ..MemoryStore::default()
    };
    assert!(matches!(
        login(&api, &store, |_uri, _code| Ok(())).await,
        Err(Error::AlreadyLoggedIn)
    ));
    assert!(api.poll_times.lock().expect("times").is_empty());
}

#[tokio::test]
async fn rotation_replaces_only_after_delivery_and_compensates_failed_persistence() {
    for fail_save in [false, true] {
        let api = Relay::new(vec![]);
        let old = credential(1);
        let new = credential(2);
        let old_id = old.credential_id;
        let new_id = new.credential_id;
        let request = super::super::rotation_request(3600, 60).expect("rotation request");
        *api.rotation.lock().expect("rotation") = Some(CredentialMutation {
            record: CredentialRecord {
                credential_id: new_id,
                principal_id: PrincipalId::from_uuid(Uuid::nil()),
                kind: CredentialKind::Human,
                issued_at: time::OffsetDateTime::now_utc(),
                expires_at: new.expires_at,
                rotation_overlap_ends_at: None,
                last_used_at: None,
                revoked_at: None,
            },
            credential: Some(new),
        });
        let store = MemoryStore {
            credential: Mutex::new(Some(old)),
            fail_save,
            ..MemoryStore::default()
        };
        let result = rotate(&api, &store, &request).await;
        if fail_save {
            assert!(matches!(result, Err(Error::StorageRevoked)));
            assert_eq!(*api.revocations.lock().expect("revocations"), [new_id]);
        } else {
            result.expect("successful rotation");
            assert!(api.revocations.lock().expect("revocations").is_empty());
        }
        assert_eq!(
            *api.rotation_targets.lock().expect("rotation targets"),
            [old_id]
        );
        assert_eq!(
            store
                .load(api.origin())
                .await
                .expect("store")
                .expect("credential")
                .credential_id,
            if fail_save { old_id } else { new_id }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn terminal_login_outcomes_do_not_write_a_credential() {
    for outcome in [
        DevicePollResult::Denied,
        DevicePollResult::Expired,
        DevicePollResult::Cancelled,
    ] {
        let api = Relay::new(vec![outcome]);
        let store = MemoryStore::default();
        assert!(matches!(
            login(&api, &store, |_uri, _code| Ok(())).await,
            Err(Error::Denied | Error::Expired | Error::Cancelled)
        ));
        assert!(store.load(api.origin()).await.expect("store").is_none());
        assert!(api.revocations.lock().expect("revocations").is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn device_delivery_rejects_service_and_inactive_accounts_but_accepts_infrastructure() {
    for (kind, state, accepted) in [
        (PrincipalKind::Service, PrincipalState::Active, false),
        (PrincipalKind::Human, PrincipalState::Deprovisioned, false),
        (PrincipalKind::Infrastructure, PrincipalState::Active, true),
    ] {
        let issued = credential(1);
        let id = issued.credential_id;
        let mut api = Relay::new(vec![DevicePollResult::Complete { credential: issued }]);
        api.account_kind = kind;
        api.account_state = state;
        let store = MemoryStore::default();
        let result = login(&api, &store, |_uri, _code| Ok(())).await;
        if accepted {
            result.expect("human infrastructure credential");
            assert!(api.revocations.lock().expect("revocations").is_empty());
        } else {
            assert!(matches!(result, Err(Error::AccountKind)));
            assert_eq!(*api.revocations.lock().expect("revocations"), [id]);
        }
        assert_eq!(
            store.load(api.origin()).await.expect("store").is_some(),
            accepted
        );
    }
}

#[tokio::test]
async fn rotation_rejects_service_or_other_principal_delivery_without_overwriting_keyring() {
    for wrong_principal in [false, true] {
        let api = Relay::new(vec![]);
        let issued = credential(2);
        let id = issued.credential_id;
        *api.rotation.lock().expect("rotation") = Some(CredentialMutation {
            record: CredentialRecord {
                credential_id: id,
                principal_id: PrincipalId::from_uuid(if wrong_principal {
                    Uuid::from_u128(42)
                } else {
                    Uuid::nil()
                }),
                kind: if wrong_principal {
                    CredentialKind::Human
                } else {
                    CredentialKind::Service
                },
                issued_at: time::OffsetDateTime::now_utc(),
                expires_at: issued.expires_at,
                rotation_overlap_ends_at: None,
                last_used_at: None,
                revoked_at: None,
            },
            credential: Some(issued),
        });
        let store = MemoryStore {
            credential: Mutex::new(Some(credential(1))),
            fail_save: false,
            ..MemoryStore::default()
        };
        let request = super::super::rotation_request(3600, 60).expect("request");
        assert!(matches!(
            rotate(&api, &store, &request).await,
            Err(Error::AccountKind)
        ));
        assert_eq!(*api.revocations.lock().expect("revocations"), [id]);
        assert_eq!(
            store
                .load(api.origin())
                .await
                .expect("store")
                .expect("retained")
                .credential_id,
            CredentialId::from_uuid(Uuid::from_u128(1))
        );
    }
}
