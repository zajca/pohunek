//! Native authentication orchestration with an injectable credential store.

use pohunek_relay_client::{config::Origin, Client};
use relay_protocol::{
    AccountRecord, CredentialId, CredentialKind, CredentialMutation, DeviceCredential,
    DeviceLoginStart, DevicePollResult, LoginId, PrincipalKind, PrincipalState,
    RotateCredentialRequest, Secret,
};
use std::time::Duration;
use tokio::time::{sleep, Instant};

use super::{
    config,
    store::{PendingRotation, Store},
    Error,
};

pub(super) trait Api {
    fn origin(&self) -> &Origin;
    async fn start(&self) -> Result<DeviceLoginStart, pohunek_relay_client::Error>;
    async fn poll(
        &self,
        id: &LoginId,
        secret: &Secret,
    ) -> Result<DevicePollResult, pohunek_relay_client::Error>;
    async fn account(
        &self,
        credential: &DeviceCredential,
    ) -> Result<AccountRecord, pohunek_relay_client::Error>;
    async fn revoke(
        &self,
        credential: &DeviceCredential,
    ) -> Result<(), pohunek_relay_client::Error>;
    async fn rotate(
        &self,
        credential: &DeviceCredential,
        target: CredentialId,
        request: &RotateCredentialRequest,
    ) -> Result<CredentialMutation, pohunek_relay_client::Error>;
    async fn revoke_owned(
        &self,
        credential: &DeviceCredential,
        target: CredentialId,
    ) -> Result<(), pohunek_relay_client::Error>;
}

impl Api for Client {
    async fn revoke_owned(
        &self,
        credential: &DeviceCredential,
        target: CredentialId,
    ) -> Result<(), pohunek_relay_client::Error> {
        self.revoke_owned(credential, target).await
    }
    fn origin(&self) -> &Origin {
        self.origin()
    }
    async fn start(&self) -> Result<DeviceLoginStart, pohunek_relay_client::Error> {
        self.start_device().await
    }
    async fn poll(
        &self,
        id: &LoginId,
        secret: &Secret,
    ) -> Result<DevicePollResult, pohunek_relay_client::Error> {
        self.poll_device(id, secret).await
    }
    async fn account(
        &self,
        credential: &DeviceCredential,
    ) -> Result<AccountRecord, pohunek_relay_client::Error> {
        self.account(credential).await
    }
    async fn revoke(
        &self,
        credential: &DeviceCredential,
    ) -> Result<(), pohunek_relay_client::Error> {
        self.revoke_self(credential).await
    }
    async fn rotate(
        &self,
        credential: &DeviceCredential,
        target: CredentialId,
        request: &RotateCredentialRequest,
    ) -> Result<CredentialMutation, pohunek_relay_client::Error> {
        self.rotate(credential, target, request).await
    }
}

pub(super) async fn login(
    api: &impl Api,
    store: &impl Store,
    display: impl FnOnce(&str, &str) -> Result<(), Error>,
) -> Result<(), Error> {
    if let Some(existing) = store.load(api.origin()).await? {
        match api.account(&existing).await {
            Ok(_account) => return Err(Error::AlreadyLoggedIn),
            Err(pohunek_relay_client::Error::Unauthenticated) => store.remove(api.origin()).await?,
            Err(error) => return Err(error.into()),
        }
    }
    let start = api.start().await?;
    display(&start.verification_uri, &start.user_code)?;
    let lifetime = Duration::try_from(start.expires_at - time::OffsetDateTime::now_utc())
        .map_err(|_error| Error::Expired)?;
    let deadline = Instant::now() + lifetime;
    let mut interval = Duration::from_secs(u64::from(start.interval_seconds));
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::Expired);
        }
        sleep(interval.min(remaining)).await;
        if Instant::now() >= deadline {
            return Err(Error::Expired);
        }
        match api.poll(&start.login_id, &start.poll_secret).await? {
            DevicePollResult::Pending {
                retry_after_seconds,
            }
            | DevicePollResult::SlowDown {
                retry_after_seconds,
            } => {
                interval = Duration::from_secs(u64::from(retry_after_seconds));
            }
            DevicePollResult::Complete { credential } => {
                let account = api.account(&credential).await;
                if !matches!(account, Ok(ref account) if human_account(account)) {
                    return reject_delivery(api, &credential).await;
                }
                return persist(api, store, &credential).await;
            }
            DevicePollResult::Denied => return Err(Error::Denied),
            DevicePollResult::Expired => return Err(Error::Expired),
            DevicePollResult::Cancelled => return Err(Error::Cancelled),
        }
    }
}

fn human_account(account: &AccountRecord) -> bool {
    account.state == PrincipalState::Active
        && matches!(
            account.kind,
            PrincipalKind::Human | PrincipalKind::Infrastructure
        )
}

async fn reject_delivery(api: &impl Api, credential: &DeviceCredential) -> Result<(), Error> {
    api.revoke(credential)
        .await
        .map_err(|_error| Error::DeliveryUnrevoked)?;
    Err(Error::AccountKind)
}

async fn persist(
    api: &impl Api,
    store: &impl Store,
    credential: &DeviceCredential,
) -> Result<(), Error> {
    if let Err(_error) = store.save(api.origin(), credential).await {
        return match api.revoke(credential).await {
            Ok(()) => Err(Error::StorageRevoked),
            Err(_error) => Err(Error::StorageUnrevoked),
        };
    }
    Ok(())
}

pub(super) async fn status(api: &impl Api, store: &impl Store) -> Result<AccountRecord, Error> {
    let credential = store.load(api.origin()).await?.ok_or(Error::NotLoggedIn)?;
    api.account(&credential).await.map_err(Into::into)
}

pub(super) async fn logout(api: &impl Api, store: &impl Store) -> Result<(), Error> {
    if let Some(credential) = store.load(api.origin()).await? {
        // Keep the exact credential available for retry when the relay is offline.
        api.revoke(&credential).await?;
        store.remove(api.origin()).await?;
    }
    Ok(())
}

pub(super) async fn rotate(
    api: &impl Api,
    store: &impl Store,
    plan: &super::RotationPlan,
) -> Result<(), Error> {
    rotate_with_clock(api, store, plan, time::OffsetDateTime::now_utc).await
}

async fn rotate_with_clock(
    api: &impl Api,
    store: &impl Store,
    plan: &super::RotationPlan,
    now: impl Fn() -> time::OffsetDateTime,
) -> Result<(), Error> {
    let current = store.load(api.origin()).await?.ok_or(Error::NotLoggedIn)?;
    let account = api.account(&current).await?;
    if !human_account(&account) {
        return Err(Error::AccountKind);
    }
    if store.pending(api.origin()).await?.is_some() {
        return Err(Error::RotationPending);
    }
    if current.expires_at - now() < config::MIN_REMAINING {
        return Err(Error::Lifetime);
    }
    let pending = PendingRotation {
        created_at: plan.created_at,
        principal_id: account.principal_id,
        credential_id: current.credential_id,
        request: plan.request.clone(),
    };
    store.save_pending(api.origin(), &pending).await?;
    // The platform keyring may wait for user interaction. No POST has been sent
    // yet, so abandoning this journal is safe if that wait consumed the budget.
    if current.expires_at - now() < config::MIN_REMAINING {
        store.clear_pending(api.origin()).await?;
        return Err(Error::Lifetime);
    }
    complete_rotation(api, store, &current, &account, &pending).await
}

pub(super) async fn resume_rotation(api: &impl Api, store: &impl Store) -> Result<(), Error> {
    let pending = store
        .pending(api.origin())
        .await?
        .ok_or(Error::NoPendingRotation)?;
    let current = store.load(api.origin()).await?.ok_or(Error::NotLoggedIn)?;
    let account = api.account(&current).await?;
    if !human_account(&account) || account.principal_id != pending.principal_id {
        return Err(Error::AccountKind);
    }
    complete_rotation(api, store, &current, &account, &pending).await
}

async fn complete_rotation(
    api: &impl Api,
    store: &impl Store,
    current: &DeviceCredential,
    account: &AccountRecord,
    pending: &PendingRotation,
) -> Result<(), Error> {
    let first = api
        .rotate(current, pending.credential_id, &pending.request)
        .await;
    let result = match first {
        Err(pohunek_relay_client::Error::Transport) => {
            api.rotate(current, pending.credential_id, &pending.request)
                .await
        }
        result => result,
    };
    let issued = match result {
        Ok(issued) => issued,
        Err(pohunek_relay_client::Error::RotationExpired) => {
            // The relay checks the exact receipt before declaring a never-issued
            // request expired. Local wall-clock expiry is never authoritative.
            store.clear_pending(api.origin()).await?;
            return Err(Error::Expired);
        }
        Err(pohunek_relay_client::Error::RotationRejected) => {
            store.clear_pending(api.origin()).await?;
            return Err(Error::RotationRejected);
        }
        Err(_error) => return Err(Error::RotationPending),
    };
    let Some(replacement) = issued.credential else {
        if issued.record.principal_id != account.principal_id
            || issued.record.kind != CredentialKind::Human
        {
            return Err(Error::RotationPending);
        }
        if issued.record.credential_id == current.credential_id {
            // Credential persistence succeeded before a crash clearing the journal.
            return store.clear_pending(api.origin()).await;
        }
        api.revoke_owned(current, issued.record.credential_id)
            .await
            .map_err(|_error| Error::RotationPending)?;
        store.clear_pending(api.origin()).await?;
        return Err(Error::DeliveryConsumed);
    };
    if issued.record.kind != CredentialKind::Human
        || issued.record.principal_id != account.principal_id
        || issued.record.revoked_at.is_some()
    {
        let result = reject_delivery(api, &replacement).await;
        if matches!(result, Err(Error::AccountKind)) {
            store.clear_pending(api.origin()).await?;
        }
        return result;
    }
    let replacement_account = api.account(&replacement).await;
    if !matches!(replacement_account, Ok(ref replacement_account)
        if human_account(replacement_account) && replacement_account.principal_id == account.principal_id)
    {
        let result = reject_delivery(api, &replacement).await;
        if matches!(result, Err(Error::AccountKind)) {
            store.clear_pending(api.origin()).await?;
        }
        return result;
    }
    let result = persist(api, store, &replacement).await;
    if matches!(result, Ok(()) | Err(Error::StorageRevoked)) {
        store.clear_pending(api.origin()).await?;
    }
    result
}

#[cfg(test)]
mod tests;
