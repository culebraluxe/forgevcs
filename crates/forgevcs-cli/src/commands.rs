//! Command execution: each command is a thin call to one façade route.
//!
//! The commands return the parsed JSON so the caller decides how to print it (the binary prints
//! pretty JSON; tests assert on the value). No command has ForgeVCS semantics of its own beyond
//! assembling the request shape the façade documents.

use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::args::{Command, GlobalOptions};
use crate::client::Client;
use crate::settings::Settings;
use crate::token::{self, Session};

/// The environment variable accepted as a non-interactive secret source.
pub const HUMAN_SECRET_VAR: &str = "FORGEVCS_HUMAN_SECRET";

/// A token supplied directly (short-lived shell/CI jobs); it wins over the stored file.
pub const TOKEN_VAR: &str = "FORGEVCS_TOKEN";

/// Execute one parsed command and return the service's JSON response (plus CLI-local fields).
pub async fn execute(global: GlobalOptions, command: Command) -> Result<Value> {
    let settings = Settings::resolve(&global)?;
    match command {
        Command::AuthStatus => auth_status(&settings).await,
        Command::AuthMint {
            login,
            secret_file,
            show_token,
        } => auth_mint(&settings, &login, secret_file.as_deref(), show_token).await,
        Command::AuthLogout => auth_logout(&settings),
        Command::Health => health(&settings).await,
        Command::Init { repo } => {
            let client = authenticated(&settings)?;
            client
                .post(&format!("/repos/{repo}/init"), &json!({}))
                .await
        }
        Command::Refs { repo } => {
            let client = authenticated(&settings)?;
            client.get(&format!("/repos/{repo}/refs"), &[]).await
        }
        Command::RefCreate {
            repo,
            name,
            target,
            reason,
        } => {
            let client = authenticated(&settings)?;
            client
                .post(
                    &format!("/repos/{repo}/refs"),
                    &json!({
                        "name": name,
                        "target": target,
                        "reason": reason.unwrap_or_default(),
                    }),
                )
                .await
        }
        Command::Log {
            repo,
            ref_name,
            limit,
            path,
            first_parent_only,
            page_token,
        } => {
            log(
                &settings,
                &repo,
                &ref_name,
                limit,
                path,
                first_parent_only,
                page_token,
            )
            .await
        }
        Command::Pull {
            repo,
            ref_name,
            limit,
            path,
            objects,
        } => pull(&settings, &repo, &ref_name, limit, path, objects).await,
        Command::Push {
            repo,
            changeset,
            ref_name,
            mode,
        } => push(&settings, &repo, &changeset, ref_name, mode).await,
        Command::Merge {
            repo,
            a,
            b,
            change_id,
            author,
            message,
        } => merge(&settings, &repo, &a, &b, &change_id, author, message).await,
        Command::Rebase {
            repo,
            changeset,
            onto,
        } => {
            let client = authenticated(&settings)?;
            client
                .post(
                    &format!("/repos/{repo}/rebase"),
                    &json!({ "changeset": changeset, "onto": onto }),
                )
                .await
        }
        Command::MergeLand {
            repo,
            head,
            ref_name,
            author,
            message,
            change_id,
        } => {
            merge_land(
                &settings, &repo, &head, &ref_name, author, message, change_id,
            )
            .await
        }
        Command::WorkspaceCreate {
            repo,
            ref_name,
            changeset,
            sparse,
        } => {
            let client = authenticated(&settings)?;
            client
                .post(
                    &format!("/repos/{repo}/workspaces"),
                    &json!({
                        "ref": ref_name,
                        "changeset": changeset,
                        "sparse": sparse,
                    }),
                )
                .await
        }
        Command::WorkspaceStatus { repo, id } => {
            let client = authenticated(&settings)?;
            client
                .get(&format!("/repos/{repo}/workspaces/{id}"), &[])
                .await
        }
        Command::WorkspaceDestroy { repo, id } => {
            let client = authenticated(&settings)?;
            client
                .delete(&format!("/repos/{repo}/workspaces/{id}"))
                .await
        }
    }
}

/// The effective bearer token and where it came from. `$FORGEVCS_TOKEN` wins over the stored
/// file so a short-lived shell or CI job can use a token without writing it.
fn effective_token(settings: &Settings) -> Result<Option<(String, &'static str)>> {
    let env_token = std::env::var(TOKEN_VAR)
        .ok()
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty());
    if let Some(token) = env_token {
        return Ok(Some((token, "env")));
    }
    Ok(token::read_token(&settings.token_file)?.map(|token| (token, "file")))
}

/// A client with the effective bearer token, or a clear "mint a token" error.
fn authenticated(settings: &Settings) -> Result<Client> {
    let token = effective_token(settings)?
        .map(|(token, _)| token)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "not authenticated: no token at {} and {TOKEN_VAR} is unset; run \
             `forgevcs auth mint --login <login>`",
                settings.token_file.display()
            )
        })?;
    Client::new(&settings.base_url, Some(token))
}

fn anonymous(settings: &Settings) -> Result<Client> {
    Client::new(&settings.base_url, None)
}

fn stored_session(settings: &Settings) -> Option<Session> {
    token::read_session(&settings.session_file()).ok().flatten()
}

fn default_author(settings: &Settings) -> String {
    stored_session(settings).map_or_else(
        || "forgevcs-cli <forgevcs-cli@localhost>".to_owned(),
        |session| session.author(),
    )
}

async fn auth_status(settings: &Settings) -> Result<Value> {
    let effective = effective_token(settings)?;
    let session = stored_session(settings);
    let mut out = json!({
        "base_url": settings.base_url,
        "config_dir": settings.config_dir,
        "token_file": settings.token_file,
        "token_present": effective.is_some(),
        "token_source": effective.as_ref().map(|(_, source)| *source),
        "session": session,
    });
    if let Some((token, _)) = effective {
        // A stale/revoked token should not make `auth status` itself fail: report the verifier's
        // answer next to the local state.
        let client = Client::new(&settings.base_url, Some(token))?;
        match client.get("/auth/whoami", &[]).await {
            Ok(whoami) => out["whoami"] = whoami,
            Err(error) => out["whoami_error"] = json!(format!("{error:#}")),
        }
    }
    Ok(out)
}

async fn auth_mint(
    settings: &Settings,
    login: &str,
    secret_file: Option<&Path>,
    show_token: bool,
) -> Result<Value> {
    let secret = read_secret(secret_file)?;
    let client = anonymous(settings)?;
    let response = client
        .post(
            "/auth/human-token",
            &json!({ "login": login, "secret": secret }),
        )
        .await?;
    let token = response
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("the service did not return a token"))?
        .to_owned();
    let session = Session {
        principal: string_field(&response, "principal")?,
        email: string_field(&response, "email")?,
        display: string_field(&response, "display")?,
        expires_at_unix: response
            .get("expires_at_unix")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
    };
    token::write_token(&settings.token_file, &token)?;
    token::write_session(&settings.session_file(), &session)?;
    let mut out = json!({
        "status": "ok",
        "base_url": settings.base_url,
        "token_file": settings.token_file,
        "principal": session.principal,
        "email": session.email,
        "display": session.display,
        "expires_at_unix": session.expires_at_unix,
    });
    if show_token {
        out["token"] = json!(token);
    }
    Ok(out)
}

/// Read the human secret: `--secret-file`, `$FORGEVCS_HUMAN_SECRET`, or stdin. It is never
/// echoed by the CLI and never written to disk.
fn read_secret(secret_file: Option<&Path>) -> Result<String> {
    if let Some(path) = secret_file {
        check_secret_file_permissions(path)?;
        let mut raw = String::new();
        std::fs::File::open(path)
            .with_context(|| format!("opening {}", path.display()))?
            .read_to_string(&mut raw)
            .with_context(|| format!("reading {}", path.display()))?;
        return non_empty_secret(raw.trim_end_matches(['\r', '\n']), "--secret-file");
    }
    if let Ok(raw) = std::env::var(HUMAN_SECRET_VAR) {
        return non_empty_secret(&raw, HUMAN_SECRET_VAR);
    }
    eprintln!("Human secret (input is not hidden; pipe it in or use --secret-file):");
    let mut raw = String::new();
    std::io::stdin()
        .read_line(&mut raw)
        .context("reading the secret from stdin")?;
    non_empty_secret(raw.trim_end_matches(['\r', '\n']), "stdin")
}

fn non_empty_secret(raw: &str, source: &str) -> Result<String> {
    if raw.is_empty() {
        bail!("empty human secret from {source}");
    }
    Ok(raw.to_owned())
}

fn check_secret_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("reading metadata of {}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            bail!(
                "secret file {} is readable by group/other; run `chmod 600 {}`",
                path.display(),
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn auth_logout(settings: &Settings) -> Result<Value> {
    let removed_token = token::remove_token(&settings.token_file)?;
    let removed_session = token::remove_session(&settings.session_file())?;
    Ok(json!({
        "status": "ok",
        "token_file": settings.token_file,
        "removed_token": removed_token,
        "removed_session": removed_session,
    }))
}

async fn health(settings: &Settings) -> Result<Value> {
    let client = anonymous(settings)?;
    let health = client.get("/health", &[]).await?;
    let version = client.get("/version", &[]).await?;
    // Readiness is allowed to be 503 while the service starts; report it instead of failing the
    // whole probe, so `forgevcs health` is still useful from a watchdog.
    let ready = match client.get("/ready", &[]).await {
        Ok(value) => json!({ "ok": true, "body": value }),
        Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
    };
    Ok(json!({ "health": health, "ready": ready, "version": version }))
}

async fn log(
    settings: &Settings,
    repo: &str,
    ref_name: &str,
    limit: u32,
    path: Option<String>,
    first_parent_only: bool,
    page_token: Option<String>,
) -> Result<Value> {
    let client = authenticated(settings)?;
    let mut query = vec![
        ("ref".to_owned(), ref_name.to_owned()),
        ("limit".to_owned(), limit.to_string()),
    ];
    if let Some(path) = path {
        query.push(("path".to_owned(), path));
    }
    if first_parent_only {
        query.push(("first_parent_only".to_owned(), "true".to_owned()));
    }
    if let Some(token) = page_token {
        query.push(("page_token".to_owned(), token));
    }
    client.get(&format!("/repos/{repo}/commits"), &query).await
}

async fn pull(
    settings: &Settings,
    repo: &str,
    ref_name: &str,
    limit: u32,
    path: Option<String>,
    objects: Vec<String>,
) -> Result<Value> {
    let client = authenticated(settings)?;
    let mut body = json!({ "ref": ref_name, "limit": limit, "objects": objects });
    if let Some(path) = path {
        body["path"] = json!(path);
    }
    client.post(&format!("/repos/{repo}/pull"), &body).await
}

async fn push(
    settings: &Settings,
    repo: &str,
    changeset: &Path,
    ref_name: Option<String>,
    mode: Option<String>,
) -> Result<Value> {
    let raw = std::fs::read_to_string(changeset)
        .with_context(|| format!("reading changeset body {}", changeset.display()))?;
    let mut body: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing changeset body {}", changeset.display()))?;
    if !body.is_object() {
        bail!(
            "changeset body {} must be a JSON object",
            changeset.display()
        );
    }
    if let Some(ref_name) = ref_name {
        body["ref"] = json!(ref_name);
    }
    if let Some(mode) = mode {
        body["mode"] = json!(mode);
    }
    let client = authenticated(settings)?;
    client.post(&format!("/repos/{repo}/push"), &body).await
}

async fn merge(
    settings: &Settings,
    repo: &str,
    a: &str,
    b: &str,
    change_id: &str,
    author: Option<String>,
    message: Option<String>,
) -> Result<Value> {
    let author = author.unwrap_or_else(|| default_author(settings));
    let message = message.unwrap_or_else(|| format!("merge {b} into {a}"));
    let client = authenticated(settings)?;
    client
        .post(
            &format!("/repos/{repo}/merge"),
            &json!({
                "a": a,
                "b": b,
                "change_id": change_id,
                "meta": { "author": author, "message": message },
            }),
        )
        .await
}

#[allow(clippy::too_many_arguments)]
async fn merge_land(
    settings: &Settings,
    repo: &str,
    head: &str,
    ref_name: &str,
    author: Option<String>,
    message: Option<String>,
    change_id: Option<String>,
) -> Result<Value> {
    let client = authenticated(settings)?;
    client
        .post(
            &format!("/repos/{repo}/merge-land"),
            &json!({
                "ref": ref_name,
                "head": head,
                "author": author.unwrap_or_else(|| default_author(settings)),
                "message": message,
                "change_id": change_id,
            }),
        )
        .await
}

fn string_field(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("the service response is missing `{field}`"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::args::{Command, GlobalOptions, Parsed};

    fn command(args: &[&str]) -> Command {
        match crate::args::parse(args.iter().map(|arg| (*arg).to_owned()).collect()).expect("parse")
        {
            Parsed::Run(_, command) => command,
            Parsed::Help(_) => panic!("expected a command"),
        }
    }

    #[tokio::test]
    async fn commands_without_a_token_fail_with_a_mint_hint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let global = GlobalOptions {
            config_dir: Some(dir.path().to_owned()),
            ..GlobalOptions::default()
        };
        let error = execute(global, command(&["refs", "repo"]))
            .await
            .expect_err("no token");
        assert!(format!("{error:#}").contains("auth mint"), "{error:#}");
    }

    #[tokio::test]
    async fn auth_logout_is_safe_without_any_stored_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let global = GlobalOptions {
            config_dir: Some(dir.path().to_owned()),
            ..GlobalOptions::default()
        };
        let value = execute(global, command(&["auth", "logout"]))
            .await
            .expect("logout");
        assert_eq!(value["removed_token"], false);
        assert_eq!(value["removed_session"], false);
    }

    #[test]
    fn a_secret_file_must_be_private_and_non_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");
        std::fs::write(&path, "s3cret\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
            assert!(
                read_secret(Some(&path)).is_err(),
                "world-readable secret refused"
            );
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        assert_eq!(read_secret(Some(&path)).expect("private secret"), "s3cret");

        let empty = dir.path().join("empty");
        std::fs::write(&empty, "\n").expect("write");
        assert!(read_secret(Some(&empty)).is_err());
    }
}
