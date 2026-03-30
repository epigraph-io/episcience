-- migrations/5006_create_countersignatures.sql
-- Countersignatures: a second agent signs an existing claim to attest
-- they witnessed, reviewed, or approved the content.

CREATE TABLE IF NOT EXISTS countersignatures (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    claim_id UUID NOT NULL REFERENCES claims(id) ON DELETE RESTRICT,
    signer_id UUID NOT NULL REFERENCES agents(id) ON DELETE RESTRICT,
    signature_meaning VARCHAR(50) NOT NULL
        CHECK (signature_meaning IN ('witnessed', 'approved', 'reviewed', 'certified', 'countersigned')),
    content_hash BYTEA NOT NULL,
    signature BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT cs_content_hash_length CHECK (octet_length(content_hash) = 32),
    CONSTRAINT cs_signature_length CHECK (octet_length(signature) = 64),
    CONSTRAINT cs_unique_signer_claim UNIQUE (claim_id, signer_id, signature_meaning)
);

CREATE INDEX IF NOT EXISTS idx_cs_claim ON countersignatures(claim_id);
CREATE INDEX IF NOT EXISTS idx_cs_signer ON countersignatures(signer_id);
CREATE INDEX IF NOT EXISTS idx_cs_created ON countersignatures(created_at DESC);
