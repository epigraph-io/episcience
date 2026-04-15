-- Add prev_signature_hash for hash-chaining countersignatures.
-- Add signature_version to support canonical message versioning.
ALTER TABLE countersignatures
    ADD COLUMN IF NOT EXISTS prev_signature_hash BYTEA,
    ADD COLUMN IF NOT EXISTS signature_version   SMALLINT NOT NULL DEFAULT 1;

ALTER TABLE countersignatures
    ADD CONSTRAINT cs_prev_hash_length
    CHECK (prev_signature_hash IS NULL OR octet_length(prev_signature_hash) = 32);

CREATE INDEX IF NOT EXISTS idx_cs_prev_hash
    ON countersignatures(prev_signature_hash)
    WHERE prev_signature_hash IS NOT NULL;
