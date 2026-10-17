//! W6 authorization A/B harness: Scion policy and Culebra entitlements evaluate one request.
//!
//! Scion remains authoritative in `SCION_ONLY` (the default) and in `SHADOW_COMPARE`; Culebra's
//! decision is recorded and compared but cannot override Scion. `DUAL_ENFORCE` requires both to
//! allow. `CULEBRA_ONLY` is accepted by configuration but not served by this façade: the
//! vendored handlers still enforce Scion policy, so claiming Culebra-only would be false.
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

/// The A/B authorization service shared by the façade.
pub struct AuthorizationService {
    mode: AuthMode,
    pool: Option<Pool>,
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
    /// Build the service from configuration. In `SCION_ONLY` no database connection is opened;
    /// the other modes create a small pool and ensure the decision table exists.
    pub async fn from_config(config: &Config) -> Self {
        let mode = AuthMode::parse(config.auth_mode.as_deref());
        tracing::info!(mode = mode.as_str(), "authorization A/B mode parsed");
        if mode == AuthMode::ScionOnly {
            return Self { mode, pool: None };
        }
        let Some(url) = config.database_url.as_deref() else {
            tracing::error!(
                mode = mode.as_str(),
                "FORGEVCS_AUTH_MODE needs FORGEVCS_DATABASE_URL for Culebra decisions"
            );
            return Self { mode, pool: None };
        };
        let pool = match crate::db::build_pool(url) {
            Ok(pool) => pool,
            Err(error) => {
                tracing::error!(%error, "Culebra authorization pool did not start");
                return Self { mode, pool: None };
            }
        };
        if let Err(error) = ensure_schema(&pool).await {
            tracing::error!(%error, "could not ensure forgevcs_auth_decisions");
        }
        tracing::info!(mode = mode.as_str(), "authorization A/B mode active");
        Self {
            mode,
            pool: Some(pool),
        }
    }

    /// The configured mode.
    pub fn mode(&self) -> AuthMode {
        self.mode
    }

    /// Evaluate one request. Scion is always evaluated; Culebra is added for every mode except
    /// `SCION_ONLY`. See the module docs for the combination rule.
    pub async fn authorize(
        &self,
        policy: &Policy,
        principal: &Principal,
        repository: &str,
        action: Action,
        path: Option<&str>,
        ref_name: Option<&str>,
    ) -> Result<Decision, Status> {
        let repo_path = match path {
            Some(raw) if !raw.is_empty() && raw != "/" => {
                RepoPath::parse(raw.as_bytes()).map_err(|error| status_from_error(&error))?
            }
            _ => RepoPath::root(),
        };
        let scion_allow = match action {
            Action::Read => policy.can_read(principal, &repo_path),
            Action::Write => policy.can_write(principal, &repo_path),
            Action::UpdateRef => policy.can_update_ref_name(principal, ref_name.unwrap_or("")),
        };

        if self.mode == AuthMode::ScionOnly {
            return Ok(if scion_allow {
                allow()
            } else {
                deny("Scion policy denied")
            });
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
        } else if !scion_allow {
            "Scion policy denied".to_owned()
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
        let Some(pool) = &self.pool else {
            return Err("no database pool".to_owned());
        };
        let client = pool.get().await.map_err(|error| error.to_string())?;
        let subject = principal.id_string();
        client
            .execute(
                INSERT_DECISION,
                &[
                    &subject,
                    &action.as_str(),
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

async fn ensure_schema(pool: &Pool) -> Result<(), String> {
    let client = pool.get().await.map_err(|error| error.to_string())?;
    client
        .batch_execute(CREATE_DECISIONS)
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
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
    fn action_names_are_stable() {
        assert_eq!(Action::Read.as_str(), "read");
        assert_eq!(Action::Write.as_str(), "write");
        assert_eq!(Action::UpdateRef.as_str(), "update_ref");
    }
}
