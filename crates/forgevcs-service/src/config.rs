//! Environment configuration for the ForgeVCS service.
//!
//! Everything the service needs arrives through the environment: the deploy
//! target injects it and there are no embedded secrets (ADR-0001 §4). ForgeVCS
//! variables are consulted first, then Neon's injected `AWS_*` names, so one
//! binary runs unchanged against local MinIO, a Neon bucket, or real S3.

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use scion_store_s3::S3Config;

/// The vendored Scion commit this build was assembled from (`docs/BASELINE.md`).
pub const UPSTREAM_COMMIT: &str = "e0ffadf9c755f1b99812df5beffcabdebcb38c19";

/// Default listen port; Vercel supplies `$PORT`.
const DEFAULT_PORT: u16 = 3000;

/// Default repository name when `$FORGEVCS_REPO` is unset.
pub const DEFAULT_REPO: &str = "forgevcs";

/// Default metadata pool size (upstream's SPEC 6.5 default).
const DEFAULT_MAX_CONNECTIONS: u16 = 64;

/// Default policy file, relative to the working directory.
const DEFAULT_POLICY_PATH: &str = "acl.toml";

/// Resolved process configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// TCP port to listen on (`$PORT`, else 3000).
    pub port: u16,
    /// Postgres connection string (`$FORGEVCS_DATABASE_URL`, else `$DATABASE_URL`).
    pub database_url: Option<String>,
    /// Object-storage bucket (`$FORGEVCS_BUCKET`), when storage is configured.
    pub bucket: Option<String>,
    /// Object-storage region (`$FORGEVCS_AWS_REGION`, else `$AWS_REGION`).
    pub region: Option<String>,
    /// Object-storage endpoint (`$FORGEVCS_AWS_ENDPOINT_URL_S3`, else `$AWS_ENDPOINT_URL_S3`).
    pub endpoint: Option<String>,
    /// Object-storage access key (`$FORGEVCS_AWS_ACCESS_KEY_ID`, else `$AWS_ACCESS_KEY_ID`).
    pub access_key: Option<String>,
    /// Object-storage secret key (`$FORGEVCS_AWS_SECRET_ACCESS_KEY`, else `$AWS_SECRET_ACCESS_KEY`).
    pub secret_key: Option<String>,
    /// Prefix every object key is written under (`$FORGEVCS_KEY_PREFIX`; empty by default).
    pub key_prefix: Option<String>,
    /// Repository this process serves (`$FORGEVCS_REPO`; `forgevcs` by default). ADR-0002 D3:
    /// one engine per repository, this is the one booted at startup.
    pub repo: Option<String>,
    /// Metadata pool size (`$FORGEVCS_DB_MAX_CONNECTIONS`; 64 by default).
    pub max_connections: Option<u16>,
    /// `acl.toml` path (`$FORGEVCS_POLICY_PATH`; `acl.toml` in the working directory).
    pub policy_path: Option<PathBuf>,
    /// PKCS#8 PEM Ed25519 signing key (`$FORGEVCS_SIGNING_KEY_PATH`). Absent means the engine
    /// generates an ephemeral key and tokens die with the process (ADR-0002 D5).
    pub signing_key: Option<PathBuf>,
    /// PKCS#8 PEM Ed25519 signing key supplied inline (`$FORGEVCS_SIGNING_KEY_PEM`), for a
    /// platform that injects secrets as environment values rather than files (Vercel). The
    /// engine writes it to a private temporary file, loads it, and removes the file.
    pub signing_key_pem: Option<String>,
    /// GitHub OAuth App client id (`$FORGEVCS_GITHUB_CLIENT_ID`), used by `AuthService.Login`.
    pub github_client_id: Option<String>,
    /// Service principals, comma-separated `name=<sha256 hex>`
    /// (`$FORGEVCS_SERVICE_PRINCIPALS`). These are the agent identities — Maestro, OpenCode,
    /// importers — that policy refers to as `service:<name>`.
    pub service_principals: Option<String>,
    /// Authorization A/B mode (`$FORGEVCS_AUTH_MODE`; `SCION_ONLY` by default).
    pub auth_mode: Option<String>,
    /// Operator-configured repository ACLs (`FORGEVCS_REPO_ACLS`), JSON
    /// `repository -> {owner, readers?, writers?}`. Repository identity is not part of the
    /// vendored `acl.toml` policy; this value is how a pilot pre-assigns a repository to an
    /// agent without an admin API. Owners are otherwise claimed by the first `init`.
    pub repo_acls: Option<String>,
    /// Configured human principals (`FORGEVCS_HUMAN_PRINCIPALS`), JSON
    /// `login -> {email, display, secret_sha256}`. A human authenticates through
    /// `POST /auth/human-token` and is then the same `github:<login>` principal the browser
    /// login issues; the secret itself is never stored here.
    pub human_principals: Option<String>,
    /// Per-workspace write-lease lifetime (`FORGEVCS_WORKSPACE_LEASE_TTL_SECS`; 300 s by
    /// default). A lease that is not heartbeated within this window is stale and may be taken
    /// over by another writer.
    pub workspace_lease_ttl_secs: Option<u64>,
}

impl Config {
    /// Read the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|key| env::var(key).ok())
    }

    /// Read configuration from an arbitrary lookup, so parsing is testable
    /// without mutating process-global environment variables.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |keys: &[&str]| keys.iter().find_map(|key| non_empty(lookup(key)));
        Self {
            port: get(&["PORT"])
                .and_then(|raw| raw.parse::<u16>().ok())
                .unwrap_or(DEFAULT_PORT),
            database_url: get(&["FORGEVCS_DATABASE_URL", "DATABASE_URL"]),
            bucket: get(&["FORGEVCS_BUCKET", "FORGEVCS_S3_BUCKET"]),
            region: get(&["FORGEVCS_AWS_REGION", "AWS_REGION"]),
            endpoint: get(&["FORGEVCS_AWS_ENDPOINT_URL_S3", "AWS_ENDPOINT_URL_S3"]),
            access_key: get(&["FORGEVCS_AWS_ACCESS_KEY_ID", "AWS_ACCESS_KEY_ID"]),
            secret_key: get(&["FORGEVCS_AWS_SECRET_ACCESS_KEY", "AWS_SECRET_ACCESS_KEY"]),
            key_prefix: get(&["FORGEVCS_KEY_PREFIX"]),
            repo: get(&["FORGEVCS_REPO"]),
            max_connections: get(&["FORGEVCS_DB_MAX_CONNECTIONS"])
                .and_then(|raw| raw.parse::<u16>().ok()),
            policy_path: get(&["FORGEVCS_POLICY_PATH"]).map(PathBuf::from),
            signing_key: get(&["FORGEVCS_SIGNING_KEY_PATH"]).map(PathBuf::from),
            signing_key_pem: get(&["FORGEVCS_SIGNING_KEY_PEM"]),
            github_client_id: get(&["FORGEVCS_GITHUB_CLIENT_ID"]),
            service_principals: get(&["FORGEVCS_SERVICE_PRINCIPALS"]),
            auth_mode: get(&["FORGEVCS_AUTH_MODE"]),
            repo_acls: get(&["FORGEVCS_REPO_ACLS"]),
            human_principals: get(&["FORGEVCS_HUMAN_PRINCIPALS"]),
            workspace_lease_ttl_secs: get(&["FORGEVCS_WORKSPACE_LEASE_TTL_SECS"])
                .and_then(|raw| raw.parse::<u64>().ok()),
        }
    }

    /// The repository this process serves (ADR-0002 D3).
    pub fn repo(&self) -> &str {
        self.repo.as_deref().unwrap_or(DEFAULT_REPO)
    }

    /// Metadata pool size.
    pub fn max_connections(&self) -> usize {
        usize::from(self.max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS))
    }

    /// Per-workspace write-lease lifetime (`FORGEVCS_WORKSPACE_LEASE_TTL_SECS`, default 5 min).
    pub fn workspace_lease_ttl(&self) -> Duration {
        Duration::from_secs(
            self.workspace_lease_ttl_secs
                .unwrap_or(crate::lease::DEFAULT_TTL_SECS),
        )
    }

    /// The `acl.toml` to load.
    pub fn policy_path(&self) -> PathBuf {
        self.policy_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_POLICY_PATH))
    }

    /// Signing-key paths, in signing-then-verify order.
    pub fn signing_keys(&self) -> Vec<PathBuf> {
        self.signing_key.iter().cloned().collect()
    }

    /// Reject configurations that cannot be served, with a reason suitable for
    /// `/ready`. Deliberately about *coherence*, not reachability: probing Neon
    /// belongs to the store layer (W2), not to config parsing.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(url) = &self.database_url {
            let scheme_ok = url.starts_with("postgres://") || url.starts_with("postgresql://");
            if !scheme_ok {
                return Err("FORGEVCS_DATABASE_URL must be a postgres:// URL".to_owned());
            }
        }
        if self.endpoint.is_some() && self.bucket.is_none() {
            return Err("object-storage endpoint set without FORGEVCS_BUCKET".to_owned());
        }
        if self.access_key.is_some() != self.secret_key.is_some() {
            return Err("object-storage credentials are half-configured: set both \
                 FORGEVCS_AWS_ACCESS_KEY_ID and FORGEVCS_AWS_SECRET_ACCESS_KEY"
                .to_owned());
        }
        if let Some(raw) = &self.repo_acls {
            crate::repo_acl::parse_configured(raw)
                .map_err(|error| format!("FORGEVCS_REPO_ACLS is invalid: {error}"))?;
        }
        if let Some(raw) = &self.human_principals {
            crate::human::parse_configured(raw)
                .map_err(|error| format!("FORGEVCS_HUMAN_PRINCIPALS is invalid: {error}"))?;
        }
        if self.workspace_lease_ttl_secs == Some(0) {
            return Err("FORGEVCS_WORKSPACE_LEASE_TTL_SECS must be at least 1 second".to_owned());
        }
        if let Some(mode) = &self.auth_mode
            && !matches!(
                mode.as_str(),
                "SCION_ONLY" | "SHADOW_COMPARE" | "DUAL_ENFORCE" | "CULEBRA_ONLY"
            )
        {
            return Err(format!(
                "FORGEVCS_AUTH_MODE must be SCION_ONLY, SHADOW_COMPARE, DUAL_ENFORCE or \
                 CULEBRA_ONLY, got {mode:?}"
            ));
        }
        Ok(())
    }

    /// The object-storage client settings (W3 work order §4), or `None` when storage is not
    /// configured.
    ///
    /// Path-style addressing is mandatory on Neon Object Storage — it rejects virtual-hosted
    /// style — and plain HTTP is refused because the endpoint is HTTPS-only (ADR-0001 §7.2,
    /// §7.3). Credentials, when present, are passed explicitly so an ambient `AWS_*` profile
    /// on a developer machine cannot silently take over from the injected ones.
    pub fn s3_config(&self) -> Option<S3Config> {
        Some(S3Config {
            endpoint: self.endpoint.clone()?,
            bucket: self.bucket.clone()?,
            region: self.region.clone()?,
            access_key: self.access_key.clone(),
            secret_key: self.secret_key.clone(),
            // Production endpoints are HTTPS; an explicit `http://` endpoint is a local/CI harness
            // (moto/minio) and must opt in by scheme. Never infer HTTP for an https endpoint.
            allow_http: self
                .endpoint
                .as_deref()
                .is_some_and(|endpoint| endpoint.starts_with("http://")),
            path_style: true,
            key_prefix: self.key_prefix.clone().unwrap_or_default(),
        })
    }

    /// True when object storage is configured well enough to construct a client.
    pub fn has_object_storage(&self) -> bool {
        self.s3_config().is_some()
    }
}

/// Treat empty variables as absent: deploy platforms set some to `""`.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|raw| !raw.trim().is_empty())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::collections::HashMap;

    fn config(pairs: &[(&str, &str)]) -> Config {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Config::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn defaults_to_port_3000_without_env() {
        let cfg = Config::from_lookup(|_| None);
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert!(cfg.database_url.is_none());
        assert!(!cfg.has_object_storage());
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn honours_port_and_falls_back_from_forgevcs_to_aws_names() {
        let cfg = config(&[
            ("PORT", "8080"),
            ("DATABASE_URL", "postgres://user@host/db"),
            ("AWS_REGION", "aws-us-east-2"),
            ("AWS_ENDPOINT_URL_S3", "https://s3.example"),
            ("FORGEVCS_BUCKET", "forgevcs-objects"),
        ]);
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.database_url.as_deref(), Some("postgres://user@host/db"));
        assert_eq!(cfg.region.as_deref(), Some("aws-us-east-2"));
        assert_eq!(cfg.endpoint.as_deref(), Some("https://s3.example"));
        assert!(cfg.has_object_storage());
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn forgevcs_names_win_over_aws_names() {
        let cfg = config(&[
            ("FORGEVCS_AWS_REGION", "local"),
            ("AWS_REGION", "aws-us-east-2"),
            ("FORGEVCS_BUCKET", "mine"),
            ("FORGEVCS_S3_BUCKET", "ignored"),
        ]);
        assert_eq!(cfg.region.as_deref(), Some("local"));
        assert_eq!(cfg.bucket.as_deref(), Some("mine"));
    }

    #[test]
    fn empty_values_count_as_absent_and_bad_port_falls_back() {
        let cfg = config(&[
            ("PORT", ""),
            ("FORGEVCS_DATABASE_URL", "  "),
            ("AWS_REGION", ""),
        ]);
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert!(cfg.database_url.is_none());
        assert!(cfg.region.is_none());
    }

    #[test]
    fn validate_rejects_non_postgres_url_and_orphan_endpoint() {
        let cfg = config(&[("FORGEVCS_DATABASE_URL", "mysql://x/y")]);
        assert!(cfg.validate().is_err());

        let cfg = config(&[("AWS_ENDPOINT_URL_S3", "https://s3.example")]);
        assert!(cfg.validate().is_err());

        let cfg = config(&[("FORGEVCS_DATABASE_URL", "postgresql://x/y")]);
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn s3_config_is_path_style_and_https_only() {
        let cfg = config(&[
            ("FORGEVCS_BUCKET", "forgevcs-objects"),
            ("AWS_REGION", "aws-us-east-2"),
            ("AWS_ENDPOINT_URL_S3", "https://s3.example"),
            ("AWS_ACCESS_KEY_ID", "ak"),
            ("AWS_SECRET_ACCESS_KEY", "sk"),
            ("FORGEVCS_KEY_PREFIX", "w3/run"),
        ]);
        let s3 = cfg.s3_config().expect("configured");
        assert!(s3.path_style, "Neon rejects virtual-hosted style");
        assert!(!s3.allow_http, "the endpoint is HTTPS-only");
        assert_eq!(s3.endpoint, "https://s3.example");
        assert_eq!(s3.bucket, "forgevcs-objects");
        assert_eq!(s3.region, "aws-us-east-2");
        assert_eq!(s3.access_key.as_deref(), Some("ak"));
        assert_eq!(s3.secret_key.as_deref(), Some("sk"));
        assert_eq!(s3.key_prefix, "w3/run");
    }

    #[test]
    fn s3_config_needs_an_endpoint_and_defaults_the_prefix() {
        let cfg = config(&[
            ("FORGEVCS_BUCKET", "forgevcs-objects"),
            ("AWS_REGION", "aws-us-east-2"),
        ]);
        assert!(!cfg.has_object_storage());
        assert!(cfg.s3_config().is_none());

        let cfg = config(&[
            ("FORGEVCS_BUCKET", "forgevcs-objects"),
            ("AWS_REGION", "aws-us-east-2"),
            ("AWS_ENDPOINT_URL_S3", "https://s3.example"),
        ]);
        assert_eq!(cfg.s3_config().expect("configured").key_prefix, "");
    }

    #[test]
    fn auth_mode_accepts_only_the_four_modes() {
        let unset = config(&[]);
        assert!(unset.auth_mode.is_none());

        for mode in [
            "SCION_ONLY",
            "SHADOW_COMPARE",
            "DUAL_ENFORCE",
            "CULEBRA_ONLY",
        ] {
            let cfg = config(&[("FORGEVCS_AUTH_MODE", mode)]);
            assert_eq!(cfg.validate(), Ok(()), "{mode}");
            assert_eq!(cfg.auth_mode.as_deref(), Some(mode));
        }

        let cfg = config(&[("FORGEVCS_AUTH_MODE", "SCION_AND_CULEBRA")]);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn repo_acls_are_validated_at_startup() {
        let cfg = config(&[(
            "FORGEVCS_REPO_ACLS",
            r#"{"pilot-a":{"owner":"service:agent-a","writers":["service:agent-a"]}}"#,
        )]);
        assert!(cfg.repo_acls.is_some());
        assert_eq!(cfg.validate(), Ok(()));

        let cfg = config(&[("FORGEVCS_REPO_ACLS", r#"{"pilot-a":{"owner":"agent-a"}}"#)]);
        assert!(
            cfg.validate().is_err(),
            "a malformed repo ACL must not silently disable isolation"
        );
        assert!(
            cfg.validate()
                .err()
                .is_some_and(|error| error.contains("FORGEVCS_REPO_ACLS")),
            "the error names the variable an operator must fix"
        );
    }

    #[test]
    fn human_principals_are_validated_at_startup() {
        let digest = "a".repeat(64);
        let raw = format!(
            r#"{{"culebraluxe":{{"email":"culebraluxe@gmail.com","display":"Culebraluxe","secret_sha256":"{digest}"}}}}"#
        );
        let cfg = config(&[("FORGEVCS_HUMAN_PRINCIPALS", &raw)]);
        assert!(cfg.human_principals.is_some());
        assert_eq!(cfg.validate(), Ok(()));

        let cfg = config(&[("FORGEVCS_HUMAN_PRINCIPALS", "not json")]);
        assert!(
            cfg.validate().is_err(),
            "a malformed human principal must not silently disable the path"
        );
        assert!(
            cfg.validate()
                .err()
                .is_some_and(|error| error.contains("FORGEVCS_HUMAN_PRINCIPALS")),
            "the error names the variable an operator must fix"
        );
    }

    #[test]
    fn validate_rejects_half_configured_credentials() {
        let cfg = config(&[("AWS_ACCESS_KEY_ID", "ak")]);
        assert!(cfg.validate().is_err(), "secret key missing");

        let cfg = config(&[("AWS_ACCESS_KEY_ID", "ak"), ("AWS_SECRET_ACCESS_KEY", "sk")]);
        assert_eq!(cfg.validate(), Ok(()));
    }

    /// R4: the lease lifetime defaults to five minutes and is overridable, but a zero TTL would
    /// make every lease instantly stale, so it is refused at startup.
    #[test]
    fn workspace_lease_ttl_defaults_to_five_minutes_and_is_validated() {
        let cfg = config(&[]);
        assert_eq!(cfg.workspace_lease_ttl(), Duration::from_secs(300));
        assert_eq!(cfg.validate(), Ok(()));

        let cfg = config(&[("FORGEVCS_WORKSPACE_LEASE_TTL_SECS", "60")]);
        assert_eq!(cfg.workspace_lease_ttl(), Duration::from_secs(60));
        assert_eq!(cfg.validate(), Ok(()));

        let cfg = config(&[("FORGEVCS_WORKSPACE_LEASE_TTL_SECS", "0")]);
        assert!(
            cfg.validate()
                .err()
                .is_some_and(|error| error.contains("FORGEVCS_WORKSPACE_LEASE_TTL_SECS")),
            "a zero lease TTL must be refused, not accepted as instantly stale"
        );
    }
}
