CREATE TABLE synthesis_shares (
    synthesis_id          UUID NOT NULL REFERENCES syntheses(id) ON DELETE CASCADE,
    shared_with_agent_id  UUID NOT NULL,
    shared_by_agent_id    UUID NOT NULL,
    granted_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    permission            TEXT NOT NULL DEFAULT 'read',
    PRIMARY KEY (synthesis_id, shared_with_agent_id),
    CHECK (permission = 'read')
);

CREATE INDEX synthesis_shares_recipient_idx
    ON synthesis_shares (shared_with_agent_id);
