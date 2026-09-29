ALTER TABLE syntheses ADD COLUMN IF NOT EXISTS failure_reason TEXT;
COMMENT ON COLUMN syntheses.failure_reason IS 'Reason set by SynthesisRepository::mark_failed when a synthesis transitions to status=failed; preserves stage error text for ops debugging.';
