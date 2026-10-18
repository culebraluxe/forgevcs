-- ForgeVCS conflict escalation queue (M1).
--
-- The service also creates this table at startup with CREATE TABLE IF NOT EXISTS
-- (crates/forgevcs-service/src/escalations.rs); this file is the readable record, matching the
-- other ForgeVCS-owned tables. There is no migration runner yet (W2 gap), so the runtime DDL is
-- the one that runs.

CREATE TABLE IF NOT EXISTS forgevcs_conflict_escalations (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    repo        TEXT NOT NULL,
    ref_name    TEXT NOT NULL,
    head        TEXT NOT NULL,
    onto        TEXT,
    base        TEXT,
    note        TEXT,
    report      JSONB,
    status      TEXT NOT NULL DEFAULT 'open',
    created_by  TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    claimed_by  TEXT,
    claimed_at  TIMESTAMPTZ,
    resolution  TEXT,
    resolved_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS forgevcs_conflict_escalations_status
    ON forgevcs_conflict_escalations (repo, status, created_at);
