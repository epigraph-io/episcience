-- Migration 5003: Sample and material tracking for ELN
--
-- Evidence: Nanotech lab requires chain-of-custody for physical materials —
-- DNA origami, protein constructs, substrates, reagents, aliquots.
--
-- Reasoning:
-- - Hierarchical: parent_sample_id enables aliquots and derived samples
-- - Status lifecycle: prepared → in_use → consumed/disposed → archived
-- - BLAKE3 content_hash for integrity verification
-- - JSONB properties for domain-specific metadata without schema changes
-- - Junction table sample_claims links samples to EpiGraph observations

CREATE TABLE IF NOT EXISTS samples (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    sample_type VARCHAR(50) NOT NULL,
    status VARCHAR(30) NOT NULL DEFAULT 'prepared'
        CHECK (status IN ('prepared', 'in_use', 'consumed', 'disposed', 'archived')),
    parent_sample_id UUID REFERENCES samples(id) ON DELETE SET NULL,
    prepared_by UUID NOT NULL REFERENCES agents(id) ON DELETE RESTRICT,
    preparation_date TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expiry_date TIMESTAMPTZ,
    storage_location TEXT,
    quantity_value DOUBLE PRECISION,
    quantity_unit VARCHAR(30),
    hazard_info JSONB DEFAULT '{}',
    labels TEXT[] NOT NULL DEFAULT '{}',
    properties JSONB NOT NULL DEFAULT '{}',
    content_hash BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT samples_name_not_empty CHECK (length(trim(name)) > 0),
    CONSTRAINT samples_content_hash_length CHECK (octet_length(content_hash) = 32)
);

CREATE INDEX IF NOT EXISTS idx_samples_type ON samples(sample_type);
CREATE INDEX IF NOT EXISTS idx_samples_status ON samples(status);
CREATE INDEX IF NOT EXISTS idx_samples_prepared_by ON samples(prepared_by);
CREATE INDEX IF NOT EXISTS idx_samples_parent ON samples(parent_sample_id) WHERE parent_sample_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_samples_labels ON samples USING GIN(labels);
CREATE INDEX IF NOT EXISTS idx_samples_properties ON samples USING GIN(properties);
CREATE INDEX IF NOT EXISTS idx_samples_created_at ON samples(created_at DESC);

CREATE TABLE IF NOT EXISTS sample_claims (
    sample_id UUID NOT NULL REFERENCES samples(id) ON DELETE CASCADE,
    claim_id UUID NOT NULL REFERENCES claims(id) ON DELETE CASCADE,
    relationship VARCHAR(30) NOT NULL DEFAULT 'observation'
        CHECK (relationship IN ('observation', 'measurement', 'characterization', 'preparation_note')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (sample_id, claim_id)
);

CREATE INDEX IF NOT EXISTS idx_sample_claims_claim ON sample_claims(claim_id);

-- Reuse EpiGraph's update_updated_at_column() trigger function (from migration 001)
CREATE TRIGGER samples_updated_at
    BEFORE UPDATE ON samples
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();
