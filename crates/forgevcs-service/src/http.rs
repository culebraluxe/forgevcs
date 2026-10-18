//! The HTTP surface: probes, auth, and the business façade routes.
//!
//! Façade endpoints never re-implement Scion's rules. They convert JSON to `scion.v1`
//! messages, call the vendored handler in-process, and convert the answer back — with the
//! caller's `CallerCtx` produced by `interceptor::authenticate`, the same code the gRPC
//! stack runs (ADR-0002 D4). No loopback gRPC, no second authorization path.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use scion_core::{EntryInfo, ErrorPayload};
use scion_obs::{Metrics, Readiness, health_router};
use scion_proto::{error_from_status, status_from_error, v1};
use scion_server::context::CallerCtx;
use scion_server::{handlers, interceptor};
use serde::Deserialize;
use serde_json::{Value, json};

use deadpool_postgres::Pool;

use crate::authz::AuthorizationService;
use crate::config::{self, Config};
use crate::engine::{Engine, Engines};
use crate::facade;

/// Service name reported by every probe and façade response.
pub const SERVICE: &str = "forgevcs";

/// Everything the handlers need: probe state, the engine registry and the repo this process
/// serves (ADR-0002 D3).
pub struct AppState {
    /// Last dependency probe result, surfaced by `/ready` (D5).
    pub ready: Arc<dyn Readiness>,
    /// Engines that booted, keyed by repo name.
    pub engines: Arc<Engines>,
    /// The repo served by this process.
    pub repo: String,
    /// The process configuration, so `POST /repos/{repo}/init` can boot another engine.
    pub config: Config,
    /// The Scion/Culebra authorization A/B harness (W6).
    pub authz: Arc<AuthorizationService>,
    /// Pool for the `repos` registry, used to boot an existing repository on demand.
    pub db: Option<Pool>,
}

impl AppState {
    /// The engine the façade talks to. `None` when boot failed or the repo is unknown: every
    /// endpoint that needs storage answers 503 instead of panicking (D5).
    fn engine(&self) -> Option<Arc<Engine>> {
        self.engines.get(&self.repo)
    }

    /// The engine for an explicit repo path segment.
    pub(crate) fn engine_for(&self, repo: &str) -> Option<Arc<Engine>> {
        self.engines.get(repo)
    }

    /// The engine that verifies bearer tokens for the whole process.
    ///
    /// Every engine loads the same auth configuration, but an ephemeral keyring is generated
    /// per engine, so verification must not follow the repo being addressed: a token minted by
    /// `/auth/token` (which uses the default engine) has to be accepted on a repo initialized
    /// later in the same process. Preferring the default engine and falling back to any booted
    /// engine gives one verifier; with `FORGEVCS_SIGNING_KEY_PATH` set all of them are
    /// interchangeable anyway.
    fn auth_engine(&self) -> Option<Arc<Engine>> {
        self.engine()
            .or_else(|| self.engines.all().into_iter().next())
    }
}

/// Verify the caller and return the engine whose handlers should run plus the `CallerCtx`
/// every handler expects. This is the only auth insertion point of the façade (ADR-0002 D4).
#[allow(clippy::result_large_err)] // the error is the HTTP response this helper would send
pub(crate) fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(Arc<Engine>, CallerCtx), Response> {
    let engine = state.auth_engine().ok_or_else(|| no_engine(&state.repo))?;
    let app = engine.context();
    let claims = interceptor::authenticate(headers, &app.verifier, &*app.clock)
        .map_err(|error| status_response(status_from_error(&error)))?;
    Ok((
        engine,
        CallerCtx {
            principal: claims.sub.clone(),
            claims,
            request_id: String::new(),
        },
    ))
}

/// Build the `tonic::Request` a handler reads its `CallerCtx` from.
pub(crate) fn caller_request<T>(body: T, caller: &CallerCtx) -> tonic::Request<T> {
    let mut request = tonic::Request::new(body);
    request.extensions_mut().insert(caller.clone());
    request
}

fn payload(status: &str) -> Value {
    json!({
        "service": SERVICE,
        "status": status,
        "version": env!("CARGO_PKG_VERSION"),
    })
}

async fn health() -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(payload("ok")))
}

async fn version() -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "service": SERVICE,
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "upstream": config::UPSTREAM_COMMIT,
        })),
    )
}

async fn ready_handler(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    match state.ready.check() {
        Ok(()) => (StatusCode::OK, Json(payload("ready"))),
        Err(reason) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "service": SERVICE,
                "status": "not_ready",
                "version": env!("CARGO_PKG_VERSION"),
                "reason": reason,
            })),
        ),
    }
}

/// Build the full router. Public so tests drive exactly the routes the container serves.
pub fn router(state: Arc<AppState>) -> Router {
    let metrics = Metrics::new();
    let ready = Arc::clone(&state.ready);

    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready_handler))
        .route("/version", get(version))
        .route("/auth/token", post(token))
        .route("/auth/whoami", get(whoami))
        .route("/repos/{repo}/init", post(facade::init))
        .route("/repos/{repo}/push", post(facade::push))
        .route("/repos/{repo}/pull", post(facade::pull))
        .route("/repos/{repo}/refs", get(facade::refs).post(facade::ref_create))
        .route("/repos/{repo}/commits", get(facade::commits))
        .route("/repos/{repo}/changesets/{id}", get(facade::changeset))
        .route("/repos/{repo}/file", get(facade::file))
        .route("/repos/{repo}/tree", get(facade::tree))
        .route("/repos/{repo}/diff", get(facade::diff))
        .route("/repos/{repo}/merge-base", get(facade::merge_base))
        .route("/repos/{repo}/rebase", post(facade::rebase))
        .route("/repos/{repo}/merge", post(facade::merge))
        .route("/repos/{repo}/land", post(facade::land))
        .route("/repos/{repo}/merge-land", post(facade::merge_land))
        .route(
            "/repos/{repo}/conflicts/escalate",
            post(facade::escalate_conflict),
        )
        .route("/repos/{repo}/conflicts", get(facade::list_conflicts))
        .route(
            "/repos/{repo}/conflicts/{id}/claim",
            post(facade::claim_conflict),
        )
        .route(
            "/repos/{repo}/conflicts/{id}/resolve",
            post(facade::resolve_conflict),
        )
        .route(
            "/repos/{repo}/conflict-context",
            post(facade::conflict_context),
        )
        .route("/repos/{repo}/workspaces", post(facade::workspace_create))
        .route(
            "/repos/{repo}/workspaces/{id}",
            get(facade::workspace_status).delete(facade::workspace_destroy),
        )
        .route(
            "/repos/{repo}/workspaces/{id}/materialize",
            post(facade::workspace_materialize),
        )
        .route(
            "/repos/{repo}/workspaces/{id}/file",
            get(facade::workspace_read_file)
                .put(facade::workspace_write_file)
                .delete(facade::workspace_delete_file),
        )
        .route(
            "/repos/{repo}/workspaces/{id}/diff",
            get(facade::workspace_diff),
        )
        .route(
            "/repos/{repo}/workspaces/{id}/submit",
            post(facade::workspace_submit),
        )
        // Pushes carry base64 objects; axum's 2 MiB JSON default is smaller than one
        // upstream blob limit. 32 MiB leaves room for base64 expansion of a 16 MiB blob.
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .with_state(state)
        // Scion's own probes, unmodified: /metrics, /healthz, /readyz.
        .merge(health_router(metrics, ready))
}

/// `POST /auth/token` body: a configured service principal and its secret.
#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    /// Principal name, without the `service:` prefix (e.g. `maestro`).
    service: String,
    /// The secret; hashed and compared in constant time upstream (SPEC 12.3).
    secret: String,
}

impl TokenRequest {
    /// The `AuthService.Login` request this body stands for.
    fn into_login(self) -> v1::LoginRequest {
        v1::LoginRequest {
            credential: Some(v1::login_request::Credential::Service(
                v1::ServiceCredential {
                    service_name: self.service,
                    service_secret: self.secret,
                },
            )),
        }
    }
}

/// `POST /auth/token` (D4 seam for `AuthService.Login`): verifies a service secret and returns
/// the same signed token `Login` would, so Maestro can call the façade with a real bearer token.
///
/// A wrong secret is 401 with no detail beyond the upstream message (the check itself is
/// upstream's, constant time and logged there). No engine means 503, never a 500.
async fn token(State(state): State<Arc<AppState>>, Json(body): Json<TokenRequest>) -> Response {
    let Some(engine) = state.auth_engine() else {
        return no_engine(&state.repo);
    };
    match handlers::auth::login(&engine.context(), tonic::Request::new(body.into_login())).await {
        Ok(response) => {
            let login = response.into_inner();
            (
                StatusCode::OK,
                Json(json!({
                    "service": SERVICE,
                    "status": "ok",
                    "token": login.token,
                    "principal": login.principal,
                    "expires_at_unix": login.expires_at_unix,
                })),
            )
                .into_response()
        }
        Err(status) => status_response(status),
    }
}

/// `GET /auth/whoami` (D4 seam for `AuthService.WhoAmI`): proves the token this service minted
/// is accepted by the same verifier every façade call will use.
async fn whoami(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(engine) = state.auth_engine() else {
        return no_engine(&state.repo);
    };
    let app = engine.context();
    let claims = match interceptor::authenticate(&headers, &app.verifier, &*app.clock) {
        Ok(claims) => claims,
        Err(error) => return status_response(status_from_error(&error)),
    };

    let mut request = tonic::Request::new(v1::WhoAmIRequest {});
    request.extensions_mut().insert(CallerCtx {
        principal: claims.sub.clone(),
        claims,
        request_id: String::new(),
    });
    match handlers::auth::who_am_i(&app, request).await {
        Ok(response) => {
            let who = response.into_inner();
            (
                StatusCode::OK,
                Json(json!({
                    "service": SERVICE,
                    "status": "ok",
                    "principal": who.principal,
                    "groups": who.groups,
                    "expires_at_unix": who.expires_at_unix,
                })),
            )
                .into_response()
        }
        Err(status) => status_response(status),
    }
}

/// 503 for "storage is not up", the only failure mode a probe-shaped client can act on (D5).
pub(crate) fn no_engine(repo: &str) -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        &format!("no engine is running for repo {repo:?}"),
    )
}

/// The single error shape of the façade, mirroring the probes' `{"service","status",...}`.
pub(crate) fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({
            "service": SERVICE,
            "status": "error",
            "code": code,
            "message": message,
        })),
    )
        .into_response()
}

/// One entry of a conflict payload as JSON: `{mode, id, size}`.
fn entry_payload_json(entry: &EntryInfo) -> Value {
    json!({
        "mode": entry.mode as u8,
        "id": entry.id.to_text(),
        "size": entry.size,
    })
}

/// A `MERGE_CONFLICT` payload as JSON (SPEC 8.3): every conflict plus the clean entries.
///
/// This is the contract an agent resolves against. The server never writes conflict markers
/// into objects, so the paths, kinds, three entries and line hunks are the entire description
/// of what the caller must decide.
fn conflict_report_json(report: &scion_core::ConflictReport) -> Value {
    let conflicts: Vec<Value> = report
        .conflicts
        .iter()
        .map(|conflict| {
            json!({
                "path": String::from_utf8_lossy(&conflict.path).into_owned(),
                "kind": conflict.kind.as_str(),
                "base": conflict.base.as_ref().map(entry_payload_json),
                "ours": conflict.ours.as_ref().map(entry_payload_json),
                "theirs": conflict.theirs.as_ref().map(entry_payload_json),
                "hunks": conflict.hunks.iter().map(|hunk| json!({
                    "base":   { "start": hunk.base.start,   "end": hunk.base.end },
                    "ours":   { "start": hunk.ours.start,   "end": hunk.ours.end },
                    "theirs": { "start": hunk.theirs.start, "end": hunk.theirs.end },
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let clean_entries: Vec<Value> = report
        .clean_entries
        .iter()
        .map(|clean| {
            json!({
                "path": String::from_utf8_lossy(&clean.path).into_owned(),
                "entry": clean.entry.as_ref().map(entry_payload_json),
            })
        })
        .collect();
    json!({ "conflicts": conflicts, "clean_entries": clean_entries })
}

/// The `scion.v1.ErrorDetail` payload as JSON, when the status carries one.
fn error_payload_json(payload: &ErrorPayload) -> Option<Value> {
    match payload {
        ErrorPayload::None => None,
        ErrorPayload::Subcode(subcode) => Some(json!({ "kind": "subcode", "subcode": subcode })),
        ErrorPayload::Limit { name, limit, actual } => Some(json!({
            "kind": "limit", "name": name, "limit": limit, "actual": actual
        })),
        ErrorPayload::MergeConflict(report) => Some(json!({
            "kind": "merge_conflict",
            "report": conflict_report_json(report),
        })),
        ErrorPayload::RefMismatch {
            current_version,
            current_target,
        } => Some(json!({
            "kind": "ref_version_mismatch",
            "current_version": current_version,
            "current_target": current_target.as_ref().map(|id| id.to_text()),
        })),
        ErrorPayload::AlreadyLanded {
            source,
            result,
            version,
        } => Some(json!({
            "kind": "already_landed",
            "source": source.to_text(),
            "result": result.to_text(),
            "version": version,
        })),
        ErrorPayload::Candidates(ids) => Some(json!({
            "kind": "candidates",
            "ids": ids.iter().map(|id| id.to_text()).collect::<Vec<_>>(),
        })),
    }
}

/// Map an upstream `tonic::Status` to HTTP: the façade and the gRPC surface must not disagree
/// about what a failure means (ADR-0002 D4).
///
/// The status message alone is not enough for the failures a merge produces. `scion.v1`
/// carries an `ErrorDetail` payload in the status details (a `ConflictReport` for
/// MERGE_CONFLICT, the current ref state for REF_VERSION_MISMATCH, …); it is decoded here and
/// returned as `details` so an HTTP client sees the same structured conflict the gRPC client
/// sees. Without this the REST surface collapsed every conflict into `{"code":"conflict"}` and
/// an agent had no way to learn which paths it must decide.
pub(crate) fn status_response(status: tonic::Status) -> Response {
    use tonic::Code;
    let (http, slug) = match status.code() {
        Code::InvalidArgument => (StatusCode::BAD_REQUEST, "invalid_argument"),
        Code::Unauthenticated => (StatusCode::UNAUTHORIZED, "unauthenticated"),
        Code::PermissionDenied => (StatusCode::FORBIDDEN, "permission_denied"),
        Code::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        Code::AlreadyExists => (StatusCode::CONFLICT, "already_exists"),
        Code::Aborted | Code::FailedPrecondition => (StatusCode::CONFLICT, "conflict"),
        Code::ResourceExhausted => (StatusCode::TOO_MANY_REQUESTS, "resource_exhausted"),
        Code::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        Code::DeadlineExceeded => (StatusCode::GATEWAY_TIMEOUT, "deadline_exceeded"),
        Code::Unimplemented => (StatusCode::NOT_IMPLEMENTED, "unimplemented"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    };
    let error = error_from_status(&status);
    let mut body = json!({
        "service": SERVICE,
        "status": "error",
        "code": slug,
        "message": status.message(),
    });
    if let Some(details) = error_payload_json(&error.payload)
        && let Some(object) = body.as_object_mut()
    {
        object.insert("details".to_owned(), details);
    }
    (http, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::config::Config;

    #[test]
    fn merge_conflict_payload_is_exposed_as_details() {
        use scion_core::{Conflict, ConflictHunk, ConflictKind, ConflictReport};
        let report = ConflictReport {
            conflicts: vec![Conflict {
                path: b"src/lib.rs".to_vec(),
                kind: ConflictKind::Content,
                base: None,
                ours: None,
                theirs: None,
                hunks: vec![ConflictHunk {
                    base: 1..2,
                    ours: 1..3,
                    theirs: 1..2,
                }],
            }],
            clean_entries: Vec::new(),
        };
        let details = error_payload_json(&ErrorPayload::MergeConflict(Box::new(report)))
            .expect("merge conflict payload");
        assert_eq!(details["kind"], "merge_conflict");
        let conflict = &details["report"]["conflicts"][0];
        assert_eq!(conflict["path"], "src/lib.rs");
        assert_eq!(conflict["kind"], "content");
        assert_eq!(conflict["hunks"][0]["ours"]["end"], 3);
    }

    #[test]
    fn ref_mismatch_payload_is_exposed_as_details() {
        let target = scion_core::ObjectId::parse_text(
            "c12597b62a1634a5e5c5bd505e47cf5fb15939cde2a52c3d692c7bdf1c92391c33",
        )
        .unwrap();
        let details = error_payload_json(&ErrorPayload::RefMismatch {
            current_version: 7,
            current_target: Some(target),
        })
        .expect("ref mismatch payload");
        assert_eq!(details["kind"], "ref_version_mismatch");
        assert_eq!(details["current_version"], 7);
        assert!(
            details["current_target"]
                .as_str()
                .is_some_and(|id| id.starts_with("c1"))
        );
    }

    /// Router-level tests must not depend on Neon: with no `FORGEVCS_DATABASE_URL` the registry
    /// boots empty, which is exactly the "engine is down" state /ready and the façade report.
    async fn app() -> Router {
        let config = Config::from_lookup(|_| None);
        let engines = Arc::new(Engines::boot(&config).await);
        router(Arc::new(AppState {
            ready: Arc::new(StubReadiness),
            engines,
            repo: config.repo().to_owned(),
            authz: Arc::new(AuthorizationService::from_config(&config).await),
            db: config
                .database_url
                .as_deref()
                .and_then(|url| crate::db::build_pool(url).ok()),
            config,
        }))
    }

    struct StubReadiness;

    impl Readiness for StubReadiness {
        fn check(&self) -> std::result::Result<(), String> {
            Ok(())
        }
    }

    async fn send(app: Router, request: Request<Body>) -> (StatusCode, String, String) {
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        (
            status,
            content_type,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    async fn get_at(path: &str) -> (StatusCode, String, String) {
        send(
            app().await,
            Request::builder().uri(path).body(Body::empty()).unwrap(),
        )
        .await
    }

    async fn post_json(path: &str, body: &str) -> (StatusCode, String, String) {
        send(
            app().await,
            Request::builder()
                .method("POST")
                .uri(path)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
    }

    async fn json_at(path: &str) -> (StatusCode, Value) {
        let (status, _, body) = get_at(path).await;
        (status, serde_json::from_str(&body).unwrap())
    }

    #[tokio::test]
    async fn health_returns_service_json() {
        let (status, body) = json_at("/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["service"], SERVICE);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn ready_returns_service_json() {
        let (status, body) = json_at("/ready").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["service"], SERVICE);
        assert_eq!(body["status"], "ready");
    }

    #[tokio::test]
    async fn version_reports_pinned_upstream_commit() {
        let (status, body) = json_at("/version").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["service"], SERVICE);
        assert_eq!(body["upstream"], config::UPSTREAM_COMMIT);
    }

    #[tokio::test]
    async fn ready_is_503_when_no_engine_is_configured() {
        let config = Config::from_lookup(|_| None);
        let engines = Arc::new(Engines::boot(&config).await);
        let ready = crate::engine::EngineHealth::start(config.clone(), Arc::clone(&engines)).await;
        let router = router(Arc::new(AppState {
            ready,
            engines,
            repo: config.repo().to_owned(),
            authz: Arc::new(AuthorizationService::from_config(&config).await),
            db: config
                .database_url
                .as_deref()
                .and_then(|url| crate::db::build_pool(url).ok()),
            config,
        }));
        let (status, _, body) = send(
            router,
            Request::builder()
                .uri("/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["status"], "not_ready");
        assert!(
            body["reason"]
                .as_str()
                .unwrap()
                .contains("FORGEVCS_DATABASE_URL")
        );
    }

    #[tokio::test]
    async fn upstream_probes_stay_mounted() {
        let (status, _, body) = get_at("/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.trim(), "ok");

        let (status, _, _) = get_at("/readyz").await;
        assert_eq!(status, StatusCode::OK);

        let (status, content_type, body) = get_at("/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/plain"));
        assert!(body.contains("scion_"));
    }

    #[tokio::test]
    async fn token_is_503_without_storage() {
        let (status, _, body) = post_json(
            "/auth/token",
            r#"{"service":"maestro","secret":"never-configured"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["status"], "error");
        assert_eq!(body["code"], "unavailable");
    }

    #[tokio::test]
    async fn whoami_is_503_without_storage() {
        let (status, _, body) = get_at("/auth/whoami").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["code"], "unavailable");
    }

    #[test]
    fn token_body_becomes_a_service_credential() {
        let request = TokenRequest {
            service: "maestro".to_owned(),
            secret: "s3cret".to_owned(),
        }
        .into_login();
        match request.credential {
            Some(v1::login_request::Credential::Service(cred)) => {
                assert_eq!(cred.service_name, "maestro");
                assert_eq!(cred.service_secret, "s3cret");
            }
            other => panic!("expected a service credential, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unauthenticated_maps_to_401() {
        let response = status_response(tonic::Status::unauthenticated("bad token"));
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["service"], SERVICE);
        assert_eq!(body["status"], "error");
        assert_eq!(body["code"], "unauthenticated");
        assert_eq!(body["message"], "bad token");
    }

    #[tokio::test]
    async fn business_routes_are_503_without_storage() {
        for path in [
            "/repos/forgevcs/refs",
            "/repos/forgevcs/commits",
            "/repos/forgevcs/changesets/c12597b62a1634a5e5c5bd505e47cf5fb15939cde2a52c3d692c7bdf1c92391c33",
            "/repos/forgevcs/file?changeset=c12597b62a1634a5e5c5bd505e47cf5fb15939cde2a52c3d692c7bdf1c92391c33&path=x",
            "/repos/forgevcs/tree?changeset=c12597b62a1634a5e5c5bd505e47cf5fb15939cde2a52c3d692c7bdf1c92391c33",
            "/repos/forgevcs/diff?b=c12597b62a1634a5e5c5bd505e47cf5fb15939cde2a52c3d692c7bdf1c92391c33",
            "/repos/forgevcs/merge-base?a=c12597b62a1634a5e5c5bd505e47cf5fb15939cde2a52c3d692c7bdf1c92391c33&b=c12597b62a1634a5e5c5bd505e47cf5fb15939cde2a52c3d692c7bdf1c92391c33",
            "/repos/forgevcs/workspaces/00000000-0000-0000-0000-000000000000",
        ] {
            let (status, _, body) = get_at(path).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["code"], "unavailable", "{path}");
        }
        for path in [
            "/repos/forgevcs/pull",
            "/repos/forgevcs/push",
            "/repos/forgevcs/workspaces",
        ] {
            let (status, _, body) = post_json(path, "{}").await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["code"], "unavailable", "{path}");
        }
    }

    #[tokio::test]
    async fn init_rejects_an_invalid_repository_name_before_auth() {
        let (status, _, body) = post_json("/repos/BadName/init", "{}").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["code"], "invalid_argument");
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("repository names")
        );
    }
}
