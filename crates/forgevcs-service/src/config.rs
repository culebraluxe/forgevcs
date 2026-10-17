//! Environment configuration for the ForgeVCS service.
//!
//! Everything the service needs arrives through the environment: the deploy
//! target injects it and there are no embedded secrets (ADR-0001 §4). ForgeVCS
//! variables are consulted first, then Neon's injected `AWS_*` names, so one
//! binary runs unchanged against local MinIO, a Neon bucket, or real S3.

use std::env;

/// The vendored Scion commit this build was assembled from (`docs/BASELINE.md`).
pub const UPSTREAM_COMMIT: &str = "e0ffadf9c755f1b99812df5beffcabdebcb38c19";

/// Default listen port; Vercel supplies `$PORT`.
const DEFAULT_PORT: u16 = 3000;

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
        }
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
        Ok(())
    }

    /// True when object storage is configured well enough to construct a client.
    pub fn has_object_storage(&self) -> bool {
        self.bucket.is_some() && self.region.is_some()
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
}
