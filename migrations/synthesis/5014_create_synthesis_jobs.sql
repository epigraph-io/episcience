CREATE TABLE synthesis_jobs (
    id              UUID PRIMARY KEY REFERENCES syntheses(id) ON DELETE CASCADE,
    job_type        TEXT NOT NULL DEFAULT 'synthesis',
    payload         JSONB NOT NULL,
    state           TEXT NOT NULL DEFAULT 'queued',
    attempts        INT  NOT NULL DEFAULT 0,
    max_attempts    INT  NOT NULL DEFAULT 3,
    scheduled_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at      TIMESTAMPTZ NULL,
    completed_at    TIMESTAMPTZ NULL,
    last_error      TEXT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (state IN ('queued','running','complete','failed','retry'))
);

CREATE INDEX synthesis_jobs_state_scheduled_idx
    ON synthesis_jobs (state, scheduled_at)
    WHERE state IN ('queued','retry');
