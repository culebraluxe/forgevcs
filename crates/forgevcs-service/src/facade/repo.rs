//! Repository resolution: which engine answers for a path segment, or a typed 404.
//!
//! This is the one place that decides "boot or not": a repository that this process has not
//! booted yet is booted only when its `repos` row already exists, so a typo in a path can never
//! mint a repository as a side effect of a read. Every handler calls [`repo_engine`] before it
//! does anything else, and inherits the same 404-vs-503 answer.

use std::sync::Arc;

use crate::api_error::{ApiError, ApiResult};
use crate::engine::Engine;
use crate::http::AppState;

/// The repository-name rule, shared with configuration and `POST /repos/{repo}/init`.
pub(super) fn valid_repo_name(name: &str) -> bool {
    crate::repo_acl::valid_repo_name(name)
}

/// The engine for an addressed repository, or a typed 404/503.
///
/// Reads never create a repository: a typo in a path must not mint one. A repository this
/// process has not booted yet (a different Vercel instance, or a restart) is booted only when
/// its `repos` row already exists.
pub(super) async fn repo_engine(state: &AppState, repo: &str) -> ApiResult<Arc<Engine>> {
    if let Some(engine) = state.engine_for(repo) {
        return Ok(engine);
    }
    let Some(db) = &state.db else {
        return Err(ApiError::unavailable("repository registry is unavailable"));
    };
    let client = db.get().await.map_err(|error| {
        ApiError::unavailable(format!("repository registry connection: {error}"))
    })?;
    let exists = client
        .query_opt("SELECT 1 FROM repos WHERE name = $1", &[&repo])
        .await
        .map_err(|error| ApiError::unavailable(format!("repository registry query: {error}")))?;
    if exists.is_none() {
        return Err(ApiError::not_found(format!(
            "repository {repo:?} does not exist; POST /repos/{repo}/init first"
        )));
    }
    state
        .engines
        .get_or_boot(&state.config, repo)
        .await
        .map_err(|error| ApiError::unavailable(format!("booting repository {repo:?}: {error:#}")))
}
