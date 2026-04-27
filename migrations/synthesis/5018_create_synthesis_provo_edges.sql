CREATE TABLE synthesis_provo_edges (
    synthesis_id      UUID NOT NULL REFERENCES syntheses(id) ON DELETE CASCADE,
    predicate         TEXT NOT NULL,
    target_kind       TEXT NOT NULL,
    target_id         UUID NOT NULL,
    written_at        TIMESTAMPTZ NULL,
    epigraph_edge_id  UUID NULL,
    attempt_count     INT NOT NULL DEFAULT 0,
    last_error        TEXT NULL,
    PRIMARY KEY (synthesis_id, predicate, target_kind, target_id),
    CHECK (predicate IN ('WAS_DERIVED_FROM','REFINES','COMPOSED_OF','ATTRIBUTED_TO')),
    CHECK (target_kind IN ('claim','synthesis','agent'))
);

CREATE INDEX synthesis_provo_edges_pending_idx
    ON synthesis_provo_edges (synthesis_id)
    WHERE written_at IS NULL;
