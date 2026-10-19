//! First-class human principals and the explicit human token-mint path.
//!
//! The service authenticates two kinds of identity today: agents (upstream `service:` principals
//! with a configured SHA-256 secret digest) and humans who log in through GitHub. This module
//! adds the human that a CLI can use **without a browser**: `culebraluxe` is a configured human
//! principal with a login, email and display name, and a locally supplied secret whose digest
//! (never the secret) lives in `FORGEVCS_HUMAN_PRINCIPALS`.
//!
//! The authenticated principal is the same `github:<login>` identity the GitHub login path
//! issues, so policy, repository ACLs and authorization decisions treat a CLI human exactly like
//! any other human: `github:culebraluxe` must be granted in `acl.toml` like any principal.
//!
//! Verification always hashes the supplied secret (SHA-256, the SPEC 12.3 digest form) and
//! compares it with [`scion_auth::ct_eq`], the single constant-time primitive the engine already
//! uses for service secrets. An unknown login compares against a dummy digest so timing does not
//! reveal which logins are configured.
//!
//! Configuration (`FORGEVCS_HUMAN_PRINCIPALS`, JSON):
//!
//! ```json
//! {
//!   "culebraluxe": {
//!     "email": "culebraluxe@gmail.com",
//!     "display": "Culebraluxe",
//!     "secret_sha256": "<64 lowercase hex characters>"
//!   }
//! }
//! ```
//!
//! The id is derived from the login (`github:<login>`); the secret is supplied to
//! `POST /auth/human-token`. No secret or password appears in the repository.

use std::collections::BTreeMap;

use scion_auth::ct_eq;
use scion_core::Principal;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::Config;

/// The environment variable that configures human principals.
pub const HUMAN_PRINCIPALS_VAR: &str = "FORGEVCS_HUMAN_PRINCIPALS";

/// Digest compared against for unknown logins, so the unknown-login path does the same work as
/// the known-login path. It is not the SHA-256 of any known input.
const DUMMY_DIGEST: [u8; 32] = [0xA5; 32];

/// One configured human principal: display identity plus the secret digest to verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanPrincipal {
    /// `github:<login>`; the identity policy and repository ACLs see.
    id: String,
    login: String,
    email: String,
    display: String,
}

impl HumanPrincipal {
    /// The principal id string (`github:<login>`).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The GitHub-style login the policy grants.
    pub fn login(&self) -> &str {
        &self.login
    }

    /// The human's email address (for the CLI's default commit author).
    pub fn email(&self) -> &str {
        &self.email
    }

    /// The human-readable display name.
    pub fn display(&self) -> &str {
        &self.display
    }

    /// The engine principal this human authenticates as. No numeric GitHub id is known without a
    /// browser login, so the token carries none (the upstream GitHub path carries one when it has
    /// it; policy only reads the login).
    pub fn principal(&self) -> Principal {
        Principal::GitHub {
            login: self.login.clone(),
            id: None,
        }
    }
}

/// One entry: the display identity and the digest that authenticates it.
struct HumanEntry {
    human: HumanPrincipal,
    digest: [u8; 32],
}

/// The configured human principals, verified in constant time.
#[derive(Default)]
pub struct HumanPrincipals {
    entries: BTreeMap<String, HumanEntry>,
}

impl HumanPrincipals {
    /// Build from validated config entries.
    fn from_parsed(parsed: BTreeMap<String, ParsedHuman>) -> Result<Self, String> {
        let mut entries = BTreeMap::new();
        for (login, human) in parsed {
            let principal = Principal::parse(&format!("github:{login}"))
                .map_err(|error| format!("human login {login:?}: {}", error.message))?;
            let id = principal.id_string();
            let mut digest = [0u8; 32];
            hex::decode_to_slice(&human.secret_sha256, &mut digest)
                .map_err(|_| format!("{login:?}: secret_sha256 must be 64 hex characters"))?;
            entries.insert(
                login.clone(),
                HumanEntry {
                    human: HumanPrincipal {
                        id,
                        login,
                        email: human.email,
                        display: human.display,
                    },
                    digest,
                },
            );
        }
        Ok(Self { entries })
    }

    /// Build from process configuration. A malformed value is reported by `Config::validate`
    /// before `/ready`; here a caller that bypassed validation gets an empty set (deny by
    /// default) rather than a panic, matching the repository-ACL precedent.
    pub fn from_config(config: &Config) -> Self {
        match config.human_principals.as_deref() {
            Some(raw) => match parse_configured(raw) {
                Ok(humans) => humans,
                Err(error) => {
                    tracing::error!(%error, "{HUMAN_PRINCIPALS_VAR} is invalid; human tokens disabled");
                    Self::default()
                }
            },
            None => Self::default(),
        }
    }

    /// Authenticate `login` + `secret`. Always hashes the secret; an unknown login compares
    /// against a dummy digest. Returns the configured human only when both match.
    pub fn authenticate(&self, login: &str, secret: &[u8]) -> Option<&HumanPrincipal> {
        let digest: [u8; 32] = Sha256::digest(secret).into();
        let (known, expected) = match self.entries.get(login) {
            Some(entry) => (true, &entry.digest),
            None => (false, &DUMMY_DIGEST),
        };
        let matches = ct_eq(&digest, expected);
        if matches & known {
            self.entries.get(login).map(|entry| &entry.human)
        } else {
            None
        }
    }

    /// How many humans are configured (operator diagnostics; never lists them on the wire).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no human is configured.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// One entry of the raw JSON configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParsedHuman {
    email: String,
    display: String,
    /// SHA-256 of the human's locally supplied secret, lowercase hex (SPEC 12.3 convention).
    secret_sha256: String,
}

/// Parse and validate `FORGEVCS_HUMAN_PRINCIPALS`, returning the ready-to-use principal set.
///
/// Login must form a valid `github:` principal; email/display must be non-empty (email also
/// contains `@` and no whitespace); the digest must be 64 lowercase hex characters. Unknown
/// fields are rejected so a typo cannot silently disable a human.
pub fn parse_configured(raw: &str) -> Result<HumanPrincipals, String> {
    let parsed: BTreeMap<String, ParsedHuman> =
        serde_json::from_str(raw).map_err(|error| format!("not a JSON object: {error}"))?;
    if parsed.is_empty() {
        return Err("at least one human principal is required".to_owned());
    }
    for (login, human) in &parsed {
        Principal::parse(&format!("github:{login}"))
            .map_err(|_| format!("{login:?} is not a valid human login"))?;
        // GitHub logins compare case-insensitively and policy stores them lowercased; require
        // the configured login to already be lowercase so the id string and policy grant agree.
        if login != &login.to_ascii_lowercase() {
            return Err(format!("{login:?}: human login must be lowercase"));
        }
        if human.display.trim().is_empty() {
            return Err(format!("{login:?}: display must not be empty"));
        }
        if human.email.trim().is_empty()
            || !human.email.contains('@')
            || human.email.chars().any(char::is_whitespace)
        {
            return Err(format!("{login:?}: email must be a non-empty address"));
        }
        let digest = &human.secret_sha256;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(format!(
                "{login:?}: secret_sha256 must be 64 lowercase hex characters"
            ));
        }
    }
    HumanPrincipals::from_parsed(parsed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn config(raw: &str) -> Config {
        Config::from_lookup(|key| (key == HUMAN_PRINCIPALS_VAR).then(|| raw.to_owned()))
    }

    #[test]
    fn a_configured_human_exposes_id_email_and_display() {
        let digest = hex::encode(Sha256::digest(b"test-human-secret"));
        let raw = format!(
            r#"{{"culebraluxe":{{"email":"culebraluxe@gmail.com","display":"Culebraluxe","secret_sha256":"{digest}"}}}}"#
        );
        let humans = parse_configured(&raw).expect("valid human config");
        assert_eq!(humans.len(), 1);
        assert!(!humans.is_empty());
        let human = humans
            .authenticate("culebraluxe", b"test-human-secret")
            .expect("configured human authenticates");
        assert_eq!(human.id(), "github:culebraluxe");
        assert_eq!(human.login(), "culebraluxe");
        assert_eq!(human.email(), "culebraluxe@gmail.com");
        assert_eq!(human.display(), "Culebraluxe");
        assert_eq!(
            human.principal(),
            Principal::parse("github:culebraluxe").expect("principal")
        );
    }

    #[test]
    fn the_secret_verifier_is_constant_time_and_fail_closed() {
        let digest = hex::encode(Sha256::digest(b"test-human-secret"));
        let raw = format!(
            r#"{{"culebraluxe":{{"email":"culebraluxe@gmail.com","display":"Culebraluxe","secret_sha256":"{digest}"}}}}"#
        );
        let humans = HumanPrincipals::from_config(&config(&raw));
        assert!(
            humans
                .authenticate("culebraluxe", b"test-human-secret")
                .is_some()
        );
        assert!(humans.authenticate("culebraluxe", b"wrong").is_none());
        assert!(
            humans
                .authenticate("someone-else", b"test-human-secret")
                .is_none()
        );
        assert!(humans.authenticate("culebraluxe", b"").is_none());
    }

    #[test]
    fn malformed_config_is_rejected_loudly() {
        for raw in [
            "not json",
            "[]",
            r#"{"Culebraluxe":{"email":"c@example.com","display":"C","secret_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
            r#"{"culebraluxe":{"email":"","display":"C","secret_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
            r#"{"culebraluxe":{"email":"c@example.com","display":"","secret_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
            r#"{"culebraluxe":{"email":"c@example.com","display":"C","secret_sha256":"ZZ"}}"#,
            r#"{"culebraluxe":{"email":"c@example.com","display":"C","secret_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","admin":true}}"#,
            r#"{"culebraluxe":{}}"#,
            "{}",
        ] {
            assert!(parse_configured(raw).is_err(), "must reject {raw}");
        }
    }

    #[test]
    fn a_malformed_value_disables_configured_humans_instead_of_panicking() {
        let humans = HumanPrincipals::from_config(&config("not json"));
        assert!(humans.is_empty());
        assert!(humans.authenticate("culebraluxe", b"x").is_none());
    }

    #[test]
    fn no_configuration_means_no_humans() {
        let humans = HumanPrincipals::from_config(&Config::from_lookup(|_| None));
        assert!(humans.is_empty());
        assert_eq!(humans.len(), 0);
    }
}
