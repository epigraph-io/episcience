-- Prevent two protocols sharing the same (supersedes, version) pair.
CREATE UNIQUE INDEX IF NOT EXISTS idx_protocols_supersedes_version
    ON protocols(supersedes, version)
    WHERE supersedes IS NOT NULL;

-- Root protocols (no supersedes) must always be version 1.
ALTER TABLE protocols
    ADD CONSTRAINT protocols_root_version_is_one
    CHECK (supersedes IS NOT NULL OR version = 1);
