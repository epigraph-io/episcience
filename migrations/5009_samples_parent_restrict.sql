-- Change parent_sample_id FK from SET NULL to RESTRICT.
-- PostgreSQL auto-named this constraint samples_parent_sample_id_fkey.
ALTER TABLE samples DROP CONSTRAINT IF EXISTS samples_parent_sample_id_fkey;
ALTER TABLE samples
    ADD CONSTRAINT samples_parent_sample_id_fkey
    FOREIGN KEY (parent_sample_id) REFERENCES samples(id)
    ON DELETE RESTRICT;
