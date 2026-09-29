-- Migration 5001: Add signature_meaning to provenance_log
--
-- Evidence: 21 CFR Part 11 requires electronic signatures to include the
-- printed name, date/time, and MEANING of the signature (e.g. authorship,
-- review, approval). W3C PROV-O models this as hadRole on wasAssociatedWith.
--
-- Reasoning: TEXT with CHECK constraint (not ENUM) allows vocabulary evolution.
-- Nullable for backward compatibility with existing EpiGraph rows.

ALTER TABLE provenance_log
    ADD COLUMN IF NOT EXISTS signature_meaning VARCHAR(50)
    CHECK (signature_meaning IS NULL OR signature_meaning IN (
        'authored', 'witnessed', 'approved', 'reviewed', 'certified', 'countersigned'
    ));

CREATE INDEX IF NOT EXISTS idx_provenance_log_sig_meaning
    ON provenance_log(signature_meaning)
    WHERE signature_meaning IS NOT NULL;

COMMENT ON COLUMN provenance_log.signature_meaning IS
    'W3C PROV-O role qualifier: authored, witnessed, approved, reviewed, certified, countersigned';
