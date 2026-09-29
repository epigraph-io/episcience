-- Migration 5004: Lab protocol (SOP) table
--
-- Evidence: Reproducible science requires versioned, traceable protocols.
-- Every experiment must reference the specific protocol version used.
--
-- Reasoning:
-- - Versioned via supersedes chain + version counter
-- - Steps stored as JSONB array for flexibility (order, duration, temperature, notes)
-- - Equipment list for instrument dependency tracking
-- - BLAKE3 content_hash over steps for change detection

CREATE TABLE IF NOT EXISTS protocols (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    title TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    authored_by UUID NOT NULL REFERENCES agents(id) ON DELETE RESTRICT,
    steps JSONB NOT NULL DEFAULT '[]',
    equipment TEXT[] DEFAULT '{}',
    safety_notes TEXT,
    supersedes UUID REFERENCES protocols(id),
    labels TEXT[] NOT NULL DEFAULT '{}',
    properties JSONB NOT NULL DEFAULT '{}',
    content_hash BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT protocols_title_not_empty CHECK (length(trim(title)) > 0),
    CONSTRAINT protocols_content_hash_length CHECK (octet_length(content_hash) = 32)
);

CREATE INDEX IF NOT EXISTS idx_protocols_authored_by ON protocols(authored_by);
CREATE INDEX IF NOT EXISTS idx_protocols_labels ON protocols USING GIN(labels);
CREATE INDEX IF NOT EXISTS idx_protocols_supersedes ON protocols(supersedes) WHERE supersedes IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_protocols_created_at ON protocols(created_at DESC);

CREATE TRIGGER protocols_updated_at
    BEFORE UPDATE ON protocols
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();
