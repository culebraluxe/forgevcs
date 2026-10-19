# Work order — human CLI path (2026-10-19)

**Why:** ForgeVCS today has only service-principal tokens (`/auth/token`); no browser login, no
human principal, no `git`-like CLI, no Git wire protocol. The principal is a human and needs to run
the same operations agents do. Identity for this work: user `culebraluxe`, email
`culebraluxe@gmail.com`.

**Scope v1 (small vertical, not 180h):**
1. Human principal: add `culebraluxe` as a first-class principal (id/email/display), with an explicit
   token-mint path for a human (bearer token stored locally, 0600; no password in code). Audit-log
   the mint and every decision (existing `forgevcs_auth_decisions`).
2. `forgevcs` CLI (thin wrapper over the REST façade) with the commands a human needs:
   `auth` (mint/status), `init`, `refs`, `log`, `pull`, `push` (upload changeset), `merge`,
   `rebase`, `merge-land`, `workspace create/status/destroy`, `health`.
   Local config under `~/.config/forgevcs/`; target base URL configurable; no deploy/publish.
3. Acceptance: as `culebraluxe`, create a repo, push a changeset, read refs/log, merge, pull it
   back — all from the CLI, no browser. Tests for principal authorization and each command's happy
   path + one refusal.
4. Explicit non-goals: Git-compatible `git clone/push` (separate, large), browser UI, production
   enablement without principal sign-off.

**Estimate:** 1–2 engineer-days for the horizontal wrapper + principal, not weeks. The 180–220h
multi-agent estimate is not accepted; it includes speculative items unrelated to this path.
