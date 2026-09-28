CREATE TABLE syntheses (
    id                   UUID PRIMARY KEY,
    query                TEXT NOT NULL,
    agent_id             UUID NOT NULL,
    status               TEXT NOT NULL,
    parent_synthesis_id  UUID NULL REFERENCES syntheses(id),
    narrative            TEXT NULL,
    narrative_format     TEXT NULL,
    subgraph_snapshot    JSONB NOT NULL,
    clustering_method    TEXT NOT NULL,
    llm_provider         TEXT NOT NULL,
    llm_model            TEXT NOT NULL,
    llm_call_count       INT  NOT NULL DEFAULT 0,
    prereq_synthesis_ids UUID[] NULL,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at         TIMESTAMPTZ NULL,
    stale_since          TIMESTAMPTZ NULL,
    stale_reason         TEXT NULL,
    content_hash         BYTEA NOT NULL,
    visibility           TEXT NOT NULL DEFAULT 'private',

    CHECK ((status = 'complete') = (narrative IS NOT NULL)),
    CHECK ((status = 'complete') = (completed_at IS NOT NULL)),
    CHECK ((stale_since IS NULL) = (stale_reason IS NULL)),
    CHECK (octet_length(content_hash) = 32),
    CHECK (visibility IN ('private', 'shared', 'public')),
    CHECK (status IN ('pending', 'running', 'complete', 'failed', 'deleted')),
    CHECK (stale_reason IS NULL OR stale_reason IN
           ('belief_drift', 'new_contradiction', 'claim_superseded', 'frame_changed', 'edge_revoked')),
    CHECK (narrative_format IS NULL OR narrative_format = 'markdown'),
    CHECK (clustering_method = 'signed_louvain')
);

CREATE INDEX syntheses_agent_created_idx ON syntheses (agent_id, created_at DESC);
CREATE INDEX syntheses_status_idx ON syntheses (status) WHERE status IN ('pending', 'running');
CREATE INDEX syntheses_stale_idx ON syntheses (stale_since) WHERE stale_since IS NOT NULL;
