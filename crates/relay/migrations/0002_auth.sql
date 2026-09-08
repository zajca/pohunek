-- Relay authentication: OIDC transactions, opaque browser sessions, and credential digests.
-- Raw OAuth values and bearer secrets intentionally never cross this persistence boundary.

CREATE FUNCTION prevent_principal_kind_change() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.kind IS DISTINCT FROM OLD.kind THEN
        RAISE EXCEPTION 'principal kind is immutable' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER principals_kind_immutable
BEFORE UPDATE OF kind ON principals
FOR EACH ROW EXECUTE FUNCTION prevent_principal_kind_change();

CREATE TABLE oidc_identities (
    identity_id UUID PRIMARY KEY,
    issuer TEXT NOT NULL CHECK (char_length(issuer) BETWEEN 1 AND 2048),
    subject TEXT NOT NULL CHECK (char_length(subject) BETWEEN 1 AND 2048),
    principal_id UUID NOT NULL REFERENCES principals(id) ON DELETE RESTRICT,
    link_generation BIGINT NOT NULL CHECK (link_generation > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    removed_at TIMESTAMPTZ,
    UNIQUE (issuer, subject)
    , UNIQUE (identity_id, principal_id)
);

CREATE FUNCTION enforce_oidc_identity_principal_kind() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM principals
        WHERE id = NEW.principal_id AND kind IN ('human', 'infrastructure')
    ) THEN
        RAISE EXCEPTION 'OIDC identity principal must have human or infrastructure kind' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER oidc_identities_require_human_or_infrastructure_principal
BEFORE INSERT OR UPDATE OF principal_id ON oidc_identities
FOR EACH ROW EXECUTE FUNCTION enforce_oidc_identity_principal_kind();

CREATE TABLE account_link_transactions (
    link_id UUID PRIMARY KEY,
    principal_id UUID NOT NULL REFERENCES principals(id) ON DELETE RESTRICT,
    source_identity_id UUID NOT NULL,
    account_link_generation BIGINT NOT NULL CHECK (account_link_generation > 0),
    recovery_generation BIGINT NOT NULL CHECK (recovery_generation > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    expires_at TIMESTAMPTZ NOT NULL,
    completed_at TIMESTAMPTZ,
    CHECK (expires_at > created_at),
    FOREIGN KEY (source_identity_id, principal_id)
        REFERENCES oidc_identities(identity_id, principal_id) ON DELETE RESTRICT
);

CREATE TABLE browser_logins (
    login_id UUID PRIMARY KEY,
    state_digest BYTEA NOT NULL CHECK (octet_length(state_digest) = 32),
    nonce_digest BYTEA NOT NULL CHECK (octet_length(nonce_digest) = 32),
    pkce_verifier_digest BYTEA NOT NULL CHECK (octet_length(pkce_verifier_digest) = 32),
    login_binding_digest BYTEA NOT NULL CHECK (octet_length(login_binding_digest) = 32),
    issuer TEXT NOT NULL CHECK (char_length(issuer) BETWEEN 1 AND 2048),
    client_id TEXT NOT NULL CHECK (char_length(client_id) BETWEEN 1 AND 1024),
    audience TEXT NOT NULL CHECK (char_length(audience) BETWEEN 1 AND 1024),
    redirect_uri TEXT NOT NULL CHECK (char_length(redirect_uri) BETWEEN 1 AND 2048),
    action TEXT NOT NULL CHECK (action IN ('login', 'account_link')),
    link_id UUID REFERENCES account_link_transactions(link_id) ON DELETE RESTRICT,
    account_link_generation BIGINT NOT NULL CHECK (account_link_generation > 0),
    recovery_generation BIGINT NOT NULL CHECK (recovery_generation > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ,
    outcome TEXT CHECK (outcome IN ('succeeded', 'failed', 'expired', 'cancelled')),
    CHECK (expires_at > created_at),
    CHECK ((action = 'login' AND link_id IS NULL) OR (action = 'account_link' AND link_id IS NOT NULL))
);

CREATE UNIQUE INDEX browser_logins_active_state_idx
    ON browser_logins(state_digest) WHERE consumed_at IS NULL;

CREATE TABLE device_logins (
    login_id UUID PRIMARY KEY,
    device_code_digest BYTEA NOT NULL CHECK (octet_length(device_code_digest) = 32),
    poll_secret_digest BYTEA NOT NULL CHECK (octet_length(poll_secret_digest) = 32),
    issuer TEXT NOT NULL CHECK (char_length(issuer) BETWEEN 1 AND 2048),
    client_id TEXT NOT NULL CHECK (char_length(client_id) BETWEEN 1 AND 1024),
    audience TEXT NOT NULL CHECK (char_length(audience) BETWEEN 1 AND 1024),
    action TEXT NOT NULL CHECK (action IN ('login', 'host_enrollment', 'account_link')),
    link_id UUID REFERENCES account_link_transactions(link_id) ON DELETE RESTRICT,
    account_link_generation BIGINT NOT NULL CHECK (account_link_generation > 0),
    recovery_generation BIGINT NOT NULL CHECK (recovery_generation > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    expires_at TIMESTAMPTZ NOT NULL,
    poll_interval_seconds INTEGER NOT NULL CHECK (poll_interval_seconds BETWEEN 1 AND 3600),
    next_poll_at TIMESTAMPTZ NOT NULL,
    poll_lease_until TIMESTAMPTZ,
    poll_lease_token BYTEA CHECK (poll_lease_token IS NULL OR octet_length(poll_lease_token) = 32),
    consumed_at TIMESTAMPTZ,
    outcome TEXT CHECK (outcome IN ('succeeded', 'denied', 'expired', 'failed', 'cancelled')),
    CHECK (expires_at > created_at),
    CHECK ((action = 'account_link' AND link_id IS NOT NULL) OR (action <> 'account_link' AND link_id IS NULL))
);

CREATE UNIQUE INDEX device_logins_active_device_code_idx
    ON device_logins(device_code_digest) WHERE consumed_at IS NULL;

CREATE TABLE browser_sessions (
    session_id UUID PRIMARY KEY,
    cookie_digest BYTEA NOT NULL UNIQUE CHECK (octet_length(cookie_digest) = 32),
    csrf_digest BYTEA NOT NULL CHECK (octet_length(csrf_digest) = 32),
    principal_id UUID NOT NULL REFERENCES principals(id) ON DELETE RESTRICT,
    identity_id UUID NOT NULL,
    digest_key_id TEXT NOT NULL CHECK (char_length(digest_key_id) BETWEEN 1 AND 128),
    session_generation BIGINT NOT NULL CHECK (session_generation > 0),
    recovery_generation BIGINT NOT NULL CHECK (recovery_generation > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    expires_at TIMESTAMPTZ NOT NULL,
    idle_deadline TIMESTAMPTZ NOT NULL,
    last_used_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    revoked_at TIMESTAMPTZ,
    CHECK (expires_at > created_at),
    CHECK (idle_deadline <= expires_at)
    , FOREIGN KEY (identity_id, principal_id) REFERENCES oidc_identities(identity_id, principal_id) ON DELETE RESTRICT
);

CREATE TABLE relay_credentials (
    credential_id UUID PRIMARY KEY,
    public_id TEXT NOT NULL UNIQUE CHECK (char_length(public_id) BETWEEN 1 AND 256),
    secret_digest BYTEA NOT NULL UNIQUE CHECK (octet_length(secret_digest) = 32),
    principal_id UUID NOT NULL REFERENCES principals(id) ON DELETE RESTRICT,
    identity_id UUID,
    digest_key_id TEXT NOT NULL CHECK (char_length(digest_key_id) BETWEEN 1 AND 128),
    credential_kind TEXT NOT NULL CHECK (credential_kind IN ('human', 'service')),
    credential_generation BIGINT NOT NULL CHECK (credential_generation > 0),
    rotation_family_id UUID,
    predecessor_credential_id UUID REFERENCES relay_credentials(credential_id) ON DELETE RESTRICT,
    recovery_generation BIGINT NOT NULL CHECK (recovery_generation > 0),
    issued_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    expires_at TIMESTAMPTZ NOT NULL,
    rotation_overlap_ends_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ,
    CHECK (expires_at > issued_at),
    CHECK (rotation_overlap_ends_at IS NULL OR rotation_overlap_ends_at <= expires_at),
    CHECK (public_id = credential_id::TEXT),
    CHECK ((credential_kind = 'human' AND identity_id IS NOT NULL) OR (credential_kind = 'service' AND identity_id IS NULL)),
    FOREIGN KEY (identity_id, principal_id) REFERENCES oidc_identities(identity_id, principal_id) ON DELETE RESTRICT
);

CREATE INDEX relay_credentials_current_principal_idx
    ON relay_credentials(principal_id) WHERE revoked_at IS NULL;

-- Service accounts are explicit team-scoped service principals.  Membership
-- grants remain separate: creation intentionally confers no authorization.
CREATE TABLE service_accounts (
    principal_id UUID PRIMARY KEY REFERENCES principals(id) ON DELETE RESTRICT,
    team_id UUID NOT NULL REFERENCES teams(team_id) ON DELETE RESTRICT,
    display_name TEXT NOT NULL CHECK (char_length(display_name) BETWEEN 1 AND 256),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    deprovisioned_at TIMESTAMPTZ
);

CREATE INDEX service_accounts_active_team_idx
    ON service_accounts(team_id) WHERE deprovisioned_at IS NULL;

CREATE FUNCTION enforce_service_account_principal() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM principals WHERE id = NEW.principal_id AND kind = 'service') THEN
        RAISE EXCEPTION 'service account principal must have service kind';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER service_accounts_require_service_principal
BEFORE INSERT OR UPDATE OF principal_id ON service_accounts
FOR EACH ROW EXECUTE FUNCTION enforce_service_account_principal();

CREATE FUNCTION prevent_service_account_reparenting() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.principal_id IS DISTINCT FROM OLD.principal_id
       OR NEW.team_id IS DISTINCT FROM OLD.team_id THEN
        RAISE EXCEPTION 'service account principal and team are immutable' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER service_accounts_identity_immutable
BEFORE UPDATE OF principal_id, team_id ON service_accounts
FOR EACH ROW EXECUTE FUNCTION prevent_service_account_reparenting();

-- A service credential must reference its canonical service-account row. The
-- generated column is NULL for human credentials, so the foreign key applies
-- only to service credentials and PostgreSQL's referential locking closes
-- concurrent insert/delete races.
ALTER TABLE relay_credentials
    ADD COLUMN service_account_principal_id UUID
        GENERATED ALWAYS AS (
            CASE WHEN credential_kind = 'service' THEN principal_id ELSE NULL END
        ) STORED,
    ADD CONSTRAINT relay_credentials_service_account_parent_fk
        FOREIGN KEY (service_account_principal_id)
        REFERENCES service_accounts(principal_id) ON DELETE RESTRICT;

CREATE FUNCTION enforce_credential_principal_kind() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM principals WHERE id = NEW.principal_id
        AND ((NEW.credential_kind = 'service' AND kind = 'service')
             OR (NEW.credential_kind = 'human' AND kind IN ('human', 'infrastructure')))
    ) THEN
        RAISE EXCEPTION 'credential kind does not match principal kind';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER relay_credentials_require_matching_principal
BEFORE INSERT OR UPDATE OF principal_id, credential_kind ON relay_credentials
FOR EACH ROW EXECUTE FUNCTION enforce_credential_principal_kind();

-- #92 will use this one-use, audience-bound boundary. It deliberately has no
-- eligibility projection or provider credential storage in #85.
CREATE TABLE evidence_challenges (
    challenge_id UUID PRIMARY KEY,
    relay_id TEXT NOT NULL REFERENCES relay_identity(relay_id) ON DELETE RESTRICT,
    principal_id UUID NOT NULL REFERENCES principals(id) ON DELETE RESTRICT,
    audience TEXT NOT NULL CHECK (char_length(audience) BETWEEN 1 AND 1024),
    nonce_digest BYTEA NOT NULL CHECK (octet_length(nonce_digest) = 32),
    team_id UUID REFERENCES teams(team_id) ON DELETE RESTRICT,
    admission_rule_revision BIGINT NOT NULL CHECK (admission_rule_revision > 0),
    account_link_generation BIGINT NOT NULL CHECK (account_link_generation > 0),
    recovery_generation BIGINT NOT NULL CHECK (recovery_generation > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ,
    CHECK (expires_at > created_at)
);
