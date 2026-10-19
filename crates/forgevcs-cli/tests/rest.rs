//! End-to-end command tests against an in-process HTTP server.
//!
//! The mock speaks the exact routes the ForgeVCS façade owns, so these tests exercise the whole
//! CLI path — argument parsing, config resolution, token storage (0600), request shape, bearer
//! auth and response/error rendering — without a database, object store or network. The human
//! acceptance flow (mint → init → push → refs/log → merge → pull) runs exactly as it would
//! against a deployed service; only the server's answer is canned.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test crate

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde_json::{Value, json};

use forgevcs_cli::args::{self, Parsed};
use forgevcs_cli::commands;
use forgevcs_cli::token;

/// One request the CLI sent.
#[derive(Debug, Clone)]
struct Captured {
    method: String,
    uri: String,
    authorization: Option<String>,
    body: Value,
}

#[derive(Clone)]
struct MockState {
    captured: Arc<Mutex<Vec<Captured>>>,
    deny: bool,
}

struct Mock {
    base_url: String,
    captured: Arc<Mutex<Vec<Captured>>>,
}

async fn mock(deny: bool) -> Mock {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let state = MockState {
        captured: Arc::clone(&captured),
        deny,
    };
    let router = axum::Router::new()
        .route("/health", any(capture))
        .route("/ready", any(capture))
        .route("/version", any(capture))
        .route("/auth/human-token", any(capture))
        .route("/auth/whoami", any(capture))
        .fallback(any(capture))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Mock {
        base_url: format!("http://{addr}"),
        captured,
    }
}

async fn capture(State(state): State<MockState>, request: axum::extract::Request) -> Response {
    let method = request.method().as_str().to_owned();
    let uri = request.uri().to_string();
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = axum::body::to_bytes(request.into_body(), 10_000_000)
        .await
        .unwrap_or_default();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    state.captured.lock().expect("capture lock").push(Captured {
        method,
        uri: uri.clone(),
        authorization,
        body,
    });
    if state.deny {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "service": "forgevcs",
                "status": "error",
                "code": "permission_denied",
                "message": "policy denied the request",
            })),
        )
            .into_response();
    }
    let path = uri.split('?').next().unwrap_or("/");
    let value = match path {
        "/health" => json!({ "service": "forgevcs", "status": "ok" }),
        "/ready" => json!({ "service": "forgevcs", "status": "ready" }),
        "/version" => json!({ "service": "forgevcs", "status": "ok", "upstream": "test-commit" }),
        "/auth/human-token" => json!({
            "service": "forgevcs",
            "status": "ok",
            "token": "tok-culebraluxe",
            "principal": "github:culebraluxe",
            "email": "culebraluxe@gmail.com",
            "display": "Culebraluxe",
            "expires_at_unix": 1_900_000_000_i64,
        }),
        "/auth/whoami" => json!({
            "service": "forgevcs",
            "status": "ok",
            "principal": "github:culebraluxe",
            "groups": ["agents", "devops"],
            "expires_at_unix": 1_900_000_000_i64,
        }),
        _ => json!({ "service": "forgevcs", "status": "ok", "path": path }),
    };
    (StatusCode::OK, Json(value)).into_response()
}

fn cli_args(base_url: &str, config_dir: &Path, command: &[&str]) -> Vec<String> {
    let mut args = vec![
        "--base-url".to_owned(),
        base_url.to_owned(),
        "--config-dir".to_owned(),
        config_dir.display().to_string(),
    ];
    args.extend(command.iter().map(|arg| (*arg).to_owned()));
    args
}

async fn run(base_url: &str, config_dir: &Path, command: &[&str]) -> anyhow::Result<Value> {
    match args::parse(cli_args(base_url, config_dir, command))? {
        Parsed::Run(global, command) => commands::execute(global, command).await,
        Parsed::Help(_) => panic!("expected a command"),
    }
}

/// Write a private secret file for `auth mint --secret-file`.
fn write_secret(dir: &Path, secret: &str) -> std::path::PathBuf {
    let path = dir.join("secret");
    std::fs::write(&path, format!("{secret}\n")).expect("write secret");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("secret permissions");
    }
    path
}

/// A minimal valid `PushBody` on disk.
fn write_changeset(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("changeset.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&json!({
            "objects": [],
            "changeset": {
                "parents": [],
                "delta": [{"path": "hello.txt", "op": 2}],
                "meta": {
                    "author": "Culebraluxe <culebraluxe@gmail.com>",
                    "message": "delete hello.txt",
                },
                "change_id": "ab".repeat(16),
            },
            "ref": "main",
        }))
        .expect("serialize body"),
    )
    .expect("write changeset");
    path
}

fn mode(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }
    #[cfg(not(unix))]
    {
        0o600
    }
}

#[tokio::test]
async fn acceptance_as_culebraluxe_mint_init_push_refs_log_merge_pull() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = mock(false).await;
    let secret = write_secret(dir.path(), "test-human-secret");
    let changeset = write_changeset(dir.path());

    // 1. Mint and store the human token locally (0600), no browser involved.
    let minted = run(
        &mock.base_url,
        dir.path(),
        &[
            "auth",
            "mint",
            "--login",
            "culebraluxe",
            "--secret-file",
            secret.to_str().expect("secret path"),
        ],
    )
    .await
    .expect("mint");
    assert_eq!(minted["principal"], "github:culebraluxe");
    assert_eq!(minted["email"], "culebraluxe@gmail.com");
    assert_eq!(minted["display"], "Culebraluxe");
    let token_file = dir.path().join("token");
    assert_eq!(
        std::fs::read_to_string(&token_file).expect("token file"),
        "tok-culebraluxe\n"
    );
    assert_eq!(mode(&token_file), 0o600, "bearer token must be owner-only");
    assert_eq!(mode(&dir.path().join("session.json")), 0o600);
    assert!(
        minted.get("token").is_none(),
        "the token is stored, not printed, unless --show-token is passed"
    );

    // 2. Create the repository.
    let created = run(&mock.base_url, dir.path(), &["init", "human-repo"])
        .await
        .expect("init");
    assert_eq!(created["status"], "ok");

    // 3. Push a changeset from a file.
    let pushed = run(
        &mock.base_url,
        dir.path(),
        &[
            "push",
            "human-repo",
            "--changeset",
            changeset.to_str().expect("changeset path"),
            "--ref",
            "main",
        ],
    )
    .await
    .expect("push");
    assert_eq!(pushed["status"], "ok");

    // 4. Read refs and log.
    let _ = run(&mock.base_url, dir.path(), &["refs", "human-repo"])
        .await
        .expect("refs");
    let _ = run(
        &mock.base_url,
        dir.path(),
        &["log", "human-repo", "--ref", "main", "--limit", "5"],
    )
    .await
    .expect("log");

    // 5. Merge two changesets (author defaults to the stored human identity).
    let _ = run(
        &mock.base_url,
        dir.path(),
        &[
            "merge",
            "human-repo",
            "--a",
            "c1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--b",
            "c1bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "--change-id",
            &"cd".repeat(16),
            "--message",
            "merge lane",
        ],
    )
    .await
    .expect("merge");

    // 6. Pull history back.
    let pulled = run(
        &mock.base_url,
        dir.path(),
        &["pull", "human-repo", "--ref", "main", "--limit", "5"],
    )
    .await
    .expect("pull");
    assert_eq!(pulled["status"], "ok");

    let captured = mock.captured.lock().expect("captured").clone();
    let paths: Vec<&str> = captured
        .iter()
        .map(|request| request.uri.split('?').next().unwrap_or(""))
        .collect();
    assert_eq!(
        paths,
        vec![
            "/auth/human-token",
            "/repos/human-repo/init",
            "/repos/human-repo/push",
            "/repos/human-repo/refs",
            "/repos/human-repo/commits",
            "/repos/human-repo/merge",
            "/repos/human-repo/pull",
        ]
    );
    // The mint is anonymous; every later call carries the human's bearer token.
    assert_eq!(captured[0].authorization, None);
    for request in &captured[1..] {
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer tok-culebraluxe"),
            "{} must be authenticated",
            request.uri
        );
    }
    assert_eq!(captured[2].method, "POST");
    assert_eq!(captured[2].body["ref"], "main");
    assert_eq!(
        captured[2].body["changeset"]["change_id"],
        "ab".repeat(16),
        "the file body reaches the façade unchanged"
    );
    let log_query = &captured[4].uri;
    assert!(log_query.contains("ref=main"), "{log_query}");
    assert!(log_query.contains("limit=5"), "{log_query}");
    assert_eq!(
        captured[5].body["meta"]["author"], "Culebraluxe <culebraluxe@gmail.com>",
        "merge author defaults to the stored human session"
    );
    assert_eq!(captured[6].method, "POST");
    assert_eq!(captured[6].body["ref"], "main");
}

#[tokio::test]
async fn every_command_hits_its_façade_route() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = mock(false).await;
    token::write_token(&dir.path().join("token"), "tok-culebraluxe").expect("token");

    // `health` is deliberately anonymous (probes only); every other command sends the token.
    let before = mock.captured.lock().expect("captured").len();
    run(&mock.base_url, dir.path(), &["health"])
        .await
        .expect("health");
    {
        let captured = mock.captured.lock().expect("captured");
        assert_eq!(captured[before].uri, "/health");
        assert_eq!(captured[before].authorization, None);
        assert_eq!(captured.len(), before + 3);
    }

    let commands: Vec<(Vec<&str>, &str, &str)> = vec![
        (vec!["init", "repo"], "POST", "/repos/repo/init"),
        (vec!["refs", "repo"], "GET", "/repos/repo/refs"),
        (
            vec![
                "ref",
                "create",
                "repo",
                "--name",
                "lane/a",
                "--target",
                "c1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--reason",
                "provision lane",
            ],
            "POST",
            "/repos/repo/refs",
        ),
        (
            vec!["log", "repo", "--ref", "lane/a", "--limit", "3"],
            "GET",
            "/repos/repo/commits?ref=lane%2Fa&limit=3",
        ),
        (
            vec!["pull", "repo", "--objects", "b1aa"],
            "POST",
            "/repos/repo/pull",
        ),
        (
            vec!["rebase", "repo", "--changeset", "c1aa", "--onto", "c1bb"],
            "POST",
            "/repos/repo/rebase",
        ),
        (
            vec!["merge-land", "repo", "--head", "c1aa", "--ref", "main"],
            "POST",
            "/repos/repo/merge-land",
        ),
        (
            vec![
                "workspace",
                "create",
                "repo",
                "--ref",
                "main",
                "--sparse",
                "a,b",
            ],
            "POST",
            "/repos/repo/workspaces",
        ),
        (
            vec!["workspace", "status", "repo", "ws-1"],
            "GET",
            "/repos/repo/workspaces/ws-1",
        ),
        (
            vec!["workspace", "destroy", "repo", "ws-1"],
            "DELETE",
            "/repos/repo/workspaces/ws-1",
        ),
    ];
    for (command, method, path) in commands {
        let before = mock.captured.lock().expect("captured").len();
        run(&mock.base_url, dir.path(), &command)
            .await
            .unwrap_or_else(|error| panic!("{command:?}: {error:#}"));
        let captured = mock.captured.lock().expect("captured");
        let request = &captured[before];
        assert_eq!(request.method, method, "{command:?}");
        assert_eq!(request.uri, path, "{command:?}");
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer tok-culebraluxe"),
            "{command:?}"
        );
    }
    // The pull request carries the requested object id.
    let pull = mock
        .captured
        .lock()
        .expect("captured")
        .iter()
        .find(|request| request.uri == "/repos/repo/pull")
        .cloned()
        .expect("pull request");
    assert_eq!(pull.body["objects"], json!(["b1aa"]));
}

#[tokio::test]
async fn a_refusal_reports_the_facade_error_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = mock(true).await;
    token::write_token(&dir.path().join("token"), "tok-culebraluxe").expect("token");

    let error = run(&mock.base_url, dir.path(), &["init", "human-repo"])
        .await
        .expect_err("the mock denies every request");
    let message = format!("{error:#}");
    assert!(message.contains("403"), "{message}");
    assert!(message.contains("permission_denied"), "{message}");
    assert!(message.contains("policy denied the request"), "{message}");
}

#[tokio::test]
async fn auth_status_reports_the_local_token_and_the_service_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = mock(false).await;
    token::write_token(&dir.path().join("token"), "tok-culebraluxe").expect("token");
    token::write_session(
        &dir.path().join("session.json"),
        &token::Session {
            principal: "github:culebraluxe".to_owned(),
            email: "culebraluxe@gmail.com".to_owned(),
            display: "Culebraluxe".to_owned(),
            expires_at_unix: 1_900_000_000,
        },
    )
    .expect("session");

    let status = run(&mock.base_url, dir.path(), &["auth", "status"])
        .await
        .expect("auth status");
    assert_eq!(status["token_present"], true);
    assert_eq!(status["base_url"], mock.base_url);
    assert_eq!(status["whoami"]["principal"], "github:culebraluxe");
    assert_eq!(status["session"]["email"], "culebraluxe@gmail.com");
    assert_eq!(mock.captured.lock().expect("captured").len(), 1);
}

#[tokio::test]
async fn auth_status_without_a_token_is_informative_not_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = mock(false).await;
    let status = run(&mock.base_url, dir.path(), &["auth", "status"])
        .await
        .expect("auth status");
    assert_eq!(status["token_present"], false);
    assert!(status["whoami"].is_null());
    assert!(mock.captured.lock().expect("captured").is_empty());
}

#[tokio::test]
async fn health_reports_all_three_probes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = mock(false).await;
    let health = run(&mock.base_url, dir.path(), &["health"])
        .await
        .expect("health");
    assert_eq!(health["health"]["status"], "ok");
    assert_eq!(health["ready"]["ok"], true);
    assert_eq!(health["ready"]["body"]["status"], "ready");
    assert_eq!(health["version"]["upstream"], "test-commit");
    assert_eq!(mock.captured.lock().expect("captured").len(), 3);
}

#[tokio::test]
async fn push_honours_ref_and_mode_overrides() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = mock(false).await;
    token::write_token(&dir.path().join("token"), "tok-culebraluxe").expect("token");
    let changeset = write_changeset(dir.path());
    run(
        &mock.base_url,
        dir.path(),
        &[
            "push",
            "repo",
            "--changeset",
            changeset.to_str().expect("path"),
            "--ref",
            "lane/a",
            "--mode",
            "none",
        ],
    )
    .await
    .expect("push");
    let captured = mock.captured.lock().expect("captured");
    assert_eq!(captured[0].body["ref"], "lane/a");
    assert_eq!(captured[0].body["mode"], "none");
}
