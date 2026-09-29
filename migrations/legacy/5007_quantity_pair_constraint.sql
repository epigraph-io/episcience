-- Enforce that quantity_value and quantity_unit are either both present or both NULL.
ALTER TABLE samples
    ADD CONSTRAINT samples_quantity_pair
    CHECK ((quantity_value IS NULL) = (quantity_unit IS NULL));
