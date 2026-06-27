-- SciLink Lesson 4: stamp which autonomy level produced a synthesis.
-- NULL is equivalent to 'autopilot' (private, countersign required).
ALTER TABLE syntheses
    ADD COLUMN IF NOT EXISTS autonomy_level TEXT
        CHECK (autonomy_level IN ('co_pilot', 'autopilot', 'autonomous'));
