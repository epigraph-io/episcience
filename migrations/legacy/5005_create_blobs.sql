-- migrations/5005_create_blobs.sql
-- Blob metadata table. Actual file content lives on filesystem at
-- EPISCIENCE_BLOB_DIR/{hash_hex[0:2]}/{hash_hex[2:4]}/{hash_hex}.blob
-- Content-addressed: duplicate uploads are deduplicated by BLAKE3 hash.

CREATE TABLE IF NOT EXISTS blobs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    filename TEXT NOT NULL,
    mime_type VARCHAR(255) NOT NULL,
    size_bytes BIGINT NOT NULL,
    content_hash BYTEA NOT NULL,
    uploader_id UUID NOT NULL REFERENCES agents(id) ON DELETE RESTRICT,
    sample_id UUID REFERENCES samples(id) ON DELETE SET NULL,
    labels TEXT[] NOT NULL DEFAULT '{}',
    properties JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT blobs_content_hash_length CHECK (octet_length(content_hash) = 32),
    CONSTRAINT blobs_size_positive CHECK (size_bytes > 0),
    CONSTRAINT blobs_filename_not_empty CHECK (length(trim(filename)) > 0)
);

CREATE INDEX IF NOT EXISTS idx_blobs_uploader ON blobs(uploader_id);
CREATE INDEX IF NOT EXISTS idx_blobs_sample ON blobs(sample_id) WHERE sample_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_blobs_hash ON blobs(content_hash);
CREATE INDEX IF NOT EXISTS idx_blobs_labels ON blobs USING GIN(labels);
CREATE INDEX IF NOT EXISTS idx_blobs_created ON blobs(created_at DESC);
