-- W5 persistence: workspace metadata and its dirty-file overlay live in Neon, so any
-- container can restore a workspace after scale-to-zero or a different instance.
-- The service also runs this idempotent DDL when WorkspaceService connects.
CREATE TABLE IF NOT EXISTS forgevcs_workspaces (
    id         UUID PRIMARY KEY,
    repo       TEXT NOT NULL,
    state      JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS forgevcs_workspace_files (
    workspace_id UUID NOT NULL REFERENCES forgevcs_workspaces(id) ON DELETE CASCADE,
    path         TEXT NOT NULL,
    mode         INT2 NOT NULL,
    data         BYTEA,
    PRIMARY KEY (workspace_id, path)
);
