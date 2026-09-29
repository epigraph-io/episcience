-- Migration 5002: Full-text search index on claims.content
--
-- Evidence: Lab notebooks need natural-language search across observations.
-- Semantic (vector) search is great for "find similar" but grep-style keyword
-- search is essential for "find every mention of DNA origami."
--
-- Reasoning: Generated tsvector column avoids recomputing on every query.
-- GIN index provides fast full-text search. 'english' config handles stemming.

ALTER TABLE claims
    ADD COLUMN IF NOT EXISTS content_tsv tsvector
    GENERATED ALWAYS AS (to_tsvector('english', content)) STORED;

CREATE INDEX IF NOT EXISTS idx_claims_content_tsv
    ON claims USING GIN(content_tsv);

COMMENT ON COLUMN claims.content_tsv IS
    'Auto-generated tsvector for full-text search (EpiScience migration 5002)';
