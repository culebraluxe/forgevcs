-- W6: one row per authorization decision evaluated by the A/B harness.
--
-- The service also runs this DDL at authorization startup (`AuthorizationService::from_config`),
-- so a deployment without a migration runner still gets the table. Keep the two definitions in
-- step; the runtime copy is the bootstrap, not a second schema.
CREATE TABLE IF NOT EXISTS forgevcs_auth_decisions (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    subject       TEXT NOT NULL,
    action        TEXT NOT NULL,
    repository    TEXT NOT NULL,
    path          TEXT,
    ref_name      TEXT,
    scion_allow   BOOLEAN NOT NULL,
    culebra_allow BOOLEAN,
    final_allow   BOOLEAN NOT NULL,
    mismatch      BOOLEAN NOT NULL,
    mode          TEXT NOT NULL,
    reason        TEXT
);

CREATE INDEX IF NOT EXISTS forgevcs_auth_decisions_at
    ON forgevcs_auth_decisions (at DESC);
CREATE INDEX IF NOT EXISTS forgevcs_auth_decisions_mismatch
    ON forgevcs_auth_decisions (mismatch, at DESC)
    WHERE mismatch;
