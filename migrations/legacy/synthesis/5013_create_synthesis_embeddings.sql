CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE synthesis_embeddings (
    synthesis_id      UUID PRIMARY KEY REFERENCES syntheses(id) ON DELETE CASCADE,
    embedding         VECTOR(1536) NOT NULL,  -- matches epigraph's primary embedding dim
                                              -- (verified at /home/jeremy/epigraph/migrations/001_initial_schema.sql:610);
                                              -- spec §171 typo'd this as 1024
    embedding_model   TEXT NOT NULL,
    embedding_input   TEXT NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (embedding_input IN ('narrative_head','title_plus_query','summary_concat'))
);

CREATE INDEX synthesis_embeddings_hnsw_idx
    ON synthesis_embeddings
    USING hnsw (embedding vector_cosine_ops);
