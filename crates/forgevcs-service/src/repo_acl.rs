//! Repository-scoped authorization (FVCS-01 / production packet P0-1).
//!
//! `acl.toml` is path- and ref-scoped only: the vendored Scion policy has no repository identity,
//! and its parser is strict (`deny_unknown_fields`), so a `[repos]` table in `acl.toml` would
//! make the file unloadable. Repository identity is therefore evaluated **here, in the service**,
//! as a second, mandatory layer in front of every business request:
//!
//! ```text
//! token -> principal -> Scion path/ref policy AND repository ACL -> handler
//! ```
//!
//! A repository has exactly one owner. The first successful `POST /repos/{repo}/init` claims it
//! in `forgevcs_repo_acl` (first-writer wins, so an idempotent init or a concurrent instance
//! cannot reassign it). A request is allowed when the principal is the owner, is listed as a
//! reader (read only) or writer (read/write) in an operator-configured rule, or holds
//! `admin.repos` (the DevOps seat, checked by the caller in [`crate::authz`]). Everything else is
//! denied by default — including a repository that has no owner row yet, because an unowned
//! repository is not a repository whose contents any agent may read.
//!
//! Operators can pre-assign rules with `FORGEVCS_REPO_ACLS` (JSON), which is how the
//! multi-agent pilot gives `service:agent-a` its own repository when the DevOps seat performs the
//! `init`:
//!
//! ```json
//! {"pilot-a":{"owner":"service:agent-a","readers":[],"writers":["service:agent-a"]}}
//! ```
//!
//! A malformed value fails `Config::validate`, so a typo cannot silently disable isolation.
//!
//! Storage: a dedicated table in the same Neon database (`forgevcs_repo_acl`), created
//! idempotently at startup exactly like `forgevcs_auth_decisions` (W6 precedent). When no
//! database is configured (unit tests, a storage-less process) the registry is in-memory; that
//! mode is for tests and an explicitly storage-less pilot only.

use std::collections::{BTreeMap, BTreeSet};

use deadpool_postgres::Pool;
use scion_core::Principal;
use serde::Deserialize;

use crate::config::Config;

/// Idempotent schema for the repository ACL registry (service-owned, like W6's decision table).
const CREATE_REPO_ACL: &str = r#"
CREATE TABLE IF NOT EXISTS forgevcs_repo_acl (
    repository TEXT PRIMARY KEY,
    owner      TEXT NOT NULL,
    readers    JSONB NOT NULL DEFAULT '[]'::jsonb,
    writers    JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
)
"#;

/// The repository-scoped action a request wants to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoAction {
    Read,
    Write,
    UpdateRef,
}

/// One operator-configured repository rule. `writers` imply read; `owner` implies both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRule {
    pub owner: String,
    pub readers: BTreeSet<String>,
    pub writers: BTreeSet<String>,
}

impl RepoRule {
    pub fn new(
        owner: impl Into<String>,
        readers: impl IntoIterator<Item = String>,
        writers: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            owner: owner.into(),
            readers: readers.into_iter().collect(),
            writers: writers.into_iter().collect(),
        }
    }

    /// Evaluate one principal against this rule.
    pub fn decide(&self, principal: &str, action: RepoAction) -> RepoDecision {
        if principal == self.owner {
            return RepoDecision::allow("repository owner");
        }
        let granted = match action {
            RepoAction::Read => {
                self.readers.contains(principal) || self.writers.contains(principal)
            }
            RepoAction::Write | RepoAction::UpdateRef => self.writers.contains(principal),
        };
        if granted {
            RepoDecision::allow("repository ACL grant")
        } else {
            RepoDecision::deny(
                "repository ACL denied: principal is not the owner or a granted member",
            )
        }
    }
}

/// One repository-access outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoDecision {
    pub allow: bool,
    pub reason: String,
}

impl RepoDecision {
    pub fn allow(reason: impl Into<String>) -> Self {
        Self {
            allow: true,
            reason: reason.into(),
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            allow: false,
            reason: reason.into(),
        }
    }
}

/// What happened when a repository owner was claimed at `init`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// This principal is the first owner recorded.
    Claimed,
    /// The repository was already owned by the same principal (idempotent init).
    AlreadyOwner,
    /// The repository is owned by someone else; only an admin may re-initialize it.
    OwnedByOther { owner: String },
}

enum Backend {
    /// Assigned owners in memory (no database configured, or unit tests).
    Memory(tokio::sync::RwLock<BTreeMap<String, String>>),
    /// Assigned owners in Neon; errors are surfaced, never silently converted to "allowed".
    Postgres(Pool),
}

/// The repository ACL registry: configured rules plus owners assigned on `init`.
pub struct RepoAcl {
    configured: BTreeMap<String, RepoRule>,
    backend: Backend,
}

impl RepoAcl {
    /// A registry from operator-configured rules with in-memory ownership (tests / no DB).
    pub fn memory(configured: BTreeMap<String, RepoRule>) -> Self {
        Self {
            configured,
            backend: Backend::Memory(tokio::sync::RwLock::new(BTreeMap::new())),
        }
    }

    /// Build the registry from process configuration. A configured database gets the durable
    /// table; no database falls back to memory (tests and an explicitly storage-less process).
    pub async fn from_config(config: &Config) -> Self {
        let configured = match config.repo_acls.as_deref() {
            Some(raw) => match parse_configured(raw) {
                Ok(rules) => rules,
                Err(error) => {
                    // `Config::validate` rejects this before startup; keep the process alive
                    // rather than panic if a caller bypassed validation, but disable configured
                    // grants (the default deny still applies).
                    tracing::error!(%error, "FORGEVCS_REPO_ACLS is invalid; configured grants ignored");
                    BTreeMap::new()
                }
            },
            None => BTreeMap::new(),
        };
        let Some(url) = config.database_url.as_deref() else {
            tracing::warn!(
                "no database configured: repository ACLs are in-memory and do not survive restart"
            );
            return Self::memory(configured);
        };
        match crate::db::build_pool(url) {
            Ok(pool) => {
                if let Err(error) = ensure_schema(&pool).await {
                    tracing::error!(%error, "could not ensure forgevcs_repo_acl");
                }
                Self {
                    configured,
                    backend: Backend::Postgres(pool),
                }
            }
            Err(error) => {
                tracing::error!(%error, "repository ACL pool did not start; using memory");
                Self::memory(configured)
            }
        }
    }

    /// The operator-configured rule for a repository, when one exists.
    pub fn configured_rule(&self, repository: &str) -> Option<&RepoRule> {
        self.configured.get(repository)
    }

    /// The recorded owner, if any. A configured rule's owner wins over the stored owner.
    pub async fn owner(&self, repository: &str) -> Result<Option<String>, String> {
        if let Some(rule) = self.configured.get(repository) {
            return Ok(Some(rule.owner.clone()));
        }
        match &self.backend {
            Backend::Memory(owners) => Ok(owners.read().await.get(repository).cloned()),
            Backend::Postgres(pool) => {
                let client = pool.get().await.map_err(|error| error.to_string())?;
                let row = client
                    .query_opt(
                        "SELECT owner FROM forgevcs_repo_acl WHERE repository = $1",
                        &[&repository],
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(row.map(|row| row.get::<_, String>("owner")))
            }
        }
    }

    /// Claim a repository for `principal`. First writer wins; an existing owner is returned
    /// unchanged so a concurrent init on another instance cannot steal the repository.
    pub async fn claim(
        &self,
        repository: &str,
        principal: &Principal,
    ) -> Result<ClaimOutcome, String> {
        let owner = principal.id_string();
        // A configured rule is authoritative: `init` cannot reassign it, and a mismatched caller
        // must be a repository admin (checked by the façade).
        if let Some(rule) = self.configured.get(repository) {
            return Ok(if rule.owner == owner {
                ClaimOutcome::AlreadyOwner
            } else {
                ClaimOutcome::OwnedByOther {
                    owner: rule.owner.clone(),
                }
            });
        }
        match &self.backend {
            Backend::Memory(owners) => {
                let mut owners = owners.write().await;
                match owners.get(repository) {
                    Some(existing) if existing == &owner => Ok(ClaimOutcome::AlreadyOwner),
                    Some(existing) => Ok(ClaimOutcome::OwnedByOther {
                        owner: existing.clone(),
                    }),
                    None => {
                        owners.insert(repository.to_owned(), owner.clone());
                        Ok(ClaimOutcome::Claimed)
                    }
                }
            }
            Backend::Postgres(pool) => {
                let client = pool.get().await.map_err(|error| error.to_string())?;
                client
                    .execute(
                        "INSERT INTO forgevcs_repo_acl (repository, owner) VALUES ($1, $2)
                         ON CONFLICT (repository) DO NOTHING",
                        &[&repository, &owner],
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                let stored: Option<String> = client
                    .query_opt(
                        "SELECT owner FROM forgevcs_repo_acl WHERE repository = $1",
                        &[&repository],
                    )
                    .await
                    .map_err(|error| error.to_string())?
                    .map(|row| row.get(0));
                match stored {
                    Some(stored) if stored == owner => Ok(ClaimOutcome::Claimed),
                    Some(stored) => Ok(ClaimOutcome::OwnedByOther { owner: stored }),
                    None => Err("repository ACL row did not persist".to_owned()),
                }
            }
        }
    }

    /// Decide whether `principal` may perform `action` inside `repository`.
    ///
    /// Default deny: an ownerless repository, an unknown repository and a mismatched principal all
    /// deny. The caller must apply the `admin.repos` bypass before calling this (see
    /// [`crate::authz::AuthorizationService::authorize`]).
    pub async fn decide(
        &self,
        principal: &Principal,
        repository: &str,
        action: RepoAction,
    ) -> Result<RepoDecision, String> {
        if let Some(rule) = self.configured.get(repository) {
            return Ok(rule.decide(&principal.id_string(), action));
        }
        let Some(owner) = self.owner(repository).await? else {
            return Ok(RepoDecision::deny(
                "repository has no owner; only an administrator may initialize it",
            ));
        };
        if owner == principal.id_string() {
            return Ok(RepoDecision::allow("repository owner"));
        }
        Ok(RepoDecision::deny(
            "repository is owned by another principal; cross-repository access is denied",
        ))
    }
}

/// Parse `FORGEVCS_REPO_ACLS`: a JSON object `repository -> {owner, readers?, writers?}`.
///
/// Unknown fields are rejected, every principal must parse (`service:<name>` /
/// `github:<login>`), repository names use the same rule `POST /init` enforces, and unknown
/// fields are typo-guards rather than silently ignored.
pub fn parse_configured(raw: &str) -> Result<BTreeMap<String, RepoRule>, String> {
    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawRule {
        owner: String,
        #[serde(default)]
        readers: Vec<String>,
        #[serde(default)]
        writers: Vec<String>,
    }

    let raw_rules: BTreeMap<String, RawRule> =
        serde_json::from_str(raw).map_err(|error| format!("not a JSON object: {error}"))?;
    let mut rules = BTreeMap::new();
    for (repository, raw_rule) in raw_rules {
        if !valid_repo_name(&repository) {
            return Err(format!(
                "repository {repository:?} is not a valid repository name (1-100 lowercase [a-z0-9._-], not starting '.', not containing '..')"
            ));
        }
        let owner = normalised_principal(&raw_rule.owner)?;
        let readers = raw_rule
            .readers
            .iter()
            .map(|principal| normalised_principal(principal))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let writers = raw_rule
            .writers
            .iter()
            .map(|principal| normalised_principal(principal))
            .collect::<Result<BTreeSet<_>, _>>()?;
        rules.insert(
            repository,
            RepoRule {
                owner,
                readers,
                writers,
            },
        );
    }
    Ok(rules)
}

/// The repository-name rule, shared with the façade so config and `POST /init` cannot disagree.
pub fn valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && !name.contains("..")
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
}

fn normalised_principal(raw: &str) -> Result<String, String> {
    Principal::parse(raw.trim())
        .map(|principal| principal.id_string())
        .map_err(|_| {
            format!("{raw:?} is not a principal (expected service:<name> or github:<login>)")
        })
}

async fn ensure_schema(pool: &Pool) -> Result<(), String> {
    let client = pool.get().await.map_err(|error| error.to_string())?;
    client
        .batch_execute(CREATE_REPO_ACL)
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn principal(raw: &str) -> Principal {
        Principal::parse(raw).expect("test principal")
    }

    fn rule(owner: &str, readers: &[&str], writers: &[&str]) -> RepoRule {
        RepoRule::new(
            owner,
            readers.iter().map(|value| (*value).to_owned()),
            writers.iter().map(|value| (*value).to_owned()),
        )
    }

    #[test]
    fn configured_rules_parse_and_validate() {
        let rules = parse_configured(
            r#"{
                "pilot-a": {"owner":"service:agent-a","writers":["service:agent-a"]},
                "pilot-b": {"owner":"service:agent-b","readers":["github:alice"]}
            }"#,
        )
        .expect("valid configured rules");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules["pilot-a"].owner, "service:agent-a");
        assert!(rules["pilot-b"].readers.contains("github:alice"));
    }

    #[test]
    fn configured_rules_reject_typos_bad_principals_and_bad_names() {
        assert!(parse_configured(r#"{"pilot-a":{"owner":"agent-a"}}"#).is_err());
        assert!(parse_configured(r#"{"Pilot A":{"owner":"service:a"}}"#).is_err());
        assert!(
            parse_configured(r#"{"pilot-a":{"owner":"service:a","extra":true}}"#).is_err(),
            "unknown fields are rejected"
        );
        // Listing the owner again is redundant but harmless: operators do it naturally, and
        // `RepoRule::decide` already short-circuits on ownership.
        assert!(
            parse_configured(r#"{"pilot-a":{"owner":"service:a","readers":["service:a"]}}"#)
                .is_ok()
        );
        assert!(parse_configured("[]").is_err());
    }

    #[test]
    fn valid_repository_names_match_the_facade_rule() {
        for name in ["forgevcs", "team-1", "a.b_c"] {
            assert!(valid_repo_name(name), "{name}");
        }
        for name in ["", ".hidden", "a..b", "Upper", "a/b", &"x".repeat(101)] {
            assert!(!valid_repo_name(name), "{name}");
        }
    }

    #[tokio::test]
    async fn first_init_claims_ownership_and_a_second_principal_is_refused() {
        let acl = RepoAcl::memory(BTreeMap::new());
        let agent_a = principal("service:agent-a");
        let agent_b = principal("service:agent-b");

        assert_eq!(
            acl.claim("pilot-a", &agent_a).await,
            Ok(ClaimOutcome::Claimed)
        );
        assert_eq!(
            acl.claim("pilot-a", &agent_a).await,
            Ok(ClaimOutcome::AlreadyOwner)
        );
        assert_eq!(
            acl.claim("pilot-a", &agent_b).await,
            Ok(ClaimOutcome::OwnedByOther {
                owner: "service:agent-a".to_owned()
            })
        );
    }

    #[tokio::test]
    async fn owner_reads_and_writes_own_repository_but_not_another() {
        let acl = RepoAcl::memory(BTreeMap::new());
        let agent_a = principal("service:agent-a");
        let agent_b = principal("service:agent-b");
        acl.claim("pilot-a", &agent_a).await.expect("claim a");
        acl.claim("pilot-b", &agent_b).await.expect("claim b");

        for action in [RepoAction::Read, RepoAction::Write, RepoAction::UpdateRef] {
            assert!(
                acl.decide(&agent_a, "pilot-a", action)
                    .await
                    .expect("decide")
                    .allow,
                "owner may {action:?} its own repository"
            );
        }
        for action in [RepoAction::Read, RepoAction::Write, RepoAction::UpdateRef] {
            let decision = acl
                .decide(&agent_a, "pilot-b", action)
                .await
                .expect("decide");
            assert!(
                !decision.allow,
                "cross-repository {action:?} must be refused"
            );
            assert!(decision.reason.contains("cross-repository"));
        }
    }

    #[tokio::test]
    async fn configured_readers_and_writers_get_only_their_grants() {
        let mut configured = BTreeMap::new();
        configured.insert(
            "team".to_owned(),
            rule("service:maestro", &["service:reader"], &["service:writer"]),
        );
        let acl = RepoAcl::memory(configured);
        let owner = principal("service:maestro");
        let reader = principal("service:reader");
        let writer = principal("service:writer");
        let stranger = principal("service:stranger");

        assert!(
            acl.decide(&owner, "team", RepoAction::Write)
                .await
                .expect("decide")
                .allow
        );
        assert!(
            acl.decide(&reader, "team", RepoAction::Read)
                .await
                .expect("decide")
                .allow
        );
        assert!(
            !acl.decide(&reader, "team", RepoAction::Write)
                .await
                .expect("decide")
                .allow
        );
        assert!(
            acl.decide(&writer, "team", RepoAction::Read)
                .await
                .expect("decide")
                .allow
        );
        assert!(
            acl.decide(&writer, "team", RepoAction::Write)
                .await
                .expect("decide")
                .allow
        );
        assert!(
            !acl.decide(&stranger, "team", RepoAction::Read)
                .await
                .expect("decide")
                .allow
        );
    }

    #[tokio::test]
    async fn unowned_and_unknown_repositories_are_denied_by_default() {
        let acl = RepoAcl::memory(BTreeMap::new());
        let agent = principal("service:agent-a");
        for repository in ["never-initialized", "pilot-a"] {
            for action in [RepoAction::Read, RepoAction::Write] {
                let decision = acl
                    .decide(&agent, repository, action)
                    .await
                    .expect("decide");
                assert!(
                    !decision.allow,
                    "{repository} {action:?} must deny by default"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_configured_rule_cannot_be_reassigned_by_init() {
        let mut configured = BTreeMap::new();
        configured.insert("pilot-a".to_owned(), rule("service:agent-a", &[], &[]));
        let acl = RepoAcl::memory(configured);
        assert_eq!(
            acl.claim("pilot-a", &principal("service:maestro"))
                .await
                .expect("claim"),
            ClaimOutcome::OwnedByOther {
                owner: "service:agent-a".to_owned()
            }
        );
        assert_eq!(
            acl.claim("pilot-a", &principal("service:agent-a"))
                .await
                .expect("claim"),
            ClaimOutcome::AlreadyOwner
        );
    }
}
