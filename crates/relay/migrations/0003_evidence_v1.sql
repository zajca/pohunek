-- Issue #92: versioned admission rules and append-only external evidence results.
-- Self-contained and additive: the only pre-existing object touched is the 0002
-- evidence_challenges boundary, which gains the complete rule, provider, identity,
-- and broker-transaction coordinates required to bind a future evidence result.
-- This migration adds the durable rule registry (immutable per-revision rows plus
-- a current projection), the committed evidence/result rows, and the composite
-- cross-binding constraints the future internal evidence endpoint will write.
--
-- Rule semantics (RFC 13.2/13.3): one active rule of each provider kind per
-- team at most. Google Workspace rules bind an exact hosted domain; GitHub
-- rules bind an exact organization login. Revisions are monotonic per rule so
-- evidence can pin the exact policy it was checked against. Local deny is a
-- membership-level generation that self-service admission must honor; it is
-- stored on memberships.local_deny_generation (0001) and never reset by joins.

-- Immutable per-revision rule rows. Each revision is its own append-only row so
-- evidence references one exact frozen policy, and a rule can advance to a new
-- revision without the foreign-key conflict a single mutable row would cause
-- (evidence pinned to revision 1 would otherwise block a revision 2 update).
CREATE TABLE admission_rule_versions (
    admission_rule_id UUID NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0),
    team_id UUID NOT NULL REFERENCES teams(team_id) ON DELETE RESTRICT,
    provider TEXT NOT NULL CHECK (provider IN ('google', 'github')),
    method TEXT NOT NULL CHECK (method IN ('google_hosted_domain', 'github_organization')),
    -- Exact match coordinate: Workspace hosted domain or GitHub org login.
    match_value TEXT NOT NULL CHECK (char_length(match_value) BETWEEN 1 AND 256),
    state TEXT NOT NULL CHECK (state IN ('active', 'disabled')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (admission_rule_id, revision),
    UNIQUE (admission_rule_id, revision, team_id),
    UNIQUE (admission_rule_id, revision, team_id, provider, method),
    CHECK (
        (provider = 'google' AND method = 'google_hosted_domain')
        OR (provider = 'github' AND method = 'github_organization')
    )
);

-- Current-rule projection: a thin pointer to the latest immutable version plus
-- the identity coordinates mirrored for admin operations and uniqueness.
CREATE TABLE admission_rules (
    admission_rule_id UUID PRIMARY KEY,
    team_id UUID NOT NULL REFERENCES teams(team_id) ON DELETE RESTRICT,
    provider TEXT NOT NULL CHECK (provider IN ('google', 'github')),
    method TEXT NOT NULL CHECK (method IN ('google_hosted_domain', 'github_organization')),
    match_value TEXT NOT NULL CHECK (char_length(match_value) BETWEEN 1 AND 256),
    state TEXT NOT NULL CHECK (state IN ('active', 'disabled')),
    current_revision BIGINT NOT NULL CHECK (current_revision > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK (
        (provider = 'google' AND method = 'google_hosted_domain')
        OR (provider = 'github' AND method = 'github_organization')
    ),
    UNIQUE (admission_rule_id, team_id),
    FOREIGN KEY (admission_rule_id, current_revision, team_id)
        REFERENCES admission_rule_versions(admission_rule_id, revision, team_id) ON DELETE RESTRICT
);

-- At most one active rule of each provider kind per team. Disabled revisions
-- retain history; re-enabling is a new revision via the management API.
CREATE UNIQUE INDEX admission_rules_active_team_provider_idx
    ON admission_rules(team_id, provider)
    WHERE state = 'active';

-- Identity immutability: a committed revision is frozen history. Evidence pins
-- the exact policy it was checked against, so no field of a referenced version
-- row may ever change in place; corrections are new revisions.
CREATE FUNCTION prevent_admission_rule_version_change() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'admission rule versions are immutable' USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER admission_rule_versions_immutable
BEFORE UPDATE OR DELETE ON admission_rule_versions
FOR EACH ROW EXECUTE FUNCTION prevent_admission_rule_version_change();

-- Monotonic revision guard on the append-only version table.
CREATE FUNCTION enforce_admission_rule_revision() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM admission_rule_versions
        WHERE admission_rule_id = NEW.admission_rule_id AND revision >= NEW.revision
    ) THEN
        RAISE EXCEPTION 'admission rule revision must be monotonic' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER admission_rule_versions_revision_monotonic
BEFORE INSERT ON admission_rule_versions
FOR EACH ROW EXECUTE FUNCTION enforce_admission_rule_revision();

-- Projection consistency: the projection mirrors exactly one frozen version
-- row. Its identity and state are derived from that revision, so management
-- and evidence evaluation can never disagree about the same rule, and the
-- active-rule uniqueness cannot be bypassed by a diverging projection state.
CREATE FUNCTION enforce_admission_rule_projection_matches_version() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
    v_team UUID;
    v_provider TEXT;
    v_method TEXT;
    v_match TEXT;
    v_state TEXT;
BEGIN
    SELECT team_id, provider, method, match_value, state
      INTO v_team, v_provider, v_method, v_match, v_state
      FROM admission_rule_versions
     WHERE admission_rule_id = NEW.admission_rule_id
       AND revision = NEW.current_revision;
    IF v_team IS DISTINCT FROM NEW.team_id
       OR v_provider IS DISTINCT FROM NEW.provider
       OR v_method IS DISTINCT FROM NEW.method
       OR v_match IS DISTINCT FROM NEW.match_value
       OR v_state IS DISTINCT FROM NEW.state THEN
        RAISE EXCEPTION 'admission rule projection must mirror its current revision' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER admission_rules_projection_matches_version
BEFORE INSERT OR UPDATE OF team_id, provider, method, match_value, state, current_revision ON admission_rules
FOR EACH ROW EXECUTE FUNCTION enforce_admission_rule_projection_matches_version();

-- The projection advances only to a new revision; identity is immutable and
-- state transitions are new revisions, never in-place edits. Rewriting the
-- identity in place would defeat the append-only versioning and let a later
-- revision rewrite history.
CREATE FUNCTION enforce_admission_rule_projection_identity() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.team_id IS DISTINCT FROM OLD.team_id
       OR NEW.provider IS DISTINCT FROM OLD.provider
       OR NEW.method IS DISTINCT FROM OLD.method
       OR NEW.match_value IS DISTINCT FROM OLD.match_value THEN
        RAISE EXCEPTION 'admission rule identity is immutable' USING ERRCODE = '23514';
    END IF;
    IF NEW.current_revision < OLD.current_revision THEN
        RAISE EXCEPTION 'admission rule revision cannot move backwards' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER admission_rules_projection_identity
BEFORE UPDATE OF team_id, provider, method, match_value, current_revision ON admission_rules
FOR EACH ROW EXECUTE FUNCTION enforce_admission_rule_projection_identity();

-- Bind each one-use challenge to the exact rule it authorizes so a consumed
-- challenge cannot be attached to a different rule, team, or frozen revision
-- at result time. The composite foreign keys cover both the live projection
-- and the frozen revision row the evidence result will pin.
ALTER TABLE evidence_challenges
    ALTER COLUMN team_id SET NOT NULL,
    ADD COLUMN admission_rule_id UUID NOT NULL,
    ADD COLUMN transaction_id UUID NOT NULL,
    ADD COLUMN issuer TEXT NOT NULL CHECK (char_length(issuer) BETWEEN 1 AND 2048),
    ADD COLUMN keycloak_subject TEXT NOT NULL
        CHECK (char_length(keycloak_subject) BETWEEN 1 AND 2048),
    ADD COLUMN provider TEXT NOT NULL CHECK (provider IN ('google', 'github')),
    ADD COLUMN expected_provider_subject TEXT NOT NULL
        CHECK (char_length(expected_provider_subject) BETWEEN 1 AND 2048),
    ADD COLUMN method TEXT NOT NULL
        CHECK (method IN ('google_hosted_domain', 'github_organization')),
    ADD COLUMN provider_identity_generation BIGINT NOT NULL
        CHECK (provider_identity_generation > 0),
    ADD COLUMN checked_monotonic_epoch UUID NOT NULL,
    ADD COLUMN binding_transaction_digest TEXT NOT NULL
        CHECK (char_length(binding_transaction_digest) BETWEEN 1 AND 256),
    ADD COLUMN consumption_state TEXT NOT NULL DEFAULT 'unused'
        CHECK (consumption_state IN ('unused', 'evidence', 'invalidated')),
    ADD CONSTRAINT evidence_challenges_relay_audience_check CHECK (audience = relay_id),
    ADD CONSTRAINT evidence_challenges_provider_method_check CHECK (
        (provider = 'google' AND method = 'google_hosted_domain')
        OR (provider = 'github' AND method = 'github_organization')
    ),
    ADD CONSTRAINT evidence_challenges_team_rule_fk
        FOREIGN KEY (admission_rule_id, team_id)
        REFERENCES admission_rules(admission_rule_id, team_id) ON DELETE RESTRICT,
    ADD CONSTRAINT evidence_challenges_rule_revision_team_provider_method_fk
        FOREIGN KEY (admission_rule_id, admission_rule_revision, team_id, provider, method)
        REFERENCES admission_rule_versions(
            admission_rule_id, revision, team_id, provider, method
        ) ON DELETE RESTRICT;

-- Challenge issuance is authorized only by the current active projection.
-- The row lock serializes issuance with a concurrent management transition;
-- the evidence commit repeats this check so an intervening disable or revision
-- advance invalidates the outstanding challenge.
CREATE FUNCTION enforce_evidence_challenge_current_rule() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
    relay_state TEXT;
    relay_generation BIGINT;
    current_state TEXT;
    current_revision BIGINT;
BEGIN
    SELECT state, recovery_generation
      INTO relay_state, relay_generation
      FROM relay_identity
     WHERE relay_id = NEW.relay_id
     FOR SHARE;
    IF NOT FOUND
       OR relay_state IS DISTINCT FROM 'normal'
       OR relay_generation IS DISTINCT FROM NEW.recovery_generation THEN
        RAISE EXCEPTION 'evidence challenge requires the current normal relay generation' USING ERRCODE = '23514';
    END IF;
    SELECT state, admission_rules.current_revision
      INTO current_state, current_revision
      FROM admission_rules
     WHERE admission_rule_id = NEW.admission_rule_id
       AND team_id = NEW.team_id
       AND provider = NEW.provider
       AND method = NEW.method
     FOR SHARE;
    IF NOT FOUND
       OR current_state IS DISTINCT FROM 'active'
       OR current_revision IS DISTINCT FROM NEW.admission_rule_revision THEN
        RAISE EXCEPTION 'evidence challenge requires the current active rule revision' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER evidence_challenges_require_current_rule
BEFORE INSERT ON evidence_challenges
FOR EACH ROW EXECUTE FUNCTION enforce_evidence_challenge_current_rule();

-- Challenge bindings are immutable once issued. The only permitted mutations
-- are one-way transitions from unused to evidence (performed by the evidence
-- insert trigger) or invalidated (performed by recovery cleanup).
CREATE FUNCTION enforce_evidence_challenge_immutability() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'evidence challenges are append-only' USING ERRCODE = '23514';
    END IF;
    IF NEW.challenge_id IS DISTINCT FROM OLD.challenge_id
       OR NEW.relay_id IS DISTINCT FROM OLD.relay_id
       OR NEW.principal_id IS DISTINCT FROM OLD.principal_id
       OR NEW.audience IS DISTINCT FROM OLD.audience
       OR NEW.nonce_digest IS DISTINCT FROM OLD.nonce_digest
       OR NEW.team_id IS DISTINCT FROM OLD.team_id
       OR NEW.admission_rule_id IS DISTINCT FROM OLD.admission_rule_id
       OR NEW.admission_rule_revision IS DISTINCT FROM OLD.admission_rule_revision
       OR NEW.transaction_id IS DISTINCT FROM OLD.transaction_id
       OR NEW.issuer IS DISTINCT FROM OLD.issuer
       OR NEW.keycloak_subject IS DISTINCT FROM OLD.keycloak_subject
       OR NEW.provider IS DISTINCT FROM OLD.provider
       OR NEW.expected_provider_subject IS DISTINCT FROM OLD.expected_provider_subject
       OR NEW.method IS DISTINCT FROM OLD.method
       OR NEW.account_link_generation IS DISTINCT FROM OLD.account_link_generation
       OR NEW.provider_identity_generation IS DISTINCT FROM OLD.provider_identity_generation
       OR NEW.recovery_generation IS DISTINCT FROM OLD.recovery_generation
       OR NEW.checked_monotonic_epoch IS DISTINCT FROM OLD.checked_monotonic_epoch
       OR NEW.binding_transaction_digest IS DISTINCT FROM OLD.binding_transaction_digest
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
       OR NEW.expires_at IS DISTINCT FROM OLD.expires_at
       OR OLD.consumption_state IS DISTINCT FROM 'unused'
       OR NEW.consumption_state NOT IN ('evidence', 'invalidated')
       OR OLD.consumed_at IS NOT NULL
       OR NEW.consumed_at IS NULL
       OR NEW.consumed_at < NEW.created_at
       OR NEW.consumed_at > clock_timestamp() THEN
        RAISE EXCEPTION 'evidence challenge binding is immutable' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER evidence_challenges_immutable
BEFORE UPDATE OR DELETE ON evidence_challenges
FOR EACH ROW EXECUTE FUNCTION enforce_evidence_challenge_immutability();
-- Append-only committed evidence results. Each row pairs one consumed
-- evidence_challenges row with the broker attestation the future verifier
-- accepted or rejected. The outcome column mirrors EvidenceOutcome; only
-- 'eligible' rows may authorize a self-service join, and that join is a
-- separate membership transaction, never an automatic effect of this insert.
CREATE TABLE evidence_results (
    evidence_id UUID PRIMARY KEY,
    challenge_id UUID NOT NULL UNIQUE REFERENCES evidence_challenges(challenge_id) ON DELETE RESTRICT,
    -- Frozen EvidenceClaims coordinates. Persisting them keeps the committed
    -- row a reconstructible authoritative v1 attestation: the schema version,
    -- broker transaction, audience, issuer identity, provider-identity
    -- generation, monotonic epoch, and the cryptographically bound digests all
    -- survive restarts, can be invalidated on provider-identity change, and can
    -- be audited after restore. The one-use nonce is deliberately absent: it is
    -- a secret that only exists as the challenge's digest binding.
    evidence_version SMALLINT NOT NULL CHECK (evidence_version = 1),
    transaction_id UUID NOT NULL,
    audience TEXT NOT NULL CHECK (char_length(audience) BETWEEN 1 AND 1024),
    issuer TEXT NOT NULL CHECK (char_length(issuer) BETWEEN 1 AND 2048),
    keycloak_subject TEXT NOT NULL CHECK (char_length(keycloak_subject) BETWEEN 1 AND 2048),
    provider_identity_generation BIGINT NOT NULL CHECK (provider_identity_generation > 0),
    account_link_generation BIGINT NOT NULL CHECK (account_link_generation > 0),
    recovery_generation BIGINT NOT NULL CHECK (recovery_generation > 0),
    checked_monotonic_epoch UUID NOT NULL,
    binding_transaction_digest TEXT NOT NULL
        CHECK (char_length(binding_transaction_digest) BETWEEN 1 AND 256),
    upstream_exchange_digest TEXT NOT NULL
        CHECK (char_length(upstream_exchange_digest) BETWEEN 1 AND 256),
    principal_id UUID NOT NULL REFERENCES principals(id) ON DELETE RESTRICT,
    team_id UUID NOT NULL REFERENCES teams(team_id) ON DELETE RESTRICT,
    admission_rule_id UUID NOT NULL,
    admission_rule_revision BIGINT NOT NULL CHECK (admission_rule_revision > 0),
    provider TEXT NOT NULL CHECK (provider IN ('google', 'github')),
    provider_subject TEXT NOT NULL CHECK (char_length(provider_subject) BETWEEN 1 AND 2048),
    outcome TEXT NOT NULL CHECK (outcome IN ('eligible', 'ineligible', 'unknown')),
    method TEXT NOT NULL CHECK (method IN ('google_hosted_domain', 'github_organization')),
    checked_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    signing_key_id TEXT NOT NULL CHECK (char_length(signing_key_id) BETWEEN 1 AND 256),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK (expires_at > checked_at),
    CHECK (checked_at <= created_at),
    -- RFC 13.2: evidence is usable for at most 60 minutes after the actual
    -- upstream check. The ceiling is enforced here so no writer can persist a
    -- longer-lived proof even if the verifier is bypassed.
    CHECK (expires_at <= checked_at + INTERVAL '60 minutes'),
    -- Evidence references the frozen rule revision, never the mutable projection.
    FOREIGN KEY (admission_rule_id, admission_rule_revision, team_id, provider, method)
        REFERENCES admission_rule_versions(
            admission_rule_id, revision, team_id, provider, method
        ) ON DELETE RESTRICT
);

-- Cross-binding: an evidence result must describe the exact same principal,
-- team, rule, and rule revision that its referenced one-use challenge
-- authorized, and the provider/method must match that frozen rule version.
CREATE FUNCTION enforce_evidence_result_challenge_binding() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
    c_principal UUID;
    c_team UUID;
    c_rule UUID;
    c_revision BIGINT;
    c_transaction UUID;
    c_audience TEXT;
    c_issuer TEXT;
    c_keycloak_subject TEXT;
    c_provider TEXT;
    c_provider_subject TEXT;
    c_method TEXT;
    c_account_link_generation BIGINT;
    c_provider_identity_generation BIGINT;
    c_recovery_generation BIGINT;
    c_checked_monotonic_epoch UUID;
    c_binding_transaction_digest TEXT;
    c_relay_id TEXT;
    challenge_recovery_generation BIGINT;
    relay_state TEXT;
    relay_generation BIGINT;
    current_state TEXT;
    current_revision BIGINT;
BEGIN
    SELECT relay_id, recovery_generation
      INTO c_relay_id, challenge_recovery_generation
      FROM evidence_challenges
     WHERE challenge_id = NEW.challenge_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'evidence challenge is unavailable' USING ERRCODE = '23514';
    END IF;
    SELECT state, recovery_generation
      INTO relay_state, relay_generation
      FROM relay_identity
     WHERE relay_id = c_relay_id
     FOR SHARE;
    IF NOT FOUND
       OR relay_state IS DISTINCT FROM 'normal'
       OR relay_generation IS DISTINCT FROM challenge_recovery_generation THEN
        RAISE EXCEPTION 'evidence challenge relay generation is unavailable' USING ERRCODE = '23514';
    END IF;
    UPDATE evidence_challenges AS challenge
       SET consumed_at = clock_timestamp(), consumption_state = 'evidence'
     WHERE challenge.challenge_id = NEW.challenge_id
       AND challenge.consumption_state = 'unused'
       AND challenge.consumed_at IS NULL
       AND challenge.expires_at > clock_timestamp()
       AND challenge.relay_id = c_relay_id
       AND challenge.recovery_generation = challenge_recovery_generation
    RETURNING challenge.principal_id, challenge.team_id,
              challenge.admission_rule_id, challenge.admission_rule_revision,
              challenge.transaction_id, challenge.audience, challenge.issuer,
              challenge.keycloak_subject, challenge.provider,
              challenge.expected_provider_subject, challenge.method,
              challenge.account_link_generation,
              challenge.provider_identity_generation,
              challenge.recovery_generation, challenge.checked_monotonic_epoch,
              challenge.binding_transaction_digest
         INTO c_principal, c_team, c_rule, c_revision, c_transaction, c_audience,
              c_issuer, c_keycloak_subject, c_provider, c_provider_subject,
              c_method, c_account_link_generation,
              c_provider_identity_generation, c_recovery_generation,
              c_checked_monotonic_epoch, c_binding_transaction_digest;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'evidence challenge is unavailable' USING ERRCODE = '23514';
    END IF;
    IF c_principal IS DISTINCT FROM NEW.principal_id
       OR c_team IS DISTINCT FROM NEW.team_id
       OR c_rule IS DISTINCT FROM NEW.admission_rule_id
       OR c_revision IS DISTINCT FROM NEW.admission_rule_revision
       OR c_transaction IS DISTINCT FROM NEW.transaction_id
       OR c_audience IS DISTINCT FROM NEW.audience
       OR c_issuer IS DISTINCT FROM NEW.issuer
       OR c_keycloak_subject IS DISTINCT FROM NEW.keycloak_subject
       OR c_provider IS DISTINCT FROM NEW.provider
       OR c_provider_subject IS DISTINCT FROM NEW.provider_subject
       OR c_method IS DISTINCT FROM NEW.method
       OR c_account_link_generation IS DISTINCT FROM NEW.account_link_generation
       OR c_provider_identity_generation IS DISTINCT FROM NEW.provider_identity_generation
       OR c_recovery_generation IS DISTINCT FROM NEW.recovery_generation
       OR c_checked_monotonic_epoch IS DISTINCT FROM NEW.checked_monotonic_epoch
       OR c_binding_transaction_digest IS DISTINCT FROM NEW.binding_transaction_digest THEN
        RAISE EXCEPTION 'evidence result coordinates do not match its challenge' USING ERRCODE = '23514';
    END IF;
    SELECT state, admission_rules.current_revision
      INTO current_state, current_revision
      FROM admission_rules
     WHERE admission_rule_id = NEW.admission_rule_id
       AND team_id = NEW.team_id
       AND provider = NEW.provider
       AND method = NEW.method
     FOR SHARE;
    IF NOT FOUND
       OR current_state IS DISTINCT FROM 'active'
       OR current_revision IS DISTINCT FROM NEW.admission_rule_revision THEN
        RAISE EXCEPTION 'evidence result requires the current active rule revision' USING ERRCODE = '23514';
    END IF;
    IF NEW.checked_at > clock_timestamp() OR NEW.created_at > clock_timestamp() THEN
        RAISE EXCEPTION 'evidence timestamps cannot be in the future' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER evidence_results_challenge_binding
BEFORE INSERT ON evidence_results
FOR EACH ROW EXECUTE FUNCTION enforce_evidence_result_challenge_binding();

-- Immutability: committed evidence is history. Corrections are new rows bound
-- to a new challenge, never updates.
CREATE FUNCTION prevent_evidence_result_change() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'evidence results are append-only' USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER evidence_results_append_only
BEFORE UPDATE OR DELETE ON evidence_results
FOR EACH ROW EXECUTE FUNCTION prevent_evidence_result_change();
