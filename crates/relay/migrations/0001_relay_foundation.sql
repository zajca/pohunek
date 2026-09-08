-- Relay foundation: durable recovery, identity, team RBAC, audit, and revocation.
-- Authentication transactions and credential secret digests are introduced by 0002.

CREATE TABLE relay_identity (
    relay_id TEXT PRIMARY KEY,
    recovery_generation BIGINT NOT NULL CHECK (recovery_generation > 0),
    state TEXT NOT NULL CHECK (state IN ('normal', 'recovery_quarantine')),
    revision BIGINT NOT NULL CHECK (revision > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE relay_lease (
    relay_id TEXT PRIMARY KEY REFERENCES relay_identity(relay_id) ON DELETE RESTRICT,
    process_instance_id UUID NOT NULL,
    fence_token UUID NOT NULL UNIQUE,
    recovery_generation BIGINT NOT NULL CHECK (recovery_generation > 0),
    expires_at TIMESTAMPTZ NOT NULL,
    heartbeat_sequence BIGINT NOT NULL CHECK (heartbeat_sequence > 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE principals (
    id UUID PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('human', 'service', 'infrastructure')),
    state TEXT NOT NULL CHECK (state IN ('active', 'deprovisioned')),
    generation BIGINT NOT NULL CHECK (generation > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    deprovisioned_at TIMESTAMPTZ
);

CREATE TABLE teams (
    team_id UUID PRIMARY KEY,
    display_name TEXT NOT NULL CHECK (char_length(display_name) BETWEEN 1 AND 256),
    state TEXT NOT NULL CHECK (state IN ('active', 'disabled')),
    policy_generation BIGINT NOT NULL CHECK (policy_generation > 0),
    revision BIGINT NOT NULL CHECK (revision > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE memberships (
    membership_id UUID PRIMARY KEY,
    team_id UUID NOT NULL REFERENCES teams(team_id) ON DELETE RESTRICT,
    principal_id UUID NOT NULL REFERENCES principals(id) ON DELETE RESTRICT,
    builtin_role TEXT NOT NULL CHECK (builtin_role IN ('owner', 'admin', 'member')),
    state TEXT NOT NULL CHECK (state IN ('active', 'removed')),
    revision BIGINT NOT NULL CHECK (revision > 0),
    local_deny_generation BIGINT NOT NULL CHECK (local_deny_generation > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    removed_at TIMESTAMPTZ,
    UNIQUE (team_id, principal_id),
    UNIQUE (team_id, membership_id)
);

CREATE TABLE permissions (
    permission TEXT PRIMARY KEY CHECK (char_length(permission) BETWEEN 1 AND 128)
);

CREATE TABLE custom_roles (
    team_id UUID NOT NULL REFERENCES teams(team_id) ON DELETE RESTRICT,
    role_id UUID NOT NULL,
    display_name TEXT NOT NULL CHECK (char_length(display_name) BETWEEN 1 AND 256),
    state TEXT NOT NULL CHECK (state IN ('active', 'removed')),
    revision BIGINT NOT NULL CHECK (revision > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (team_id, role_id)
);

CREATE TABLE custom_role_permissions (
    team_id UUID NOT NULL,
    role_id UUID NOT NULL,
    permission TEXT NOT NULL REFERENCES permissions(permission) ON DELETE RESTRICT,
    PRIMARY KEY (team_id, role_id, permission),
    FOREIGN KEY (team_id, role_id)
        REFERENCES custom_roles(team_id, role_id) ON DELETE RESTRICT
);

-- Custom roles are assigned only to an active membership of the same team.
-- The composite key prevents a role from crossing its tenant boundary.
CREATE TABLE membership_custom_roles (
    team_id UUID NOT NULL,
    principal_id UUID NOT NULL,
    role_id UUID NOT NULL,
    PRIMARY KEY (team_id, principal_id, role_id),
    FOREIGN KEY (team_id, principal_id)
        REFERENCES memberships(team_id, principal_id) ON DELETE RESTRICT,
    FOREIGN KEY (team_id, role_id)
        REFERENCES custom_roles(team_id, role_id) ON DELETE RESTRICT
);

CREATE TABLE groups (
    team_id UUID NOT NULL REFERENCES teams(team_id) ON DELETE RESTRICT,
    group_id UUID NOT NULL,
    display_name TEXT NOT NULL CHECK (char_length(display_name) BETWEEN 1 AND 256),
    state TEXT NOT NULL CHECK (state IN ('active', 'removed')),
    revision BIGINT NOT NULL CHECK (revision > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (team_id, group_id)
);

CREATE TABLE group_members (
    team_id UUID NOT NULL,
    group_id UUID NOT NULL,
    principal_id UUID NOT NULL REFERENCES principals(id) ON DELETE RESTRICT,
    revision BIGINT NOT NULL CHECK (revision > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (team_id, group_id, principal_id),
    FOREIGN KEY (team_id, group_id) REFERENCES groups(team_id, group_id) ON DELETE RESTRICT,
    FOREIGN KEY (team_id, principal_id) REFERENCES memberships(team_id, principal_id) ON DELETE RESTRICT
);

CREATE TABLE grants (
    team_id UUID NOT NULL REFERENCES teams(team_id) ON DELETE RESTRICT,
    grant_id UUID NOT NULL,
    subject_kind TEXT NOT NULL CHECK (subject_kind IN ('principal', 'group', 'service_account')),
    subject_id UUID NOT NULL,
    resource_kind TEXT NOT NULL CHECK (resource_kind IN ('team', 'host', 'host_share', 'project', 'session')),
    resource_id TEXT NOT NULL CHECK (char_length(resource_id) BETWEEN 1 AND 256),
    permission TEXT NOT NULL REFERENCES permissions(permission) ON DELETE RESTRICT,
    state TEXT NOT NULL CHECK (state IN ('active', 'removed')),
    revision BIGINT NOT NULL CHECK (revision > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (team_id, grant_id)
);

-- The service checks subject type and team coordinates inside its serializable
-- authorization transaction. Future resource tables add composite FKs instead
-- of accepting a caller-supplied unscoped resource identity.

CREATE TABLE host_ownership_references (
    host_id TEXT PRIMARY KEY CHECK (char_length(host_id) BETWEEN 1 AND 256),
    relay_id TEXT NOT NULL REFERENCES relay_identity(relay_id) ON DELETE RESTRICT,
    owner_principal_id UUID REFERENCES principals(id) ON DELETE RESTRICT,
    owner_team_id UUID REFERENCES teams(team_id) ON DELETE RESTRICT,
    owner_revision BIGINT NOT NULL CHECK (owner_revision > 0),
    state TEXT NOT NULL CHECK (state IN ('recorded', 'quarantined', 'retired')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK (num_nonnulls(owner_principal_id, owner_team_id) = 1)
);

CREATE TABLE revocations (
    revocation_id UUID PRIMARY KEY,
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('global', 'team', 'principal', 'credential', 'membership', 'policy', 'recovery')),
    scope_id TEXT NOT NULL CHECK (char_length(scope_id) BETWEEN 1 AND 256),
    team_id UUID REFERENCES teams(team_id) ON DELETE RESTRICT,
    policy_generation BIGINT NOT NULL CHECK (policy_generation > 0),
    recovery_generation BIGINT NOT NULL CHECK (recovery_generation > 0),
    reason_code TEXT NOT NULL CHECK (char_length(reason_code) BETWEEN 1 AND 128),
    correlation_id UUID NOT NULL,
    committed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE cancellation_outbox (
    sequence BIGSERIAL PRIMARY KEY,
    revocation_id UUID NOT NULL REFERENCES revocations(revocation_id) ON DELETE RESTRICT,
    scope_kind TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    team_id UUID,
    committed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    delivered_at TIMESTAMPTZ
);

CREATE INDEX cancellation_outbox_pending_idx
    ON cancellation_outbox(sequence) WHERE delivered_at IS NULL;

CREATE TABLE audit_events (
    audit_id UUID PRIMARY KEY,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    actor_principal_id UUID REFERENCES principals(id) ON DELETE RESTRICT,
    actor_kind TEXT NOT NULL CHECK (actor_kind IN ('human', 'service', 'infrastructure', 'system')),
    team_id UUID REFERENCES teams(team_id) ON DELETE RESTRICT,
    host_id TEXT,
    action TEXT NOT NULL CHECK (char_length(action) BETWEEN 1 AND 128),
    decision TEXT NOT NULL CHECK (decision IN ('allow', 'deny', 'changed', 'revoke')),
    policy_generation BIGINT NOT NULL CHECK (policy_generation > 0),
    recovery_generation BIGINT NOT NULL CHECK (recovery_generation > 0),
    correlation_id UUID NOT NULL,
    idempotency_key UUID,
    parameter_code TEXT NOT NULL CHECK (char_length(parameter_code) <= 128),
    parameter_value TEXT NOT NULL CHECK (char_length(parameter_value) <= 256),
    outcome TEXT NOT NULL CHECK (char_length(outcome) <= 128)
);

CREATE UNIQUE INDEX audit_events_idempotency_idx
    ON audit_events(actor_principal_id, action, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE TABLE mutation_receipts (
    actor_principal_id UUID NOT NULL REFERENCES principals(id) ON DELETE RESTRICT,
    action TEXT NOT NULL CHECK (char_length(action) BETWEEN 1 AND 128),
    idempotency_key UUID NOT NULL,
    request_digest BYTEA NOT NULL CHECK (octet_length(request_digest) = 32),
    audit_id UUID NOT NULL REFERENCES audit_events(audit_id) ON DELETE RESTRICT,
    outcome_code TEXT NOT NULL CHECK (char_length(outcome_code) <= 128),
    result_id UUID,
    result_revision BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (actor_principal_id, action, idempotency_key)
);

INSERT INTO permissions (permission) VALUES
    ('team.admin'),
    ('team.membership.read'),
    ('team.membership.manage'),
    ('team.group.manage'),
    ('team.role.manage'),
    ('team.grant.manage'),
    ('session.metadata.read'),
    ('session.terminal.observe'),
    ('session.terminal.control'),
    ('session.lifecycle.control'),
    ('session.share.manage'),
    ('session.remove')
ON CONFLICT DO NOTHING;

CREATE TABLE restore_reviews (
    review_id UUID PRIMARY KEY,
    relay_id TEXT NOT NULL REFERENCES relay_identity(relay_id) ON DELETE RESTRICT,
    witness_generation BIGINT NOT NULL CHECK (witness_generation > 0),
    manifest_digest BYTEA NOT NULL CHECK (octet_length(manifest_digest) = 32),
    decided_by_principal_id UUID REFERENCES principals(id) ON DELETE RESTRICT,
    action TEXT NOT NULL CHECK (action IN ('quarantine', 'reopen')),
    audit_id UUID UNIQUE REFERENCES audit_events(audit_id) ON DELETE RESTRICT,
    decided_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE FUNCTION enforce_active_owner() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM teams WHERE team_id = OLD.team_id)
       AND NOT EXISTS (
        SELECT 1 FROM memberships
        WHERE team_id = OLD.team_id AND state = 'active' AND builtin_role = 'owner'
    ) THEN
        RAISE EXCEPTION 'team must retain an active owner' USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER memberships_active_owner_required
AFTER UPDATE OR DELETE ON memberships
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION enforce_active_owner();

CREATE FUNCTION enforce_grant_subject() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.subject_kind = 'group' AND NOT EXISTS (
        SELECT 1 FROM groups WHERE team_id = NEW.team_id AND group_id = NEW.subject_id AND state = 'active'
    ) THEN
        RAISE EXCEPTION 'grant group is not an active team group' USING ERRCODE = '23503';
    ELSIF NEW.subject_kind IN ('principal', 'service_account') AND NOT EXISTS (
        SELECT 1 FROM memberships WHERE team_id = NEW.team_id AND principal_id = NEW.subject_id AND state = 'active'
    ) THEN
        RAISE EXCEPTION 'grant principal is not an active team member' USING ERRCODE = '23503';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER grants_team_subject_required
BEFORE INSERT OR UPDATE OF team_id, subject_kind, subject_id ON grants
FOR EACH ROW EXECUTE FUNCTION enforce_grant_subject();
