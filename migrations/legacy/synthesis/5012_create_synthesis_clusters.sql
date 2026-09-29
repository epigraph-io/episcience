CREATE TABLE synthesis_clusters (
    id                UUID PRIMARY KEY,
    synthesis_id      UUID NOT NULL REFERENCES syntheses(id) ON DELETE CASCADE,
    cluster_index     INT  NOT NULL,
    title             TEXT NOT NULL,
    summary           TEXT NOT NULL,
    member_claim_ids  UUID[] NOT NULL,
    support_count     INT  NOT NULL,
    contradict_count  INT  NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (synthesis_id, cluster_index),
    CHECK (cardinality(member_claim_ids) > 0),
    CHECK (length(title) <= 200),
    CHECK (support_count >= 0 AND contradict_count >= 0)
);

CREATE INDEX synthesis_clusters_synthesis_idx ON synthesis_clusters (synthesis_id);
