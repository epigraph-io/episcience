CREATE TABLE synthesis_staleness_events (
    id                 UUID PRIMARY KEY,
    synthesis_id       UUID NOT NULL REFERENCES syntheses(id) ON DELETE CASCADE,
    detected_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    trigger            TEXT NOT NULL,
    affected_claim_ids UUID[] NOT NULL,
    detail             JSONB NULL,
    CHECK (trigger IN ('belief_drift','new_contradiction','claim_superseded','frame_changed','edge_revoked'))
);

CREATE INDEX synthesis_staleness_events_synthesis_detected_idx
    ON synthesis_staleness_events (synthesis_id, detected_at DESC);

CREATE TABLE episcience_worker_state (
    worker_id      TEXT PRIMARY KEY,
    last_event_id  TEXT NULL,
    last_event_ts  TIMESTAMPTZ NULL,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
