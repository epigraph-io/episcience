CREATE TABLE synthesis_claim_membership (
    synthesis_id  UUID NOT NULL REFERENCES syntheses(id) ON DELETE CASCADE,
    claim_id      UUID NOT NULL,
    PRIMARY KEY (synthesis_id, claim_id)
);

CREATE INDEX synthesis_claim_membership_claim_idx
    ON synthesis_claim_membership (claim_id);
CREATE INDEX synthesis_claim_membership_synthesis_idx
    ON synthesis_claim_membership (synthesis_id);
