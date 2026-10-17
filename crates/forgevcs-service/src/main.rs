//! ForgeVCS HTTP service — the Vercel-facing shell around vendored Scion.
//!
//! W1 scope: bind `$PORT`, serve the work order's probe aliases
//! (`/health`, `/ready`, `/version`) and re-mount Scion's existing observability
//! router (`/metrics`, `/healthz`, `/readyz`) untouched. Business endpoints
//! arrive with W2 (metadata) and W3 (object storage).

mod config;

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use scion_obs::{LogFormat, Metrics, Readiness, health_router, init_logging};
use serde_json::{Value, json};

use crate::config::Config;

/// Service name reported by every probe.
const SERVICE: &str = "forgevcs";

struct AppState {
    ready: Arc<dyn Readiness>,
}

/// M0 readiness: the process is serving and its configuration is coherent.
///
/// W2 replaces the body with a metadata-store probe; W3 adds object storage.
/// Keeping it behind `scion_obs::Readiness` means `/readyz` (Scion's route) and
/// `/ready` (the work order's route) can never disagree.
struct ServiceReadiness {
    config: Config,
}

impl Readiness for ServiceReadiness {
    fn check(&self) -> Result<(), String> {
        self.config.validate()
    }
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

/// Build the full router. Crate-visible so tests exercise exactly the routes the
/// container serves.
fn router(config: Config) -> Router {
    let metrics = Metrics::new();
    let ready: Arc<dyn Readiness> = Arc::new(ServiceReadiness { config });
    let state = Arc::new(AppState {
        ready: Arc::clone(&ready),
    });

    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready_handler))
        .route("/version", get(version))
        .with_state(state)
        // Scion's own probes, unmodified: /metrics, /healthz, /readyz.
        .merge(health_router(metrics, ready))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _log = init_logging(LogFormat::Json);
    let config = Config::from_env();
    let port = config.port;
    let database = config.database_url.is_some();
    let object_storage = config.has_object_storage();
    let app = router(config);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(
        service = SERVICE,
        port,
        database,
        object_storage,
        version = env!("CARGO_PKG_VERSION"),
        upstream = config::UPSTREAM_COMMIT,
        "forgevcs listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

/// Drain on Ctrl-C or SIGTERM — the latter is what a platform sends before it
/// stops a container, and what a Daemon receives on restart (ADR-0001 §9).
async fn shutdown() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "interrupt handler unavailable");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                let _ = stream.recv().await;
            }
            Err(error) => tracing::warn!(%error, "SIGTERM handler unavailable"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {},
        () = terminate => {},
    }
    tracing::info!(service = SERVICE, "shutdown requested");
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn app() -> Router {
        router(Config::from_lookup(|_| None))
    }

    async fn request(app: Router, path: &str) -> (StatusCode, String, String) {
        let response = app
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
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

    async fn json_at(path: &str) -> (StatusCode, Value) {
        let (status, _, body) = request(app(), path).await;
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
    async fn ready_is_503_when_configuration_is_incoherent() {
        let incoherent = router(Config::from_lookup(|key| match key {
            "FORGEVCS_DATABASE_URL" => Some("mysql://nope".to_owned()),
            _ => None,
        }));
        let (status, _, body) = request(incoherent, "/ready").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["status"], "not_ready");
        assert!(body["reason"].as_str().unwrap().contains("postgres://"));
    }

    #[tokio::test]
    async fn upstream_probes_stay_mounted() {
        let (status, _, body) = request(app(), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.trim(), "ok");

        let (status, _, _) = request(app(), "/readyz").await;
        assert_eq!(status, StatusCode::OK);

        let (status, content_type, body) = request(app(), "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/plain"));
        assert!(body.contains("scion_"));
    }
}
