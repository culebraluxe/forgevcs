//! Contract tests for the human CLI against an in-process mock of the REST façade.
//!
//! `tests/rest.rs` owns the end-to-end human walk-through (mint → init → push → refs/log →
//! merge → pull). This file deliberately covers the surrounding contract instead, so a failure
//! points at a different seam:
//!
//! - argument parser refusals (unknown flags/commands, missing required options, `--help`);
//! - configuration precedence (`config.toml` < environment < command-line flags);
//! - token/session storage: 0600, tightening a loose file, empty-token refusal, round-trip;
//! - the mint refusal path (401 `unauthenticated` is reported with the façade's code);
//! - request shapes for `log`/`pull --objects`/`push`/`merge`/`rebase`/`merge-land`/workspace;
//! - the "no token" refusal for every authenticated command;
//! - `health` probing `/health`, `/version` and `/ready`.
//!
//! No database, no object store, no external network: the server is a loopback axum router whose
//! answers come from a scripted queue, and every command runs through the same
//! `args::parse` + `commands::execute` path the binary uses.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test crate

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde_json::{Value, json};

use forgevcs_cli::args::{self, GlobalOptions, Parsed};
use forgevcs_cli::commands;
use forgevcs_cli::settings::Settings;
use forgevcs_cli::token::{self, Session};

// ---------------------------------------------------------------------------------------------
// Mock façade
// ---------------------------------------------------------------------------------------------

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
    responses: Arc<Mutex<VecDeque<(u16, Value)>>>,
}

/// A loopback HTTP server answering from a scripted queue and recording every request.
struct Mock {
    base_url: String,
    captured: Arc<Mutex<Vec<Captured>>>,
    responses: Arc<Mutex<VecDeque<(u16, Value)>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Mock {
    /// Start the server. The queue is consumed in order; an exhausted queue answers 500.
    async fn start(responses: Vec<(u16, Value)>) -> Self {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let state = MockState {
            captured: Arc::clone(&captured),
            responses: Arc::clone(&responses),
        };
        let router = axum::Router::new().fallback(any(capture)).with_state(state);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind a loopback port");
        let address = listener.local_addr().expect("local address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            base_url: format!("http://{address}"),
            captured,
            responses,
            server,
        }
    }

    fn requests(&self) -> Vec<Captured> {
        self.captured.lock().expect("captured lock").clone()
    }

    fn request(&self, index: usize) -> Captured {
        self.requests()
            .get(index)
            .cloned()
            .unwrap_or_else(|| panic!("no request at index {index}"))
    }

    fn pending(&self) -> usize {
        self.responses.lock().expect("responses lock").len()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn capture(State(state): State<MockState>, request: Request) -> Response {
    let method = request.method().as_str().to_owned();
    let uri = request.uri().to_string();
    let authorization = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .map(|bytes| serde_json::from_slice(&bytes).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);
    state
        .captured
        .lock()
        .expect("captured lock")
        .push(Captured {
            method,
            uri,
            authorization,
            body,
        });
    let (status, body) = state
        .responses
        .lock()
        .expect("responses lock")
        .pop_front()
        .unwrap_or_else(|| {
            (
                500,
                json!({ "code": "mock_exhausted", "message": "no scripted response left" }),
            )
        });
    (
        StatusCode::from_u16(status).expect("scripted status"),
        Json(body),
    )
        .into_response()
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

const TOKEN: &str = "test-bearer-token";

/// A temp config directory holding a private token file, as `auth mint` would leave it.
struct Store {
    dir: tempfile::TempDir,
}

impl Store {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        token::write_token(&dir.path().join("token"), TOKEN).expect("write token");
        Self { dir }
    }

    fn config_dir(&self) -> &Path {
        self.dir.path()
    }

    fn token_path(&self) -> PathBuf {
        self.dir.path().join("token")
    }

    fn write_changeset(&self, body: &Value) -> PathBuf {
        let path = self.dir.path().join("push-body.json");
        std::fs::write(&path, serde_json::to_vec_pretty(body).expect("serialize"))
            .expect("write changeset");
        path
    }

    fn write_secret(&self, secret: &str) -> PathBuf {
        let path = self.dir.path().join("human-secret");
        token::write_token(&path, secret).expect("write private secret");
        path
    }
}

/// Parse and execute exactly as the binary does, pointed at the mock service.
async fn run_against(store: &Store, base_url: &str, command: &[&str]) -> anyhow::Result<Value> {
    let mut argv = vec![
        "--base-url".to_owned(),
        base_url.to_owned(),
        "--config-dir".to_owned(),
        store.config_dir().display().to_string(),
    ];
    argv.extend(command.iter().map(|arg| (*arg).to_owned()));
    execute_argv(argv).await
}

async fn execute_argv(argv: Vec<String>) -> anyhow::Result<Value> {
    match args::parse(argv)? {
        Parsed::Run(global, command) => commands::execute(global, command).await,
        Parsed::Help(text) => Ok(Value::String(text)),
    }
}

fn mode_of(path: &Path) -> u32 {
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
        let _ = path;
        0
    }
}

// ---------------------------------------------------------------------------------------------
// Argument parsing refusals
// ---------------------------------------------------------------------------------------------

#[test]
fn malformed_invocations_are_refused_locally() {
    let cases: &[&[&str]] = &[
        &["bogus"],                  // unknown command
        &["--nope", "refs", "repo"], // unknown global flag
        &["init"],                   // missing positional
        &["init", "a", "b"],         // extra positional
        &["push", "repo"],           // push needs --changeset
        &["push", "repo", "--mode", "bad", "--changeset", "x"],
        &["merge", "repo", "--a", "a"], // missing --b/--change-id
        &["rebase", "repo", "--changeset", "id"], // missing --onto
        &["merge-land", "repo"],        // missing --head
        &["workspace"],                 // missing subcommand
        &["workspace", "create"],       // missing repo
        &["auth"],                      // missing subcommand
        &["auth", "mint"],              // missing --login
        &["ref"],                       // missing subcommand
        &["ref", "delete", "repo"],     // unknown ref subcommand
        &["ref", "create", "repo"],     // missing --name/--target
        &["log", "repo", "--limit", "not-a-number"],
    ];
    for argv in cases {
        let error = args::parse(argv.iter().map(|arg| (*arg).to_owned()).collect())
            .expect_err(&format!("must refuse {argv:?}"));
        let text = format!("{error:#}");
        assert!(!text.is_empty(), "{argv:?} produced an empty error");
    }
}

#[test]
fn help_is_success_and_names_every_work_order_command() {
    let Parsed::Help(text) = args::parse(vec!["--help".to_owned()]).expect("help") else {
        panic!("--help must not run a command");
    };
    for command in [
        "auth status",
        "auth mint",
        "health",
        "init",
        "refs",
        "ref create",
        "log",
        "pull",
        "push",
        "merge",
        "rebase",
        "merge-land",
        "workspace create",
        "workspace status",
        "workspace destroy",
    ] {
        assert!(text.contains(command), "usage text is missing {command:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Configuration and token storage
// ---------------------------------------------------------------------------------------------

#[test]
fn settings_precedence_is_flag_then_environment_then_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("config.toml"),
        "base_url = \"https://file.example\"\ntoken_file = \"file-token\"\n",
    )
    .expect("write config");
    let env = |key: &str| match key {
        "FORGEVCS_BASE_URL" => Some("https://env.example".to_owned()),
        "FORGEVCS_TOKEN_FILE" => Some("/env/token".to_owned()),
        _ => None,
    };
    let from_file_and_env = Settings::resolve_with(
        &GlobalOptions {
            config_dir: Some(dir.path().to_owned()),
            ..GlobalOptions::default()
        },
        env,
    )
    .expect("settings");
    assert_eq!(from_file_and_env.base_url, "https://env.example");
    assert_eq!(from_file_and_env.token_file, PathBuf::from("/env/token"));

    let from_flags = Settings::resolve_with(
        &GlobalOptions {
            base_url: Some("https://cli.example".to_owned()),
            config_dir: Some(dir.path().to_owned()),
            token_file: Some(dir.path().join("cli-token")),
        },
        env,
    )
    .expect("settings");
    assert_eq!(from_flags.base_url, "https://cli.example");
    assert_eq!(from_flags.token_file, dir.path().join("cli-token"));

    // A config-file token path is relative to the config directory.
    let from_file = Settings::resolve_with(
        &GlobalOptions {
            config_dir: Some(dir.path().to_owned()),
            ..GlobalOptions::default()
        },
        |_| None,
    )
    .expect("settings");
    assert_eq!(from_file.token_file, dir.path().join("file-token"));
    assert_eq!(from_file.session_file(), dir.path().join("session.json"));
}

#[test]
fn token_storage_is_private_and_a_loose_file_is_tightened() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("token");
    token::write_token(&path, "secret-value").expect("write");
    assert_eq!(
        token::read_token(&path).expect("read").as_deref(),
        Some("secret-value")
    );
    assert_eq!(mode_of(&path), 0o600, "a minted token must be owner-only");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(
            token::read_token(&path).expect("read").as_deref(),
            Some("secret-value")
        );
        assert_eq!(
            mode_of(&path),
            0o600,
            "a loose token file is re-tightened on read"
        );
    }

    std::fs::write(&path, "  \n").expect("write empty");
    assert!(
        token::read_token(&path).is_err(),
        "empty token must be an error"
    );
    assert!(
        token::write_token(&path, "   ").is_err(),
        "empty token must not be stored"
    );
    assert_eq!(
        token::read_token(&dir.path().join("missing")).expect("missing"),
        None
    );
}

#[test]
fn session_metadata_round_trips_and_formats_the_default_author() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.json");
    let session = Session {
        principal: "github:culebraluxe".to_owned(),
        email: "culebraluxe@gmail.com".to_owned(),
        display: "Culebraluxe".to_owned(),
        expires_at_unix: 1_770_000_000,
    };
    assert_eq!(session.author(), "Culebraluxe <culebraluxe@gmail.com>");
    token::write_session(&path, &session).expect("write session");
    assert_eq!(
        token::read_session(&path).expect("read session"),
        Some(session)
    );
    assert_eq!(
        mode_of(&path),
        0o600,
        "session metadata is not world-readable either"
    );
}

// ---------------------------------------------------------------------------------------------
// auth mint / auth status
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn auth_mint_refusal_reports_the_facade_code_and_stores_nothing() {
    let mock = Mock::start(vec![(
        401,
        json!({
            "service": "forgevcs",
            "status": "error",
            "code": "unauthenticated",
            "message": "invalid human credentials",
        }),
    )])
    .await;
    let store = Store::new();
    let secret = store.write_secret("not-the-right-secret");
    let error = run_against(
        &store,
        &mock.base_url,
        &[
            "auth",
            "mint",
            "--login",
            "culebraluxe",
            "--secret-file",
            secret.to_str().unwrap(),
        ],
    )
    .await
    .expect_err("a 401 must fail the mint");
    let text = format!("{error:#}");
    assert!(text.contains("unauthenticated"), "{text}");
    assert!(text.contains("invalid human credentials"), "{text}");

    let request = mock.request(0);
    assert_eq!(request.method, "POST");
    assert_eq!(request.uri, "/auth/human-token");
    assert_eq!(request.body["login"], json!("culebraluxe"));
    assert_eq!(request.body["secret"], json!("not-the-right-secret"));
    assert_eq!(mock.pending(), 0);
    // Nothing was written: the token file still holds the fixture token, not a minted one.
    assert_eq!(
        token::read_token(&store.token_path())
            .expect("read")
            .as_deref(),
        Some(TOKEN)
    );
}

#[tokio::test]
async fn auth_mint_stores_a_private_token_and_non_secret_session() {
    let mock = Mock::start(vec![(
        200,
        json!({
            "service": "forgevcs",
            "status": "ok",
            "token": "minted-token-value",
            "principal": "github:culebraluxe",
            "email": "culebraluxe@gmail.com",
            "display": "Culebraluxe",
            "expires_at_unix": 1_770_000_000i64,
        }),
    )])
    .await;
    let store = Store::new();
    let secret = store.write_secret("the-human-secret");
    let value = run_against(
        &store,
        &mock.base_url,
        &[
            "auth",
            "mint",
            "--login",
            "culebraluxe",
            "--secret-file",
            secret.to_str().unwrap(),
        ],
    )
    .await
    .expect("mint");
    assert_eq!(value["principal"], json!("github:culebraluxe"));
    assert!(
        value.get("token").is_none(),
        "the token must not be printed by default"
    );
    assert_eq!(
        token::read_token(&store.token_path())
            .expect("read")
            .as_deref(),
        Some("minted-token-value")
    );
    assert_eq!(mode_of(&store.token_path()), 0o600);
    let session = token::read_session(&store.config_dir().join("session.json"))
        .expect("read session")
        .expect("session written");
    assert_eq!(session.principal, "github:culebraluxe");
    assert_eq!(session.email, "culebraluxe@gmail.com");
    assert_eq!(session.display, "Culebraluxe");
    assert_eq!(session.expires_at_unix, 1_770_000_000);
}

#[tokio::test]
async fn auth_status_reports_local_state_and_the_verified_identity() {
    let store = Store::new();
    token::write_session(
        &store.config_dir().join("session.json"),
        &Session {
            principal: "github:culebraluxe".to_owned(),
            email: "culebraluxe@gmail.com".to_owned(),
            display: "Culebraluxe".to_owned(),
            expires_at_unix: 1_770_000_000,
        },
    )
    .expect("write session");
    let mock = Mock::start(vec![(
        200,
        json!({
            "service": "forgevcs",
            "status": "ok",
            "principal": "github:culebraluxe",
            "groups": [],
            "expires_at_unix": 1_770_000_000i64,
        }),
    )])
    .await;
    let value = run_against(&store, &mock.base_url, &["auth", "status"])
        .await
        .expect("auth status");
    assert_eq!(value["token_present"], json!(true));
    assert_eq!(value["whoami"]["principal"], json!("github:culebraluxe"));
    assert_eq!(value["session"]["email"], json!("culebraluxe@gmail.com"));
    let request = mock.request(0);
    assert_eq!(request.method, "GET");
    assert_eq!(request.uri, "/auth/whoami");
    assert_eq!(
        request.authorization.as_deref(),
        Some("Bearer test-bearer-token")
    );
}

// ---------------------------------------------------------------------------------------------
// Authenticated commands: request shapes
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn init_refs_log_pull_and_push_hit_their_facade_routes() {
    let store = Store::new();
    let changeset_path = store.write_changeset(&json!({
        "objects": [],
        "changeset": {
            "parents": [],
            "delta": [],
            "meta": { "author": "Culebraluxe <culebraluxe@gmail.com>", "message": "demo" },
            "change_id": "00000000000000000000000000000000"
        },
        "ref": "main"
    }));
    let mock = Mock::start(vec![
        (
            200,
            json!({ "status": "ok", "repo": "demo", "created": true, "refs": [] }),
        ),
        (200, json!({ "status": "ok", "repo": "demo", "refs": [] })),
        (
            200,
            json!({
                "status": "ok",
                "repo": "demo",
                "ref": "lane/a",
                "target": "c1aa",
                "version": 1,
                "commits": [],
                "next_page_token": null
            }),
        ),
        (
            200,
            json!({ "status": "ok", "repo": "demo", "objects": [] }),
        ),
        (
            200,
            json!({ "status": "ok", "repo": "demo", "changeset": { "id": "c1bb" } }),
        ),
    ])
    .await;

    run_against(&store, &mock.base_url, &["init", "demo"])
        .await
        .expect("init");
    run_against(&store, &mock.base_url, &["refs", "demo"])
        .await
        .expect("refs");
    run_against(
        &store,
        &mock.base_url,
        &[
            "log",
            "demo",
            "--ref",
            "lane/a",
            "--limit",
            "3",
            "--path",
            "src/lib.rs",
            "--first-parent-only",
        ],
    )
    .await
    .expect("log");
    run_against(
        &store,
        &mock.base_url,
        &["pull", "demo", "--ref", "lane/a", "--objects", "b1aa,b1bb"],
    )
    .await
    .expect("pull");
    run_against(
        &store,
        &mock.base_url,
        &[
            "push",
            "demo",
            "--changeset",
            changeset_path.to_str().unwrap(),
            "--ref",
            "lane/a",
            "--mode",
            "none",
        ],
    )
    .await
    .expect("push");

    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].uri, "/repos/demo/init");
    assert_eq!(requests[1].method, "GET");
    assert_eq!(requests[1].uri, "/repos/demo/refs");
    assert_eq!(requests[2].method, "GET");
    let log_uri = &requests[2].uri;
    for fragment in ["ref=lane", "limit=3", "path=src", "first_parent_only=true"] {
        assert!(
            log_uri.contains(fragment),
            "{log_uri} is missing {fragment}"
        );
    }
    assert_eq!(requests[3].method, "POST");
    assert_eq!(requests[3].uri, "/repos/demo/pull");
    assert_eq!(requests[3].body["ref"], json!("lane/a"));
    assert_eq!(requests[3].body["objects"], json!(["b1aa", "b1bb"]));
    assert_eq!(requests[3].body["limit"], json!(100));
    assert_eq!(requests[4].method, "POST");
    assert_eq!(requests[4].uri, "/repos/demo/push");
    assert_eq!(
        requests[4].body["ref"],
        json!("lane/a"),
        "--ref overrides the file"
    );
    assert_eq!(
        requests[4].body["mode"],
        json!("none"),
        "--mode overrides the file"
    );
    assert_eq!(
        requests[4].body["changeset"]["change_id"],
        json!("00000000000000000000000000000000")
    );
    for request in &requests {
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer test-bearer-token"),
            "{} {} must be authenticated",
            request.method,
            request.uri
        );
    }
}

/// `ref create` provisions a lane ref at an existing changeset (`POST /repos/{repo}/refs`),
/// which is what makes `push --mode update` usable; it needs `admin.refs` and its refusal is
/// surfaced like any other façade decision.
#[tokio::test]
async fn ref_create_provisions_a_ref_and_reports_refusals() {
    let store = Store::new();
    let mock = Mock::start(vec![
        (
            200,
            json!({
                "status": "ok",
                "repo": "demo",
                "ref": { "name": "lane/a", "target": "c1aa", "version": 1 }
            }),
        ),
        (
            403,
            json!({
                "service": "forgevcs",
                "status": "error",
                "code": "permission_denied",
                "message": "creating a ref requires admin.refs",
            }),
        ),
    ])
    .await;
    let created = run_against(
        &store,
        &mock.base_url,
        &[
            "ref",
            "create",
            "demo",
            "--name",
            "lane/a",
            "--target",
            "c1aa",
            "--reason",
            "cut lane/a",
        ],
    )
    .await
    .expect("ref create");
    assert_eq!(created["ref"]["name"], json!("lane/a"));
    let request = mock.request(0);
    assert_eq!(request.method, "POST");
    assert_eq!(request.uri, "/repos/demo/refs");
    assert_eq!(request.body["name"], json!("lane/a"));
    assert_eq!(request.body["target"], json!("c1aa"));
    assert_eq!(request.body["reason"], json!("cut lane/a"));
    assert_eq!(
        request.authorization.as_deref(),
        Some("Bearer test-bearer-token")
    );

    let error = run_against(
        &store,
        &mock.base_url,
        &[
            "ref", "create", "demo", "--name", "lane/b", "--target", "c1bb",
        ],
    )
    .await
    .expect_err("admin.refs is required");
    let text = format!("{error:#}");
    assert!(text.contains("HTTP 403"), "{text}");
    assert!(text.contains("permission_denied"), "{text}");
    assert!(text.contains("admin.refs"), "{text}");
    // `--reason` is optional and defaults to the empty string the route expects.
    assert_eq!(mock.request(1).body["reason"], json!(""));
}

#[tokio::test]
async fn merge_rebase_and_merge_land_send_the_documented_bodies() {
    let store = Store::new();
    let mock = Mock::start(vec![
        (
            200,
            json!({ "status": "ok", "changeset": { "id": "c1merge" } }),
        ),
        (
            200,
            json!({ "status": "ok", "changeset": { "id": "c1rebase" } }),
        ),
        (
            200,
            json!({
                "status": "ok",
                "merge_changeset": { "id": "c1m" },
                "changeset": { "id": "c1linear" },
                "ref": { "name": "main", "target": "c1linear", "version": 2 },
                "landed": []
            }),
        ),
    ])
    .await;

    run_against(
        &store,
        &mock.base_url,
        &[
            "merge",
            "demo",
            "--a",
            "c1aa",
            "--b",
            "c1bb",
            "--change-id",
            "0123456789abcdef0123456789abcdef",
            "--message",
            "join the lanes",
        ],
    )
    .await
    .expect("merge");
    run_against(
        &store,
        &mock.base_url,
        &["rebase", "demo", "--changeset", "c1aa", "--onto", "c1bb"],
    )
    .await
    .expect("rebase");
    run_against(
        &store,
        &mock.base_url,
        &[
            "merge-land",
            "demo",
            "--head",
            "c1head",
            "--message",
            "land it",
        ],
    )
    .await
    .expect("merge-land");

    let requests = mock.requests();
    assert_eq!(requests[0].uri, "/repos/demo/merge");
    assert_eq!(requests[0].body["a"], json!("c1aa"));
    assert_eq!(requests[0].body["b"], json!("c1bb"));
    assert_eq!(
        requests[0].body["change_id"],
        json!("0123456789abcdef0123456789abcdef")
    );
    assert_eq!(requests[0].body["meta"]["message"], json!("join the lanes"));
    assert!(requests[0].body["meta"]["author"].as_str().is_some());

    assert_eq!(requests[1].uri, "/repos/demo/rebase");
    assert_eq!(
        requests[1].body,
        json!({ "changeset": "c1aa", "onto": "c1bb" })
    );

    assert_eq!(requests[2].uri, "/repos/demo/merge-land");
    assert_eq!(requests[2].body["ref"], json!("main"));
    assert_eq!(requests[2].body["head"], json!("c1head"));
    assert_eq!(requests[2].body["message"], json!("land it"));
}

#[tokio::test]
async fn workspace_lifecycle_sends_the_documented_bodies() {
    let store = Store::new();
    let mock = Mock::start(vec![
        (
            200,
            json!({ "status": "ok", "repo": "demo", "workspace_id": "ws-1", "state": {} }),
        ),
        (
            200,
            json!({ "status": "ok", "repo": "demo", "workspace": { "id": "ws-1" } }),
        ),
        (
            200,
            json!({ "status": "ok", "repo": "demo", "workspace_id": "ws-1", "destroyed": true }),
        ),
    ])
    .await;

    run_against(
        &store,
        &mock.base_url,
        &["workspace", "create", "demo", "--sparse", "a,b"],
    )
    .await
    .expect("workspace create");
    run_against(
        &store,
        &mock.base_url,
        &["workspace", "status", "demo", "ws-1"],
    )
    .await
    .expect("workspace status");
    run_against(
        &store,
        &mock.base_url,
        &["workspace", "destroy", "demo", "ws-1"],
    )
    .await
    .expect("workspace destroy");

    let requests = mock.requests();
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].uri, "/repos/demo/workspaces");
    assert_eq!(requests[0].body["sparse"], json!(["a", "b"]));
    assert_eq!(requests[1].method, "GET");
    assert_eq!(requests[1].uri, "/repos/demo/workspaces/ws-1");
    assert_eq!(requests[2].method, "DELETE");
    assert_eq!(requests[2].uri, "/repos/demo/workspaces/ws-1");
}

#[tokio::test]
async fn health_probes_health_version_and_ready_without_a_token() {
    let store = Store::new();
    let mock = Mock::start(vec![
        (
            200,
            json!({ "service": "forgevcs", "status": "ok", "version": "0.1.0" }),
        ),
        (
            200,
            json!({ "service": "forgevcs", "status": "ok", "version": "0.1.0", "upstream": "abc" }),
        ),
        (200, json!({ "service": "forgevcs", "status": "ready" })),
    ])
    .await;
    let value = run_against(&store, &mock.base_url, &["health"])
        .await
        .expect("health");
    assert_eq!(value["health"]["status"], json!("ok"));
    assert_eq!(value["version"]["upstream"], json!("abc"));
    assert_eq!(value["ready"]["ok"], json!(true));
    let requests = mock.requests();
    let paths: Vec<&str> = requests
        .iter()
        .map(|request| request.uri.as_str())
        .collect();
    assert_eq!(paths, vec!["/health", "/version", "/ready"]);
    for request in &requests {
        assert!(
            request.authorization.is_none(),
            "probes must not send the bearer token"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_service_refusal_keeps_the_facade_error_code() {
    let store = Store::new();
    let changeset_path = store.write_changeset(&json!({
        "objects": [],
        "changeset": {
            "parents": [],
            "delta": [],
            "meta": { "author": "Culebraluxe <culebraluxe@gmail.com>", "message": "demo" },
            "change_id": "00000000000000000000000000000000"
        }
    }));
    let mock = Mock::start(vec![(
        403,
        json!({
            "service": "forgevcs",
            "status": "error",
            "code": "permission_denied",
            "message": "push denied: write on lane/b/** required",
        }),
    )])
    .await;
    let error = run_against(
        &store,
        &mock.base_url,
        &[
            "push",
            "demo",
            "--changeset",
            changeset_path.to_str().unwrap(),
        ],
    )
    .await
    .expect_err("403 must fail");
    let text = format!("{error:#}");
    assert!(text.contains("HTTP 403"), "{text}");
    assert!(text.contains("permission_denied"), "{text}");
    assert!(text.contains("write on lane/b/** required"), "{text}");
    assert_eq!(mock.request(0).uri, "/repos/demo/push");
}

/// `--mode update` can only update an existing ref: a new-ref update is a façade 404, not an
/// implicit ref creation. The CLI must surface that verdict with the façade's code and pass
/// `expected_version` through untouched.
#[tokio::test]
async fn push_mode_update_against_a_missing_ref_surfaces_the_404() {
    let store = Store::new();
    let changeset_path = store.write_changeset(&json!({
        "objects": [],
        "changeset": {
            "parents": [],
            "delta": [],
            "meta": { "author": "Culebraluxe <culebraluxe@gmail.com>", "message": "demo" },
            "change_id": "00000000000000000000000000000000",
            "expected_version": 3
        },
        "ref": "lane/a",
        "mode": "update"
    }));
    let mock = Mock::start(vec![(
        404,
        json!({
            "service": "forgevcs",
            "status": "error",
            "code": "not_found",
            "message": "ref lane/a not found",
        }),
    )])
    .await;
    let error = run_against(
        &store,
        &mock.base_url,
        &[
            "push",
            "demo",
            "--changeset",
            changeset_path.to_str().unwrap(),
        ],
    )
    .await
    .expect_err("a missing ref must fail the update");
    let text = format!("{error:#}");
    assert!(text.contains("HTTP 404"), "{text}");
    assert!(text.contains("not_found"), "{text}");
    assert!(text.contains("ref lane/a not found"), "{text}");
    let request = mock.request(0);
    assert_eq!(request.uri, "/repos/demo/push");
    assert_eq!(request.body["mode"], json!("update"));
    assert_eq!(request.body["ref"], json!("lane/a"));
    assert_eq!(request.body["changeset"]["expected_version"], json!(3));
}

#[tokio::test]
async fn authenticated_commands_fail_early_with_a_mint_hint_when_no_token_exists() {
    let store = Store::new();
    std::fs::remove_file(store.token_path()).expect("remove fixture token");
    let mock = Mock::start(Vec::new()).await;
    for command in [
        vec!["init", "demo"],
        vec!["refs", "demo"],
        vec!["log", "demo"],
        vec!["pull", "demo"],
        vec!["merge-land", "demo", "--head", "c1aa"],
        vec!["workspace", "status", "demo", "ws-1"],
    ] {
        let error = run_against(&store, &mock.base_url, &command)
            .await
            .expect_err(&format!("{command:?} must need a token"));
        let text = format!("{error:#}");
        assert!(text.contains("auth mint"), "{command:?}: {text}");
    }
    assert!(
        mock.requests().is_empty(),
        "a missing token must fail before any request is sent"
    );
}

#[tokio::test]
async fn auth_logout_removes_local_state_and_needs_no_service() {
    let store = Store::new();
    token::write_session(
        &store.config_dir().join("session.json"),
        &Session {
            principal: "github:culebraluxe".to_owned(),
            email: "culebraluxe@gmail.com".to_owned(),
            display: "Culebraluxe".to_owned(),
            expires_at_unix: 1_770_000_000,
        },
    )
    .expect("write session");
    let value = execute_argv(vec![
        "--config-dir".to_owned(),
        store.config_dir().display().to_string(),
        "auth".to_owned(),
        "logout".to_owned(),
    ])
    .await
    .expect("logout");
    assert_eq!(value["removed_token"], json!(true));
    assert_eq!(value["removed_session"], json!(true));
    assert!(!store.token_path().exists());
    assert!(!store.config_dir().join("session.json").exists());

    // Idempotent: a second logout reports that nothing was left to remove.
    let again = execute_argv(vec![
        "--config-dir".to_owned(),
        store.config_dir().display().to_string(),
        "auth".to_owned(),
        "logout".to_owned(),
    ])
    .await
    .expect("second logout");
    assert_eq!(again["removed_token"], json!(false));
    assert_eq!(again["removed_session"], json!(false));
}

// ---------------------------------------------------------------------------------------------
// Binary-level: the `FORGEVCS_TOKEN` escape hatch for CI
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn forgevcs_token_environment_supplies_the_bearer_without_a_token_file() {
    let mock = Mock::start(vec![
        (200, json!({ "status": "ok", "repo": "demo", "refs": [] })),
        (
            200,
            json!({
                "status": "ok",
                "principal": "github:culebraluxe",
                "groups": [],
                "expires_at_unix": 1_770_000_000i64,
            }),
        ),
    ])
    .await;
    let config_dir = tempfile::tempdir().expect("tempdir");
    // No token file in config_dir: the environment must be the only credential.
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_forgevcs"))
        .env("FORGEVCS_TOKEN", "foreground-ci-token")
        .env_remove("FORGEVCS_BASE_URL")
        .env_remove("FORGEVCS_CONFIG_DIR")
        .env_remove("FORGEVCS_TOKEN_FILE")
        .arg("--base-url")
        .arg(&mock.base_url)
        .arg("--config-dir")
        .arg(config_dir.path())
        .args(["refs", "demo"])
        .output()
        .await
        .expect("run the binary");
    assert!(
        output.status.success(),
        "exit {:?}, stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let request = mock.request(0);
    assert_eq!(request.method, "GET");
    assert_eq!(request.uri, "/repos/demo/refs");
    assert_eq!(
        request.authorization.as_deref(),
        Some("Bearer foreground-ci-token")
    );
    // Nothing was persisted by the read-only command.
    assert!(!config_dir.path().join("token").exists());

    // `auth status` must report the environment credential as such (no file involved).
    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_forgevcs"))
        .env("FORGEVCS_TOKEN", "foreground-ci-token")
        .env_remove("FORGEVCS_BASE_URL")
        .env_remove("FORGEVCS_CONFIG_DIR")
        .env_remove("FORGEVCS_TOKEN_FILE")
        .arg("--base-url")
        .arg(&mock.base_url)
        .arg("--config-dir")
        .arg(config_dir.path())
        .args(["auth", "status"])
        .output()
        .await
        .expect("run auth status");
    assert!(
        status.status.success(),
        "exit {:?}, stderr: {}",
        status.status.code(),
        String::from_utf8_lossy(&status.stderr)
    );
    let value: Value = serde_json::from_slice(&status.stdout).expect("auth status JSON");
    assert_eq!(value["token_present"], json!(true));
    assert_eq!(value["token_source"], json!("env"));
    assert_eq!(value["whoami"]["principal"], json!("github:culebraluxe"));
    assert!(
        value.get("token").is_none(),
        "auth status must never echo the token"
    );
    let whoami = mock.request(1);
    assert_eq!(whoami.uri, "/auth/whoami");
    assert_eq!(
        whoami.authorization.as_deref(),
        Some("Bearer foreground-ci-token")
    );
}
