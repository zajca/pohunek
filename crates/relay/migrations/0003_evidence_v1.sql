-- Issue #92: versioned admission rules and append-only external evidence results.
-- Self-contained and additive: the only pre-existing object touched is the 0002
-- evidence_challenges boundary, which gains one admission_rule_id binding column.
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
    FOREIGN KEY (admission_rule_id, current_revision)
        REFERENCES admission_rule_versions(admission_rule_id, revision) ON DELETE RESTRICT
);

-- At most one active rule of each provider kind per team. Disabled revisions
-- retain history; re-enabling is a new revision via the management API.
CREATE UNIQUE INDEX admission_rules_active_team_provider_idx
    ON admission_rules(team_id, provider)
    WHERE state = 'active';

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

-- The current projection's identity is immutable; only state and the revision
-- pointer may change. Rewriting the identity in place would defeat the
-- append-only versioning and let a later revision rewrite history.
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
-- challenge cannot be attached to a different rule or team at result time.
ALTER TABLE evidence_challenges
    ADD COLUMN admission_rule_id UUID REFERENCES admission_rules(admission_rule_id) ON DELETE RESTRICT;
-- Append-only committed evidence results. Each row pairs one consumed
-- evidence_challenges row with the broker attestation the future verifier
-- accepted or rejected. The outcome column mirrors EvidenceOutcome; only
-- 'eligible' rows may authorize a self-service join, and that join is a
-- separate membership transaction, never an automatic effect of this insert.
CREATE TABLE evidence_results (
    evidence_id UUID PRIMARY KEY,
    challenge_id UUID NOT NULL UNIQUE REFERENCES evidence_challenges(challenge_id) ON DELETE RESTRICT,
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
    -- RFC 13.2: evidence is usable for at most 60 minutes after the actual
    -- upstream check. The ceiling is enforced here so no writer can persist a
    -- longer-lived proof even if the verifier is bypassed.
    CHECK (expires_at <= checked_at + INTERVAL '60 minutes'),
    -- Evidence references the frozen rule revision, never the mutable projection.
    FOREIGN KEY (admission_rule_id, admission_rule_revision)
        REFERENCES admission_rule_versions(admission_rule_id, revision) ON DELETE RESTRICT
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
    v_provider TEXT;
    v_method TEXT;
BEGIN
    SELECT principal_id, team_id, admission_rule_id, admission_rule_revision
      INTO c_principal, c_team, c_rule, c_revision
      FROM evidence_challenges
     WHERE challenge_id = NEW.challenge_id
     FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'evidence challenge not found' USING ERRCODE = '23514';
    END IF;
    IF c_principal IS DISTINCT FROM NEW.principal_id
       OR c_team IS DISTINCT FROM NEW.team_id
       OR c_rule IS DISTINCT FROM NEW.admission_rule_id
       OR c_revision IS DISTINCT FROM NEW.admission_rule_revision THEN
        RAISE EXCEPTION 'evidence result coordinates do not match its challenge' USING ERRCODE = '23514';
    END IF;
    SELECT provider, method INTO v_provider, v_method
      FROM admission_rule_versions
     WHERE admission_rule_id = NEW.admission_rule_id
       AND revision = NEW.admission_rule_revision;
    IF v_provider IS DISTINCT FROM NEW.provider OR v_method IS DISTINCT FROM NEW.method THEN
        RAISE EXCEPTION 'evidence result provider/method do not match its rule version' USING ERRCODE = '23514';
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
