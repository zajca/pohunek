-- Issue #92: versioned admission rules and append-only external evidence results.
-- Additive only: no existing table is altered. The broker extension boundary
-- (evidence_challenges, introduced in 0002) stays untouched; this migration
-- adds the durable rule registry and the committed evidence/result rows that
-- the future internal evidence endpoint will write.
--
-- Rule semantics (RFC 13.2/13.3): one active rule of each provider kind per
-- team at most. Google Workspace rules bind an exact hosted domain; GitHub
-- rules bind an exact organization login. Revisions are monotonic per rule so
-- evidence can pin the exact policy it was checked against. Local deny is a
-- membership-level generation that self-service admission must honor; it is
-- stored on memberships.local_deny_generation (0001) and never reset by joins.

CREATE TABLE admission_rules (
    admission_rule_id UUID PRIMARY KEY,
    team_id UUID NOT NULL REFERENCES teams(team_id) ON DELETE RESTRICT,
    provider TEXT NOT NULL CHECK (provider IN ('google', 'github')),
    method TEXT NOT NULL CHECK (method IN ('google_hosted_domain', 'github_organization')),
    -- Exact match coordinate: Workspace hosted domain or GitHub org login.
    match_value TEXT NOT NULL CHECK (char_length(match_value) BETWEEN 1 AND 256),
    revision BIGINT NOT NULL CHECK (revision > 0),
    state TEXT NOT NULL CHECK (state IN ('active', 'disabled')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK (
        (provider = 'google' AND method = 'google_hosted_domain')
        OR (provider = 'github' AND method = 'github_organization')
    ),
    UNIQUE (admission_rule_id, team_id),
    UNIQUE (admission_rule_id, revision)
);

-- At most one active rule of each provider kind per team. Disabled revisions
-- retain history; re-enabling is a new revision via the management API.
CREATE UNIQUE INDEX admission_rules_active_team_provider_idx
    ON admission_rules(team_id, provider)
    WHERE state = 'active';

-- Monotonic revision guard: a rule row may only advance its own revision.
CREATE FUNCTION enforce_admission_rule_revision() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.revision < OLD.revision THEN
        RAISE EXCEPTION 'admission rule revision cannot move backwards' USING ERRCODE = '23514';
    END IF;
    IF NEW.team_id IS DISTINCT FROM OLD.team_id
       OR NEW.provider IS DISTINCT FROM OLD.provider
       OR NEW.method IS DISTINCT FROM OLD.method
       OR NEW.match_value IS DISTINCT FROM OLD.match_value THEN
        RAISE EXCEPTION 'admission rule identity is immutable' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER admission_rules_revision_monotonic
BEFORE UPDATE OF team_id, provider, method, match_value, revision ON admission_rules
FOR EACH ROW EXECUTE FUNCTION enforce_admission_rule_revision();

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
    FOREIGN KEY (admission_rule_id, team_id)
        REFERENCES admission_rules(admission_rule_id, team_id) ON DELETE RESTRICT,
    FOREIGN KEY (admission_rule_id, admission_rule_revision)
        REFERENCES admission_rules(admission_rule_id, revision) ON DELETE RESTRICT
);

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

-- Rule management is a team administration action owned by owner/admin roles.
INSERT INTO permissions (permission) VALUES
    ('team.admission.manage')
ON CONFLICT DO NOTHING;
