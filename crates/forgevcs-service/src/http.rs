//! The HTTP surface: probes, auth, and the business façade routes.
//!
//! Façade endpoints never re-implement Scion's rules. They convert JSON to `scion.v1`
//! messages, call the vendored handler in-process, and convert the answer back — with the
//! caller's `CallerCtx` produced by `interceptor::authenticate`, the same code the gRPC
//! stack runs (ADR-0002 D4). No loopback gRPC, no second authorization path.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use scion_obs::{Metrics, Readiness, health_router};
use scion_proto::v1;
use scion_server::context::CallerCtx;
use scion_server::{handlers, interceptor};
use serde::Deserialize;
use serde_json::{Value, json};

use deadpool_postgres::Pool;

use crate::api_error::{ApiError, ApiResult};
use crate::authz::AuthorizationService;
use crate::config::{self, Config};
use crate::engine::{Engine, Engines};
use crate::facade;
use crate::human::HumanPrincipals;

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
    /// Configured human principals for the browserless token path (`FORGEVCS_HUMAN_PRINCIPALS`).
    pub humans: Arc<HumanPrincipals>,
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
pub(crate) fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> ApiResult<(Arc<Engine>, CallerCtx)> {
    let engine = state.auth_engine().ok_or_else(|| no_engine(&state.repo))?;
    let app = engine.context();
    let claims =
        interceptor::authenticate(headers, &app.verifier, &*app.clock).map_err(ApiError::from)?;
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
        .route("/auth/human-token", post(human_token))
        .route("/auth/whoami", get(whoami))
        .route("/repos/{repo}/init", post(facade::init))
        .route("/repos/{repo}/push", post(facade::push))
        .route("/repos/{repo}/pull", post(facade::pull))
        .route(
            "/repos/{repo}/refs",
            get(facade::refs).post(facade::ref_create),
        )
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
        .route(
            "/repos/{repo}/conflicts",
            get(facade::list_conflicts).delete(facade::clear_conflicts),
        )
        .route(
            "/repos/{repo}/conflicts/{id}",
            delete(facade::delete_conflict),
        )
        .route("/repos", get(facade::list_repos))
        .route("/admin/gc/objects", post(facade::gc_objects))
        .route("/repos/{repo}/blobs/export", get(facade::export_blobs))
        .route("/repos/{repo}/blobs/import", post(facade::import_blobs))
        .route("/repos/{repo}", delete(facade::delete_repo))
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
        return no_engine(&state.repo).into_response();
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
        Err(status) => ApiError::from(status).into_response(),
    }
}

/// `POST /auth/human-token` body: a configured human login plus the secret supplied to the CLI.
#[derive(Debug, Deserialize)]
pub struct HumanTokenRequest {
    /// Human login configured in `FORGEVCS_HUMAN_PRINCIPALS` (e.g. `culebraluxe`).
    login: String,
    /// The human's locally supplied secret. Never stored or logged; only its digest is compared.
    secret: String,
}

fn unix_seconds(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map_or(i64::MAX, |duration| {
        i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
    })
}

/// `POST /auth/human-token` — mint a bearer token for a configured human, without a browser.
///
/// This is the human counterpart of `AuthService.Login`'s GitHub flow: the credential is a
/// locally supplied secret whose SHA-256 digest is configured (never the secret itself), and the
/// issued principal is the same `github:<login>` identity a browser login would produce — so
/// `acl.toml`, the repository ACL and the decision audit need no human-specific branch. The mint
/// is written to `forgevcs_auth_decisions` (`action = token_mint`) when an audit store is
/// configured; human authorization decisions are audited there too.
///
/// No password is stored in this repository or in the token: the token is signed by the engine
/// keyring exactly like the service-principal token, and the CLI stores it locally with mode
/// 0600.
async fn human_token(
    State(state): State<Arc<AppState>>,
    body: Result<Json<HumanTokenRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let Some(human) = state
        .humans
        .authenticate(&body.login, body.secret.as_bytes())
    else {
        tracing::info!(login = %body.login, "human login rejected");
        return ApiError::unauthenticated("invalid human credentials").into_response();
    };
    let Some(engine) = state.auth_engine() else {
        return no_engine(&state.repo).into_response();
    };
    let app = engine.context();
    let principal = human.principal();
    let (token, claims) =
        match app
            .keyring
            .issue(&principal, app.config.auth.token_ttl, app.clock.now())
        {
            Ok(value) => value,
            Err(error) => return ApiError::from(error).into_response(),
        };
    if let Err(error) = state.authz.record_token_mint(human.id()).await {
        // The mint is not blocked on the audit store (it may be a storage-less pilot); the
        // failure is visible in the service log instead.
        tracing::warn!(%error, subject = human.id(), "human token mint was not audited");
    }
    (
        StatusCode::OK,
        Json(json!({
            "service": SERVICE,
            "status": "ok",
            "token": token,
            "principal": principal.id_string(),
            "email": human.email(),
            "display": human.display(),
            "expires_at_unix": unix_seconds(claims.exp),
        })),
    )
        .into_response()
}

/// `GET /auth/whoami` (D4 seam for `AuthService.WhoAmI`): proves the token this service minted
/// is accepted by the same verifier every façade call will use.
async fn whoami(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(engine) = state.auth_engine() else {
        return no_engine(&state.repo).into_response();
    };
    let app = engine.context();
    let claims = match interceptor::authenticate(&headers, &app.verifier, &*app.clock) {
        Ok(claims) => claims,
        Err(error) => return ApiError::from(error).into_response(),
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
        Err(status) => ApiError::from(status).into_response(),
    }
}

/// 503 for "storage is not up", the only failure mode a probe-shaped client can act on (D5).
fn no_engine(repo: &str) -> ApiError {
    ApiError::unavailable(format!("no engine is running for repo {repo:?}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::config::Config;

    /// Router-level tests must not depend on Neon: with no `FORGEVCS_DATABASE_URL` the registry
    /// boots empty, which is exactly the "engine is down" state /ready and the façade report.
    async fn app_with(config: Config) -> Router {
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
            humans: Arc::new(HumanPrincipals::from_config(&config)),
            config,
        }))
    }

    async fn app() -> Router {
        app_with(Config::from_lookup(|_| None)).await
    }

    /// SHA-256 of `test-human-secret` (never a real secret).
    const HUMAN_DIGEST: &str = "1f3fb03fc0864bd597bcb0264e48b55d7138cf5c49a9f5f9007b579d43ef86a7";

    async fn app_with_human() -> Router {
        let raw = format!(
            r#"{{"culebraluxe":{{"email":"culebraluxe@gmail.com","display":"Culebraluxe","secret_sha256":"{HUMAN_DIGEST}"}}}}"#
        );
        app_with(Config::from_lookup(move |key| {
            (key == crate::human::HUMAN_PRINCIPALS_VAR).then(|| raw.clone())
        }))
        .await
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
            humans: Arc::new(HumanPrincipals::default()),
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
        let response = ApiError::from(tonic::Status::unauthenticated("bad token")).into_response();
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
    async fn human_token_rejects_an_unknown_login_or_wrong_secret() {
        let router = app_with_human().await;
        for body in [
            r#"{"login":"someone-else","secret":"test-human-secret"}"#,
            r#"{"login":"culebraluxe","secret":"wrong"}"#,
            r#"{"login":"","secret":""}"#,
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/auth/human-token")
                        .header(axum::http::header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body.to_owned()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{body}");
            let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["code"], "unauthenticated", "{body}");
            assert_eq!(body["message"], "invalid human credentials");
        }
    }

    #[tokio::test]
    async fn human_token_with_valid_credentials_needs_an_engine() {
        // The credential is checked first, so a storage-less process answers 503 (not 401) once
        // the human is authenticated; that is the same D5 contract as every engine-backed route.
        let (status, _, body) = send(
            app_with_human().await,
            Request::builder()
                .method("POST")
                .uri("/auth/human-token")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"login":"culebraluxe","secret":"test-human-secret"}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["code"], "unavailable");
    }

    #[tokio::test]
    async fn human_token_rejects_a_malformed_body() {
        let (status, _, body) = send(
            app_with_human().await,
            Request::builder()
                .method("POST")
                .uri("/auth/human-token")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"login":"culebraluxe"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["code"], "invalid_argument");
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
