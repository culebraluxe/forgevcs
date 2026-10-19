-- R4: per-workspace write lease (WO-R4).
--
-- A workspace is a disposable working copy restored from Neon on every request, so two agents
-- writing the same workspace would silently last-writer-win on the persisted overlay. The lease
-- makes the workspace single-writer: one owner row with a fencing token, a heartbeat and an
-- expiry. An expired row may be taken over, and the takeover is recorded in the event table.
--
-- The service also runs this DDL at startup from `crates/forgevcs-service/src/lease.rs`
-- (`ensure_schema`), so a deployment without a migration runner still gets the tables. Keep the
-- two definitions in step; the runtime copy is the bootstrap, not a second schema.

CREATE TABLE IF NOT EXISTS forgevcs_workspace_leases (
    workspace_id UUID PRIMARY KEY REFERENCES forgevcs_workspaces(id) ON DELETE CASCADE,
    repo         TEXT NOT NULL,
    owner_id     TEXT NOT NULL,
    lease_id     UUID NOT NULL,
    acquired_at  TIMESTAMPTZ NOT NULL,
    heartbeat_at TIMESTAMPTZ NOT NULL,
    expires_at   TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS forgevcs_workspace_leases_expiry
    ON forgevcs_workspace_leases (repo, expires_at);

-- Audit trail for lease ownership changes. Deliberately no foreign key to
-- `forgevcs_workspaces`: destroy deletes the workspace (and cascades the lease row), and the
-- takeover/release evidence must outlive the workspace it describes.
CREATE TABLE IF NOT EXISTS forgevcs_workspace_lease_events (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id   UUID NOT NULL,
    repo           TEXT NOT NULL,
    event          TEXT NOT NULL,
    owner_id       TEXT NOT NULL,
    prior_owner_id TEXT,
    lease_id       UUID NOT NULL,
    at             TIMESTAMPTZ NOT NULL,
    detail         JSONB
);

CREATE INDEX IF NOT EXISTS forgevcs_workspace_lease_events_workspace
    ON forgevcs_workspace_lease_events (workspace_id, at DESC);
CREATE INDEX IF NOT EXISTS forgevcs_workspace_lease_events_repo
    ON forgevcs_workspace_lease_events (repo, at DESC);
