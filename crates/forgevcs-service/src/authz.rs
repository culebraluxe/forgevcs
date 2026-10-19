//! W6 authorization A/B harness: Scion policy and Culebra entitlements evaluate one request.
//!
//! Scion remains authoritative in `SCION_ONLY` (the default) and in `SHADOW_COMPARE`; Culebra's
//! decision is recorded and compared but cannot override Scion. `DUAL_ENFORCE` requires both to
//! allow. `CULEBRA_ONLY` is accepted by configuration but not served by this façade: the
//! vendored handlers still enforce Scion policy, so claiming Culebra-only would be false.
//!
//! Repository identity is a **separate mandatory layer** (FVCS-01): the vendored Scion policy is
//! path/ref-scoped and cannot see a repository name, so [`AuthorizationService`] additionally
//! evaluates [`RepoAcl`] and combines the two decisions with AND. `admin.repos` (the DevOps seat)
//! bypasses the repository layer for administration, not the path/ref policy.
//!
//! Culebra decisions come from the existing CulebraLuxe tables on the same Neon database:
//! `auth_identity → app_user → app_user_role → security_role → role_authority → authority`.
//! Service principals have no Culebra identity yet, so Culebra denies them — which is exactly the
//! mismatch shadow mode exists to expose. Source-control is a tech capability: `root` and
//! `tech.access` allow read/write/ref operations; `portal.read` allows reads only. That mapping
//! is provisional and recorded in `docs/adr/0004-authorization-ab.md`.

use std::collections::HashSet;

use deadpool_postgres::Pool;
use scion_core::Principal;
use scion_object::RepoPath;
use scion_policy::Policy;
use scion_proto::status_from_error;
use tonic::Status;

use crate::config::Config;
use crate::repo_acl::{ClaimOutcome, RepoAcl, RepoAction, RepoDecision};

/// The `FORGEVCS_AUTH_MODE` values (work order §10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    /// Scion only; no Culebra call, no decision row.
    ScionOnly,
    /// Scion decides; Culebra is evaluated and recorded, mismatch included.
    ShadowCompare,
    /// Both decide; allow only when both allow.
    DualEnforce,
    /// Accepted in configuration, not served: Scion handlers still enforce Scion.
    CulebraOnly,
}

impl AuthMode {
    /// Parse the environment value; `None`/unknown defaults to `SCION_ONLY` (the service logs
    /// an invalid value; `Config::validate` rejects it before readiness).
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some("SHADOW_COMPARE") => Self::ShadowCompare,
            Some("DUAL_ENFORCE") => Self::DualEnforce,
            Some("CULEBRA_ONLY") => Self::CulebraOnly,
            _ => Self::ScionOnly,
        }
    }

    /// Stable name recorded with each decision.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScionOnly => "SCION_ONLY",
            Self::ShadowCompare => "SHADOW_COMPARE",
            Self::DualEnforce => "DUAL_ENFORCE",
            Self::CulebraOnly => "CULEBRA_ONLY",
        }
    }
}

/// The source-control operations the A/B harness evaluates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Read a repository path.
    Read,
    /// Write a repository path.
    Write,
    /// Update a ref.
    UpdateRef,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::UpdateRef => "update_ref",
        }
    }
}

/// The DevOps capabilities the façade gates, and the single mapping from a capability to the
/// engine `[admin]` permit it requires (SPEC 6.2).
///
/// Repository lifecycle is `admin.repos`, never `admin.refs`: `admin.refs` is upstream's permit
/// for creating/deleting refs and is checked inside `RefService.CreateRef`; the façade does not
/// substitute it for repository or trunk authority. Every ForgeVCS check reads `permit()`, so a
/// code path cannot quietly drift to a different grant. `docs/M1-roles.md` mirrors this table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminCapability {
    /// Repository lifecycle and object administration — `admin.repos`.
    Repositories,
    /// Trunk advance and conflict-queue administration — `admin.trunk`.
    Trunk,
}

impl AdminCapability {
    /// The engine permit this capability requires.
    pub fn permit(self) -> scion_policy::AdminPerm {
        match self {
            Self::Repositories => scion_policy::AdminPerm::Repos,
            Self::Trunk => scion_policy::AdminPerm::Trunk,
        }
    }
}

/// One authorization outcome.
#[derive(Clone, Debug)]
pub struct Decision {
    /// Whether the request may proceed.
    pub allow: bool,
    /// Human-readable reason, safe for the caller only in the generic sense.
    pub reason: String,
}

fn allow() -> Decision {
    Decision {
        allow: true,
        reason: "allowed".to_owned(),
    }
}

fn deny(reason: impl Into<String>) -> Decision {
    Decision {
        allow: false,
        reason: reason.into(),
    }
}

fn internal(error: impl std::fmt::Display) -> Status {
    Status::internal(error.to_string())
}

fn repo_acl_unavailable(error: String) -> Status {
    // Fail closed: an ACL store that cannot answer is never an allow.
    Status::unavailable(format!("repository ACL store unavailable: {error}"))
}

fn repo_action(action: Action) -> RepoAction {
    match action {
        Action::Read => RepoAction::Read,
        Action::Write => RepoAction::Write,
        Action::UpdateRef => RepoAction::UpdateRef,
    }
}

/// The A/B authorization service shared by the façade.
pub struct AuthorizationService {
    mode: AuthMode,
    pool: Option<Pool>,
    /// Repository identity layer (FVCS-01), evaluated after the path/ref policy.
    repo_acl: RepoAcl,
}

const CREATE_DECISIONS: &str = r#"
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
)
"#;

const INSERT_DECISION: &str = r#"
INSERT INTO forgevcs_auth_decisions
    (subject, action, repository, path, ref_name, scion_allow, culebra_allow,
     final_allow, mismatch, mode, reason)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
"#;

const CULEBRA_AUTHORITIES: &str = r#"
SELECT r.code AS role_code, a.code AS authority_code
FROM auth_identity i
JOIN app_user u       ON u.id = i.app_user_id AND u.active
JOIN app_user_role ur ON ur.app_user_id = u.id
JOIN security_role r  ON r.id = ur.role_id AND r.active
LEFT JOIN role_authority ra ON ra.role_id = r.id
LEFT JOIN authority a       ON a.id = ra.authority_id
WHERE i.provider = $1 AND i.provider_subject = $2
"#;

impl AuthorizationService {
    /// Build the service from configuration. In `SCION_ONLY` service-principal decisions are not
    /// written; the pool is opened only to audit human decisions (the human CLI path requires
    /// it). The other modes create the pool for Culebra decisions and ensure the table exists.
    pub async fn from_config(config: &Config) -> Self {
        let mode = AuthMode::parse(config.auth_mode.as_deref());
        tracing::info!(mode = mode.as_str(), "authorization A/B mode parsed");
        // Repository identity is independent of the A/B mode: it is mandatory in `SCION_ONLY`
        // too (the hosted default), so it is built before the mode-specific early returns.
        let repo_acl = RepoAcl::from_config(config).await;
        if mode == AuthMode::ScionOnly {
            // Humans are audited in the hosted default (the CLI path depends on the table), while
            // agent traffic keeps the existing zero-write behavior.
            return Self {
                mode,
                pool: human_audit_pool(config).await,
                repo_acl,
            };
        }
        let Some(url) = config.database_url.as_deref() else {
            tracing::error!(
                mode = mode.as_str(),
                "FORGEVCS_AUTH_MODE needs FORGEVCS_DATABASE_URL for Culebra decisions"
            );
            return Self {
                mode,
                pool: None,
                repo_acl,
            };
        };
        let pool = match crate::db::build_pool(url) {
            Ok(pool) => pool,
            Err(error) => {
                tracing::error!(%error, "Culebra authorization pool did not start");
                return Self {
                    mode,
                    pool: None,
                    repo_acl,
                };
            }
        };
        if let Err(error) = ensure_schema(&pool).await {
            tracing::error!(%error, "could not ensure forgevcs_auth_decisions");
        }
        tracing::info!(mode = mode.as_str(), "authorization A/B mode active");
        Self {
            mode,
            pool: Some(pool),
            repo_acl,
        }
    }

    /// The configured mode.
    pub fn mode(&self) -> AuthMode {
        self.mode
    }

    /// Claim a repository for `principal` (called by `POST /repos/{repo}/init`). First writer
    /// wins; an existing owner is returned rather than overwritten.
    pub async fn claim_repo_owner(
        &self,
        repository: &str,
        principal: &Principal,
    ) -> Result<ClaimOutcome, Status> {
        self.repo_acl
            .claim(repository, principal)
            .await
            .map_err(repo_acl_unavailable)
    }

    /// The recorded owner, if any. Used by the façade to describe an idempotent `init`.
    pub async fn repository_owner(&self, repository: &str) -> Result<Option<String>, Status> {
        self.repo_acl
            .owner(repository)
            .await
            .map_err(repo_acl_unavailable)
    }

    /// Authorize a push by authorizing **each changeset delta path** (FVCS-02) instead of the
    /// coarse write on `/`. An empty delta has no path to scope, so it keeps the coarse root gate.
    /// The engine still re-checks every path inside `create_changeset` (defense in depth).
    pub async fn authorize_push(
        &self,
        policy: &Policy,
        principal: &Principal,
        repository: &str,
        ref_name: &str,
        delta_paths: &[String],
    ) -> Result<Decision, Status> {
        if delta_paths.is_empty() {
            return self
                .authorize(
                    policy,
                    principal,
                    repository,
                    Action::Write,
                    Some("/"),
                    Some(ref_name),
                )
                .await;
        }
        for path in delta_paths {
            let decision = self
                .authorize(
                    policy,
                    principal,
                    repository,
                    Action::Write,
                    Some(path),
                    Some(ref_name),
                )
                .await?;
            if !decision.allow {
                return Ok(decision);
            }
        }
        Ok(allow())
    }

    /// The repository layer, with the DevOps `admin.repos` bypass for administration.
    async fn repository_scope(
        &self,
        policy: &Policy,
        principal: &Principal,
        repository: &str,
        action: Action,
    ) -> Result<RepoDecision, Status> {
        if policy.can_admin(principal, AdminCapability::Repositories.permit()) {
            return Ok(RepoDecision::allow(
                "repository administrator (admin.repos)",
            ));
        }
        self.repo_acl
            .decide(principal, repository, repo_action(action))
            .await
            .map_err(repo_acl_unavailable)
    }

    /// The repository-identity layer alone, for operations whose upstream handler already
    /// enforces the path/ref policy (`Merge`, `Rebase`, `Land`, `CreateRef`).
    ///
    /// FVCS-01 requires repository identity on **every** business request; handlers that call a
    /// vendored handler directly must ask for this layer explicitly, otherwise the upstream
    /// path/ref check would let a principal act in a repository it does not own. The decision is
    /// audited for humans exactly like `authorize`'s repository half.
    pub async fn authorize_repository(
        &self,
        policy: &Policy,
        principal: &Principal,
        repository: &str,
        action: Action,
    ) -> Result<Decision, Status> {
        let decision = self
            .repository_scope(policy, principal, repository, action)
            .await?;
        if is_human(principal)
            && let Err(error) = self
                .record(
                    principal,
                    repository,
                    action,
                    None,
                    None,
                    decision.allow,
                    None,
                    decision.allow,
                    false,
                    &decision.reason,
                )
                .await
        {
            tracing::error!(%error, "could not record human repository decision");
            if self.mode == AuthMode::DualEnforce {
                return Err(Status::unavailable(
                    "authorization decision could not be audited; DUAL_ENFORCE fails closed",
                ));
            }
        }
        Ok(Decision {
            allow: decision.allow,
            reason: decision.reason,
        })
    }

    /// Evaluate one business request: path/ref policy AND repository identity. Culebra is added
    /// for every mode except `SCION_ONLY`. See the module docs for the combination rule.
    pub async fn authorize(
        &self,
        policy: &Policy,
        principal: &Principal,
        repository: &str,
        action: Action,
        path: Option<&str>,
        ref_name: Option<&str>,
    ) -> Result<Decision, Status> {
        self.evaluate(policy, principal, repository, action, path, ref_name, true)
            .await
    }

    /// Evaluate repository **provisioning** (`POST /repos/{repo}/init`). The repository's identity
    /// does not exist yet — the owner is claimed immediately after the engine boots — so this
    /// evaluates the path/ref policy only; it is not usable for business requests.
    pub async fn authorize_provisioning(
        &self,
        policy: &Policy,
        principal: &Principal,
        repository: &str,
        action: Action,
        path: Option<&str>,
        ref_name: Option<&str>,
    ) -> Result<Decision, Status> {
        self.evaluate(policy, principal, repository, action, path, ref_name, false)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn evaluate(
        &self,
        policy: &Policy,
        principal: &Principal,
        repository: &str,
        action: Action,
        path: Option<&str>,
        ref_name: Option<&str>,
        include_repository: bool,
    ) -> Result<Decision, Status> {
        let repo_path = match path {
            Some(raw) if !raw.is_empty() && raw != "/" => {
                RepoPath::parse(raw.as_bytes()).map_err(|error| status_from_error(&error))?
            }
            _ => RepoPath::root(),
        };
        let path_allow = match action {
            Action::Read => policy.can_read(principal, &repo_path),
            Action::Write => policy.can_write(principal, &repo_path),
            Action::UpdateRef => policy.can_update_ref_name(principal, ref_name.unwrap_or("")),
        };
        // Both layers are mandatory for business requests: the vendored path/ref policy AND
        // repository identity. Provisioning runs before an owner exists, so it skips the latter.
        let repo_decision = if include_repository {
            self.repository_scope(policy, principal, repository, action)
                .await?
        } else {
            RepoDecision::allow("repository provisioning")
        };
        let scion_allow = path_allow && repo_decision.allow;

        if self.mode == AuthMode::ScionOnly {
            let decision = if scion_allow {
                allow()
            } else if !path_allow {
                deny("Scion policy denied")
            } else {
                deny(repo_decision.reason)
            };
            // A human is audited even in the hosted default. Service principals keep their
            // existing zero-write behavior in `SCION_ONLY` (W6 shadow mode covers them).
            if is_human(principal)
                && let Err(error) = self
                    .record(
                        principal,
                        repository,
                        action,
                        path,
                        ref_name,
                        scion_allow,
                        None,
                        decision.allow,
                        false,
                        &decision.reason,
                    )
                    .await
            {
                tracing::error!(%error, "could not record human authorization decision");
            }
            return Ok(decision);
        }
        if self.mode == AuthMode::CulebraOnly {
            return Err(Status::unimplemented(
                "CULEBRA_ONLY is not served: the vendored handlers still enforce Scion policy",
            ));
        }

        let culebra = self.culebra(principal, action).await;
        let (culebra_allow, culebra_reason) = match &culebra {
            Ok(value) => (Some(*value), String::new()),
            Err(error) => (None, error.message().to_owned()),
        };
        if self.mode == AuthMode::DualEnforce && culebra.is_err() {
            return Err(Status::unavailable(
                "Culebra authorization is unavailable; DUAL_ENFORCE fails closed",
            ));
        }
        let final_allow = match self.mode {
            AuthMode::ShadowCompare => scion_allow,
            AuthMode::DualEnforce => scion_allow && culebra_allow.unwrap_or(false),
            AuthMode::ScionOnly | AuthMode::CulebraOnly => unreachable!("handled above"),
        };
        let mismatch = culebra_allow.is_some_and(|value| value != scion_allow);
        let reason = if final_allow {
            "allowed".to_owned()
        } else if !path_allow {
            "Scion policy denied".to_owned()
        } else if !repo_decision.allow {
            repo_decision.reason.clone()
        } else if !culebra_reason.is_empty() {
            format!("Culebra authorization: {culebra_reason}")
        } else {
            "Culebra policy denied".to_owned()
        };
        if let Err(error) = self
            .record(
                principal,
                repository,
                action,
                path,
                ref_name,
                scion_allow,
                culebra_allow,
                final_allow,
                mismatch,
                &reason,
            )
            .await
        {
            tracing::error!(%error, "could not record authorization decision");
            if self.mode == AuthMode::DualEnforce {
                return Err(Status::unavailable(
                    "authorization decision could not be audited; DUAL_ENFORCE fails closed",
                ));
            }
        }
        Ok(Decision {
            allow: final_allow,
            reason,
        })
    }

    async fn culebra(&self, principal: &Principal, action: Action) -> Result<bool, Status> {
        let Some(pool) = &self.pool else {
            return Err(Status::unavailable(
                "Culebra authorization has no database connection",
            ));
        };
        let (provider, subject) = match principal {
            Principal::GitHub { login, .. } => ("github", login.as_str()),
            Principal::Service(_) => {
                // Service principals have no Culebra app_user identity in V0. This is the
                // mismatch shadow mode is meant to surface, not an error.
                return Ok(false);
            }
        };
        let client = pool.get().await.map_err(internal)?;
        let rows = client
            .query(CULEBRA_AUTHORITIES, &[&provider, &subject])
            .await
            .map_err(internal)?;
        if rows.is_empty() {
            return Ok(false);
        }
        let mut roles = HashSet::new();
        let mut authorities = HashSet::new();
        for row in &rows {
            if let Ok(Some(role)) = row.try_get::<_, Option<String>>("role_code") {
                roles.insert(role);
            }
            if let Ok(Some(authority)) = row.try_get::<_, Option<String>>("authority_code") {
                authorities.insert(authority);
            }
        }
        Ok(roles.contains("root")
            || authorities.contains("tech.access")
            || (action == Action::Read && authorities.contains("portal.read")))
    }

    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        principal: &Principal,
        repository: &str,
        action: Action,
        path: Option<&str>,
        ref_name: Option<&str>,
        scion_allow: bool,
        culebra_allow: Option<bool>,
        final_allow: bool,
        mismatch: bool,
        reason: &str,
    ) -> Result<(), String> {
        self.record_row(
            &principal.id_string(),
            action.as_str(),
            repository,
            path,
            ref_name,
            scion_allow,
            culebra_allow,
            final_allow,
            mismatch,
            reason,
        )
        .await
    }

    /// Audit a human token mint in `forgevcs_auth_decisions`. The row records the identity and
    /// the outcome; it never contains the secret or the issued token. An absent audit store is
    /// reported to the caller (which logs it) rather than blocking the mint.
    pub async fn record_token_mint(&self, subject: &str) -> Result<(), String> {
        self.record_row(
            subject,
            "token_mint",
            "-",
            None,
            None,
            true,
            None,
            true,
            false,
            "human token minted",
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_row(
        &self,
        subject: &str,
        action: &str,
        repository: &str,
        path: Option<&str>,
        ref_name: Option<&str>,
        scion_allow: bool,
        culebra_allow: Option<bool>,
        final_allow: bool,
        mismatch: bool,
        reason: &str,
    ) -> Result<(), String> {
        let Some(pool) = &self.pool else {
            return Err("no database pool".to_owned());
        };
        let client = pool.get().await.map_err(|error| error.to_string())?;
        client
            .execute(
                INSERT_DECISION,
                &[
                    &subject,
                    &action,
                    &repository,
                    &path,
                    &ref_name,
                    &scion_allow,
                    &culebra_allow,
                    &final_allow,
                    &mismatch,
                    &self.mode.as_str(),
                    &reason,
                ],
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

/// A human principal is the `github:` identity the browser login also issues.
fn is_human(principal: &Principal) -> bool {
    matches!(principal, Principal::GitHub { .. })
}

/// Build the pool used to audit human decisions in `SCION_ONLY`.
async fn human_audit_pool(config: &Config) -> Option<Pool> {
    let url = config.database_url.as_deref()?;
    match crate::db::build_pool(url) {
        Ok(pool) => {
            if let Err(error) = ensure_schema(&pool).await {
                tracing::error!(%error, "could not ensure forgevcs_auth_decisions");
            }
            Some(pool)
        }
        Err(error) => {
            tracing::error!(%error, "human authorization audit pool did not start");
            None
        }
    }
}

async fn ensure_schema(pool: &Pool) -> Result<(), String> {
    let client = pool.get().await.map_err(|error| error.to_string())?;
    client
        .batch_execute(CREATE_DECISIONS)
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn modes_parse_and_name_their_wire_value() {
        assert_eq!(AuthMode::parse(None), AuthMode::ScionOnly);
        assert_eq!(AuthMode::parse(Some("SCION_ONLY")), AuthMode::ScionOnly);
        assert_eq!(
            AuthMode::parse(Some("SHADOW_COMPARE")),
            AuthMode::ShadowCompare
        );
        assert_eq!(AuthMode::parse(Some("DUAL_ENFORCE")), AuthMode::DualEnforce);
        assert_eq!(AuthMode::parse(Some("CULEBRA_ONLY")), AuthMode::CulebraOnly);
        assert_eq!(AuthMode::parse(Some("nonsense")), AuthMode::ScionOnly);
        assert_eq!(AuthMode::DualEnforce.as_str(), "DUAL_ENFORCE");
    }

    #[test]
    fn devops_capabilities_map_to_the_engine_permits() {
        use scion_policy::AdminPerm;
        // One mapping: repository lifecycle is `admin.repos`, trunk is `admin.trunk`.
        assert_eq!(
            AdminCapability::Repositories.permit(),
            AdminPerm::Repos,
            "repository lifecycle must use admin.repos, not admin.refs"
        );
        assert_eq!(AdminCapability::Trunk.permit(), AdminPerm::Trunk);
    }

    #[test]
    fn action_names_are_stable() {
        assert_eq!(Action::Read.as_str(), "read");
        assert_eq!(Action::Write.as_str(), "write");
        assert_eq!(Action::UpdateRef.as_str(), "update_ref");
    }
    use std::collections::BTreeMap;

    fn test_policy() -> Policy {
        Policy::parse(
            r#"
version = 1

[groups]
agents = ["service:agent-a", "service:agent-b", "service:maestro", "github:culebraluxe"]
devops = ["service:maestro", "github:culebraluxe"]

[[paths]]
prefix = "/"
read = ["group:agents"]
write = ["group:devops"]

[[paths]]
prefix = "/lane/a"
read = ["service:agent-a"]
write = ["service:agent-a"]

[[paths]]
prefix = "/lane/b"
read = ["service:agent-b"]
write = ["service:agent-b"]

[refs]
"refs/heads/*" = { update = ["group:agents"] }

[admin]
refs = ["group:devops"]
repos = ["group:devops"]
trunk = ["group:devops"]
"#,
        )
        .expect("test policy parses")
    }

    fn service_with_repo_acl() -> AuthorizationService {
        AuthorizationService {
            mode: AuthMode::ScionOnly,
            pool: None,
            repo_acl: RepoAcl::memory(BTreeMap::new()),
        }
    }

    fn principal(raw: &str) -> Principal {
        Principal::parse(raw).expect("test principal")
    }

    async fn claimed(service: &AuthorizationService, repository: &str, raw: &str) {
        service
            .claim_repo_owner(repository, &principal(raw))
            .await
            .expect("claim repository");
    }

    #[tokio::test]
    async fn cross_repository_read_and_write_are_denied_by_repository_identity() {
        let service = service_with_repo_acl();
        let policy = test_policy();
        let agent_a = principal("service:agent-a");
        claimed(&service, "pilot-a", "service:agent-a").await;
        claimed(&service, "pilot-b", "service:agent-b").await;

        assert!(
            service
                .authorize(&policy, &agent_a, "pilot-a", Action::Read, None, None)
                .await
                .expect("read own repo")
                .allow
        );
        assert!(
            service
                .authorize(
                    &policy,
                    &agent_a,
                    "pilot-a",
                    Action::Write,
                    Some("lane/a/x"),
                    None,
                )
                .await
                .expect("write own repo")
                .allow
        );
        let cross_read = service
            .authorize(&policy, &agent_a, "pilot-b", Action::Read, None, None)
            .await
            .expect("read other repo");
        assert!(
            !cross_read.allow,
            "agent-a must not read agent-b's repository"
        );
        let cross_write = service
            .authorize(
                &policy,
                &agent_a,
                "pilot-b",
                Action::Write,
                Some("lane/a/x"),
                None,
            )
            .await
            .expect("write other repo");
        assert!(
            !cross_write.allow,
            "agent-a must not write agent-b's repository"
        );
        let cross_ref = service
            .authorize(
                &policy,
                &agent_a,
                "pilot-b",
                Action::UpdateRef,
                None,
                Some("refs/heads/lane"),
            )
            .await
            .expect("update ref in other repo");
        assert!(
            !cross_ref.allow,
            "agent-a must not update refs in agent-b's repository"
        );
    }

    #[tokio::test]
    async fn repository_admin_bypasses_identity_but_not_path_policy() {
        let service = service_with_repo_acl();
        let policy = test_policy();
        let admin = principal("service:maestro");
        claimed(&service, "pilot-b", "service:agent-b").await;

        assert!(
            service
                .authorize(&policy, &admin, "pilot-b", Action::Read, None, None)
                .await
                .expect("admin read")
                .allow,
            "admin.repos may read a repository it does not own"
        );
        assert!(
            !service
                .authorize(
                    &policy,
                    &admin,
                    "pilot-b",
                    Action::Write,
                    Some("lane/a/x"),
                    None,
                )
                .await
                .expect("admin write")
                .allow,
            "the admin bypass is repository identity only; path grants still apply"
        );
    }

    #[tokio::test]
    async fn push_authorizes_each_delta_path_and_refuses_outside_the_grant() {
        let service = service_with_repo_acl();
        let policy = test_policy();
        let agent_a = principal("service:agent-a");
        claimed(&service, "pilot-a", "service:agent-a").await;

        let allowed = service
            .authorize_push(
                &policy,
                &agent_a,
                "pilot-a",
                "refs/heads/lane",
                &["lane/a/one.rs".to_owned()],
            )
            .await
            .expect("push");
        assert!(allowed.allow, "write on lane/a/** may push lane/a/**");

        let outside = service
            .authorize_push(
                &policy,
                &agent_a,
                "pilot-a",
                "refs/heads/lane",
                &["lane/b/other.rs".to_owned()],
            )
            .await
            .expect("push");
        assert!(
            !outside.allow,
            "write on lane/a/** must not push outside it"
        );

        let mixed = service
            .authorize_push(
                &policy,
                &agent_a,
                "pilot-a",
                "refs/heads/lane",
                &["lane/a/one.rs".to_owned(), "src/lib.rs".to_owned()],
            )
            .await
            .expect("push");
        assert!(!mixed.allow, "one refused path refuses the whole push");

        let empty = service
            .authorize_push(&policy, &agent_a, "pilot-a", "refs/heads/lane", &[])
            .await
            .expect("push");
        assert!(
            !empty.allow,
            "an empty delta has no path to scope and keeps the coarse write-on-root gate"
        );

        let cross_repo = service
            .authorize_push(
                &policy,
                &agent_a,
                "pilot-b",
                "refs/heads/lane",
                &["lane/a/one.rs".to_owned()],
            )
            .await
            .expect("push");
        assert!(!cross_repo.allow, "repository identity applies to push");
    }
    #[tokio::test]
    async fn repository_identity_applies_to_upstream_checked_operations() {
        // Merge/Rebase/Land/CreateRef call a vendored handler directly, so the façade asks for
        // the repository layer itself (qa finding). Every action must still be scoped.
        let service = service_with_repo_acl();
        let policy = test_policy();
        let agent_a = principal("service:agent-a");
        claimed(&service, "pilot-a", "service:agent-a").await;
        claimed(&service, "pilot-b", "service:agent-b").await;

        for action in [Action::Read, Action::Write, Action::UpdateRef] {
            assert!(
                service
                    .authorize_repository(&policy, &agent_a, "pilot-a", action)
                    .await
                    .expect("own repository")
                    .allow,
                "owner must pass {action:?}"
            );
            let refused = service
                .authorize_repository(&policy, &agent_a, "pilot-b", action)
                .await
                .expect("other repository");
            assert!(
                !refused.allow,
                "cross-repository {action:?} must be refused for merge/rebase/land/ref-create"
            );
            assert!(refused.reason.contains("cross-repository"));
        }
    }

    #[tokio::test]
    async fn a_human_principal_is_authorized_like_any_github_login() {
        let service = service_with_repo_acl();
        let policy = test_policy();
        let human = principal("github:culebraluxe");
        claimed(&service, "human-repo", "github:culebraluxe").await;
        claimed(&service, "pilot-b", "service:agent-b").await;

        assert!(
            service
                .authorize(
                    &policy,
                    &human,
                    "human-repo",
                    Action::Write,
                    Some("docs/readme.md"),
                    Some("refs/heads/lane"),
                )
                .await
                .expect("human write own repo")
                .allow
        );
        // One refusal: path policy still governs where the human may write. The longest write
        // rule for `lane/a/**` names only `service:agent-a`, so the DevOps human is refused even
        // though its `/` write grant and `admin.repos` bypass would allow the repository.
        let refused = service
            .authorize(
                &policy,
                &human,
                "human-repo",
                Action::Write,
                Some("lane/a/x"),
                Some("refs/heads/lane"),
            )
            .await
            .expect("human refused path");
        assert!(!refused.allow);
        assert_eq!(refused.reason, "Scion policy denied");
        assert_eq!(
            service.record_token_mint("github:culebraluxe").await,
            Err("no database pool".to_owned()),
            "the audit is optional in a storage-less process, never an allow decision"
        );
    }

    #[tokio::test]
    async fn provisioning_skips_repository_identity_but_business_requests_deny() {
        let service = service_with_repo_acl();
        let policy = test_policy();
        let admin = principal("service:maestro");

        // `init` runs before an owner exists, so provisioning evaluates the path/ref policy only.
        assert!(
            service
                .authorize_provisioning(
                    &policy,
                    &admin,
                    "brand-new-repo",
                    Action::Write,
                    Some("/"),
                    None,
                )
                .await
                .expect("provisioning")
                .allow,
            "the DevOps seat may initialize a repository that has no owner yet"
        );
        // The same request as a business write is denied by the repository layer: an ownerless
        // repository is not readable/writable by anyone except via the admin bypass. Here the
        // admin bypass applies, so assert an agent is the one refused.
        let agent_a = principal("service:agent-a");
        assert!(
            !service
                .authorize(
                    &policy,
                    &agent_a,
                    "brand-new-repo",
                    Action::Write,
                    Some("lane/a/x"),
                    None,
                )
                .await
                .expect("business write")
                .allow,
            "an unowned repository must not be writable by an agent"
        );
    }
}
