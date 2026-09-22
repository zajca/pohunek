-- Relay account linking: provider-neutral proof of an additional stable OIDC identity.
-- A link changes durable authority only after the current relay actor and the new
-- identity both prove themselves inside one audited transaction. Email, display
-- name and provider profile attributes are never linking inputs, so no column
-- here stores them.

-- Per-principal generation that invalidates every in-flight link transaction as
-- soon as the account's identity set changes.
ALTER TABLE principals
    ADD COLUMN account_link_generation BIGINT NOT NULL DEFAULT 1
        CHECK (account_link_generation > 0);

CREATE FUNCTION prevent_account_link_generation_rewind() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.account_link_generation < OLD.account_link_generation THEN
        RAISE EXCEPTION 'account link generation cannot move backwards' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER principals_account_link_generation_monotonic
BEFORE UPDATE OF account_link_generation ON principals
FOR EACH ROW EXECUTE FUNCTION prevent_account_link_generation_rewind();

-- A removed identity keeps its row so credential and browser-session provenance
-- stays resolvable, while only an active identity claims an (issuer, subject)
-- coordinate. This partial unique index is the authoritative collision and
-- self-link constraint for linking.
ALTER TABLE oidc_identities DROP CONSTRAINT oidc_identities_issuer_subject_key;

CREATE UNIQUE INDEX oidc_identities_active_coordinate_idx
    ON oidc_identities(issuer, subject) WHERE removed_at IS NULL;

ALTER TABLE oidc_identities
    ADD COLUMN linked_via_link_id UUID,
    ADD COLUMN removed_via_link_id UUID,
    ADD CONSTRAINT oidc_identities_removal_provenance_chk
        CHECK (removed_via_link_id IS NULL OR removed_at IS NOT NULL);

-- One completed link transaction can create at most one identity, so a replayed
-- completion cannot mint a second identity from the same proof.
CREATE UNIQUE INDEX oidc_identities_link_once_idx
    ON oidc_identities(linked_via_link_id) WHERE linked_via_link_id IS NOT NULL;

ALTER TABLE account_link_transactions
    -- Possession channel; a browser transaction is completed only by a browser
    -- session and a device transaction only by a bearer credential.
    ADD COLUMN channel TEXT NOT NULL CHECK (channel IN ('browser', 'device')),
    -- Exact authentication row that initiated the transaction, with the
    -- generation it had then, so a rotated or revoked source cannot complete it.
    ADD COLUMN source_authentication_id UUID NOT NULL,
    ADD COLUMN source_authentication_generation BIGINT NOT NULL
        CHECK (source_authentication_generation > 0),
    ADD COLUMN issuer TEXT NOT NULL CHECK (char_length(issuer) BETWEEN 1 AND 2048),
    ADD COLUMN client_id TEXT NOT NULL CHECK (char_length(client_id) BETWEEN 1 AND 1024),
    ADD COLUMN audience TEXT NOT NULL CHECK (char_length(audience) BETWEEN 1 AND 1024),
    -- Keyed digest of the one-use possession secret handed to the caller: the
    -- host-only binding cookie for a browser link, the poll secret for a device
    -- link. The raw value never reaches the database.
    ADD COLUMN possession_digest BYTEA NOT NULL CHECK (octet_length(possession_digest) = 32),
    ADD COLUMN digest_key_id TEXT NOT NULL CHECK (char_length(digest_key_id) BETWEEN 1 AND 128),
    ADD COLUMN state TEXT NOT NULL
        CHECK (state IN ('pending', 'completed', 'cancelled', 'expired', 'failed')),
    ADD COLUMN revision BIGINT NOT NULL CHECK (revision > 0),
    ADD COLUMN linked_identity_id UUID,
    ADD COLUMN correlation_id UUID NOT NULL,
    ADD COLUMN idempotency_key UUID NOT NULL,
    ADD CONSTRAINT account_link_transactions_terminal_chk
        CHECK ((state = 'pending') = (completed_at IS NULL)),
    ADD CONSTRAINT account_link_transactions_result_chk
        CHECK ((state = 'completed') = (linked_identity_id IS NOT NULL)),
    -- An account can never link an identity it already holds.
    ADD CONSTRAINT account_link_transactions_not_self_chk
        CHECK (linked_identity_id IS NULL OR linked_identity_id <> source_identity_id),
    ADD CONSTRAINT account_link_transactions_idempotency_key
        UNIQUE (principal_id, idempotency_key),
    -- The created identity must belong to the same principal that proved the
    -- transaction, which forecloses cross-principal completion in the database.
    ADD CONSTRAINT account_link_transactions_result_owner_fk
        FOREIGN KEY (linked_identity_id, principal_id)
        REFERENCES oidc_identities(identity_id, principal_id) ON DELETE RESTRICT;

ALTER TABLE oidc_identities
    ADD CONSTRAINT oidc_identities_linked_via_fk
        FOREIGN KEY (linked_via_link_id)
        REFERENCES account_link_transactions(link_id) ON DELETE RESTRICT,
    ADD CONSTRAINT oidc_identities_removed_via_fk
        FOREIGN KEY (removed_via_link_id)
        REFERENCES account_link_transactions(link_id) ON DELETE RESTRICT;

-- At most one in-flight link per account: two concurrent starts serialize on
-- this index instead of racing to create two provable transactions.
CREATE UNIQUE INDEX account_link_transactions_single_pending_idx
    ON account_link_transactions(principal_id) WHERE state = 'pending';

CREATE INDEX account_link_transactions_principal_idx
    ON account_link_transactions(principal_id, link_id);

-- At most one unconsumed provider transaction per link, so two callbacks or two
-- device polls cannot both reach completion.
CREATE UNIQUE INDEX browser_logins_active_link_idx
    ON browser_logins(link_id) WHERE consumed_at IS NULL AND link_id IS NOT NULL;

CREATE UNIQUE INDEX device_logins_active_link_idx
    ON device_logins(link_id) WHERE consumed_at IS NULL AND link_id IS NOT NULL;

CREATE FUNCTION prevent_account_link_provenance_change() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.link_id IS DISTINCT FROM OLD.link_id
       OR NEW.principal_id IS DISTINCT FROM OLD.principal_id
       OR NEW.source_identity_id IS DISTINCT FROM OLD.source_identity_id
       OR NEW.channel IS DISTINCT FROM OLD.channel
       OR NEW.source_authentication_id IS DISTINCT FROM OLD.source_authentication_id
       OR NEW.source_authentication_generation IS DISTINCT FROM OLD.source_authentication_generation
       OR NEW.issuer IS DISTINCT FROM OLD.issuer
       OR NEW.client_id IS DISTINCT FROM OLD.client_id
       OR NEW.audience IS DISTINCT FROM OLD.audience
       OR NEW.possession_digest IS DISTINCT FROM OLD.possession_digest
       OR NEW.digest_key_id IS DISTINCT FROM OLD.digest_key_id
       OR NEW.account_link_generation IS DISTINCT FROM OLD.account_link_generation
       OR NEW.recovery_generation IS DISTINCT FROM OLD.recovery_generation
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
       OR NEW.expires_at IS DISTINCT FROM OLD.expires_at
       OR NEW.correlation_id IS DISTINCT FROM OLD.correlation_id
       OR NEW.idempotency_key IS DISTINCT FROM OLD.idempotency_key THEN
        RAISE EXCEPTION 'account link transaction provenance is immutable' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER account_link_transactions_provenance_immutable
BEFORE UPDATE ON account_link_transactions
FOR EACH ROW EXECUTE FUNCTION prevent_account_link_provenance_change();

-- A terminal transaction stays terminal and every update advances the revision,
-- so a replayed completion cannot revive or silently rewrite a closed link.
CREATE FUNCTION enforce_account_link_transition() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.state <> 'pending' THEN
        RAISE EXCEPTION 'account link transaction is already terminal' USING ERRCODE = '23514';
    END IF;
    IF NEW.revision <= OLD.revision THEN
        RAISE EXCEPTION 'account link transaction revision must advance' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER account_link_transactions_transition_guard
BEFORE UPDATE ON account_link_transactions
FOR EACH ROW EXECUTE FUNCTION enforce_account_link_transition();
