//! Bearer-token and session storage under the config directory.
//!
//! The token is a bearer credential: it is written with mode 0600 (and re-tightened when read, so
//! a copy that drifted to group/world-readable does not stay that way), never printed unless the
//! caller explicitly asks (`auth mint --show-token`), and removed by `auth logout`. The session
//! file next to it holds only non-secret metadata (principal, email, display, expiry).

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The non-secret metadata returned by the mint flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// `github:<login>`.
    pub principal: String,
    /// The human's email (used as the default commit author).
    pub email: String,
    /// The human-readable display name.
    pub display: String,
    /// Token expiry, Unix seconds (as reported by the service).
    pub expires_at_unix: i64,
}

impl Session {
    /// `Display <email>`, the default commit author for human-created changesets.
    pub fn author(&self) -> String {
        format!("{} <{}>", self.display, self.email)
    }
}

/// Write `token` to `path` with mode 0600, creating the directory as needed.
pub fn write_token(path: &Path, token: &str) -> Result<()> {
    if token.trim().is_empty() {
        anyhow::bail!("refusing to store an empty token");
    }
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(token.trim().as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("writing {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flushing {}", path.display()))?;
    tighten(path)?;
    Ok(())
}

/// Read the stored token, if any. A token file readable by group/other is tightened first.
pub fn read_token(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            tighten(path)?;
            let token = raw.trim().to_owned();
            if token.is_empty() {
                anyhow::bail!(
                    "token file {} is empty; run `forgevcs auth mint`",
                    path.display()
                );
            }
            Ok(Some(token))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Remove the stored token; returns whether a file existed.
pub fn remove_token(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

/// Write the non-secret session metadata (0600 like the token; it is not public information).
pub fn write_session(path: &Path, session: &Session) -> Result<()> {
    let body = serde_json::to_vec_pretty(session).context("serializing session")?;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(&body)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("writing {}", path.display()))?;
    tighten(path)?;
    Ok(())
}

/// Read the stored session metadata, if any.
pub fn read_session(path: &Path) -> Result<Option<Session>> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            tighten(path)?;
            let session = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            Ok(Some(session))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Remove the stored session; returns whether a file existed.
pub fn remove_session(path: &Path) -> Result<bool> {
    remove_token(path)
}

/// Create `dir` with mode 0700 (an existing directory is left as the operator set it).
fn ensure_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = fs::metadata(dir)
            .with_context(|| format!("reading metadata of {}", dir.display()))?
            .permissions();
        if permissions.mode() & 0o077 != 0 {
            permissions.set_mode(0o700);
            fs::set_permissions(dir, permissions)
                .with_context(|| format!("making {} private", dir.display()))?;
        }
    }
    Ok(())
}

/// Ensure a file is not readable by group/other on Unix (mode 0600).
fn tighten(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = fs::metadata(path)
            .with_context(|| format!("reading metadata of {}", path.display()))?
            .permissions();
        if permissions.mode() & 0o777 != 0o600 {
            permissions.set_mode(0o600);
            fs::set_permissions(path, permissions)
                .with_context(|| format!("making {} mode 0600", path.display()))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// The token path shown to the user.
pub fn display_path(path: &Path) -> String {
    path.display().to_string()
}

/// A `PathBuf` helper for callers that only have the config dir.
pub fn default_token_path(config_dir: &Path) -> PathBuf {
    config_dir.join("token")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn token_round_trips_through_a_0600_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token");
        write_token(&path, "tok-abc").expect("write token");
        assert_eq!(
            read_token(&path).expect("read token").as_deref(),
            Some("tok-abc")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the bearer token must be owner-only");
        }
    }

    #[test]
    fn a_loose_token_file_is_tightened_on_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token");
        fs::write(&path, "tok-loose").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        }
        assert_eq!(
            read_token(&path).expect("read").as_deref(),
            Some("tok-loose")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn missing_token_is_none_and_empty_token_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            read_token(&dir.path().join("token")).expect("missing"),
            None
        );
        fs::write(dir.path().join("token"), " \n").expect("write empty");
        assert!(read_token(&dir.path().join("token")).is_err());
        assert!(write_token(&dir.path().join("token"), "  ").is_err());
    }

    #[test]
    fn session_round_trips_and_formats_the_author() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = Session {
            principal: "github:culebraluxe".to_owned(),
            email: "culebraluxe@gmail.com".to_owned(),
            display: "Culebraluxe".to_owned(),
            expires_at_unix: 42,
        };
        assert_eq!(session.author(), "Culebraluxe <culebraluxe@gmail.com>");
        let path = dir.path().join("session.json");
        write_session(&path, &session).expect("write session");
        assert_eq!(read_session(&path).expect("read session"), Some(session));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn logout_removes_both_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token");
        write_token(&path, "tok").expect("write");
        assert!(remove_token(&path).expect("remove"));
        assert!(!remove_token(&path).expect("remove again"));
        assert_eq!(read_token(&path).expect("read"), None);
    }
}
