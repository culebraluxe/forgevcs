//! ForgeVCS HTTP service — the Vercel-facing shell around vendored Scion.
//!
//! Startup boots one engine per process (ADR-0002 D3): the Neon metadata store, the Neon
//! object store, the policy file and the signing keys are wired into upstream's `ServerApp`.
//! The routes live in [`forgevcs_service::http`] so the binary and its tests serve the
//! identical router — probes (`/health`, `/ready`, `/version`), auth (`/auth/token`,
//! `/auth/whoami`), the business façade (`/repos/{repo}/init|push|pull|refs|commits`), and
//! Scion's observability router (`/metrics`, `/healthz`, `/readyz`) untouched.
//!
//! `/ready` reports the last dependency probe (Neon + bucket), never a bare config check;
//! `/health` never touches a dependency (D5).

use std::sync::Arc;

use scion_obs::{LogFormat, init_logging};

use forgevcs_service::config::{self, Config};
use forgevcs_service::engine::{EngineHealth, Engines};
use forgevcs_service::http::{AppState, SERVICE, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _log = init_logging(LogFormat::Json);
    let config = Config::from_env();
    let port = config.port;
    let repo = config.repo().to_owned();

    // Boot the engine before binding: a failure is recorded and surfaced by /ready, so the
    // process stays up (ADR-0002 D5) but never claims to be ready without Neon.
    let engines = Arc::new(Engines::boot(&config).await);
    let ready = EngineHealth::start(config.clone(), Arc::clone(&engines)).await;

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(
        service = SERVICE,
        port,
        repos = engines.all().len(),
        version = env!("CARGO_PKG_VERSION"),
        upstream = config::UPSTREAM_COMMIT,
        "forgevcs listening"
    );

    let state = Arc::new(AppState {
        ready,
        engines,
        repo,
        config,
    });
    axum::serve(listener, router(state))
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
