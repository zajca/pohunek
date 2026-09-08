//! Commits revocations and cancellation registrations atomically.

// Rust guideline compliant 2026-09-08

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use sqlx::{Postgres, Row, Transaction};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    authorization::{
        rbac::require_team_admin, valid_name, valid_permissions, valid_resource_kind,
        valid_team_name, verify_current_actor, CreateGrant, CreateGroup, CreateRole, CreateTeam,
        DisableTeam, GrantRecord, GrantSubject, GroupMemberChange, GroupRecord, MembershipChange,
        RemoveGrant, RemoveGroup, RemoveMember, RemoveRole, RoleAssignmentChange, RoleRecord,
        TeamRecord, UpdateGrant, UpdateGroup, UpdateRole, UpdateTeam,
    },
    recovery::{DenyIncident, RecoveryError, WitnessRecord, WitnessStore},
    store::{
        actor_kind_name, bounded_coordinate, lease::LeaseGuard, ActorContext, AuthorizationRequest,
        Store, StoreError,
    },
};

/// Bounded admission limits required before relay ingress can open.
#[derive(Debug, Clone, Copy)]
pub struct AuthorityLimits {
    pub global: usize,
    pub per_team: usize,
    pub per_principal: usize,
}

/// Coordinates durable authorization with active cancellation registrations.
#[derive(Debug)]
pub struct Authority {
    store: Store,
    lease: Arc<Mutex<LeaseGuard>>,
    lease_deadline: Arc<Mutex<Instant>>,
    witness: Arc<WitnessStore>,
    current: Arc<Mutex<WitnessRecord>>,
    limits: AuthorityLimits,
    registry: Arc<Mutex<Registry>>,
    closed: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    #[cfg(test)]
    admission_hook: Mutex<Option<AdmissionHook>>,
    #[cfg(test)]
    mutation_hook: Mutex<Option<AdmissionHook>>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct AdmissionHook {
    entered: Arc<tokio::sync::Barrier>,
    release: Arc<tokio::sync::Barrier>,
}

#[derive(Debug)]
struct Registry {
    next: u64,
    epoch: u64,
    mutations: usize,
    active: HashMap<u64, Active>,
}

/// Prevents an admission authorized against a pre-mutation snapshot from registering.
#[derive(Debug)]
struct MutationGate {
    registry: Arc<Mutex<Registry>>,
    closed: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    active: bool,
}

impl MutationGate {
    fn complete(mut self) -> Result<(), AuthorityError> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?;
        registry.mutations = registry
            .mutations
            .checked_sub(1)
            .ok_or(AuthorityError::Cancelled)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for MutationGate {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.failed.store(true, Ordering::Release);
        self.closed.store(true, Ordering::Release);
        if let Ok(mut registry) = self.registry.lock() {
            registry.mutations = registry.mutations.saturating_sub(1);
            for access in registry.active.values() {
                access.token.cancel();
            }
        }
    }
}
#[derive(Debug)]
struct Active {
    team_id: Uuid,
    principal_id: Uuid,
    authentication_id: Uuid,
    token: CancellationToken,
}

/// Holds one admitted request; dropping it returns all bounded capacity.
#[derive(Debug)]
pub struct AccessGuard {
    id: u64,
    request: AuthorizationRequest,
    store: Store,
    lease: Arc<Mutex<LeaseGuard>>,
    lease_deadline: Arc<Mutex<Instant>>,
    token: CancellationToken,
    registry: Arc<Mutex<Registry>>,
    closed: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthorityError {
    #[error("relay authority is unavailable")]
    Store(#[from] StoreError),
    #[error("relay recovery witness is unavailable")]
    Recovery(#[from] RecoveryError),
    #[error("relay admission capacity is exhausted")]
    Capacity,
    #[error("relay access was cancelled")]
    Cancelled,
    #[error("relay admission configuration is invalid")]
    InvalidLimits,
}

/// Couples an auth mutation's witnessed incident to its active-access scope.
#[derive(Debug, Clone, Copy)]
pub(crate) enum AuthMutationTarget {
    /// Replaces or revokes one credential without affecting sibling credentials.
    Credential(Uuid),
    /// Deprovisions one principal and all of its active authentication paths.
    Principal(Uuid),
}

impl AuthMutationTarget {
    const fn incident(self) -> DenyIncident {
        match self {
            Self::Credential(credential_id) => DenyIncident::Credential { credential_id },
            Self::Principal(principal_id) => DenyIncident::Principal { principal_id },
        }
    }
}

macro_rules! management_transaction {
    ($authority:expr, $actor:expr, $team_id:expr, $target:expr, $command:expr, $method:ident) => {{
        let management_team_id = $team_id;
        if $authority.closed.load(Ordering::Acquire) {
            return Err(AuthorityError::Cancelled);
        }
        let mut transaction = $authority.store.begin_serializable().await?;
        let lease = $authority
            .lease
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?
            .clone();
        if let Err(error) = $authority
            .store
            .verify_lease_in_transaction(&mut transaction, &lease)
            .await
        {
            $authority.close_all();
            return Err(error.into());
        }
        require_team_admin(&mut transaction, $actor, management_team_id).await?;
        if !($target)
            .exists(&mut transaction, management_team_id)
            .await?
        {
            return Err(AuthorityError::Store(StoreError::Forbidden));
        }
        let result = $authority
            .store
            .$method(&mut transaction, $actor, $command)
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                $authority.close_all();
                return Err(error.into());
            }
        };
        let mutation = $authority.begin_mutation()?;
        $authority.record_team_deny(management_team_id)?;
        #[cfg(test)]
        $authority.pause_mutation().await?;
        if let Err(error) = $authority
            .store
            .verify_lease_in_transaction(&mut transaction, &lease)
            .await
        {
            $authority.close_all();
            return Err(error.into());
        }
        if let Err(error) = $authority
            .verify_fence_in_transaction(&mut transaction)
            .await
        {
            $authority.close_all();
            return Err(error);
        }
        if let Err(error) = transaction.commit().await.map_err(StoreError::Database) {
            $authority.close_all();
            return Err(error.into());
        }
        mutation.complete()?;
        Ok(result)
    }};
}

impl Authority {
    /// Finalizes a prepared auth transaction without splitting its witness boundary.
    ///
    /// The caller must perform all predictable validation, idempotency receipt
    /// lookup, durable mutation, and audit insertion before calling this method.
    /// An exact receipt replay passes `changed = false`, preserving its current
    /// fence commit without creating a witness incident or cancelling access.
    pub(crate) async fn commit_auth_mutation(
        &self,
        mut transaction: Transaction<'_, Postgres>,
        changed: bool,
        target: Option<AuthMutationTarget>,
    ) -> Result<(), AuthorityError> {
        if !changed {
            if target.is_some() {
                return Err(AuthorityError::Store(StoreError::Forbidden));
            }
            self.verify_fence_in_transaction(&mut transaction).await?;
            return transaction
                .commit()
                .await
                .map_err(StoreError::Database)
                .map_err(Into::into);
        }
        let target = target.ok_or(AuthorityError::Store(StoreError::Forbidden))?;
        let mutation = self.begin_mutation()?;
        let next_witness = {
            let current = self
                .current
                .lock()
                .map_err(|_error| AuthorityError::Cancelled)?;
            match self
                .witness
                .record_deny_incident(&current, target.incident())
            {
                Ok(next) => next,
                Err(error) => {
                    self.close_all();
                    return Err(error.into());
                }
            }
        };
        if let Err(error) = self.cancel_auth_mutation(target) {
            self.close_all();
            return Err(error);
        }
        if let Ok(mut current) = self.current.lock() {
            *current = next_witness;
        } else {
            self.close_all();
            return Err(AuthorityError::Cancelled);
        }
        #[cfg(test)]
        self.pause_mutation().await?;
        if let Err(error) = self.verify_fence_in_transaction(&mut transaction).await {
            self.close_all();
            return Err(error);
        }
        if let Err(error) = transaction.commit().await.map_err(StoreError::Database) {
            self.close_all();
            return Err(error.into());
        }
        mutation.complete()
    }

    fn begin_mutation(&self) -> Result<MutationGate, AuthorityError> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?;
        if self.closed.load(Ordering::Acquire) || registry.mutations != 0 {
            return Err(AuthorityError::Cancelled);
        }
        registry.mutations = registry
            .mutations
            .checked_add(1)
            .ok_or(AuthorityError::Capacity)?;
        Ok(MutationGate {
            registry: Arc::clone(&self.registry),
            closed: Arc::clone(&self.closed),
            failed: Arc::clone(&self.failed),
            active: true,
        })
    }

    #[cfg(test)]
    async fn pause_mutation(&self) -> Result<(), AuthorityError> {
        let hook = self
            .mutation_hook
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?
            .clone();
        if let Some(hook) = hook {
            hook.entered.wait().await;
            hook.release.wait().await;
        }
        Ok(())
    }

    /// Creates a fail-closed authority with explicit nonzero capacity limits.
    pub fn new(
        store: Store,
        lease: LeaseGuard,
        witness: Arc<WitnessStore>,
        current: WitnessRecord,
        limits: AuthorityLimits,
    ) -> Result<Self, AuthorityError> {
        if limits.global == 0 || limits.per_team == 0 || limits.per_principal == 0 {
            return Err(AuthorityError::InvalidLimits);
        }
        Ok(Self {
            store,
            lease: Arc::new(Mutex::new(lease)),
            lease_deadline: Arc::new(Mutex::new(Instant::now() + LeaseGuard::duration())),
            witness,
            current: Arc::new(Mutex::new(current)),
            limits,
            registry: Arc::new(Mutex::new(Registry {
                next: 1,
                epoch: 1,
                mutations: 0,
                active: HashMap::new(),
            })),
            closed: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            admission_hook: Mutex::new(None),
            #[cfg(test)]
            mutation_hook: Mutex::new(None),
        })
    }

    /// Admits a request only after the current fence and durable permission succeed.
    pub async fn admit(
        &self,
        request: AuthorizationRequest,
    ) -> Result<AccessGuard, AuthorityError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(AuthorityError::Cancelled);
        }
        let observed_epoch = {
            let registry = self
                .registry
                .lock()
                .map_err(|_error| AuthorityError::Cancelled)?;
            if registry.mutations != 0 {
                return Err(AuthorityError::Cancelled);
            }
            registry.epoch
        };
        let lease = self
            .lease
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?
            .clone();
        if let Err(error) = self.store.validate_lease(&lease).await {
            self.close_all();
            return Err(error.into());
        }
        if let Err(error) = self
            .store
            .authorize_sensitive_with_lease(request.clone(), &lease)
            .await
        {
            if !matches!(error, StoreError::Forbidden) {
                self.close_all();
            }
            return Err(error.into());
        }
        #[cfg(test)]
        let hook = {
            self.admission_hook
                .lock()
                .map_err(|_error| AuthorityError::Cancelled)?
                .clone()
        };
        #[cfg(test)]
        if let Some(hook) = hook {
            hook.entered.wait().await;
            hook.release.wait().await;
        }
        let mut registry = self
            .registry
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?;
        if registry.mutations != 0 || registry.epoch != observed_epoch {
            return Err(AuthorityError::Cancelled);
        }
        let team = registry
            .active
            .values()
            .filter(|active| active.team_id == request.team_id)
            .count();
        let principal = registry
            .active
            .values()
            .filter(|active| active.principal_id == request.actor.principal_id())
            .count();
        if registry.active.len() >= self.limits.global
            || team >= self.limits.per_team
            || principal >= self.limits.per_principal
        {
            return Err(AuthorityError::Capacity);
        }
        let id = registry.next;
        registry.next = registry
            .next
            .checked_add(1)
            .ok_or(AuthorityError::Capacity)?;
        let token = CancellationToken::new();
        registry.active.insert(
            id,
            Active {
                team_id: request.team_id,
                principal_id: request.actor.principal_id(),
                authentication_id: request.actor.authentication_id(),
                token: token.clone(),
            },
        );
        Ok(AccessGuard {
            id,
            request,
            store: self.store.clone(),
            lease: Arc::clone(&self.lease),
            lease_deadline: Arc::clone(&self.lease_deadline),
            token,
            registry: Arc::clone(&self.registry),
            closed: Arc::clone(&self.closed),
            failed: Arc::clone(&self.failed),
        })
    }

    /// Revokes durable authority after recording an independent denial witness.
    ///
    /// The witness is intentionally committed while the `PostgreSQL` authorization
    /// locks remain held. Once it exists, a later database failure closes this
    /// process authority instead of acknowledging a revocation it cannot prove.
    pub async fn revoke(
        &self,
        command: RevocationCommand,
    ) -> Result<RevocationCommitted, AuthorityError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(AuthorityError::Cancelled);
        }
        let (mut transaction, command, policy_generation) =
            self.store.prepare_revocation(command).await?;
        let lease = self
            .lease
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?
            .clone();
        if let Err(error) = self
            .store
            .verify_lease_in_transaction(&mut transaction, &lease)
            .await
        {
            self.close_all();
            return Err(error.into());
        }
        let incident = deny_incident(&command)?;
        let mutation = self.begin_mutation()?;
        let next_witness = {
            let current = self
                .current
                .lock()
                .map_err(|_error| AuthorityError::Cancelled)?;
            match self.witness.record_deny_incident(&current, incident) {
                Ok(next) => next,
                Err(error) => {
                    self.close_all();
                    return Err(error.into());
                }
            }
        };
        if let Err(error) = self.cancel_revocation_scope(&command) {
            self.close_all();
            return Err(error);
        }
        if let Ok(mut current) = self.current.lock() {
            *current = next_witness;
        } else {
            self.close_all();
            return Err(AuthorityError::Cancelled);
        }
        #[cfg(test)]
        self.pause_mutation().await?;
        let committed =
            match persist_revocation(&mut transaction, &command, policy_generation).await {
                Ok(committed) => committed,
                Err(error) => {
                    self.close_all();
                    return Err(error.into());
                }
            };
        if let Err(error) = self.verify_fence_in_transaction(&mut transaction).await {
            self.close_all();
            return Err(error);
        }
        if let Err(error) = transaction.commit().await.map_err(StoreError::Database) {
            self.close_all();
            return Err(error.into());
        }
        mutation.complete()?;
        Ok(committed)
    }

    /// Updates team metadata through the fail-closed admission authority.
    pub async fn update_team(
        &self,
        actor: ActorContext,
        command: UpdateTeam,
    ) -> Result<TeamRecord, AuthorityError> {
        if let Some(name) = command.display_name.as_deref() {
            valid_team_name(name)?;
        }
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::Team,
            command,
            update_team_in_transaction
        )
    }

    /// Creates a team through the fenced infrastructure authority.
    pub async fn create_team(
        &self,
        actor: ActorContext,
        command: CreateTeam,
    ) -> Result<TeamRecord, AuthorityError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(AuthorityError::Cancelled);
        }
        if actor.kind() != crate::store::ActorKind::Infrastructure {
            return Err(AuthorityError::Store(StoreError::Forbidden));
        }
        valid_team_name(&command.display_name)?;
        let mut transaction = self.store.begin_serializable().await?;
        let lease = self
            .lease
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?
            .clone();
        if let Err(error) = self
            .store
            .verify_lease_in_transaction(&mut transaction, &lease)
            .await
        {
            self.close_all();
            return Err(error.into());
        }
        verify_current_actor(&mut transaction, actor).await?;
        let relay_id = self
            .current
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?
            .relay_id
            .clone();
        let record = match self
            .store
            .create_team_in_transaction(&mut transaction, actor, command)
            .await
        {
            Ok(record) => record,
            Err(error) => {
                self.close_all();
                return Err(error.into());
            }
        };
        let mutation = self.begin_mutation()?;
        self.record_deny_incident(DenyIncident::Global { relay_id }, None)?;
        #[cfg(test)]
        self.pause_mutation().await?;
        if let Err(error) = self
            .store
            .verify_lease_in_transaction(&mut transaction, &lease)
            .await
        {
            self.close_all();
            return Err(error.into());
        }
        if let Err(error) = transaction.commit().await.map_err(StoreError::Database) {
            self.close_all();
            return Err(error.into());
        }
        mutation.complete()?;
        Ok(record)
    }

    /// Disables a team through the fail-closed admission authority.
    pub async fn disable_team(
        &self,
        actor: ActorContext,
        command: DisableTeam,
    ) -> Result<TeamRecord, AuthorityError> {
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::Team,
            command,
            disable_team_in_transaction
        )
    }

    /// Changes membership through the fail-closed admission authority.
    pub async fn change_member(
        &self,
        actor: ActorContext,
        command: MembershipChange,
    ) -> Result<(), AuthorityError> {
        validate_membership_role(command.role)?;
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::Membership(command.principal_id, command.role),
            command,
            change_member_in_transaction
        )
    }

    /// Removes a member through the fail-closed admission authority.
    pub async fn remove_member(
        &self,
        actor: ActorContext,
        command: RemoveMember,
    ) -> Result<(), AuthorityError> {
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::Membership(command.principal_id, None),
            command,
            remove_member_in_transaction
        )
    }

    /// Removes a role through the fail-closed admission authority.
    pub async fn remove_role(
        &self,
        actor: ActorContext,
        command: RemoveRole,
    ) -> Result<(), AuthorityError> {
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::Role(command.role_id),
            command,
            remove_role_in_transaction
        )
    }

    /// Removes a grant through the fail-closed admission authority.
    pub async fn remove_grant(
        &self,
        actor: ActorContext,
        command: RemoveGrant,
    ) -> Result<(), AuthorityError> {
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::Grant(command.grant_id),
            command,
            remove_grant_in_transaction
        )
    }

    /// Creates a group through the fenced authority.
    pub async fn create_group(
        &self,
        actor: ActorContext,
        command: CreateGroup,
    ) -> Result<GroupRecord, AuthorityError> {
        valid_name(&command.display_name)?;
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::Team,
            command,
            create_group_in_transaction
        )
    }

    /// Updates a group through the fenced authority.
    pub async fn update_group(
        &self,
        actor: ActorContext,
        command: UpdateGroup,
    ) -> Result<GroupRecord, AuthorityError> {
        valid_name(&command.display_name)?;
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::Group(command.group_id),
            command,
            update_group_in_transaction
        )
    }

    /// Removes a group through the fenced authority.
    pub async fn remove_group(
        &self,
        actor: ActorContext,
        command: RemoveGroup,
    ) -> Result<(), AuthorityError> {
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::Group(command.group_id),
            command,
            remove_group_in_transaction
        )
    }

    /// Changes group membership through the fenced authority.
    pub async fn change_group_member(
        &self,
        actor: ActorContext,
        command: GroupMemberChange,
    ) -> Result<(), AuthorityError> {
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::GroupMembership(command.group_id, command.principal_id),
            command,
            change_group_member_in_transaction
        )
    }

    /// Creates a custom role through the fenced authority.
    pub async fn create_role(
        &self,
        actor: ActorContext,
        command: CreateRole,
    ) -> Result<RoleRecord, AuthorityError> {
        valid_name(&command.display_name)?;
        valid_permissions(&command.permissions)?;
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::Team,
            command,
            create_role_in_transaction
        )
    }

    /// Updates a custom role through the fenced authority.
    pub async fn update_role(
        &self,
        actor: ActorContext,
        command: UpdateRole,
    ) -> Result<RoleRecord, AuthorityError> {
        valid_name(&command.display_name)?;
        valid_permissions(&command.permissions)?;
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::Role(command.role_id),
            command,
            update_role_in_transaction
        )
    }

    /// Changes a custom-role assignment through the fenced authority.
    pub async fn change_role_assignment(
        &self,
        actor: ActorContext,
        command: RoleAssignmentChange,
    ) -> Result<(), AuthorityError> {
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::RoleAssignment(command.role_id, command.principal_id),
            command,
            change_role_assignment_in_transaction
        )
    }

    /// Creates a grant through the fenced authority.
    pub async fn create_grant(
        &self,
        actor: ActorContext,
        command: CreateGrant,
    ) -> Result<GrantRecord, AuthorityError> {
        validate_grant_input(&command)?;
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::GrantSubject(command.subject),
            command,
            create_grant_in_transaction
        )
    }

    /// Updates a grant through the fenced authority.
    pub async fn update_grant(
        &self,
        actor: ActorContext,
        command: UpdateGrant,
    ) -> Result<GrantRecord, AuthorityError> {
        validate_grant_input(&CreateGrant {
            team_id: command.team_id,
            subject: command.subject,
            resource_kind: command.resource_kind,
            resource_id: command.resource_id.clone(),
            permission: command.permission.clone(),
            correlation_id: command.correlation_id,
            idempotency_key: command.idempotency_key,
        })?;
        management_transaction!(
            self,
            actor,
            command.team_id,
            TeamTarget::GrantUpdate(command.grant_id, command.subject),
            command,
            update_grant_in_transaction
        )
    }

    /// Cancels all currently registered access for a policy-mutated team.
    pub fn invalidate_team(&self, team_id: Uuid) -> Result<(), AuthorityError> {
        self.record_team_deny(team_id)
    }

    fn record_team_deny(&self, team_id: Uuid) -> Result<(), AuthorityError> {
        self.record_deny_incident(DenyIncident::Team { team_id }, Some(team_id))
    }

    fn record_deny_incident(
        &self,
        incident: DenyIncident,
        affected_team: Option<Uuid>,
    ) -> Result<(), AuthorityError> {
        let mut current = self
            .current
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?;
        let next = match self.witness.record_deny_incident(&current, incident) {
            Ok(next) => next,
            Err(error) => {
                self.close_all();
                return Err(error.into());
            }
        };
        let registry = self
            .registry
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?;
        // Registry epoch is advanced before cancellation so admissions which
        // completed their database check before this point cannot register.
        let mut registry = registry;
        registry.epoch = registry
            .epoch
            .checked_add(1)
            .ok_or(AuthorityError::Capacity)?;
        if let Some(team_id) = affected_team {
            for active in registry
                .active
                .values()
                .filter(|active| active.team_id == team_id)
            {
                active.token.cancel();
            }
        }
        *current = next;
        Ok(())
    }

    /// Cancels every guard authenticated by the revoked credential.
    pub fn invalidate_credential(&self, credential_id: Uuid) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.epoch = registry.epoch.wrapping_add(1);
            for active in registry
                .active
                .values()
                .filter(|active| active.authentication_id == credential_id)
            {
                active.token.cancel();
            }
        }
    }

    /// Cancels every guard belonging to the revoked principal.
    pub fn invalidate_principal(&self, principal_id: Uuid) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.epoch = registry.epoch.wrapping_add(1);
            for active in registry
                .active
                .values()
                .filter(|active| active.principal_id == principal_id)
            {
                active.token.cancel();
            }
        }
    }

    /// Renews the fence once; failure permanently closes this process authority.
    pub async fn renew_once(&self) -> Result<(), AuthorityError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(AuthorityError::Cancelled);
        }
        let previous = self
            .lease
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?
            .clone();
        match self.store.renew_lease(&previous).await {
            Ok(renewed) => {
                *self
                    .lease
                    .lock()
                    .map_err(|_error| AuthorityError::Cancelled)? = renewed;
                *self
                    .lease_deadline
                    .lock()
                    .map_err(|_error| AuthorityError::Cancelled)? =
                    Instant::now() + LeaseGuard::duration();
                Ok(())
            }
            Err(error) => {
                self.close_all();
                Err(error.into())
            }
        }
    }

    /// Validates the currently held relay fence and closes on loss.
    pub async fn validate_fence(&self) -> Result<(), AuthorityError> {
        self.check_fence(false).await
    }

    /// Checks process ownership without consuming or sharing request capacity.
    pub(crate) async fn watch_fence(&self) -> Result<(), AuthorityError> {
        self.check_fence(true).await
    }

    async fn check_fence(&self, control: bool) -> Result<(), AuthorityError> {
        if Instant::now()
            >= *self
                .lease_deadline
                .lock()
                .map_err(|_error| AuthorityError::Cancelled)?
        {
            self.close_all();
            return Err(AuthorityError::Cancelled);
        }
        let lease = self
            .lease
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?
            .clone();
        let validation = if control {
            self.store.validate_control_lease(&lease).await
        } else {
            self.store.validate_lease(&lease).await
        };
        match validation {
            Ok(()) => Ok(()),
            Err(error) => {
                self.close_all();
                Err(error.into())
            }
        }
    }

    /// Verifies the exact relay fence inside a privileged mutation transaction.
    pub(crate) async fn verify_fence_in_transaction(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), AuthorityError> {
        if self.closed.load(Ordering::Acquire)
            || Instant::now()
                >= *self
                    .lease_deadline
                    .lock()
                    .map_err(|_error| AuthorityError::Cancelled)?
        {
            self.close_all();
            return Err(AuthorityError::Cancelled);
        }
        let lease = self
            .lease
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?
            .clone();
        match self
            .store
            .verify_lease_in_transaction(transaction, &lease)
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => {
                self.close_all();
                Err(error.into())
            }
        }
    }

    /// Starts orderly shutdown without clearing a dirty failure latch.
    pub fn begin_stop(&self) {
        self.close_ingress();
    }

    /// Records orderly shutdown after all admitted work has stopped.
    pub async fn release_clean(&self) -> Result<(), AuthorityError> {
        if self.failed.load(Ordering::Acquire) {
            return Err(AuthorityError::Cancelled);
        }
        self.begin_stop();
        if !self
            .registry
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?
            .active
            .is_empty()
        {
            return Err(AuthorityError::Cancelled);
        }
        let lease = self
            .lease
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?
            .clone();
        if let Err(error) = self.store.validate_control_lease(&lease).await {
            self.close_all();
            return Err(error.into());
        }
        if let Err(error) = self.store.release_lease(&lease).await {
            self.close_all();
            return Err(error.into());
        }
        let next = {
            let current = self
                .current
                .lock()
                .map_err(|_error| AuthorityError::Cancelled)?;
            match self.witness.end_run(&current) {
                Ok(next) => next,
                Err(error) => {
                    self.close_all();
                    return Err(error.into());
                }
            }
        };
        *self
            .current
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)? = next;
        Ok(())
    }

    /// Returns whether this process authority has permanently stopped ingress.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Closes ingress after a failure and preserves the dirty witness latch.
    pub fn close_all(&self) {
        self.failed.store(true, Ordering::Release);
        self.close_ingress();
    }

    fn close_ingress(&self) {
        self.closed.store(true, Ordering::Release);
        if let Ok(registry) = self.registry.lock() {
            for active in registry.active.values() {
                active.token.cancel();
            }
        }
    }

    fn cancel_revocation_scope(&self, command: &RevocationCommand) -> Result<(), AuthorityError> {
        let scope_id = match command.scope_kind {
            "credential" | "principal" => Some(parse_scope_uuid(command)?),
            "membership" | "team" | "policy" | "global" | "recovery" => None,
            _ => return Err(AuthorityError::Store(StoreError::Forbidden)),
        };
        let mut registry = self
            .registry
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?;
        // Advance before cancellation so an admission which passed PostgreSQL
        // validation cannot register after the durable denial marker.
        registry.epoch = registry
            .epoch
            .checked_add(1)
            .ok_or(AuthorityError::Capacity)?;
        for active in registry.active.values() {
            let affected = match command.scope_kind {
                "credential" => Some(active.authentication_id) == scope_id,
                "principal" => Some(active.principal_id) == scope_id,
                "membership" | "team" | "policy" => Some(active.team_id) == command.team_id,
                "global" | "recovery" => true,
                _ => return Err(AuthorityError::Store(StoreError::Forbidden)),
            };
            if affected {
                active.token.cancel();
            }
        }
        Ok(())
    }

    fn cancel_auth_mutation(&self, target: AuthMutationTarget) -> Result<(), AuthorityError> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?;
        // Advance before cancellation so an admission which passed PostgreSQL
        // validation cannot register its old authorization after the durable
        // deny incident was recorded.
        registry.epoch = registry
            .epoch
            .checked_add(1)
            .ok_or(AuthorityError::Capacity)?;
        for active in registry.active.values() {
            let affected = match target {
                AuthMutationTarget::Credential(credential_id) => {
                    active.authentication_id == credential_id
                }
                AuthMutationTarget::Principal(principal_id) => active.principal_id == principal_id,
            };
            if affected {
                active.token.cancel();
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum TeamTarget {
    Team,
    Membership(Uuid, Option<&'static str>),
    Group(Uuid),
    GroupMembership(Uuid, Uuid),
    Role(Uuid),
    RoleAssignment(Uuid, Uuid),
    Grant(Uuid),
    GrantSubject(GrantSubject),
    GrantUpdate(Uuid, GrantSubject),
}

impl TeamTarget {
    async fn exists(
        self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        team_id: Uuid,
    ) -> Result<bool, StoreError> {
        let found = match self {
            Self::Team => sqlx::query_scalar::<_, Uuid>(
                "SELECT team_id FROM teams WHERE team_id=$1 AND state='active' FOR SHARE",
            )
            .bind(team_id)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(StoreError::Database)?,
            Self::Membership(principal_id, role) => {
                let current = sqlx::query_scalar::<_, String>(
                    "SELECT builtin_role FROM memberships WHERE team_id=$1 AND principal_id=$2 AND state='active' FOR UPDATE",
                )
                .bind(team_id)
                .bind(principal_id)
                .fetch_optional(&mut **transaction)
                .await
                .map_err(StoreError::Database)?;
                if current.as_deref() == Some("owner") && role != Some("owner") {
                    let owners = sqlx::query_scalar::<_, Uuid>(
                        "SELECT principal_id FROM memberships WHERE team_id=$1 AND state='active' AND builtin_role='owner' ORDER BY principal_id FOR UPDATE",
                    )
                    .bind(team_id)
                    .fetch_all(&mut **transaction)
                    .await
                    .map_err(StoreError::Database)?;
                    if owners.len() <= 1 {
                        return Ok(false);
                    }
                }
                current.map(|_| principal_id)
            }
            Self::Group(group_id) => sqlx::query_scalar::<_, Uuid>(
                "SELECT group_id FROM groups WHERE team_id=$1 AND group_id=$2 AND state='active' FOR SHARE",
            )
            .bind(team_id)
            .bind(group_id)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(StoreError::Database)?,
            Self::GroupMembership(group_id, principal_id) => sqlx::query_scalar::<_, Uuid>(
                "SELECT g.group_id FROM groups g JOIN memberships m ON m.team_id=g.team_id WHERE g.team_id=$1 AND g.group_id=$2 AND g.state='active' AND m.principal_id=$3 AND m.state='active' FOR SHARE OF g,m",
            )
            .bind(team_id)
            .bind(group_id)
            .bind(principal_id)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(StoreError::Database)?,
            Self::Role(role_id) => sqlx::query_scalar::<_, Uuid>(
                "SELECT role_id FROM custom_roles WHERE team_id=$1 AND role_id=$2 AND state='active' FOR SHARE",
            )
            .bind(team_id)
            .bind(role_id)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(StoreError::Database)?,
            Self::RoleAssignment(role_id, principal_id) => sqlx::query_scalar::<_, Uuid>(
                "SELECT cr.role_id FROM custom_roles cr JOIN memberships m ON m.team_id=cr.team_id WHERE cr.team_id=$1 AND cr.role_id=$2 AND cr.state='active' AND m.principal_id=$3 AND m.state='active' FOR SHARE OF cr,m",
            )
            .bind(team_id)
            .bind(role_id)
            .bind(principal_id)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(StoreError::Database)?,
            Self::Grant(grant_id) => sqlx::query_scalar::<_, Uuid>(
                "SELECT grant_id FROM grants WHERE team_id=$1 AND grant_id=$2 AND state='active' FOR SHARE",
            )
            .bind(team_id)
            .bind(grant_id)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(StoreError::Database)?,
            Self::GrantSubject(subject) => {
                grant_subject_exists(transaction, team_id, subject).await?.then(|| Uuid::nil())
            }
            Self::GrantUpdate(grant_id, subject) => {
                let grant = sqlx::query_scalar::<_, Uuid>(
                    "SELECT grant_id FROM grants WHERE team_id=$1 AND grant_id=$2 AND state='active' FOR SHARE",
                )
                .bind(team_id)
                .bind(grant_id)
                .fetch_optional(&mut **transaction)
                .await
                .map_err(StoreError::Database)?;
                if grant.is_some() && grant_subject_exists(transaction, team_id, subject).await? {
                    grant
                } else {
                    None
                }
            }
        };
        Ok(found.is_some())
    }
}

async fn grant_subject_exists(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    team_id: Uuid,
    subject: GrantSubject,
) -> Result<bool, StoreError> {
    let (kind, subject_id) = match subject {
        GrantSubject::Group(group_id) => (
            "SELECT EXISTS (SELECT 1 FROM groups WHERE team_id=$1 AND group_id=$2 AND state='active')",
            group_id,
        ),
        GrantSubject::Principal(principal_id) => (
            "SELECT EXISTS (SELECT 1 FROM memberships m JOIN principals p ON p.id=m.principal_id WHERE m.team_id=$1 AND m.principal_id=$2 AND m.state='active' AND p.state='active' AND p.kind='human')",
            principal_id,
        ),
        GrantSubject::ServiceAccount(principal_id) => (
            "SELECT EXISTS (SELECT 1 FROM memberships m JOIN principals p ON p.id=m.principal_id WHERE m.team_id=$1 AND m.principal_id=$2 AND m.state='active' AND p.state='active' AND p.kind='service')",
            principal_id,
        ),
    };
    sqlx::query_scalar::<_, bool>(kind)
        .bind(team_id)
        .bind(subject_id)
        .fetch_one(&mut **transaction)
        .await
        .map_err(StoreError::Database)
}

fn parse_scope_uuid(command: &RevocationCommand) -> Result<Uuid, AuthorityError> {
    Uuid::parse_str(&command.scope_id)
        .map_err(|_error| AuthorityError::Store(StoreError::Forbidden))
}

fn deny_incident(command: &RevocationCommand) -> Result<DenyIncident, AuthorityError> {
    match command.scope_kind {
        "global" => Ok(DenyIncident::Global {
            relay_id: command.scope_id.clone(),
        }),
        "team" => Ok(DenyIncident::Team {
            team_id: command
                .team_id
                .ok_or(AuthorityError::Store(StoreError::Forbidden))?,
        }),
        "principal" => Ok(DenyIncident::Principal {
            principal_id: parse_scope_uuid(command)?,
        }),
        "credential" => Ok(DenyIncident::Credential {
            credential_id: parse_scope_uuid(command)?,
        }),
        "membership" => Ok(DenyIncident::Membership {
            membership_id: parse_scope_uuid(command)?,
        }),
        "policy" => Ok(DenyIncident::Policy {
            team_id: command
                .team_id
                .ok_or(AuthorityError::Store(StoreError::Forbidden))?,
        }),
        "recovery" => Ok(DenyIncident::Recovery {
            relay_id: command.scope_id.clone(),
        }),
        _ => Err(AuthorityError::Store(StoreError::Forbidden)),
    }
}

fn validate_membership_role(role: Option<&'static str>) -> Result<(), AuthorityError> {
    if matches!(role, None | Some("owner" | "admin" | "member")) {
        Ok(())
    } else {
        Err(AuthorityError::Store(StoreError::InputTooLarge))
    }
}

fn validate_grant_input(command: &CreateGrant) -> Result<(), AuthorityError> {
    valid_resource_kind(command.resource_kind)?;
    bounded_coordinate(&command.resource_id)?;
    bounded_coordinate(&command.permission)?;
    Ok(())
}

impl AccessGuard {
    /// Revalidates the fence and durable authorization before every downstream action.
    pub async fn validate(&self) -> Result<(), AuthorityError> {
        if self.closed.load(Ordering::Acquire)
            || self.token.is_cancelled()
            || Instant::now()
                >= *self
                    .lease_deadline
                    .lock()
                    .map_err(|_error| AuthorityError::Cancelled)?
        {
            return Err(AuthorityError::Cancelled);
        }
        let lease = self
            .lease
            .lock()
            .map_err(|_error| AuthorityError::Cancelled)?
            .clone();
        if let Err(error) = self.store.validate_lease(&lease).await {
            self.failed.store(true, Ordering::Release);
            self.closed.store(true, Ordering::Release);
            self.token.cancel();
            return Err(error.into());
        }
        if let Err(error) = self
            .store
            .authorize_sensitive_with_lease(self.request.clone(), &lease)
            .await
        {
            self.token.cancel();
            if !matches!(error, StoreError::Forbidden) {
                self.failed.store(true, Ordering::Release);
                self.closed.store(true, Ordering::Release);
            }
            return Err(error.into());
        }
        Ok(())
    }
    /// Returns cancellation state for idle monitors that poll no slower than their configured interval.
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Rechecks idle authorization no less frequently than the RFC 100 ms maximum.
    pub async fn monitor_idle(&self) -> Result<(), AuthorityError> {
        const IDLE_RECHECK: Duration = Duration::from_millis(100);
        loop {
            tokio::time::sleep(IDLE_RECHECK).await;
            self.validate().await?;
        }
    }
}

impl Drop for AccessGuard {
    fn drop(&mut self) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.active.remove(&self.id);
        }
    }
}

/// Names one revocation scope without admitting caller-provided authority.
#[derive(Debug, Clone)]
pub struct RevocationCommand {
    /// Authenticated administrator performing the mutation.
    pub actor: ActorContext,
    /// Optional affected team coordinate.
    pub team_id: Option<Uuid>,
    /// Fixed revocation scope name.
    pub scope_kind: &'static str,
    /// Opaque affected coordinate.
    pub scope_id: String,
    /// Stable non-secret reason code.
    pub reason_code: &'static str,
    /// Request correlation coordinate.
    pub correlation_id: Uuid,
}

/// Returns the durable cancellation sequence to dispatch after commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevocationCommitted {
    /// Append-only revocation coordinate.
    pub revocation_id: Uuid,
    /// Cancellation registration sequence.
    pub sequence: i64,
}

impl Store {
    async fn prepare_revocation(
        &self,
        command: RevocationCommand,
    ) -> Result<
        (
            sqlx::Transaction<'_, sqlx::Postgres>,
            RevocationCommand,
            i64,
        ),
        StoreError,
    > {
        validate_command(&command)?;
        let mut transaction = self.begin_reserved().await?;
        let command = canonicalize_command(&mut transaction, command).await?;
        verify_current_actor(&mut transaction, command.actor).await?;
        let recovery_generation = command.actor.recovery_generation();
        let policy_generation = if let Some(team_id) = command.team_id {
            let (generation, current_recovery) =
                require_team_admin(&mut transaction, command.actor, team_id).await?;
            if current_recovery != recovery_generation {
                return Err(StoreError::StaleState);
            }
            generation
        } else {
            if command.actor.kind() != crate::store::ActorKind::Infrastructure {
                return Err(StoreError::Forbidden);
            }
            1_i64
        };
        Ok((transaction, command, policy_generation))
    }
}

async fn persist_revocation(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    command: &RevocationCommand,
    policy_generation: i64,
) -> Result<RevocationCommitted, StoreError> {
    apply_revocation(transaction, command).await?;
    let revocation_id = Uuid::now_v7();
    sqlx::query("INSERT INTO revocations (revocation_id, scope_kind, scope_id, team_id, policy_generation, recovery_generation, reason_code, correlation_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(revocation_id).bind(command.scope_kind).bind(&command.scope_id).bind(command.team_id).bind(policy_generation).bind(command.actor.recovery_generation()).bind(command.reason_code).bind(command.correlation_id)
        .execute(&mut **transaction).await.map_err(StoreError::Database)?;
    let sequence: i64 = sqlx::query("INSERT INTO cancellation_outbox (revocation_id, scope_kind, scope_id, team_id) VALUES ($1,$2,$3,$4) RETURNING sequence")
        .bind(revocation_id).bind(command.scope_kind).bind(&command.scope_id).bind(command.team_id)
        .fetch_one(&mut **transaction).await.map_err(StoreError::Database)?.get("sequence");
    sqlx::query("INSERT INTO audit_events (audit_id, actor_principal_id, actor_kind, team_id, action, decision, policy_generation, recovery_generation, correlation_id, parameter_code, parameter_value, outcome) VALUES ($1,$2,$3,$4,'authority.revoke','revoke',$5,$6,$7,'scope',$8,'committed')")
        .bind(Uuid::now_v7()).bind(command.actor.principal_id()).bind(actor_kind_name(command.actor.kind())).bind(command.team_id).bind(policy_generation).bind(command.actor.recovery_generation()).bind(command.correlation_id).bind(command.scope_kind)
        .execute(&mut **transaction).await.map_err(|_error| StoreError::AuditUnavailable)?;
    Ok(RevocationCommitted {
        revocation_id,
        sequence,
    })
}

async fn canonicalize_command(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    mut command: RevocationCommand,
) -> Result<RevocationCommand, StoreError> {
    match command.scope_kind {
        "credential" | "principal" | "membership" => {
            command.scope_id = Uuid::parse_str(&command.scope_id)
                .map_err(|_error| StoreError::Forbidden)?
                .to_string();
        }
        "team" | "policy" => {
            let team_id = command.team_id.ok_or(StoreError::Forbidden)?;
            let requested =
                Uuid::parse_str(&command.scope_id).map_err(|_error| StoreError::Forbidden)?;
            if requested != team_id {
                return Err(StoreError::Forbidden);
            }
            command.scope_id = team_id.to_string();
        }
        "global" | "recovery" => {
            let relay_id = sqlx::query_scalar::<_, String>(
                "SELECT relay_id FROM relay_identity WHERE state = 'normal' AND recovery_generation = $1 FOR UPDATE",
            )
            .bind(command.actor.recovery_generation())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(StoreError::Database)?
            .ok_or(StoreError::Forbidden)?;
            if command.scope_id != relay_id {
                return Err(StoreError::Forbidden);
            }
            command.scope_id = relay_id;
        }
        _ => return Err(StoreError::Forbidden),
    }
    ensure_revocation_target(transaction, &command).await?;
    Ok(command)
}

async fn ensure_revocation_target(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    command: &RevocationCommand,
) -> Result<(), StoreError> {
    let found = match command.scope_kind {
        "credential" => sqlx::query_scalar::<_, Uuid>(
            "SELECT credential_id FROM relay_credentials WHERE credential_id = $1::uuid AND revoked_at IS NULL FOR SHARE",
        )
        .bind(&command.scope_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::Database)?
        .is_some(),
        "principal" => sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM principals WHERE id = $1::uuid AND state = 'active' FOR SHARE",
        )
        .bind(&command.scope_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::Database)?
        .is_some(),
        "membership" => sqlx::query_scalar::<_, Uuid>(
            "SELECT membership_id FROM memberships WHERE membership_id = $1::uuid AND team_id = $2 AND state = 'active' FOR SHARE",
        )
        .bind(&command.scope_id)
        .bind(command.team_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::Database)?
        .is_some(),
        "team" | "policy" => sqlx::query_scalar::<_, Uuid>(
            "SELECT team_id FROM teams WHERE team_id = $1 AND state = 'active' FOR SHARE",
        )
        .bind(command.team_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::Database)?
        .is_some(),
        "global" | "recovery" => true,
        _ => false,
    };
    if found {
        Ok(())
    } else {
        Err(StoreError::Forbidden)
    }
}

fn validate_command(command: &RevocationCommand) -> Result<(), StoreError> {
    if command.scope_id.is_empty() || command.scope_id.len() > 256 || command.reason_code.is_empty()
    {
        return Err(StoreError::InputTooLarge);
    }
    let team_scoped = matches!(command.scope_kind, "team" | "membership" | "policy");
    if team_scoped != command.team_id.is_some() {
        return Err(StoreError::Forbidden);
    }
    if !matches!(
        command.scope_kind,
        "global" | "team" | "principal" | "credential" | "membership" | "policy" | "recovery"
    ) {
        return Err(StoreError::Forbidden);
    }
    Ok(())
}

async fn apply_revocation(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    command: &RevocationCommand,
) -> Result<(), StoreError> {
    let affected = match command.scope_kind {
        "credential" => sqlx::query(
            "UPDATE relay_credentials SET revoked_at = clock_timestamp(), credential_generation = credential_generation + 1 WHERE credential_id = $1::uuid AND revoked_at IS NULL",
        )
        .bind(&command.scope_id)
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::Database)?,
        "principal" => {
            let principal_id = Uuid::parse_str(&command.scope_id).map_err(|_error| StoreError::Forbidden)?;
            ensure_effective_owner_remains(transaction, principal_id).await?;
            let principal = sqlx::query(
                "UPDATE principals SET state = 'deprovisioned', generation = generation + 1, deprovisioned_at = clock_timestamp() WHERE id = $1::uuid AND state = 'active'",
            )
            .bind(principal_id)
            .execute(&mut **transaction)
            .await
            .map_err(StoreError::Database)?;
            if principal.rows_affected() == 0 {
                return Err(StoreError::Forbidden);
            }
            sqlx::query("UPDATE relay_credentials SET revoked_at = clock_timestamp(), credential_generation = credential_generation + 1 WHERE principal_id = $1::uuid AND revoked_at IS NULL")
                .bind(principal_id).execute(&mut **transaction).await.map_err(StoreError::Database)?;
            sqlx::query("UPDATE browser_sessions SET revoked_at = clock_timestamp(), session_generation = session_generation + 1 WHERE principal_id = $1::uuid AND revoked_at IS NULL")
                .bind(principal_id).execute(&mut **transaction).await.map_err(StoreError::Database)?;
            sqlx::query("UPDATE memberships SET local_deny_generation = local_deny_generation + 1, revision = revision + 1 WHERE principal_id = $1::uuid AND state = 'active'")
                .bind(principal_id).execute(&mut **transaction).await.map_err(StoreError::Database)?;
            principal
        }
        "membership" => sqlx::query(
            "UPDATE memberships SET state = 'removed', local_deny_generation = local_deny_generation + 1, revision = revision + 1, removed_at = clock_timestamp() WHERE membership_id = $1::uuid AND team_id = $2 AND state = 'active'",
        )
        .bind(&command.scope_id)
        .bind(command.team_id)
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::Database)?,
        "team" | "policy" => {
            let state = if command.scope_kind == "team" { "disabled" } else { "active" };
            let team = sqlx::query(
                "UPDATE teams SET state = $1, policy_generation = policy_generation + 1, revision = revision + 1, updated_at = clock_timestamp() WHERE team_id = $2 AND state = 'active'",
            )
            .bind(state)
            .bind(command.team_id)
            .execute(&mut **transaction)
            .await
            .map_err(StoreError::Database)?;
            sqlx::query("UPDATE memberships SET local_deny_generation = local_deny_generation + 1, revision = revision + 1 WHERE team_id = $1 AND state = 'active'")
                .bind(command.team_id).execute(&mut **transaction).await.map_err(StoreError::Database)?;
            team
        }
        "global" | "recovery" => sqlx::query(
            "UPDATE relay_identity SET state = 'recovery_quarantine', revision = revision + 1, updated_at = clock_timestamp() WHERE relay_id = $1 AND state = 'normal'",
        )
        .bind(&command.scope_id)
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::Database)?,
        _ => return Err(StoreError::Forbidden),
    };
    if affected.rows_affected() == 0 {
        return Err(StoreError::Forbidden);
    }
    Ok(())
}

async fn ensure_effective_owner_remains(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    principal_id: Uuid,
) -> Result<(), StoreError> {
    let teams = sqlx::query_scalar::<_, Uuid>(
        "SELECT t.team_id FROM teams t WHERE t.state = 'active' AND EXISTS (SELECT 1 FROM memberships m WHERE m.team_id = t.team_id AND m.principal_id = $1 AND m.state = 'active' AND m.builtin_role = 'owner') ORDER BY t.team_id FOR UPDATE OF t",
    )
    .bind(principal_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(StoreError::Database)?;
    for team_id in teams {
        let owners = sqlx::query_scalar::<_, Uuid>(
            "SELECT m.principal_id FROM memberships m JOIN principals p ON p.id = m.principal_id WHERE m.team_id = $1 AND m.state = 'active' AND m.builtin_role = 'owner' AND p.state = 'active' ORDER BY m.principal_id FOR UPDATE OF m, p",
        )
        .bind(team_id)
        .fetch_all(&mut **transaction)
        .await
        .map_err(StoreError::Database)?;
        if owners.into_iter().all(|owner| owner == principal_id) {
            return Err(StoreError::Forbidden);
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "postgres-tests"))]
mod tests {
    use std::{env, os::unix::fs::PermissionsExt as _};

    use ed25519_dalek::SigningKey;
    use sqlx::AssertSqlSafe;
    use tempfile::TempDir;

    use super::*;
    use crate::store::{ActorKind, AuthenticationBinding};

    const BOOTSTRAP_CONNECTIONS: u32 = 1;
    const TEST_CONNECTIONS: u32 = 4;

    async fn seed_identity(store: &Store, principal: Uuid) {
        sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'https://issuer.test',$2,$1,1)")
            .bind(principal).bind(principal.to_string()).execute(store.pool()).await.expect("seed credential provenance");
    }

    async fn fixture() -> (Store, String, Uuid, Uuid, Uuid, Uuid) {
        let url = env::var("POHUNEK_RELAY_TEST_DATABASE_URL")
            .expect("postgres-tests requires POHUNEK_RELAY_TEST_DATABASE_URL");
        let bootstrap = Store::connect(&url, BOOTSTRAP_CONNECTIONS)
            .await
            .expect("connect PostgreSQL fixture");
        let schema = format!("relay_admission_{}", Uuid::now_v7().simple());
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(bootstrap.pool())
            .await
            .expect("create isolated schema");
        sqlx::raw_sql(AssertSqlSafe(format!(
            "SET search_path TO {schema};{};{}",
            include_str!("../../migrations/0001_relay_foundation.sql"),
            include_str!("../../migrations/0002_auth.sql")
        )))
        .execute(bootstrap.pool())
        .await
        .expect("apply relay migrations");
        let store = Store::connect(
            &format!("{url}?options[search_path]={schema}"),
            TEST_CONNECTIONS,
        )
        .await
        .expect("connect schema-scoped fixture");
        sqlx::query("INSERT INTO relay_identity (relay_id,recovery_generation,state,revision) VALUES ('test-relay',1,'normal',1)")
            .execute(store.pool()).await.expect("seed relay");
        let owner = Uuid::now_v7();
        let administrator = Uuid::now_v7();
        let owner_credential = Uuid::now_v7();
        let administrator_credential = Uuid::now_v7();
        for (principal, kind) in [(owner, "human"), (administrator, "infrastructure")] {
            sqlx::query(
                "INSERT INTO principals (id,kind,state,generation) VALUES ($1,$2,'active',1)",
            )
            .bind(principal)
            .bind(kind)
            .execute(store.pool())
            .await
            .expect("seed principal");
            seed_identity(&store, principal).await;
        }
        for (credential, principal, kind, digest) in [
            (owner_credential, owner, "human", "00"),
            (administrator_credential, administrator, "human", "11"),
        ] {
            sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,credential_kind,credential_generation,recovery_generation,expires_at,identity_id,digest_key_id,rotation_family_id) VALUES ($1,$2,decode(repeat($5,32),'hex'),$3,$4,1,1,clock_timestamp()+interval '1 hour',$3,'test',$1)")
                .bind(credential).bind(credential.to_string()).bind(principal).bind(kind).bind(digest).execute(store.pool()).await.expect("seed credential");
        }
        let team = Uuid::now_v7();
        sqlx::query("INSERT INTO teams (team_id,display_name,state,policy_generation,revision) VALUES ($1,'team','active',1,1)")
            .bind(team).execute(store.pool()).await.expect("seed team");
        sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'owner','active',1,1)")
            .bind(Uuid::now_v7()).bind(team).bind(owner).execute(store.pool()).await.expect("seed owner");
        (
            store,
            schema,
            owner,
            owner_credential,
            administrator,
            administrator_credential,
        )
    }

    fn actor(principal: Uuid, credential: Uuid, kind: ActorKind) -> ActorContext {
        Store::authenticated_actor(
            principal,
            credential,
            1,
            1,
            kind,
            AuthenticationBinding::Credential,
        )
    }

    async fn authority(store: Store, limits: AuthorityLimits) -> (Authority, TempDir) {
        let directory = tempfile::tempdir().expect("create witness directory");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("make witness directory private");
        let witness = Arc::new(
            WitnessStore::open(
                directory.path(),
                SigningKey::from_bytes(&[9; 32]),
                "test-key".to_owned(),
            )
            .expect("open witness"),
        );
        let current = witness
            .begin_run(None, "test-relay", 1)
            .expect("begin witness run");
        let lease = store
            .acquire_lease("test-relay", Uuid::now_v7(), 1)
            .await
            .expect("acquire lease");
        (
            Authority::new(store, lease, witness, current, limits).expect("create authority"),
            directory,
        )
    }

    async fn cleanup(store: &Store, schema: &str) {
        sqlx::query(AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(store.pool())
            .await
            .expect("drop schema");
    }

    #[tokio::test]
    async fn credential_revocation_cancels_guard_and_records_deny_witness() {
        let (store, schema, owner, owner_credential, administrator, administrator_credential) =
            fixture().await;
        let team: Uuid = sqlx::query_scalar("SELECT team_id FROM teams")
            .fetch_one(store.pool())
            .await
            .expect("read team");
        let (authority, _directory) = authority(
            store.clone(),
            AuthorityLimits {
                global: 2,
                per_team: 2,
                per_principal: 2,
            },
        )
        .await;
        let guard = authority
            .admit(AuthorizationRequest {
                actor: actor(owner, owner_credential, ActorKind::Human),
                team_id: team,
                permission: "session.metadata.read".to_owned(),
                resource: crate::store::ResourceScope::Team,
                correlation_id: Uuid::now_v7(),
            })
            .await
            .expect("admit owner");
        authority
            .revoke(RevocationCommand {
                actor: actor(
                    administrator,
                    administrator_credential,
                    ActorKind::Infrastructure,
                ),
                team_id: None,
                scope_kind: "credential",
                scope_id: format!("{{{}}}", owner_credential.to_string().to_uppercase()),
                reason_code: "operator_revoke",
                correlation_id: Uuid::now_v7(),
            })
            .await
            .expect("revoke credential");
        assert!(guard.cancelled());
        assert!(matches!(
            guard.validate().await,
            Err(AuthorityError::Cancelled)
        ));
        let marker: i64 = sqlx::query_scalar("SELECT count(*) FROM revocations WHERE scope_id=$1")
            .bind(owner_credential.to_string())
            .fetch_one(store.pool())
            .await
            .expect("read marker");
        assert_eq!(marker, 1);
        cleanup(&store, &schema).await;
    }

    #[tokio::test]
    async fn unauthorized_revocation_has_no_marker_or_guard_effect() {
        let (store, schema, owner, owner_credential, _administrator, _administrator_credential) =
            fixture().await;
        let (authority, _directory) = authority(
            store.clone(),
            AuthorityLimits {
                global: 2,
                per_team: 2,
                per_principal: 2,
            },
        )
        .await;
        let error = authority
            .revoke(RevocationCommand {
                actor: actor(owner, owner_credential, ActorKind::Human),
                team_id: None,
                scope_kind: "credential",
                scope_id: owner_credential.to_string(),
                reason_code: "operator_revoke",
                correlation_id: Uuid::now_v7(),
            })
            .await
            .expect_err("human cannot globally revoke credential");
        assert!(matches!(
            error,
            AuthorityError::Store(StoreError::Forbidden)
        ));
        let markers: i64 = sqlx::query_scalar("SELECT count(*) FROM revocations")
            .fetch_one(store.pool())
            .await
            .expect("read markers");
        assert_eq!(markers, 0);
        assert!(!authority.closed.load(Ordering::Acquire));
        cleanup(&store, &schema).await;
    }

    #[tokio::test]
    async fn denied_admission_preserves_existing_guards_and_authority() {
        let (store, schema, owner, owner_credential, _administrator, _administrator_credential) =
            fixture().await;
        let team: Uuid = sqlx::query_scalar("SELECT team_id FROM teams")
            .fetch_one(store.pool())
            .await
            .expect("read team");
        let member = Uuid::now_v7();
        let member_credential = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)",
        )
        .bind(member)
        .execute(store.pool())
        .await
        .expect("seed denied member");
        seed_identity(&store, member).await;
        sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,credential_kind,credential_generation,recovery_generation,expires_at,identity_id,digest_key_id,rotation_family_id) VALUES ($1,$2,decode(repeat('02',32),'hex'),$3,'human',1,1,clock_timestamp()+interval '1 hour',$3,'test',$1)")
            .bind(member_credential)
            .bind(member_credential.to_string())
            .bind(member)
            .execute(store.pool())
            .await
            .expect("seed denied member credential");
        sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'member','active',1,1)")
            .bind(Uuid::now_v7())
            .bind(team)
            .bind(member)
            .execute(store.pool())
            .await
            .expect("seed denied member membership");
        let (authority, _directory) = authority(
            store.clone(),
            AuthorityLimits {
                global: 2,
                per_team: 2,
                per_principal: 2,
            },
        )
        .await;
        let guard = authority
            .admit(AuthorizationRequest {
                actor: actor(owner, owner_credential, ActorKind::Human),
                team_id: team,
                permission: "session.metadata.read".to_owned(),
                resource: crate::store::ResourceScope::Team,
                correlation_id: Uuid::now_v7(),
            })
            .await
            .expect("admit owner");
        let denied = authority
            .admit(AuthorizationRequest {
                actor: actor(member, member_credential, ActorKind::Human),
                team_id: team,
                permission: "session.terminal.control".to_owned(),
                resource: crate::store::ResourceScope::Team,
                correlation_id: Uuid::now_v7(),
            })
            .await
            .expect_err("member terminal control denied");
        assert!(matches!(
            denied,
            AuthorityError::Store(StoreError::Forbidden)
        ));
        assert!(!authority.is_closed());
        guard.validate().await.expect("owner guard remains valid");
        cleanup(&store, &schema).await;
    }

    #[tokio::test]
    async fn audit_failure_closes_authority_after_the_durable_deny_marker() {
        let (store, schema, owner, owner_credential, _administrator, _administrator_credential) =
            fixture().await;
        let team: Uuid = sqlx::query_scalar("SELECT team_id FROM teams")
            .fetch_one(store.pool())
            .await
            .expect("read team");
        let (authority, _directory) = authority(
            store.clone(),
            AuthorityLimits {
                global: 2,
                per_team: 2,
                per_principal: 2,
            },
        )
        .await;
        let guard = authority
            .admit(AuthorizationRequest {
                actor: actor(owner, owner_credential, ActorKind::Human),
                team_id: team,
                permission: "session.metadata.read".to_owned(),
                resource: crate::store::ResourceScope::Team,
                correlation_id: Uuid::now_v7(),
            })
            .await
            .expect("admit owner");
        sqlx::raw_sql(AssertSqlSafe("CREATE FUNCTION reject_revoke_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'audit unavailable'; END; $$; CREATE TRIGGER reject_revoke_audit BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.action = 'authority.revoke') EXECUTE FUNCTION reject_revoke_audit();".to_owned()))
            .execute(store.pool()).await.expect("install audit failure trigger");
        let error = authority
            .revoke(RevocationCommand {
                actor: actor(owner, owner_credential, ActorKind::Human),
                team_id: Some(team),
                scope_kind: "team",
                scope_id: team.to_string(),
                reason_code: "operator_revoke",
                correlation_id: Uuid::now_v7(),
            })
            .await
            .expect_err("audit failure cannot acknowledge revocation");
        assert!(matches!(
            error,
            AuthorityError::Store(StoreError::AuditUnavailable)
        ));
        assert!(authority.closed.load(Ordering::Acquire));
        assert!(guard.cancelled());
        assert_eq!(authority.current.lock().expect("witness lock").sequence, 2);
        assert!(matches!(
            authority.release_clean().await,
            Err(AuthorityError::Cancelled)
        ));
        assert_eq!(authority.current.lock().expect("witness lock").sequence, 2);
        let revocations: i64 = sqlx::query_scalar("SELECT count(*) FROM revocations")
            .fetch_one(store.pool())
            .await
            .expect("read revocations");
        assert_eq!(revocations, 0);
        cleanup(&store, &schema).await;
    }

    #[tokio::test]
    async fn dropped_guard_releases_admission_capacity() {
        let (store, schema, owner, owner_credential, _administrator, _administrator_credential) =
            fixture().await;
        let team: Uuid = sqlx::query_scalar("SELECT team_id FROM teams")
            .fetch_one(store.pool())
            .await
            .expect("read team");
        let (authority, _directory) = authority(
            store.clone(),
            AuthorityLimits {
                global: 1,
                per_team: 1,
                per_principal: 1,
            },
        )
        .await;
        let request = AuthorizationRequest {
            actor: actor(owner, owner_credential, ActorKind::Human),
            team_id: team,
            permission: "session.metadata.read".to_owned(),
            resource: crate::store::ResourceScope::Team,
            correlation_id: Uuid::now_v7(),
        };
        let guard = authority
            .admit(request.clone())
            .await
            .expect("first admission");
        assert!(matches!(
            authority.admit(request.clone()).await,
            Err(AuthorityError::Capacity)
        ));
        drop(guard);
        authority
            .admit(request)
            .await
            .expect("capacity released by drop");
        cleanup(&store, &schema).await;
    }

    #[tokio::test]
    async fn renewed_fence_extends_an_admitted_guard_deadline() {
        let (store, schema, owner, owner_credential, _administrator, _administrator_credential) =
            fixture().await;
        let team: Uuid = sqlx::query_scalar("SELECT team_id FROM teams")
            .fetch_one(store.pool())
            .await
            .expect("read team");
        let (authority, _directory) = authority(
            store.clone(),
            AuthorityLimits {
                global: 1,
                per_team: 1,
                per_principal: 1,
            },
        )
        .await;
        let guard = authority
            .admit(AuthorizationRequest {
                actor: actor(owner, owner_credential, ActorKind::Human),
                team_id: team,
                permission: "session.metadata.read".to_owned(),
                resource: crate::store::ResourceScope::Team,
                correlation_id: Uuid::now_v7(),
            })
            .await
            .expect("admit owner");
        tokio::time::sleep(Duration::from_secs(4)).await;
        authority.renew_once().await.expect("renew live fence");
        tokio::time::sleep(Duration::from_secs(2)).await;
        guard
            .validate()
            .await
            .expect("guard remains valid after renewed deadline");
        cleanup(&store, &schema).await;
    }

    #[tokio::test]
    async fn revocation_between_sql_authorization_and_registration_cannot_admit() {
        let (store, schema, owner, owner_credential, _administrator, _administrator_credential) =
            fixture().await;
        let team: Uuid = sqlx::query_scalar("SELECT team_id FROM teams")
            .fetch_one(store.pool())
            .await
            .expect("read team");
        let (authority, _directory) = authority(
            store.clone(),
            AuthorityLimits {
                global: 1,
                per_team: 1,
                per_principal: 1,
            },
        )
        .await;
        let authority = Arc::new(authority);
        let entered = Arc::new(tokio::sync::Barrier::new(2));
        let release = Arc::new(tokio::sync::Barrier::new(2));
        *authority
            .admission_hook
            .lock()
            .expect("admission hook lock") = Some(AdmissionHook {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        let request = AuthorizationRequest {
            actor: actor(owner, owner_credential, ActorKind::Human),
            team_id: team,
            permission: "session.metadata.read".to_owned(),
            resource: crate::store::ResourceScope::Team,
            correlation_id: Uuid::now_v7(),
        };
        let pending = {
            let authority = Arc::clone(&authority);
            let request = request.clone();
            tokio::spawn(async move { authority.admit(request).await })
        };
        entered.wait().await;
        authority
            .revoke(RevocationCommand {
                actor: actor(owner, owner_credential, ActorKind::Human),
                team_id: Some(team),
                scope_kind: "policy",
                scope_id: team.to_string(),
                reason_code: "operator_revoke",
                correlation_id: Uuid::now_v7(),
            })
            .await
            .expect("revoke policy while admission is paused");
        release.wait().await;
        assert!(matches!(
            pending.await.expect("admission task completed"),
            Err(AuthorityError::Cancelled)
        ));
        *authority
            .admission_hook
            .lock()
            .expect("admission hook lock") = None;
        authority
            .admit(request)
            .await
            .expect("failed registration retained no capacity");
        cleanup(&store, &schema).await;
    }

    #[tokio::test]
    async fn admission_started_after_revocation_marker_cannot_keep_precommit_authority() {
        let (store, schema, owner, owner_credential, _administrator, _administrator_credential) =
            fixture().await;
        let team: Uuid = sqlx::query_scalar("SELECT team_id FROM teams")
            .fetch_one(store.pool())
            .await
            .expect("read team");
        let (authority, _directory) = authority(
            store.clone(),
            AuthorityLimits {
                global: 2,
                per_team: 2,
                per_principal: 2,
            },
        )
        .await;
        let authority = Arc::new(authority);
        let entered = Arc::new(tokio::sync::Barrier::new(2));
        let release = Arc::new(tokio::sync::Barrier::new(2));
        *authority.mutation_hook.lock().expect("mutation hook lock") = Some(AdmissionHook {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        let mutating = {
            let authority = Arc::clone(&authority);
            tokio::spawn(async move {
                authority
                    .revoke(RevocationCommand {
                        actor: actor(owner, owner_credential, ActorKind::Human),
                        team_id: Some(team),
                        scope_kind: "policy",
                        scope_id: team.to_string(),
                        reason_code: "operator_revoke",
                        correlation_id: Uuid::now_v7(),
                    })
                    .await
            })
        };
        entered.wait().await;
        let admission = authority
            .admit(AuthorizationRequest {
                actor: actor(owner, owner_credential, ActorKind::Human),
                team_id: team,
                permission: "session.metadata.read".to_owned(),
                resource: crate::store::ResourceScope::Team,
                correlation_id: Uuid::now_v7(),
            })
            .await;
        assert!(matches!(admission, Err(AuthorityError::Cancelled)));
        release.wait().await;
        mutating
            .await
            .expect("mutation task")
            .expect("commit revocation");
        cleanup(&store, &schema).await;
    }
}
