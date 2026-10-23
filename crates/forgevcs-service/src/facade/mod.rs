//! Business REST façade: `init`, `push`, `pull`, `refs`, `commits` over `scion.v1`.
//!
//! This module is deliberately a *mapping* layer. Every operation that upstream implements is
//! called through its vendored handler (`handlers::upload::upload_objects`,
//! `handlers::write::create_changeset`, `handlers::write::land`, `handlers::refs::update_ref`,
//! `handlers::object::{resolve_ref, get_objects}`, `handlers::history_query::log`) with the
//! caller's [`CallerCtx`] in the request extensions. Policy evaluation, object validation,
//! change-set construction and land semantics stay inside those handlers (ADR-0002 D4).
//!
//! What is new here is only:
//!
//! - JSON request/response shapes for the five operations, using `ObjectId` text form
//!   (`b1…`, `t1…`, `c1…`) and base64 for raw object bytes;
//! - the service's typed error envelope, in [`crate::api_error`] so the probes, the auth
//!   endpoints and every façade handler share one status/code mapping;
//! - a gRPC-framed in-memory stream for `UploadObjects`, whose handler takes
//!   `Streaming<UploadItem>` rather than a vector. The handler itself is unchanged; only the
//!   wire framing that tonic normally supplies is built here.
//!
//! The module is decomposed so a handler reads as orchestration and nothing else:
//!
//! - [`requests`] — the JSON request/query DTOs and their `scion.v1` conversion (pure);
//! - [`responses`] — proto reply / id / timestamp → JSON mapping (pure apart from the envelope);
//! - [`repo`] — the thin repository resolver that answers 404-vs-boot in one place.
//!
//! `POST /repos/{repo}/refs` would be more RESTful as a list, but there is no upstream
//! `ListRefs` RPC. `GET /refs` therefore reads `MetaStore::list_refs` directly after the same
//! `read` on `/` check `GetRefLog` would perform. It is the one read that does not go through a
//! handler, and it is called out in `docs/W4-engine-wiring.md` §8.

use std::collections::BTreeMap;

use deadpool_postgres::Pool;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::StreamExt;
use scion_core::{ConflictReport, ObjectId};
use scion_object::{Canonical, RepoPath};
use scion_proto::{convert, status_from_error, v1};
use scion_server::AppContext;
use scion_server::context::CallerCtx;
use scion_server::handlers;
use scion_store::BlobStore;
use serde_json::{Value, json};
use tonic::Status;

use crate::authz::{Action, AdminCapability};
use crate::repo_acl::ClaimOutcome;

use crate::api_error::{ApiError, ApiResult};
use crate::engine::Engine;
use crate::http::{AppState, SERVICE, authenticate, caller_request};
use crate::paused_conflicts::{self, ContinueOutcome, NewConflict, Record, ResolveOutcome};

mod repo;
mod requests;
mod responses;

use repo::{repo_engine, valid_repo_name};
use requests::*;
use responses::*;

/// Default ref the push/pull/commits endpoints address.
const DEFAULT_REF: &str = "main";
/// Default page size of `commits`/`pull` (upstream's own `Log` default).
const DEFAULT_LIMIT: u32 = 100;
/// Largest page this façade forwards; upstream clamps to the same value.
const MAX_LIMIT: u32 = 1000;

// ---------------------------------------------------------------------------------------------
// Shared helpers: authorization
// ---------------------------------------------------------------------------------------------

/// Run the Scion/Culebra A/B decision for one request (W6). Scion is authoritative in the
/// default mode; see `authz.rs` for the combination rules.
async fn authorize(
    state: &Arc<AppState>,
    engine: &Arc<Engine>,
    caller: &CallerCtx,
    repo: &str,
    action: Action,
    path: Option<&str>,
    ref_name: Option<&str>,
) -> ApiResult<()> {
    let app = engine.context();
    let policy = app.policy.current();
    match state
        .authz
        .authorize(&policy, &caller.principal, repo, action, path, ref_name)
        .await
    {
        Ok(decision) if decision.allow => Ok(()),
        Ok(decision) => Err(ApiError::permission_denied(decision.reason)),
        Err(status) => Err(ApiError::from(status)),
    }
}

/// Repository provisioning (`init`) runs before the repository has an owner, so it evaluates the
/// path/ref policy only; the owner is claimed immediately after the engine boots (FVCS-01).
async fn authorize_provisioning(
    state: &Arc<AppState>,
    engine: &Arc<Engine>,
    caller: &CallerCtx,
    repo: &str,
    action: Action,
) -> ApiResult<()> {
    let app = engine.context();
    let policy = app.policy.current();
    match state
        .authz
        .authorize_provisioning(&policy, &caller.principal, repo, action, Some("/"), None)
        .await
    {
        Ok(decision) if decision.allow => Ok(()),
        Ok(decision) => Err(ApiError::permission_denied(decision.reason)),
        Err(status) => Err(ApiError::from(status)),
    }
}

// ---------------------------------------------------------------------------------------------
// POST /repos/{repo}/init
// ---------------------------------------------------------------------------------------------

/// The FVCS-01 repository-identity check alone.
///
/// Used by operations that call a vendored handler directly (`merge`, `rebase`, `land`,
/// `merge-land`, ref provisioning): upstream enforces the path/ref policy, so the façade must
/// still enforce repository identity or a principal could act in another agent's repository.
async fn authorize_repository(
    state: &Arc<AppState>,
    engine: &Arc<Engine>,
    caller: &CallerCtx,
    repo: &str,
    action: Action,
) -> ApiResult<()> {
    let policy = engine.context().policy.current();
    match state
        .authz
        .authorize_repository(&policy, &caller.principal, repo, action)
        .await
    {
        Ok(decision) if decision.allow => Ok(()),
        Ok(decision) => Err(ApiError::permission_denied(decision.reason)),
        Err(status) => Err(ApiError::from(status)),
    }
}

/// `POST /repos/{repo}/init` — create the repository if this process has not already booted it.
///
/// `Engine::connect` creates the `repos` row and upstream's `ServerApp::build` bootstraps the
/// root changeset and ref `main`, so this is a real initialization rather than a row insert.
/// Idempotent: a second call returns `created:false` with the same refs.
///
/// Authorization is `write` on `/` under the current policy, plus `admin.repos`
/// ([`AdminCapability::Repositories`]) when the request would create the repository. The check
/// runs on the verifier engine's policy because the target engine does not exist until after it.
pub(crate) async fn init(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !valid_repo_name(&repo) {
        return ApiError::invalid_argument(
            "repository names must be 1-100 lowercase [a-z0-9._-] characters, not start with \
             '.', and not contain '..'",
        )
        .into_response();
    }
    let (auth_engine, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if let Err(error) =
        authorize_provisioning(&state, &auth_engine, &caller, &repo, Action::Write).await
    {
        return error.into_response();
    }
    let app = auth_engine.context();
    if !app
        .policy
        .current()
        .can_write(&caller.principal, &RepoPath::root())
    {
        return ApiError::permission_denied("creating a repository requires write on /")
            .into_response();
    }

    let created = state.engine_for(&repo).is_none();
    // Repository lifecycle is a DevOps seat, not a worker permission: workers check in, read
    // and merge-base, but creating and deleting a repository needs `admin.repos`
    // (`AdminCapability::Repositories`). `admin.refs` is upstream's permit for creating and
    // deleting refs; it is never substituted for repository authority.
    if created
        && !app
            .policy
            .current()
            .can_admin(&caller.principal, AdminCapability::Repositories.permit())
    {
        return ApiError::permission_denied(
            "creating a repository requires the DevOps seat (admin.repos)",
        )
        .into_response();
    }
    let engine = if created {
        match state.engines.get_or_boot(&state.config, &repo).await {
            Ok(engine) => engine,
            Err(error) => {
                return ApiError::unavailable(format!("{error:#}")).into_response();
            }
        }
    } else {
        match state.engine_for(&repo) {
            Some(engine) => engine,
            None => {
                return ApiError::internal("engine vanished").into_response();
            }
        }
    };

    // FVCS-01: the first successful init claims the repository. `claim` is first-writer-wins and
    // never reassigns, so an idempotent init or a concurrent instance cannot steal it. Only the
    // DevOps seat (admin.repos) may initialize a repository owned by another principal.
    match state.authz.claim_repo_owner(&repo, &caller.principal).await {
        Ok(ClaimOutcome::Claimed | ClaimOutcome::AlreadyOwner) => {}
        Ok(ClaimOutcome::OwnedByOther { owner }) => {
            if !app
                .policy
                .current()
                .can_admin(&caller.principal, AdminCapability::Repositories.permit())
            {
                return ApiError::permission_denied(format!(
                    "repository {repo:?} is owned by {owner}"
                ))
                .into_response();
            }
        }
        Err(status) => return ApiError::from(status).into_response(),
    }

    match list_refs(&engine, &caller).await {
        Ok(refs) => (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "repo": repo,
                "created": created,
                "refs": refs,
            })),
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// GET /repos/{repo}/refs
// ---------------------------------------------------------------------------------------------

/// List refs with their current target and version.
///
/// Upstream has no `ListRefs` RPC; `MetaStore::list_refs` is the same strong read `ResolveRef`
/// uses. The policy check is upstream's repository-read rule (`read` on `/`), spelled once here
/// because there is no handler to carry it.
async fn list_refs(engine: &Arc<Engine>, caller: &CallerCtx) -> ApiResult<Vec<Value>> {
    let app = engine.context();
    if !app
        .policy
        .current()
        .can_read(&caller.principal, &RepoPath::root())
    {
        return Err(ApiError::permission_denied(
            "repository read (read on /) denied",
        ));
    }
    let rows = app.storage.meta.list_refs().await.map_err(ApiError::from)?;
    Ok(rows
        .iter()
        .map(|row| {
            json!({
                "name": row.name.as_str(),
                "target": row.target.to_text(),
                "version": row.version,
            })
        })
        .collect())
}

pub(crate) async fn refs(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some("/"),
        None,
    )
    .await
    {
        return error.into_response();
    }
    match list_refs(&engine, &caller).await {
        Ok(refs) => (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "repo": repo,
                "refs": refs,
            })),
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// Resolve + Log, shared by GET /commits and POST /pull
// ---------------------------------------------------------------------------------------------

/// Resolve `ref_name` and return the first page of its log.
async fn resolve_and_log(
    engine: &Arc<Engine>,
    caller: &CallerCtx,
    ref_name: &str,
    limit: u32,
    path: Option<&str>,
    first_parent_only: bool,
    page_token: Option<&str>,
) -> ApiResult<(String, u64, Vec<Value>, Option<String>)> {
    let app = engine.context();
    let page_token = match page_token {
        Some(raw) if !raw.is_empty() => hex::decode(raw).map_err(|error| {
            ApiError::invalid_argument(format!("page_token is not hex: {error}"))
        })?,
        _ => Vec::new(),
    };
    let resolved = handlers::object::resolve_ref(
        &app,
        caller_request(
            v1::ResolveRefRequest {
                name: ref_name.to_owned(),
                at_version: None,
            },
            caller,
        ),
    )
    .await
    .map_err(ApiError::from)?
    .into_inner();
    let target = ObjectId::from_bytes(&resolved.target).map_err(ApiError::from)?;
    let log = handlers::history_query::log(
        &app,
        caller_request(
            v1::LogRequest {
                from: resolved.target,
                path: path.map(|p| p.as_bytes().to_vec()),
                first_parent_only,
                limit: limit.min(MAX_LIMIT),
                page_token,
            },
            caller,
        ),
    )
    .await
    .map_err(ApiError::from)?
    .into_inner();
    let commits = log_entries(&log.entries)?;
    let next = (!log.next_page_token.is_empty()).then(|| hex::encode(&log.next_page_token));
    Ok((target.to_text(), resolved.version, commits, next))
}

/// `GET /repos/{repo}/commits?ref=main&limit=100[&path=…][&first_parent_only=true]`.
pub(crate) async fn commits(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    query: Result<Query<CommitsQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some(query.path.as_deref().unwrap_or("/")),
        None,
    )
    .await
    {
        return error.into_response();
    }
    match resolve_and_log(
        &engine,
        &caller,
        &query.ref_name,
        query.limit,
        query.path.as_deref(),
        query.first_parent_only,
        query.page_token.as_deref(),
    )
    .await
    {
        Ok((target, version, commits, next_page_token)) => (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "repo": repo,
                "ref": query.ref_name,
                "target": target,
                "version": version,
                "commits": commits,
                "next_page_token": next_page_token,
            })),
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// POST /repos/{repo}/pull
// ---------------------------------------------------------------------------------------------

struct ObjectAccum {
    status: i32,
    encoding: i32,
    total_size: u64,
    final_chunk: bool,
    data: Vec<u8>,
}

impl Default for ObjectAccum {
    fn default() -> Self {
        Self {
            status: v1::ItemStatus::Unspecified as i32,
            encoding: v1::Encoding::Unspecified as i32,
            total_size: 0,
            final_chunk: false,
            data: Vec::new(),
        }
    }
}

/// Fetch object bytes through `ObjectService.GetObjects` and concatenate each id's chunks.
async fn fetch_objects(
    engine: &Arc<Engine>,
    caller: &CallerCtx,
    ids: &[String],
) -> ApiResult<Vec<Value>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut parsed = Vec::with_capacity(ids.len());
    for raw in ids {
        parsed.push(parse_id(raw, "objects[]").map_err(ApiError::invalid_argument)?);
    }
    let app = engine.context();
    let mut stream = handlers::object::get_objects(
        &app,
        caller_request(
            v1::GetObjectsRequest {
                ids: parsed.iter().map(convert::id_bytes).collect(),
                // `offset: Some(0)` selects upstream's range-read path, which returns the
                // decompressed bytes as RAW. The full-object path returns the stored zstd frame
                // instead; a JSON client wants content it can use without knowing the store's
                // compression choice.
                offset: Some(0),
                length: None,
            },
            caller,
        ),
    )
    .await
    .map_err(ApiError::from)?
    .into_inner();

    let mut chunks: BTreeMap<Vec<u8>, ObjectAccum> = BTreeMap::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(ApiError::from)?;
        let accum = chunks.entry(chunk.id.clone()).or_default();
        accum.status = chunk.status;
        accum.encoding = chunk.encoding;
        accum.total_size = chunk.total_size;
        accum.final_chunk |= chunk.r#final;
        accum.data.extend_from_slice(&chunk.data);
    }

    let mut out = Vec::with_capacity(parsed.len());
    for id in &parsed {
        let value = match chunks.get(&id.as_bytes()[..]) {
            Some(accum) => json!({
                "id": id.to_text(),
                "status": item_status(accum.status),
                "encoding": encoding_name(accum.encoding),
                "total_size": accum.total_size,
                "final": accum.final_chunk,
                "data": BASE64.encode(&accum.data),
            }),
            None => json!({
                "id": id.to_text(),
                "status": "not_found",
            }),
        };
        out.push(value);
    }
    Ok(out)
}

/// `POST /repos/{repo}/pull` — resolve a ref, return its commit page, and (optionally) the bytes
/// of named objects. This is the read half of a sync: `commits` for history, `objects` for
/// content. Object authorization is upstream's per-id read check inside `GetObjects`.
pub(crate) async fn pull(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Result<Json<PullBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some(body.path.as_deref().unwrap_or("/")),
        None,
    )
    .await
    {
        return error.into_response();
    }
    let history = resolve_and_log(
        &engine,
        &caller,
        &body.ref_name,
        body.limit,
        body.path.as_deref(),
        body.first_parent_only,
        body.page_token.as_deref(),
    )
    .await;
    let (target, version, commits, next_page_token) = match history {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let objects = match fetch_objects(&engine, &caller, &body.objects).await {
        Ok(objects) => objects,
        Err(error) => return error.into_response(),
    };
    (
        StatusCode::OK,
        Json(json!({
            "service": SERVICE,
            "status": "ok",
            "repo": repo,
            "ref": body.ref_name,
            "target": target,
            "version": version,
            "commits": commits,
            "next_page_token": next_page_token,
            "objects": objects,
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------------------------
// W4 read surface: get_changeset, get_file, list_tree, diff, merge_base
//
// These five complete the SourceControlService acceptance in the V0 work order (§8): a client can
// create a repo, add and modify a file, read history, diff the two changesets and reconstruct the
// tree without invoking the Scion CLI. They call the vendored QueryService handlers, so read
// policy, redaction filtering and path validation remain upstream's.
// ---------------------------------------------------------------------------------------------

/// `GET /repos/{repo}/changesets/{id}` — one changeset, decoded.
pub(crate) async fn changeset(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let id = match parse_id(&id, "changeset") {
        Ok(id) => id,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some("/"),
        None,
    )
    .await
    {
        return error.into_response();
    }
    let app = engine.context();
    let response = match handlers::query::get_changesets(
        &app,
        caller_request(
            v1::GetChangesetsRequest {
                ids: vec![convert::id_bytes(&id)],
            },
            &caller,
        ),
    )
    .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return ApiError::from(status).into_response(),
    };
    let Some(item) = response.items.first() else {
        return ApiError::not_found(format!("changeset {id} not found")).into_response();
    };
    if item.status != v1::ItemStatus::Ok as i32 {
        return ApiError::not_found(format!("changeset {id} not found")).into_response();
    }
    match changeset_json(item) {
        Ok(changeset) => (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "repo": repo,
                "changeset": changeset,
            })),
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}

/// `GET /repos/{repo}/file?changeset=<id>&path=<path>` — file bytes at a changeset.
pub(crate) async fn file(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    query: Result<Query<FileQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let changeset = match parse_id(&query.changeset, "changeset") {
        Ok(id) => id,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    let path = query.path;
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some(&path),
        None,
    )
    .await
    {
        return error.into_response();
    }
    let app = engine.context();
    let mut stream = match handlers::query::read_path(
        &app,
        caller_request(
            v1::ReadPathRequest {
                changeset: convert::id_bytes(&changeset),
                path: path.clone().into_bytes(),
            },
            &caller,
        ),
    )
    .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return ApiError::from(status).into_response(),
    };
    let mut entry = None;
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                if entry.is_none() {
                    entry = chunk.entry;
                }
                data.extend_from_slice(&chunk.data);
            }
            Err(status) => return ApiError::from(status).into_response(),
        }
    }
    (
        StatusCode::OK,
        Json(json!({
            "service": SERVICE,
            "status": "ok",
            "repo": repo,
            "changeset": changeset.to_text(),
            "path": path,
            "entry": entry_json(entry.as_ref()),
            "size": data.len(),
            "data": BASE64.encode(&data),
        })),
    )
        .into_response()
}

/// `GET /repos/{repo}/tree?changeset=<id>&path=<dir>` — one directory's entries.
pub(crate) async fn tree(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    query: Result<Query<TreeQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some(if query.path.is_empty() {
            "/"
        } else {
            query.path.as_str()
        }),
        None,
    )
    .await
    {
        return error.into_response();
    }
    let changeset = match parse_id(&query.changeset, "changeset") {
        Ok(id) => id,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    let app = engine.context();
    let tree = match handlers::query::list_tree(
        &app,
        caller_request(
            v1::ListTreeRequest {
                changeset: convert::id_bytes(&changeset),
                path: query.path.into_bytes(),
            },
            &caller,
        ),
    )
    .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return ApiError::from(status).into_response(),
    };
    let entries: Vec<Value> = tree
        .entries
        .iter()
        .filter_map(|entry| {
            entry.entry.as_ref().map(|value| {
                json!({
                    "name": bytes_text(&entry.name),
                    "mode": value.mode,
                    "id": id_text(&value.id),
                    "size": value.size,
                })
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(json!({
            "service": SERVICE,
            "status": "ok",
            "repo": repo,
            "changeset": changeset.to_text(),
            "tree_id": id_text(&tree.tree_id),
            "entries": entries,
        })),
    )
        .into_response()
}

/// `GET /repos/{repo}/diff?a=<changeset>&b=<changeset>[&paths=a,b][&include_hunks=true]`.
pub(crate) async fn diff(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    query: Result<Query<DiffQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let a = match query.a.as_deref() {
        Some(raw) if !raw.is_empty() => match parse_id(raw, "a") {
            Ok(id) => Some(id),
            Err(message) => {
                return ApiError::invalid_argument(message).into_response();
            }
        },
        _ => None,
    };
    let b = match parse_id(&query.b, "b") {
        Ok(id) => id,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    let paths: Vec<Vec<u8>> = query
        .paths
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .filter(|path| !path.is_empty())
        .map(|path| path.as_bytes().to_vec())
        .collect();
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some("/"),
        None,
    )
    .await
    {
        return error.into_response();
    }
    let app = engine.context();
    let mut stream = match handlers::query::diff(
        &app,
        caller_request(
            v1::DiffRequest {
                a: a.as_ref().map_or_else(Vec::new, convert::id_bytes),
                b: convert::id_bytes(&b),
                paths,
                include_content_hunks: query.include_hunks,
            },
            &caller,
        ),
    )
    .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return ApiError::from(status).into_response(),
    };
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(item) => items.push(diff_item_json(&item)),
            Err(status) => return ApiError::from(status).into_response(),
        }
    }
    (
        StatusCode::OK,
        Json(json!({
            "service": SERVICE,
            "status": "ok",
            "repo": repo,
            "a": a.map(|id| id.to_text()),
            "b": b.to_text(),
            "items": items,
        })),
    )
        .into_response()
}

/// `GET /repos/{repo}/merge-base?a=<changeset>&b=<changeset>`.
pub(crate) async fn merge_base(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    query: Result<Query<MergeBaseQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let a = match parse_id(&query.a, "a") {
        Ok(id) => id,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    let b = match parse_id(&query.b, "b") {
        Ok(id) => id,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some("/"),
        None,
    )
    .await
    {
        return error.into_response();
    }
    let app = engine.context();
    let response = match handlers::query::merge_base(
        &app,
        caller_request(
            v1::MergeBaseRequest {
                a: convert::id_bytes(&a),
                b: convert::id_bytes(&b),
            },
            &caller,
        ),
    )
    .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return ApiError::from(status).into_response(),
    };
    let bases: Vec<String> = response.bases.iter().map(|raw| id_text(raw)).collect();
    (
        StatusCode::OK,
        Json(json!({
            "service": SERVICE,
            "status": "ok",
            "repo": repo,
            "a": a.to_text(),
            "b": b.to_text(),
            "bases": bases,
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------------------------
// POST /repos/{repo}/push
// ---------------------------------------------------------------------------------------------

/// Advancing the trunk ref is the DevOps seat's entitlement, independent of path writes and of
/// the ref rule: both must allow it. Workers check in on their own refs; they do not land to the
/// trunk.
fn require_devops(
    engine: &Arc<Engine>,
    caller: &CallerCtx,
    capability: AdminCapability,
    message: &str,
) -> ApiResult<()> {
    let app = engine.context();
    if app
        .policy
        .current()
        .can_admin(&caller.principal, capability.permit())
    {
        Ok(())
    } else {
        Err(ApiError::permission_denied(message))
    }
}

fn require_trunk(engine: &Arc<Engine>, caller: &CallerCtx, ref_name: &str) -> ApiResult<()> {
    if ref_name != DEFAULT_REF {
        return Ok(());
    }
    require_devops(
        engine,
        caller,
        AdminCapability::Trunk,
        "advancing the trunk ref requires the DevOps seat (admin.trunk)",
    )
}

/// `POST /repos/{repo}/push` — upload objects, create a changeset, then land or update a ref.
///
/// The three steps are upstream handlers in the order the gRPC clients call them; a failure at
/// any step returns that step's `tonic::Status` mapping, and no ref is moved unless landing or
/// the explicit update succeeds. `mode:"none"` stops after the changeset, which is how a
/// caller stages a commit without publishing it.
pub(crate) async fn push(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Result<Json<PushBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    if !matches!(body.mode.as_str(), "land" | "update" | "none") {
        return ApiError::invalid_argument("mode must be one of land, update, none")
            .into_response();
    }
    let PushBody {
        objects,
        changeset,
        ref_name,
        mode,
        expected_version,
        allow_non_fast_forward,
        allow_empty,
    } = body;
    // FVCS-02: only the changeset delta names the paths this push writes. Authorize each of them
    // below instead of the coarse write-on-`/` the push used to require; the coarse gate stays
    // for repository and ref provisioning (`init`, `ref_create`, the ref rules).
    let delta_paths = push_delta_paths(&changeset);

    if matches!(mode.as_str(), "land" | "update")
        && let Err(error) = require_trunk(&engine, &caller, &ref_name)
    {
        return error.into_response();
    }

    let policy = engine.context().policy.current();
    match state
        .authz
        .authorize_push(&policy, &caller.principal, &repo, &ref_name, &delta_paths)
        .await
    {
        Ok(decision) if decision.allow => {}
        Ok(decision) => return ApiError::permission_denied(decision.reason).into_response(),
        Err(status) => return ApiError::from(status).into_response(),
    }

    let mut uploads = Vec::with_capacity(objects.len());
    for object in objects {
        match object.into_proto() {
            Ok(item) => uploads.push(item),
            Err(message) => {
                return ApiError::invalid_argument(message).into_response();
            }
        }
    }
    let changeset = match changeset.into_proto() {
        Ok(changeset) => changeset,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };

    let app = engine.context();
    let mut upload_results = Vec::new();
    if !uploads.is_empty() {
        let request = caller_request(upload_stream(uploads), &caller);
        match handlers::upload::upload_objects(&app, request).await {
            Ok(response) => upload_results = response.into_inner().results,
            Err(status) => return ApiError::from(status).into_response(),
        }
        if upload_results
            .iter()
            .any(|result| result.status != v1::ItemStatus::Ok as i32)
        {
            return upload_failed_response(&upload_results);
        }
    }

    let created =
        match handlers::write::create_changeset(&app, caller_request(changeset, &caller)).await {
            Ok(response) => response.into_inner(),
            Err(status) => return ApiError::from(status).into_response(),
        };
    let changeset_id = match ObjectId::from_bytes(&created.changeset_id) {
        Ok(id) => id,
        Err(error) => return ApiError::from(&error).into_response(),
    };
    let root_tree = ObjectId::from_bytes(&created.root_tree)
        .map(|id| id.to_text())
        .unwrap_or_default();
    let meta = ObjectId::from_bytes(&created.meta_id)
        .map(|id| id.to_text())
        .unwrap_or_default();

    let mut ref_value = Value::Null;
    let mut landed = Value::Null;
    match mode.as_str() {
        "none" => {}
        "update" => {
            let request = v1::UpdateRefRequest {
                name: ref_name.clone(),
                expected_version,
                new_target: created.changeset_id.clone(),
                allow_non_fast_forward,
            };
            match handlers::refs::update_ref(&app, caller_request(request, &caller)).await {
                Ok(response) => {
                    ref_value = json!({
                        "name": ref_name,
                        "target": changeset_id.to_text(),
                        "version": response.into_inner().version,
                    });
                }
                Err(status) => return ApiError::from(status).into_response(),
            }
        }
        "land" => {
            let request = v1::LandRequest {
                r#ref: ref_name.clone(),
                head: created.changeset_id.clone(),
                allow_empty,
            };
            match handlers::write::land(&app, caller_request(request, &caller)).await {
                Ok(response) => {
                    let response = response.into_inner();
                    ref_value = json!({
                        "name": ref_name,
                        "target": id_text(&response.target),
                        "version": response.version,
                    });
                    landed = json!(landed_json(&response.landed));
                    tracing::info!(
                        repo = %repo,
                        ref_name = %ref_name,
                        target = %id_text(&response.target),
                        version = response.version,
                        merge_engine = state.config.merge_engine(),
                        "landed"
                    );
                }
                Err(status) => {
                    let base = first_parent(&app, &caller, &changeset_id).await;
                    match ref_state(&engine, &caller, &ref_name).await {
                        Ok((ours, _)) => {
                            let paused = pause_conflict(
                                &state,
                                &engine,
                                &caller,
                                &repo,
                                land_pause(
                                    "push",
                                    ref_name.clone(),
                                    changeset_id,
                                    ours,
                                    changeset_id,
                                    base,
                                ),
                                &status,
                            )
                            .await;
                            match paused {
                                Some(response) => return response,
                                None => return ApiError::from(status).into_response(),
                            }
                        }
                        Err(_) => return ApiError::from(status).into_response(),
                    }
                }
            }
        }
        _ => {
            return ApiError::invalid_argument("mode must be one of land, update, none")
                .into_response();
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "service": SERVICE,
            "status": "ok",
            "repo": repo,
            "changeset": {
                "id": changeset_id.to_text(),
                "root_tree": root_tree,
                "meta": meta,
            },
            "merge_engine": state.config.merge_engine(),
            "uploads": uploads_json(&upload_results),
            "ref": ref_value,
            "landed": landed,
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------------------------
// Collaboration surface — refs, rebase, merge, land
//
// These are the operations a set of agents needs after the source-control floor: each agent
// writes to its own ref (provisioned with CreateRef by an `admin.refs` seat), and integration
// is `land`, which already rebases a linear stack onto the ref tip. `rebase` and `merge` expose
// the vendored handlers so a contended agent has a designed convergence step instead of only
// "retry onto the new tip".
// ---------------------------------------------------------------------------------------------

/// `POST /repos/{repo}/refs` — create a ref at an existing changeset.
///
/// Upstream `RefService.CreateRef` requires `admin.refs`; that is the provisioning seat for
/// per-agent lane refs. `GET /refs` (list) is unchanged and remains on the same route.
pub(crate) async fn ref_create(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Result<Json<RefCreateBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    if let Err(error) =
        authorize_repository(&state, &engine, &caller, &repo, Action::UpdateRef).await
    {
        return error.into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let target = match parse_id(&body.target, "target") {
        Ok(value) => value,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    let request = v1::CreateRefRequest {
        name: body.name.clone(),
        target: convert::id_bytes(&target),
        reason: body.reason.clone(),
    };
    match handlers::refs::create_ref(&engine.context(), caller_request(request, &caller)).await {
        Ok(response) => {
            let version = response.into_inner().version;
            (
                StatusCode::OK,
                Json(json!({
                    "service": SERVICE,
                    "status": "ok",
                    "repo": repo,
                    "ref": { "name": body.name, "target": target.to_text(), "version": version },
                })),
            )
                .into_response()
        }
        Err(status) => ApiError::from(status).into_response(),
    }
}

/// `POST /repos/{repo}/rebase` — replay a changeset onto another; returns the new changeset.
///
/// Requires repository read only (upstream `WriteService.Rebase`): the result is unnamed and
/// landing it is what checks write and ref authority.
pub(crate) async fn rebase(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Result<Json<RebaseBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = authorize_repository(&state, &engine, &caller, &repo, Action::Read).await {
        return error.into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let changeset = match parse_id(&body.changeset, "changeset") {
        Ok(value) => value,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    let onto = match parse_id(&body.onto, "onto") {
        Ok(value) => value,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    let request = v1::RebaseRequest {
        changeset: convert::id_bytes(&changeset),
        onto: convert::id_bytes(&onto),
    };
    match handlers::write::rebase(&engine.context(), caller_request(request, &caller)).await {
        Ok(response) => {
            let id = id_text(&response.into_inner().changeset_id);
            (
                StatusCode::OK,
                Json(json!({
                    "service": SERVICE,
                    "status": "ok",
                    "repo": repo,
                    "changeset": { "id": id },
                })),
            )
                .into_response()
        }
        Err(status) => {
            let app = engine.context();
            let base = first_parent(&app, &caller, &changeset).await;
            match pause_conflict(
                &state,
                &engine,
                &caller,
                &repo,
                rebase_pause(changeset, onto, base),
                &status,
            )
            .await
            {
                Some(response) => response,
                None => ApiError::from(status).into_response(),
            }
        }
    }
}

/// `POST /repos/{repo}/merge` — produce a merge changeset from two changesets.
///
/// Upstream `WriteService.Merge` checks write on every path of the resulting delta. The result
/// is unnamed; publish it with `/land` when it is a linear head, or rebase it first.
pub(crate) async fn merge(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Result<Json<MergeBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = authorize_repository(&state, &engine, &caller, &repo, Action::Write).await {
        return error.into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let a = match parse_id(&body.a, "a") {
        Ok(value) => value,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    let b = match parse_id(&body.b, "b") {
        Ok(value) => value,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    let change_id = match parse_change_id(&body.change_id) {
        Ok(value) => value,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    let request = v1::MergeRequest {
        a: convert::id_bytes(&a),
        b: convert::id_bytes(&b),
        meta_fields: Some(body.meta.into_proto()),
        change_id,
    };
    match handlers::write::merge(&engine.context(), caller_request(request, &caller)).await {
        Ok(response) => {
            let id = id_text(&response.into_inner().changeset_id);
            (
                StatusCode::OK,
                Json(json!({
                    "service": SERVICE,
                    "status": "ok",
                    "repo": repo,
                    "changeset": { "id": id },
                })),
            )
                .into_response()
        }
        Err(status) => {
            let app = engine.context();
            let base = merge_base_id(&app, &caller, &a, &b).await;
            match pause_conflict(
                &state,
                &engine,
                &caller,
                &repo,
                merge_pause(a, b, base),
                &status,
            )
            .await
            {
                Some(response) => response,
                None => ApiError::from(status).into_response(),
            }
        }
    }
}

/// `POST /repos/{repo}/land` — publish an existing changeset on a ref.
///
/// This is the integration step for agents that write to their own ref: Land rebases the
/// changeset's linear stack onto the ref tip, checks write on every delta path and update on
/// the ref on every attempt, and commits with a version CAS. A merge head is not a legal stack
/// member (upstream INVALID_ARGUMENT/merge_in_stack), so merge results are rebased or
/// re-derived before landing.
pub(crate) async fn land(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Result<Json<LandBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = authorize_repository(&state, &engine, &caller, &repo, Action::Write).await {
        return error.into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    if let Err(error) = require_trunk(&engine, &caller, &body.ref_name) {
        return error.into_response();
    }
    let head = match parse_id(&body.head, "head") {
        Ok(value) => value,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    let request = v1::LandRequest {
        r#ref: body.ref_name.clone(),
        head: convert::id_bytes(&head),
        allow_empty: body.allow_empty,
    };
    match handlers::write::land(&engine.context(), caller_request(request, &caller)).await {
        Ok(response) => {
            let response = response.into_inner();
            tracing::info!(
                repo = %repo,
                ref_name = %body.ref_name,
                target = %id_text(&response.target),
                version = response.version,
                merge_engine = state.config.merge_engine(),
                "landed"
            );
            (
                StatusCode::OK,
                Json(json!({
                    "service": SERVICE,
                    "status": "ok",
                    "repo": repo,
                    "merge_engine": state.config.merge_engine(),
                    "ref": {
                        "name": body.ref_name,
                        "target": id_text(&response.target),
                        "version": response.version,
                    },
                    "landed": landed_json(&response.landed),
                })),
            )
                .into_response()
        }
        Err(status) => {
            let app = engine.context();
            let base = first_parent(&app, &caller, &head).await;
            match ref_state(&engine, &caller, &body.ref_name).await {
                Ok((ours, _)) => match pause_conflict(
                    &state,
                    &engine,
                    &caller,
                    &repo,
                    land_pause("land", body.ref_name.clone(), head, ours, head, base),
                    &status,
                )
                .await
                {
                    Some(response) => response,
                    None => ApiError::from(status).into_response(),
                },
                Err(_) => ApiError::from(status).into_response(),
            }
        }
    }
}

/// `POST /repos/{repo}/merge-land` — merge a head into the ref tip and publish the result as
/// one linear changeset, then land it.
///
/// v1 Land accepts linear stacks only (SPEC 8.4: a multi-parent member fails
/// `INVALID_ARGUMENT/merge_in_stack`), so a two-parent merge changeset cannot be published
/// directly. This endpoint performs what a client otherwise does by hand:
///
/// 1. resolve the ref tip;
/// 2. `Merge(tip, head)` — the merge's delta is computed against `tip` (the `a` side);
/// 3. create a single-parent changeset with that delta;
/// 4. land it.
///
/// The published history is therefore linear while its tree is the merge result — a
/// squash-land. The two-parent merge changeset is returned as `merge_changeset` so its lineage
/// stays referencable. The merge delta is read through `GetChangesets`, so the caller's read
/// policy still applies; a filtered view (a merged path the caller cannot read) is refused
/// rather than published as a partial tree.
pub(crate) async fn merge_land(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Result<Json<MergeLandBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = authorize_repository(&state, &engine, &caller, &repo, Action::Write).await {
        return error.into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    // Lane Z: a request `topology` overrides the deployment default (`FORGEVCS_MERGE_TOPOLOGY`).
    let topology = match resolve_merge_topology(&state.config, body.topology.as_deref()) {
        Ok(topology) => topology,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require_trunk(&engine, &caller, &body.ref_name) {
        return error.into_response();
    }
    let head = match parse_id(&body.head, "head") {
        Ok(value) => value,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    let app = engine.context();

    let resolved = match handlers::object::resolve_ref(
        &app,
        caller_request(
            v1::ResolveRefRequest {
                name: body.ref_name.clone(),
                at_version: None,
            },
            &caller,
        ),
    )
    .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return ApiError::from(status).into_response(),
    };
    let tip = match ObjectId::from_bytes(&resolved.target) {
        Ok(id) => id,
        Err(error) => return ApiError::from(&error).into_response(),
    };

    let author = body
        .author
        .clone()
        .unwrap_or_else(|| "ForgeVCS merge-land <merge@example.test>".to_owned());
    let message = body
        .message
        .clone()
        .unwrap_or_else(|| format!("merge-land {} onto {}", head.to_text(), body.ref_name));
    let merge_change_id = match body.change_id.as_deref() {
        Some(raw) => match parse_change_id(raw) {
            Ok(id) => id,
            Err(message) => {
                return ApiError::invalid_argument(message).into_response();
            }
        },
        None => uuid::Uuid::new_v4().into_bytes().to_vec(),
    };

    let merged = match handlers::write::merge(
        &app,
        caller_request(
            v1::MergeRequest {
                a: convert::id_bytes(&tip),
                b: convert::id_bytes(&head),
                meta_fields: Some(derived_meta(&author, &message)),
                change_id: merge_change_id,
            },
            &caller,
        ),
    )
    .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => {
            // `merge-commit-v1` refuses an unclean merge explicitly. There is no paused-then-
            // continue path for this topology (that path lands a linear resolution changeset) and
            // no silent squash; the caller gets the typed conflict and must retry.
            if topology == crate::config::MERGE_TOPOLOGY_MERGE_COMMIT {
                return ApiError::from(status)
                    .with_extra(json!({ "topology": topology }))
                    .into_response();
            }
            let base = merge_base_id(&app, &caller, &tip, &head).await;
            match pause_conflict(
                &state,
                &engine,
                &caller,
                &repo,
                land_pause("merge-land", body.ref_name.clone(), tip, tip, head, base),
                &status,
            )
            .await
            {
                Some(response) => return response,
                None => return ApiError::from(status).into_response(),
            }
        }
    };
    let merged_id = match ObjectId::from_bytes(&merged.changeset_id) {
        Ok(id) => id,
        Err(error) => return ApiError::from(&error).into_response(),
    };

    // Lane Z: tip-parented merge-commit landing. The merge changeset itself becomes the ref
    // target when its first parent is still the tip; the history layer returns a typed
    // REF_VERSION_MISMATCH (stale: re-merge) or ALREADY_LANDED rather than rebasing/squashing.
    if topology == crate::config::MERGE_TOPOLOGY_MERGE_COMMIT {
        let landed = match handlers::write::land_merge(
            &app,
            caller_request(
                v1::LandRequest {
                    r#ref: body.ref_name.clone(),
                    head: convert::id_bytes(&merged_id),
                    allow_empty: true,
                },
                &caller,
            ),
        )
        .await
        {
            Ok(response) => response.into_inner(),
            Err(status) => {
                return ApiError::from(status)
                    .with_extra(json!({ "topology": topology }))
                    .into_response();
            }
        };
        tracing::info!(
            repo = %repo,
            ref_name = %body.ref_name,
            target = %id_text(&landed.target),
            version = landed.version,
            merge_engine = state.config.merge_engine(),
            merge_topology = topology,
            "landed merge commit"
        );
        return (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "repo": repo,
                "merge_engine": state.config.merge_engine(),
                "merge_topology": topology,
                "topology": topology,
                "merge_changeset": { "id": merged_id.to_text() },
                "changeset": { "id": merged_id.to_text() },
                "ref": {
                    "name": body.ref_name,
                    "target": id_text(&landed.target),
                    "version": landed.version,
                },
                "landed": landed_json(&landed.landed),
            })),
        )
            .into_response();
    }

    let changesets = match handlers::query::get_changesets(
        &app,
        caller_request(
            v1::GetChangesetsRequest {
                ids: vec![convert::id_bytes(&merged_id)],
            },
            &caller,
        ),
    )
    .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return ApiError::from(status).into_response(),
    };
    let Some(item) = changesets.items.first() else {
        return ApiError::not_found(format!("merge changeset {merged_id} not found"))
            .into_response();
    };
    if item.status != v1::ItemStatus::Ok as i32 {
        return ApiError::not_found(format!("merge changeset {merged_id} not found"))
            .into_response();
    }
    let delta = match &item.view {
        Some(v1::changeset_item::View::FilteredView(view)) => {
            if view.filtered {
                return ApiError::conflict(
                    "the merge delta includes a path this principal cannot read; refusing to \
                     publish a partial tree",
                )
                .into_response();
            }
            view.delta.clone()
        }
        Some(v1::changeset_item::View::Encoded(bytes)) => {
            let changeset = match scion_object::Changeset::decode(bytes) {
                Ok(changeset) => changeset,
                Err(error) => return ApiError::from(&error).into_response(),
            };
            changeset
                .delta
                .iter()
                .map(v1::DeltaEntryMsg::from)
                .collect()
        }
        None => {
            return ApiError::not_found(format!("merge changeset {merged_id} has no view"))
                .into_response();
        }
    };

    let linear = match handlers::write::create_changeset(
        &app,
        caller_request(
            v1::CreateChangesetRequest {
                parents: vec![convert::id_bytes(&tip)],
                delta,
                meta_fields: Some(derived_meta(&author, &message)),
                change_id: uuid::Uuid::new_v4().into_bytes().to_vec(),
                expected_root_tree: None,
                import_mode: false,
            },
            &caller,
        ),
    )
    .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return ApiError::from(status).into_response(),
    };
    let linear_id = match ObjectId::from_bytes(&linear.changeset_id) {
        Ok(id) => id,
        Err(error) => return ApiError::from(&error).into_response(),
    };

    let landed = match handlers::write::land(
        &app,
        caller_request(
            v1::LandRequest {
                r#ref: body.ref_name.clone(),
                head: convert::id_bytes(&linear_id),
                allow_empty: true,
            },
            &caller,
        ),
    )
    .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return ApiError::from(status).into_response(),
    };

    tracing::info!(
        repo = %repo,
        ref_name = %body.ref_name,
        target = %id_text(&landed.target),
        version = landed.version,
        merge_engine = state.config.merge_engine(),
        merge_topology = topology,
        "landed"
    );
    (
        StatusCode::OK,
        Json(json!({
            "service": SERVICE,
            "status": "ok",
            "repo": repo,
            "merge_engine": state.config.merge_engine(),
            "merge_topology": topology,
            "topology": topology,
            "merge_changeset": { "id": merged_id.to_text() },
            "changeset": { "id": linear_id.to_text() },
            "ref": {
                "name": body.ref_name,
                "target": id_text(&landed.target),
                "version": landed.version,
            },
            "landed": landed_json(&landed.landed),
        })),
    )
        .into_response()
}

/// Resolve the requested `merge-land` topology against the deployment default.
///
/// `squash`/`squash-only` is the legacy squash path; `merge-commit`/`merge-commit-v1` publishes
/// the clean tip-parented merge changeset (Lane Z). A request may opt into `merge-commit` even
/// when the deployment default is `squash-only`; any other value is a 400, never a silent
/// fallback to the other topology.
fn resolve_merge_topology(
    config: &crate::config::Config,
    requested: Option<&str>,
) -> ApiResult<&'static str> {
    use crate::config::{MERGE_TOPOLOGY_MERGE_COMMIT, MERGE_TOPOLOGY_SQUASH_ONLY};
    match requested.map(str::trim).filter(|value| !value.is_empty()) {
        None => match config.merge_topology() {
            MERGE_TOPOLOGY_MERGE_COMMIT => Ok(MERGE_TOPOLOGY_MERGE_COMMIT),
            _ => Ok(MERGE_TOPOLOGY_SQUASH_ONLY),
        },
        Some("squash") | Some("squash-only") => Ok(MERGE_TOPOLOGY_SQUASH_ONLY),
        Some("merge-commit") | Some("merge-commit-v1") => Ok(MERGE_TOPOLOGY_MERGE_COMMIT),
        Some(other) => Err(ApiError::invalid_argument(format!(
            "topology must be 'squash' or 'merge-commit', got {other:?}"
        ))),
    }
}

/// Read one path at one changeset; `None` when the path is not in that tree.
async fn read_version(
    app: &Arc<AppContext>,
    caller: &CallerCtx,
    changeset: Option<&ObjectId>,
    path: &[u8],
) -> Result<Option<Vec<u8>>, Status> {
    let Some(changeset) = changeset else {
        return Ok(None);
    };
    let mut stream = match handlers::query::read_path(
        app,
        caller_request(
            v1::ReadPathRequest {
                changeset: convert::id_bytes(changeset),
                path: path.to_vec(),
            },
            caller,
        ),
    )
    .await
    {
        Ok(response) => response.into_inner(),
        Err(status) if status.code() == tonic::Code::NotFound => return Ok(None),
        Err(status) => return Err(status),
    };
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        data.extend_from_slice(&chunk.data);
    }
    Ok(Some(data))
}

async fn changeset_parents(
    app: &Arc<AppContext>,
    caller: &CallerCtx,
    id: &ObjectId,
) -> Result<Vec<ObjectId>, Status> {
    let response = handlers::query::get_changesets(
        app,
        caller_request(
            v1::GetChangesetsRequest {
                ids: vec![convert::id_bytes(id)],
            },
            caller,
        ),
    )
    .await?
    .into_inner();
    let item = response
        .items
        .first()
        .ok_or_else(|| Status::not_found(format!("changeset {id} not found")))?;
    if item.status != v1::ItemStatus::Ok as i32 {
        return Err(Status::not_found(format!("changeset {id} not found")));
    }
    match &item.view {
        Some(v1::changeset_item::View::FilteredView(view)) => view
            .parents
            .iter()
            .map(|raw| ObjectId::from_bytes(raw).map_err(|error| status_from_error(&error)))
            .collect(),
        Some(v1::changeset_item::View::Encoded(bytes)) => {
            let changeset = scion_object::Changeset::decode(bytes)
                .map_err(|error| status_from_error(&error))?;
            Ok(changeset.parents)
        }
        None => Err(Status::not_found(format!("changeset {id} has no view"))),
    }
}

/// The readable three-way bundle for a conflict report: per-path hunks/entries and the
/// base/ours/theirs bytes (capped per side). This is the shape `conflict-context` returns and the
/// shape a paused conflict persists, so `show` needs no engine round trip.
async fn conflict_sides(
    app: &Arc<AppContext>,
    caller: &CallerCtx,
    base: Option<&ObjectId>,
    ours: &ObjectId,
    theirs: &ObjectId,
    report: &ConflictReport,
) -> Result<(Vec<Value>, Vec<Value>), Status> {
    let mut conflicts = Vec::new();
    let mut clean_entries = Vec::new();
    for conflict in &report.conflicts {
        let base_data = read_version(app, caller, base, &conflict.path).await?;
        let ours_data = read_version(app, caller, Some(ours), &conflict.path).await?;
        let theirs_data = read_version(app, caller, Some(theirs), &conflict.path).await?;
        conflicts.push(json!({
            "path": String::from_utf8_lossy(&conflict.path).into_owned(),
            "kind": conflict.kind.as_str(),
            "hunks": conflict_hunks_json(conflict),
            "entries": {
                "base": entry_info_json(conflict.base.as_ref()),
                "ours": entry_info_json(conflict.ours.as_ref()),
                "theirs": entry_info_json(conflict.theirs.as_ref()),
            },
            "versions": {
                "base": version_json(base_data),
                "ours": version_json(ours_data),
                "theirs": version_json(theirs_data),
            },
        }));
    }
    for clean in &report.clean_entries {
        clean_entries.push(json!({
            "path": String::from_utf8_lossy(&clean.path).into_owned(),
            "entry": entry_info_json(clean.entry.as_ref()),
        }));
    }
    Ok((conflicts, clean_entries))
}

/// `POST /repos/{repo}/conflict-context` — the three sides of a refused merge or rebase.
///
/// A `409 conflict` from `merge`, `merge-land`, `rebase` or a contended `land` carries the
/// conflict report (path, kind, entries, hunks) but not the bytes. Resolving a conflict needs
/// the three versions, and the base changeset is not in the report:
///
/// - **merge** (`{"a":"…","b":"…"}`): base is the unique merge base, ours is `a`, theirs is `b`;
/// - **rebase/land** (`{"changeset":"…","onto":"…"}`): base is the changeset's parent, ours is
///   `onto` (the ref tip), theirs is the changeset being replayed.
///
/// This endpoint re-runs the operation to obtain the same deterministic report, then reads each
/// conflicting path at all three changesets under the caller's read policy. The result is the
/// complete diff3 bundle an agent needs: `base`, `ours`, `theirs` per conflict (absent sides are
/// `{"present": false}`), plus the entries and hunks. The server still never writes markers: the
/// agent chooses bytes and lands them.
///
/// A clean run returns `clean: true` and the operation's changeset; that changeset is
/// unreferenced unless the caller lands it, so a context call is safe to repeat.
pub(crate) async fn conflict_context(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Result<Json<ConflictContextBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let app = engine.context();

    let (orientation, base, ours, theirs, report, clean_changeset) =
        if let (Some(raw_a), Some(raw_b)) = (body.a.as_deref(), body.b.as_deref()) {
            let a = match parse_id(raw_a, "a") {
                Ok(id) => id,
                Err(message) => {
                    return ApiError::invalid_argument(message).into_response();
                }
            };
            let b = match parse_id(raw_b, "b") {
                Ok(id) => id,
                Err(message) => {
                    return ApiError::invalid_argument(message).into_response();
                }
            };
            let bases = match handlers::query::merge_base(
                &app,
                caller_request(
                    v1::MergeBaseRequest {
                        a: convert::id_bytes(&a),
                        b: convert::id_bytes(&b),
                    },
                    &caller,
                ),
            )
            .await
            {
                Ok(response) => response.into_inner().bases,
                Err(status) => return ApiError::from(status).into_response(),
            };
            let base = match bases.as_slice() {
                [] => None,
                [one] => match ObjectId::from_bytes(one) {
                    Ok(id) => Some(id),
                    Err(error) => return ApiError::from(&error).into_response(),
                },
                _ => {
                    return ApiError::conflict(
                        "the two changesets have more than one best common ancestor",
                    )
                    .into_response();
                }
            };
            let merged = handlers::write::merge(
                &app,
                caller_request(
                    v1::MergeRequest {
                        a: convert::id_bytes(&a),
                        b: convert::id_bytes(&b),
                        meta_fields: Some(derived_meta(
                            "ForgeVCS conflict context <conflict@example.test>",
                            "conflict context",
                        )),
                        change_id: uuid::Uuid::new_v4().into_bytes().to_vec(),
                    },
                    &caller,
                ),
            )
            .await;
            match merged {
                Ok(response) => (
                    "merge",
                    base,
                    a,
                    b,
                    None,
                    Some(id_text(&response.into_inner().changeset_id)),
                ),
                Err(status) => match conflict_report_of(&status) {
                    Some(report) => ("merge", base, a, b, Some(report), None),
                    None => return ApiError::from(status).into_response(),
                },
            }
        } else if let (Some(raw_changeset), Some(raw_onto)) =
            (body.changeset.as_deref(), body.onto.as_deref())
        {
            let changeset = match parse_id(raw_changeset, "changeset") {
                Ok(id) => id,
                Err(message) => {
                    return ApiError::invalid_argument(message).into_response();
                }
            };
            let onto = match parse_id(raw_onto, "onto") {
                Ok(id) => id,
                Err(message) => {
                    return ApiError::invalid_argument(message).into_response();
                }
            };
            let parents = match changeset_parents(&app, &caller, &changeset).await {
                Ok(parents) => parents,
                Err(status) => return ApiError::from(status).into_response(),
            };
            let [parent] = parents.as_slice() else {
                return ApiError::invalid_argument(
                    "rebase context requires a single-parent changeset",
                )
                .into_response();
            };
            let rebased = handlers::write::rebase(
                &app,
                caller_request(
                    v1::RebaseRequest {
                        changeset: convert::id_bytes(&changeset),
                        onto: convert::id_bytes(&onto),
                    },
                    &caller,
                ),
            )
            .await;
            match rebased {
                Ok(response) => (
                    "rebase",
                    Some(*parent),
                    onto,
                    changeset,
                    None,
                    Some(id_text(&response.into_inner().changeset_id)),
                ),
                Err(status) => match conflict_report_of(&status) {
                    Some(report) => ("rebase", Some(*parent), onto, changeset, Some(report), None),
                    None => return ApiError::from(status).into_response(),
                },
            }
        } else {
            return ApiError::invalid_argument("provide a+b (merge) or changeset+onto (rebase)")
                .into_response();
        };

    let (conflicts, clean_entries) = if let Some(report) = &report {
        match conflict_sides(&app, &caller, base.as_ref(), &ours, &theirs, report).await {
            Ok(sides) => sides,
            Err(status) => return ApiError::from(status).into_response(),
        }
    } else {
        (Vec::new(), Vec::new())
    };

    (
        StatusCode::OK,
        Json(json!({
            "service": SERVICE,
            "status": "ok",
            "repo": repo,
            "orientation": orientation,
            "base": base.map(|id| json!({ "changeset": id.to_text() })).unwrap_or(Value::Null),
            "ours": { "changeset": ours.to_text() },
            "theirs": { "changeset": theirs.to_text() },
            "clean": report.is_none(),
            "changeset": clean_changeset.map(|id| json!({ "id": id })).unwrap_or(Value::Null),
            "conflicts": conflicts,
            "clean_entries": clean_entries,
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------------------------
// Paused conflicts (resolve/continue workflow)
//
// A conflict used to be terminal: the caller re-derived the sides with `conflict-context` and
// pushed again. Here the refused operation is persisted so the caller can resolve it (with a
// changeset that resolves the conflict) and continue (land it), with a stale-ref guard.
// ---------------------------------------------------------------------------------------------

/// One refused operation, ready to be persisted as a paused conflict.
struct PauseRequest {
    operation: &'static str,
    ref_name: Option<String>,
    head: ObjectId,
    resolve_parent: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
    base: Option<ObjectId>,
}

/// Pause shape for a land-family operation (`land`, `push` mode=land, `merge-land`).
///
/// The resolution is parented on the ref tip, **not** on the paused head: `continue` then lands
/// a single linear changeset. Parenting on the head would make land replay the conflicting head
/// onto the tip again before reaching the resolution (the original R2 defect).
fn land_pause(
    operation: &'static str,
    ref_name: String,
    head: ObjectId,
    tip: ObjectId,
    theirs: ObjectId,
    base: Option<ObjectId>,
) -> PauseRequest {
    PauseRequest {
        operation,
        ref_name: Some(ref_name),
        head,
        resolve_parent: tip,
        ours: tip,
        theirs,
        base,
    }
}

/// Pause shape for an explicit two-parent `merge`: the resolution is parented on `a` (ours).
fn merge_pause(a: ObjectId, b: ObjectId, base: Option<ObjectId>) -> PauseRequest {
    PauseRequest {
        operation: "merge",
        ref_name: None,
        head: a,
        resolve_parent: a,
        ours: a,
        theirs: b,
        base,
    }
}

/// Pause shape for `rebase`: the resolution is parented on the rebase target `onto` (usually the
/// ref tip), so `continue` lands the resolution without replaying the conflicting changeset.
fn rebase_pause(changeset: ObjectId, onto: ObjectId, base: Option<ObjectId>) -> PauseRequest {
    PauseRequest {
        operation: "rebase",
        ref_name: None,
        head: changeset,
        resolve_parent: onto,
        ours: onto,
        theirs: changeset,
        base,
    }
}

/// The 409 for a `continue` whose land itself conflicted (the resolution no longer applies
/// cleanly). The paused record is attached so the caller can see what was being continued.
fn continue_land_failure(status: Status, record: &Record) -> ApiError {
    let is_conflict = conflict_report_of(&status).is_some();
    let error = ApiError::from(status);
    if is_conflict {
        error.with_extra(json!({ "conflict": paused_conflicts::record_json(record) }))
    } else {
        error
    }
}

/// The ref tip and version, resolved through the same upstream handler every read uses.
async fn ref_state(
    engine: &Arc<Engine>,
    caller: &CallerCtx,
    ref_name: &str,
) -> Result<(ObjectId, u64), Status> {
    let app = engine.context();
    let resolved = handlers::object::resolve_ref(
        &app,
        caller_request(
            v1::ResolveRefRequest {
                name: ref_name.to_owned(),
                at_version: None,
            },
            caller,
        ),
    )
    .await?
    .into_inner();
    let target =
        ObjectId::from_bytes(&resolved.target).map_err(|error| status_from_error(&error))?;
    Ok((target, resolved.version))
}

/// The first parent of a changeset, when it has one and the caller may read it.
async fn first_parent(
    app: &Arc<AppContext>,
    caller: &CallerCtx,
    id: &ObjectId,
) -> Option<ObjectId> {
    changeset_parents(app, caller, id)
        .await
        .ok()
        .and_then(|parents| parents.first().copied())
}

/// The unique merge base of two changesets, when one exists and the caller may read it.
async fn merge_base_id(
    app: &Arc<AppContext>,
    caller: &CallerCtx,
    a: &ObjectId,
    b: &ObjectId,
) -> Option<ObjectId> {
    let bases = handlers::query::merge_base(
        app,
        caller_request(
            v1::MergeBaseRequest {
                a: convert::id_bytes(a),
                b: convert::id_bytes(b),
            },
            caller,
        ),
    )
    .await
    .ok()?
    .into_inner()
    .bases;
    match bases.as_slice() {
        [one] => ObjectId::from_bytes(one).ok(),
        _ => None,
    }
}

/// Persist a refused operation and return the augmented `409` response, or `None` when the
/// status is not a merge conflict or the store is unavailable (the caller then returns the
/// original status unchanged).
async fn pause_conflict(
    state: &Arc<AppState>,
    engine: &Arc<Engine>,
    caller: &CallerCtx,
    repo: &str,
    request: PauseRequest,
    status: &Status,
) -> Option<Response> {
    let report = conflict_report_of(status)?;
    let app = engine.context();
    let sides = match conflict_sides(
        &app,
        caller,
        request.base.as_ref(),
        &request.ours,
        &request.theirs,
        &report,
    )
    .await
    {
        Ok((conflicts, clean_entries)) => {
            json!({ "conflicts": conflicts, "clean_entries": clean_entries })
        }
        Err(error) => {
            tracing::warn!(%error, "paused conflict sides could not be read; storing the report only");
            json!({ "conflicts": [], "clean_entries": [], "error": error.message() })
        }
    };
    let (ref_version, ref_target) = match request.ref_name.as_deref() {
        Some(name) => match ref_state(engine, caller, name).await {
            Ok((target, version)) => (
                Some(i64::try_from(version).unwrap_or(i64::MAX)),
                Some(target.to_text()),
            ),
            Err(_) => (None, None),
        },
        None => (None, None),
    };
    // `operation` is a `&'static str` from the closed set {push, land, merge_land, merge,
    // rebase}; the metric normalises anything else to `other`.
    let operation = request.operation;
    let new = NewConflict {
        repo: repo.to_owned(),
        operation: operation.to_owned(),
        ref_name: request.ref_name,
        ref_version,
        ref_target,
        head: request.head.to_text(),
        resolve_parent: request.resolve_parent.to_text(),
        base: request.base.map(|id| id.to_text()),
        ours: request.ours.to_text(),
        theirs: request.theirs.to_text(),
        report: crate::api_error::conflict_report_json(&report),
        sides,
        created_by: caller.principal.id_string(),
    };
    match state.paused.open(new).await {
        Ok(record) => {
            // Observability: the pause is the "agent hit a conflict" signal, labelled by the
            // operation that produced it (closed set, see crate::metrics).
            state.forge_metrics.record_conflict_pause(operation);
            Some(paused_error_response(status, &record))
        }
        Err(error) => {
            tracing::warn!(%error, "could not persist paused conflict");
            None
        }
    }
}

/// The conflict response with the persisted record attached, keeping the upstream `details`.
fn paused_error_response(status: &Status, record: &Record) -> Response {
    ApiError::from(status.clone())
        .with_extra(json!({ "conflict": paused_conflicts::record_json(record) }))
        .into_response()
}

/// Only the principal that hit the conflict or a repository administrator may resolve/continue
/// it: the persisted sides were read under the creator's read policy.
fn can_manage_conflict(engine: &Arc<Engine>, caller: &CallerCtx, record: &Record) -> bool {
    record.created_by == caller.principal.id_string()
        || engine
            .context()
            .policy
            .current()
            .can_admin(&caller.principal, AdminCapability::Repositories.permit())
}

/// Refuse a paused conflict that is not open (resolve) with the current state attached.
fn not_open_response(record: &Record) -> Response {
    ApiError::conflict(format!(
        "conflict {} is not open (status={}); resolve only applies to an open conflict",
        record.id, record.status
    ))
    .with_extra(json!({ "conflict": paused_conflicts::record_json(record) }))
    .into_response()
}

/// Whether the ref moved since the conflict was paused. A missing recorded version (the ref
/// could not be resolved at pause time) cannot be shown stale; the land itself then reports a
/// CAS/ref mismatch.
fn stale_ref(record: &Record, current_version: u64) -> bool {
    record
        .ref_version
        .is_some_and(|paused| i64::try_from(current_version).unwrap_or(i64::MAX) != paused)
}

/// A resolving changeset must be the single child of the paused continuation.
fn valid_resolution_parent(parents: &[ObjectId], expected: &str) -> bool {
    matches!(parents, [parent] if parent.to_text() == expected)
}

/// Refuse a continue whose ref moved since the conflict was paused.
fn stale_response(record: &Record, current_version: u64, current_target: &ObjectId) -> Response {
    ApiError::conflict(format!(
        "stale conflict: ref {:?} moved since the conflict was paused",
        record.ref_name.as_deref().unwrap_or("?")
    ))
    .with_extra(json!({
        "conflict": paused_conflicts::record_json(record),
        "stale": true,
        "current_version": current_version,
        "current_target": current_target.to_text(),
    }))
    .into_response()
}

// ---------------------------------------------------------------------------------------------
// Conflict escalation queue (DevOps hand-off)
//
// The worker escalates a conflict it cannot resolve; the DevOps seat lists, claims and resolves
// it. The orchestrator polls `GET /repos/{repo}/conflicts?status=open` and launches a DevOps
// agent per item. The queue stores pointers and the conflict report, never content: the DevOps
// agent re-reads the three sides with `/conflict-context`.
// ---------------------------------------------------------------------------------------------

/// Reject a malformed paused-conflict id before it reaches a `$1::text::uuid` cast (which would
/// surface as a 500 database error rather than a 400 invalid_argument).
fn check_conflict_id(id: &str) -> ApiResult<()> {
    if uuid::Uuid::parse_str(id).is_err() {
        return Err(ApiError::invalid_argument("id is not a UUID"));
    }
    Ok(())
}

fn queue_unavailable() -> ApiError {
    ApiError::unavailable("the conflict queue needs FORGEVCS_DATABASE_URL")
}

/// `POST /repos/{repo}/conflicts/escalate` — file a stuck merge/rebase. Workers may escalate.
pub(crate) async fn escalate_conflict(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Result<Json<EscalateBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let head = match parse_id(&body.head, "head") {
        Ok(id) => id.to_text(),
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    let optional_id = |raw: Option<&str>, field: &str| -> Result<Option<String>, String> {
        raw.map(|value| parse_id(value, field).map(|id| id.to_text()))
            .transpose()
    };
    let onto = match optional_id(body.onto.as_deref(), "onto") {
        Ok(id) => id,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    let base = match optional_id(body.base.as_deref(), "base") {
        Ok(id) => id,
        Err(message) => {
            return ApiError::invalid_argument(message).into_response();
        }
    };
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some("/"),
        None,
    )
    .await
    {
        return error.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable().into_response();
    };
    match crate::escalations::insert(
        pool,
        &repo,
        &body.ref_name,
        &head,
        onto.as_deref(),
        base.as_deref(),
        body.note.as_deref(),
        body.report.as_ref(),
        &caller.principal.to_string(),
    )
    .await
    {
        Ok(row) => (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "repo": repo,
                "escalation": row,
            })),
        )
            .into_response(),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

/// `GET /repos/{repo}/conflicts?status=open|claimed|resolved|all` — the DevOps work list.
pub(crate) async fn list_conflicts(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    query: Result<Query<ConflictQueueQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    if query.scope.as_deref() == Some("paused") {
        // The paused workflow is the caller's own conflict state, not the DevOps escalation
        // queue: repository read is enough to list it.
        if let Err(error) = authorize(
            &state,
            &engine,
            &caller,
            &repo,
            Action::Read,
            Some("/"),
            None,
        )
        .await
        {
            return error.into_response();
        }
        let status = query.status.unwrap_or_else(|| "open".to_owned());
        if !paused_conflicts::valid_status(&status) {
            return ApiError::invalid_argument(
                "status must be one of open, resolved, continued, all",
            )
            .into_response();
        }
        return match state.paused.list(&repo, &status).await {
            Ok(records) => (
                StatusCode::OK,
                Json(json!({
                    "service": SERVICE,
                    "status": "ok",
                    "repo": repo,
                    "scope": "paused",
                    "filter": status,
                    "conflicts": records.iter().map(paused_conflicts::record_json).collect::<Vec<_>>(),
                })),
            )
                .into_response(),
            Err(error) => ApiError::internal(error).into_response(),
        };
    }
    if let Err(error) = require_devops(
        &engine,
        &caller,
        AdminCapability::Trunk,
        "reading the conflict queue requires the DevOps seat (admin.trunk)",
    ) {
        return error.into_response();
    }
    let status = query.status.unwrap_or_else(|| "open".to_owned());
    if !crate::escalations::valid_status(&status) {
        return ApiError::invalid_argument("status must be one of open, claimed, resolved, all")
            .into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable().into_response();
    };
    match crate::escalations::list(pool, &repo, &status).await {
        Ok(rows) => (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "repo": repo,
                "filter": status,
                "escalations": rows,
            })),
        )
            .into_response(),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

/// `POST /repos/{repo}/conflicts/{id}/claim` — take ownership of an escalation as DevOps.
pub(crate) async fn claim_conflict(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require_devops(
        &engine,
        &caller,
        AdminCapability::Trunk,
        "claiming a conflict requires the DevOps seat (admin.trunk)",
    ) {
        return error.into_response();
    }
    if uuid::Uuid::parse_str(&id).is_err() {
        return ApiError::invalid_argument("id is not a UUID").into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable().into_response();
    };
    match crate::escalations::claim(pool, &repo, &id, &caller.principal.to_string()).await {
        Ok(Some(row)) => (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "repo": repo,
                "escalation": row,
            })),
        )
            .into_response(),
        Ok(None) => ApiError::conflict("escalation is not open (already claimed or resolved)")
            .into_response(),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

/// `POST /repos/{repo}/conflicts/{id}/resolve` — close an escalation with a resolution note.
pub(crate) async fn resolve_conflict(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<ResolveEscalationBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    if let Err(error) = check_conflict_id(&id) {
        return error.into_response();
    }
    // Paused conflicts and the DevOps escalation queue share this route (as they share the
    // `/conflicts` route for listing). A paused id wins; escalation bodies never carry
    // `changeset`, so the dispatch is unambiguous.
    match state.paused.get(&repo, &id).await {
        Ok(Some(record)) => {
            return resolve_paused_conflict(
                &state,
                &engine,
                &caller,
                &repo,
                &record,
                body.changeset.as_deref(),
            )
            .await;
        }
        Ok(None) => {}
        Err(error) => return ApiError::internal(error).into_response(),
    }
    if let Err(error) = require_devops(
        &engine,
        &caller,
        AdminCapability::Trunk,
        "resolving a conflict requires the DevOps seat (admin.trunk)",
    ) {
        return error.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable().into_response();
    };
    match crate::escalations::resolve(
        pool,
        &repo,
        &id,
        &caller.principal.to_string(),
        body.resolution.as_deref(),
    )
    .await
    {
        Ok(Some(row)) => (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "repo": repo,
                "escalation": row,
            })),
        )
            .into_response(),
        Ok(None) => ApiError::conflict("escalation is already resolved or absent").into_response(),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

/// `GET /repos/{repo}/conflicts/{id}` — one paused conflict with its persisted sides.
pub(crate) async fn show_conflict(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = check_conflict_id(&id) {
        return error.into_response();
    }
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some("/"),
        None,
    )
    .await
    {
        return error.into_response();
    }
    match state.paused.get(&repo, &id).await {
        Ok(Some(record)) => (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "repo": repo,
                "conflict": paused_conflicts::record_json(&record),
            })),
        )
            .into_response(),
        Ok(None) => ApiError::not_found(format!("paused conflict {id} not found")).into_response(),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

/// Resolve a paused conflict with a single-parent changeset that continues `resolve_parent`.
async fn resolve_paused_conflict(
    state: &Arc<AppState>,
    engine: &Arc<Engine>,
    caller: &CallerCtx,
    repo: &str,
    record: &Record,
    changeset: Option<&str>,
) -> Response {
    if !can_manage_conflict(engine, caller, record) {
        return ApiError::permission_denied(
            "only the principal that hit the conflict or a repository administrator may resolve it",
        )
        .into_response();
    }
    if let Err(error) = authorize_repository(state, engine, caller, repo, Action::Write).await {
        return error.into_response();
    }
    if record.status != "open" {
        return not_open_response(record);
    }
    let Some(raw_changeset) = changeset else {
        return ApiError::invalid_argument(
            "resolve needs a resolved changeset (the changeset that contains the resolution)",
        )
        .into_response();
    };
    let resolved = match parse_id(raw_changeset, "changeset") {
        Ok(id) => id,
        Err(message) => return ApiError::invalid_argument(message).into_response(),
    };
    let app = engine.context();
    let parents = match changeset_parents(&app, caller, &resolved).await {
        Ok(parents) => parents,
        Err(status) => return ApiError::from(status).into_response(),
    };
    if !valid_resolution_parent(&parents, &record.resolve_parent) {
        return ApiError::invalid_argument(format!(
            "resolved changeset must be a single-parent child of {} (the paused continuation)",
            record.resolve_parent
        ))
        .into_response();
    }
    match state
        .paused
        .resolve(
            repo,
            &record.id,
            &caller.principal.id_string(),
            &resolved.to_text(),
        )
        .await
    {
        Ok(ResolveOutcome::Resolved(record)) => {
            state.forge_metrics.record_conflict_resolved();
            (
                StatusCode::OK,
                Json(json!({
                    "service": SERVICE,
                    "status": "ok",
                    "repo": repo,
                    "conflict": paused_conflicts::record_json(&record),
                })),
            )
                .into_response()
        }
        Ok(ResolveOutcome::NotFound) => {
            ApiError::not_found(format!("paused conflict {} not found", record.id)).into_response()
        }
        Ok(ResolveOutcome::NotOpen(record)) => not_open_response(&record),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

/// `POST /repos/{repo}/conflicts/{id}/continue` — land the resolved changeset.
///
/// The ref must still be at the version observed when the conflict was paused (stale refusal);
/// the land itself re-checks write on every delta path and the ref CAS, so a concurrent winner is
/// reported the normal way rather than silently overwriting it.
pub(crate) async fn continue_conflict(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<ContinueConflictBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = check_conflict_id(&id) {
        return error.into_response();
    }
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    if let Err(error) = authorize_repository(&state, &engine, &caller, &repo, Action::Write).await {
        return error.into_response();
    }
    let record = match state.paused.get(&repo, &id).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return ApiError::not_found(format!("paused conflict {id} not found")).into_response();
        }
        Err(error) => return ApiError::internal(error).into_response(),
    };
    if !can_manage_conflict(&engine, &caller, &record) {
        return ApiError::permission_denied(
            "only the principal that hit the conflict or a repository administrator may continue it",
        )
        .into_response();
    }
    if record.status != "resolved" {
        state.forge_metrics.record_conflict_continue("invalid");
        return ApiError::conflict(format!(
            "conflict {} is not resolved (status={}); resolve it before continuing",
            record.id, record.status
        ))
        .with_extra(json!({ "conflict": paused_conflicts::record_json(&record) }))
        .into_response();
    }
    let Some(raw_resolved) = record.resolved_changeset.as_deref() else {
        return ApiError::internal("resolved conflict has no resolved changeset").into_response();
    };
    let resolved = match parse_id(raw_resolved, "resolved_changeset") {
        Ok(id) => id,
        Err(message) => return ApiError::internal(message).into_response(),
    };
    let ref_name = match record.ref_name.clone().or(body.ref_name) {
        Some(ref_name) => ref_name,
        None => {
            return ApiError::invalid_argument(format!(
                "conflict {} was paused by a {} with no target ref; pass ref to continue",
                record.id, record.operation
            ))
            .into_response();
        }
    };
    let (current_target, current_version) = match ref_state(&engine, &caller, &ref_name).await {
        Ok(state) => state,
        Err(status) => return ApiError::from(status).into_response(),
    };
    if stale_ref(&record, current_version) {
        state.forge_metrics.record_conflict_continue("stale");
        return stale_response(&record, current_version, &current_target);
    }
    let app = engine.context();
    let request = v1::LandRequest {
        r#ref: ref_name.clone(),
        head: convert::id_bytes(&resolved),
        // The caller chose the resolved bytes; a resolution that equals the tip is an intentional
        // empty change, not a land failure.
        allow_empty: true,
    };
    match handlers::write::land(&app, caller_request(request, &caller)).await {
        Ok(response) => {
            let response = response.into_inner();
            match state
                .paused
                .continue_landed(
                    &repo,
                    &record.id,
                    &id_text(&response.target),
                    response.version,
                )
                .await
            {
                Ok(ContinueOutcome::Continued(record)) => {
                    state.forge_metrics.record_conflict_continue("ok");
                    (
                        StatusCode::OK,
                        Json(json!({
                            "service": SERVICE,
                            "status": "ok",
                            "repo": repo,
                            "conflict": paused_conflicts::record_json(&record),
                            "ref": {
                                "name": ref_name,
                                "target": id_text(&response.target),
                                "version": response.version,
                            },
                            "landed": landed_json(&response.landed),
                        })),
                    )
                        .into_response()
                }
                Ok(ContinueOutcome::NotFound) => {
                    state.forge_metrics.record_conflict_continue("invalid");
                    ApiError::not_found(format!("paused conflict {} not found", record.id))
                        .into_response()
                }
                Ok(ContinueOutcome::NotResolved(record)) => {
                    state.forge_metrics.record_conflict_continue("invalid");
                    ApiError::conflict(format!(
                        "conflict {} is no longer resolved (status={})",
                        record.id, record.status
                    ))
                    .with_extra(json!({ "conflict": paused_conflicts::record_json(&record) }))
                    .into_response()
                }
                Err(error) => {
                    state.forge_metrics.record_conflict_continue("error");
                    ApiError::internal(error).into_response()
                }
            }
        }
        Err(status) => {
            state.forge_metrics.record_conflict_continue("land_failed");
            continue_land_failure(status, &record).into_response()
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Repository lifecycle (DevOps seat)
//
// Creation is gated in `init` on admin.repos. Deletion is metadata-only on purpose: Scion
// objects are content-addressed and shared between repositories, so there is no per-repository
// blob set to remove safely. The rows are deleted in one transaction; blobs stay until a
// garbage-collection workstream can prove no other repository references them.
// ---------------------------------------------------------------------------------------------

/// ForgeVCS-owned tables keyed by the repository **name** (not `repo_id`), removed in the same
/// transaction as the repository rows so a deleted repository leaves no paused conflicts or
/// workspace/queue state behind.
const SERVICE_REPO_TABLES: [&str; 3] = [
    "forgevcs_workspaces",
    "forgevcs_conflict_escalations",
    "forgevcs_paused_conflicts",
];

/// Tables whose rows are keyed by `repo_id` (SPEC 5.2). No foreign keys, so order is only for
/// readability.
const SCION_REPO_TABLES: [&str; 13] = [
    "landed",
    "ref_log",
    "refs",
    "changeset_children",
    "changesets",
    "object_uploads",
    "object_paths",
    "objects",
    "derived",
    "derive_leases",
    "redactions",
    "counters",
    "audit_log",
];

async fn delete_repository_rows(pool: &Pool, repo: &str) -> ApiResult<Value> {
    let mut client = pool
        .get()
        .await
        .map_err(|error| ApiError::unavailable(format!("deleting repository: {error}")))?;
    let tx = client
        .transaction()
        .await
        .map_err(|error| ApiError::internal(format!("deleting repository: {error}")))?;
    let row = tx
        .query_opt("SELECT repo_id FROM repos WHERE name = $1", &[&repo])
        .await
        .map_err(|error| ApiError::internal(format!("reading repository: {error}")))?;
    let Some(row) = row else {
        return Err(ApiError::not_found(format!("repository {repo} not found")));
    };
    let repo_id: i64 = row.get(0);
    let mut rows = serde_json::Map::new();
    for table in SCION_REPO_TABLES {
        let sql = format!("DELETE FROM {table} WHERE repo_id = $1");
        let removed = tx
            .execute(&sql, &[&repo_id])
            .await
            .map_err(|error| ApiError::internal(format!("deleting {table}: {error}")))?;
        rows.insert(table.to_owned(), json!(removed));
    }
    for table in SERVICE_REPO_TABLES {
        let sql = format!("DELETE FROM {table} WHERE repo = $1");
        let removed = tx
            .execute(&sql, &[&repo])
            .await
            .map_err(|error| ApiError::internal(format!("deleting {table}: {error}")))?;
        rows.insert(table.to_owned(), json!(removed));
    }
    // FVCS-01: repository deletion releases the owner row in the same transaction, so a later
    // recreation starts with no owner instead of inheriting the deleted repository's principal.
    let removed_owner = tx
        .execute(
            "DELETE FROM forgevcs_repo_acl WHERE repository = $1",
            &[&repo],
        )
        .await
        .map_err(|error| ApiError::internal(format!("deleting forgevcs_repo_acl: {error}")))?;
    rows.insert("forgevcs_repo_acl".to_owned(), json!(removed_owner));
    let removed = tx
        .execute("DELETE FROM repos WHERE repo_id = $1", &[&repo_id])
        .await
        .map_err(|error| ApiError::internal(format!("deleting the repository row: {error}")))?;
    if removed != 1 {
        return Err(ApiError::internal(
            "the repository row changed during deletion",
        ));
    }
    tx.commit()
        .await
        .map_err(|error| ApiError::internal(format!("committing repository deletion: {error}")))?;
    Ok(json!({
        "deleted": true,
        "repo": repo,
        "rows": rows,
        "objects": "retained: Scion blobs are content-addressed and shared; garbage collection is a separate workstream",
    }))
}

/// `GET /repos` — every repository in the control plane. DevOps seat.
pub(crate) async fn list_repos(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let (auth_engine, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require_devops(
        &auth_engine,
        &caller,
        AdminCapability::Repositories,
        "listing repositories requires the DevOps seat (admin.repos)",
    ) {
        return error.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable().into_response();
    };
    let Ok(client) = pool.get().await else {
        return queue_unavailable().into_response();
    };
    let rows = match client
        .query(
            "SELECT name, created_at::text AS created_at FROM repos ORDER BY name",
            &[],
        )
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return ApiError::internal(format!("listing repositories: {error}")).into_response();
        }
    };
    let repos: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "name": row.get::<_, String>("name"),
                "created_at": row.get::<_, String>("created_at"),
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(json!({ "service": SERVICE, "status": "ok", "repos": repos })),
    )
        .into_response()
}

/// `DELETE /repos/{repo}` — remove a repository's metadata. DevOps seat only; the body must
/// repeat the repository name. History rows go in one transaction; shared objects stay.
pub(crate) async fn delete_repo(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Result<Json<DeleteRepoBody>, JsonRejection>,
) -> Response {
    let (auth_engine, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require_devops(
        &auth_engine,
        &caller,
        AdminCapability::Repositories,
        "deleting a repository requires the DevOps seat (admin.repos)",
    ) {
        return error.into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    if body.confirm != repo {
        return ApiError::invalid_argument("confirm must equal the repository name")
            .into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable().into_response();
    };
    match delete_repository_rows(pool, &repo).await {
        Ok(deleted) => {
            state.engines.remove(&repo);
            (
                StatusCode::OK,
                Json(json!({ "service": SERVICE, "status": "ok", "repository": deleted })),
            )
                .into_response()
        }
        Err(error) => error.into_response(),
    }
}

/// `DELETE /repos/{repo}/conflicts?status=resolved` — clear queue history. DevOps seat only;
/// `status=all` clears everything, any other status clears that state.
pub(crate) async fn clear_conflicts(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    query: Result<Query<ClearConflictsQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let (auth_engine, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require_devops(
        &auth_engine,
        &caller,
        AdminCapability::Trunk,
        "clearing the conflict queue requires the DevOps seat (admin.trunk)",
    ) {
        return error.into_response();
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let status = query.status.unwrap_or_else(|| "resolved".to_owned());
    if !crate::escalations::valid_status(&status) {
        return ApiError::invalid_argument("status must be one of open, claimed, resolved, all")
            .into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable().into_response();
    };
    match crate::escalations::clear(pool, &repo, &status).await {
        Ok(removed) => (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "repo": repo,
                "filter": status,
                "cleared": removed,
            })),
        )
            .into_response(),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

/// `DELETE /repos/{repo}/conflicts/{id}` — clear one queue item. DevOps seat only.
pub(crate) async fn delete_conflict(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (auth_engine, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require_devops(
        &auth_engine,
        &caller,
        AdminCapability::Trunk,
        "clearing a conflict requires the DevOps seat (admin.trunk)",
    ) {
        return error.into_response();
    }
    if uuid::Uuid::parse_str(&id).is_err() {
        return ApiError::invalid_argument("id is not a UUID").into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable().into_response();
    };
    match crate::escalations::delete(pool, &repo, &id).await {
        Ok(true) => (
            StatusCode::OK,
            Json(json!({ "service": SERVICE, "status": "ok", "repo": repo, "cleared": id })),
        )
            .into_response(),
        Ok(false) => ApiError::not_found("escalation not found").into_response(),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// Object garbage collection (DevOps seat)
// ---------------------------------------------------------------------------------------------

/// `POST /admin/gc/objects` — delete blobs whose repository no longer exists. Scion keys are
/// repository-scoped, so an orphan has one exact meaning here; dry-run unless `apply` is true.
pub(crate) async fn gc_objects(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<GcBody>, JsonRejection>,
) -> Response {
    let (auth_engine, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require_devops(
        &auth_engine,
        &caller,
        AdminCapability::Repositories,
        "object garbage collection requires the DevOps seat (admin.repos)",
    ) {
        return error.into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let Some(s3) = state.config.s3_config() else {
        return ApiError::unavailable("object storage is not configured").into_response();
    };
    let blobs = match scion_store_s3::S3BlobStore::new(s3) {
        Ok(blobs) => blobs,
        Err(error) => return ApiError::from(&error).into_response(),
    };
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable().into_response();
    };
    let report = match body.prefix.as_deref() {
        Some(prefix) => crate::gc::purge_prefix(&blobs, prefix, body.apply).await,
        None => crate::gc::sweep(&blobs, pool, body.apply).await,
    };
    match report {
        Ok(report) => (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "gc": {
                    "keys": report.keys,
                    "live_repos": report.live_repos,
                    "orphan_keys": report.orphan_keys,
                    "deleted_keys": report.deleted_keys,
                    "malformed_keys": report.malformed_keys,
                    "malformed_sample": report.malformed_sample,
                    "sample": report.sample,
                    "malformed_sample": report.malformed_sample,
                    "applied": report.applied,
                },
            })),
        )
            .into_response(),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// Backup transfer (export/import of object bytes)
//
// The metadata lives in Postgres and is exported by the backup client through psql; these two
// routes move the external object bytes, which psql cannot see. Both are DevOps-only and keys
// are confined to the repository's own two areas (`r/<repo_id>/`, `q/<repo_id>/`).
// ---------------------------------------------------------------------------------------------

async fn repo_id_for(pool: &Pool, repo: &str) -> ApiResult<i64> {
    let client = pool
        .get()
        .await
        .map_err(|error| ApiError::unavailable(format!("backup: database: {error}")))?;
    let row = client
        .query_opt("SELECT repo_id FROM repos WHERE name = $1", &[&repo])
        .await
        .map_err(|error| ApiError::internal(format!("backup: reading repository: {error}")))?;
    row.map(|row| row.get::<_, i64>(0))
        .ok_or_else(|| ApiError::not_found(format!("repository {repo} not found")))
}

fn store_for_backup(config: &crate::config::Config) -> ApiResult<scion_store_s3::S3BlobStore> {
    let Some(s3) = config.s3_config() else {
        return Err(ApiError::unavailable("object storage is not configured"));
    };
    scion_store_s3::S3BlobStore::new(s3).map_err(|error| ApiError::from(&error))
}

/// `GET /repos/{repo}/blobs/export` — every external object of the repository, base64.
pub(crate) async fn export_blobs(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (auth_engine, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require_devops(
        &auth_engine,
        &caller,
        AdminCapability::Repositories,
        "exporting object bytes requires the DevOps seat (admin.repos)",
    ) {
        return error.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable().into_response();
    };
    let repo_id = match repo_id_for(pool, &repo).await {
        Ok(id) => id,
        Err(error) => return error.into_response(),
    };
    let blobs = match store_for_backup(&state.config) {
        Ok(blobs) => blobs,
        Err(error) => return error.into_response(),
    };
    let mut entries = Vec::new();
    for area in ["r", "q"] {
        let prefix = format!("{area}/{repo_id}/");
        let keys = match blobs.list(&prefix).await {
            Ok(keys) => keys,
            Err(error) => {
                return ApiError::internal(format!("listing object keys: {error}")).into_response();
            }
        };
        for key in keys {
            let bytes = match blobs.get(&key).await {
                Ok(Some(bytes)) => bytes,
                Ok(None) => continue,
                Err(error) => {
                    return ApiError::internal(format!("reading {key}: {error}")).into_response();
                }
            };
            entries.push(BlobEntry {
                key,
                data: BASE64.encode(&bytes),
            });
        }
    }
    (
        StatusCode::OK,
        Json(json!({
            "service": SERVICE,
            "status": "ok",
            "repo": repo,
            "count": entries.len(),
            "entries": entries,
        })),
    )
        .into_response()
}

/// `POST /repos/{repo}/blobs/import` — restore external object bytes for a repository.
pub(crate) async fn import_blobs(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Result<Json<BlobImportBody>, JsonRejection>,
) -> Response {
    let (auth_engine, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    if let Err(error) = require_devops(
        &auth_engine,
        &caller,
        AdminCapability::Repositories,
        "importing object bytes requires the DevOps seat (admin.repos)",
    ) {
        return error.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable().into_response();
    };
    let repo_id = match repo_id_for(pool, &repo).await {
        Ok(id) => id,
        Err(error) => return error.into_response(),
    };
    let live = format!("r/{repo_id}/");
    let quarantine = format!("q/{repo_id}/");
    for entry in &body.entries {
        if !entry.key.starts_with(&live) && !entry.key.starts_with(&quarantine) {
            return ApiError::invalid_argument(format!(
                "key {} does not belong to repository {repo}",
                entry.key
            ))
            .into_response();
        }
    }
    let blobs = match store_for_backup(&state.config) {
        Ok(blobs) => blobs,
        Err(error) => return error.into_response(),
    };
    for entry in &body.entries {
        let Ok(bytes) = BASE64.decode(entry.data.as_bytes()) else {
            return ApiError::invalid_argument(format!("entry {} is not base64", entry.key))
                .into_response();
        };
        if let Err(error) = blobs.put(&entry.key, bytes.into()).await {
            return ApiError::internal(format!("writing {}: {error}", entry.key)).into_response();
        }
    }
    (
        StatusCode::OK,
        Json(json!({
            "service": SERVICE,
            "status": "ok",
            "repo": repo,
            "imported": body.entries.len(),
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------------------------
// W5 WorkspaceService
//
// Each workspace is a disposable scion-wc materialization in a temp directory. The working copy
// talks to the engine's private Unix-socket gRPC surface; the external caller's policy is applied
// here because the internal principal is privileged.
// ---------------------------------------------------------------------------------------------

fn require_root_read(engine: &Arc<Engine>, caller: &CallerCtx) -> ApiResult<()> {
    let app = engine.context();
    let policy = app.policy.current();
    if policy.can_read(&caller.principal, &RepoPath::root()) {
        Ok(())
    } else {
        Err(ApiError::permission_denied(
            "repository read (read on /) denied",
        ))
    }
}

fn require_root_write(engine: &Arc<Engine>, caller: &CallerCtx) -> ApiResult<()> {
    let app = engine.context();
    let policy = app.policy.current();
    if policy.can_write(&caller.principal, &RepoPath::root()) {
        Ok(())
    } else {
        Err(ApiError::permission_denied(
            "repository write (write on /) denied",
        ))
    }
}

fn workspace_rev(changeset: Option<&str>, ref_name: Option<&str>) -> ApiResult<String> {
    match changeset {
        Some(raw) => match parse_id(raw, "changeset") {
            Ok(id) => Ok(id.to_text()),
            Err(message) => Err(ApiError::invalid_argument(message)),
        },
        None => Ok(ref_name.unwrap_or(DEFAULT_REF).to_owned()),
    }
}

fn parse_sparse(sparse: &[String]) -> ApiResult<Vec<RepoPath>> {
    let mut paths = Vec::with_capacity(sparse.len());
    for raw in sparse {
        match RepoPath::parse(raw.as_bytes()) {
            Ok(path) => paths.push(path),
            Err(error) => {
                return Err(ApiError::invalid_argument(format!(
                    "sparse path {raw:?}: {}",
                    error.message
                )));
            }
        }
    }
    Ok(paths)
}

/// `POST /repos/{repo}/workspaces` — materialize a ref/changeset into a disposable workspace.
pub(crate) async fn workspace_create(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Result<Json<WorkspaceCreateBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require_root_read(&engine, &caller) {
        return error.into_response();
    }
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some("/"),
        None,
    )
    .await
    {
        return error.into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let rev = match workspace_rev(body.changeset.as_deref(), body.ref_name.as_deref()) {
        Ok(rev) => rev,
        Err(error) => return error.into_response(),
    };
    let sparse = match parse_sparse(&body.sparse) {
        Ok(sparse) => sparse,
        Err(error) => return error.into_response(),
    };
    match engine.workspaces().create(&rev, sparse).await {
        Ok((workspace_id, workspace_state)) => (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "repo": repo,
                "workspace_id": workspace_id,
                "state": workspace_state,
            })),
        )
            .into_response(),
        Err(status) => ApiError::from(status).into_response(),
    }
}

/// `GET /repos/{repo}/workspaces/{id}` — status.
pub(crate) async fn workspace_status(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require_root_read(&engine, &caller) {
        return error.into_response();
    }
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some("/"),
        None,
    )
    .await
    {
        return error.into_response();
    }
    let app = engine.context();
    let policy = app.policy.current();
    let result = engine
        .workspaces()
        .status(&id, &policy, &caller.principal)
        .await;
    workspace_result(repo, result)
}

/// `POST /repos/{repo}/workspaces/{id}/materialize` — move to another ref/changeset.
pub(crate) async fn workspace_materialize(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<WorkspaceMaterializeBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require_root_read(&engine, &caller) {
        return error.into_response();
    }
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some("/"),
        None,
    )
    .await
    {
        return error.into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let rev = match workspace_rev(body.changeset.as_deref(), body.ref_name.as_deref()) {
        Ok(rev) => rev,
        Err(error) => return error.into_response(),
    };
    let result = engine
        .workspaces()
        .materialize(&id, &rev, body.force, &caller.principal)
        .await;
    workspace_result(repo, result)
}

/// `GET /repos/{repo}/workspaces/{id}/file?path=…`
pub(crate) async fn workspace_read_file(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    query: Result<Query<WorkspaceFileQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some(&query.path),
        None,
    )
    .await
    {
        return error.into_response();
    }
    let app = engine.context();
    let policy = app.policy.current();
    let result = engine
        .workspaces()
        .read_file(&id, &query.path, &policy, &caller.principal)
        .await;
    workspace_result(repo, result)
}

/// `PUT /repos/{repo}/workspaces/{id}/file?path=…`
pub(crate) async fn workspace_write_file(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    query: Result<Query<WorkspaceFileQuery>, QueryRejection>,
    headers: HeaderMap,
    body: Result<Json<WorkspaceWriteBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    let data = match BASE64.decode(body.data.as_bytes()) {
        Ok(data) => data,
        Err(error) => {
            return ApiError::invalid_argument(format!("data is not base64: {error}"))
                .into_response();
        }
    };
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Write,
        Some(&query.path),
        None,
    )
    .await
    {
        return error.into_response();
    }
    let app = engine.context();
    let policy = app.policy.current();
    let result = engine
        .workspaces()
        .write_file(
            &id,
            &query.path,
            &data,
            body.mode,
            &policy,
            &caller.principal,
        )
        .await;
    workspace_result(repo, result)
}

/// `DELETE /repos/{repo}/workspaces/{id}/file?path=…`
pub(crate) async fn workspace_delete_file(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    query: Result<Query<WorkspaceFileQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Write,
        Some(&query.path),
        None,
    )
    .await
    {
        return error.into_response();
    }
    let app = engine.context();
    let policy = app.policy.current();
    let result = engine
        .workspaces()
        .delete_file(&id, &query.path, &policy, &caller.principal)
        .await;
    workspace_result(repo, result)
}

/// `GET /repos/{repo}/workspaces/{id}/diff?context=3`
pub(crate) async fn workspace_diff(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    query: Result<Query<WorkspaceDiffQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    if let Err(error) = require_root_read(&engine, &caller) {
        return error.into_response();
    }
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Read,
        Some("/"),
        None,
    )
    .await
    {
        return error.into_response();
    }
    let app = engine.context();
    let policy = app.policy.current();
    let result = engine
        .workspaces()
        .diff(&id, query.context.min(100), &policy, &caller.principal)
        .await;
    workspace_result(repo, result)
}

/// `POST /repos/{repo}/workspaces/{id}/submit` — commit and optionally land.
pub(crate) async fn workspace_submit(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<WorkspaceSubmitBody>, JsonRejection>,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::from(rejection).into_response(),
    };
    if body.message.is_empty() {
        return ApiError::invalid_argument("message must not be empty").into_response();
    }
    if matches!(body.mode.as_str(), "land" | "update")
        && let Err(error) = require_trunk(
            &engine,
            &caller,
            body.ref_name.as_deref().unwrap_or(DEFAULT_REF),
        )
    {
        return error.into_response();
    }
    let ref_name = body.ref_name.as_deref().unwrap_or(DEFAULT_REF);
    let author = body
        .author
        .as_deref()
        .unwrap_or("ForgeVCS workspace <workspace@example.test>");
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Write,
        Some("/"),
        Some(ref_name),
    )
    .await
    {
        return error.into_response();
    }
    let app = engine.context();
    let policy = app.policy.current();
    let result = engine
        .workspaces()
        .submit(
            &id,
            crate::workspace::SubmitRequest {
                message: body.message.as_bytes(),
                author: author.as_bytes(),
                ref_name,
                mode: &body.mode,
            },
            &policy,
            &caller.principal,
        )
        .await;
    workspace_result(repo, result)
}

/// `DELETE /repos/{repo}/workspaces/{id}` — dispose of the workspace.
pub(crate) async fn workspace_destroy(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require_root_write(&engine, &caller) {
        return error.into_response();
    }
    if let Err(error) = authorize(
        &state,
        &engine,
        &caller,
        &repo,
        Action::Write,
        Some("/"),
        None,
    )
    .await
    {
        return error.into_response();
    }
    match engine.workspaces().destroy(&id, &caller.principal).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "repo": repo,
                "workspace_id": id,
                "destroyed": true,
            })),
        )
            .into_response(),
        Err(status) => workspace_error(status),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use prost::Message as _;

    #[test]
    fn repo_names_reject_path_shaped_input() {
        for name in ["forgevcs", "team-1", "a.b_c"] {
            assert!(valid_repo_name(name), "{name}");
        }
        for name in ["", ".hidden", "a..b", "Upper", "a/b", &"x".repeat(101)] {
            assert!(!valid_repo_name(name), "{name}");
        }
    }

    #[test]
    fn object_ids_use_the_scion_text_form() {
        let id = ObjectId::hash_blob(b"hello\n");
        assert_eq!(parse_id(&id.to_text(), "id").unwrap(), id);
        assert!(parse_id("not-an-id", "id").is_err());
    }

    #[test]
    fn change_id_is_exactly_16_bytes() {
        assert_eq!(parse_change_id(&"ab".repeat(16)).unwrap().len(), 16);
        assert!(parse_change_id(&"ab".repeat(15)).is_err());
        assert!(parse_change_id("zz").is_err());
    }

    #[test]
    fn a_push_body_becomes_the_same_proto_a_grpc_client_sends() {
        let blob = ObjectId::hash_blob(b"hello\n");
        let body: PushBody = serde_json::from_value(json!({
            "objects": [{
                "id": blob.to_text(),
                "data": BASE64.encode(b"hello\n"),
            }],
            "changeset": {
                "parents": [],
                "delta": [{
                    "path": "hello.txt",
                    "id": blob.to_text(),
                    "size": 6,
                }],
                "meta": {
                    "author": "Maestro <maestro@example.test>",
                    "message": "add hello",
                },
                "change_id": "00112233445566778899aabbccddeeff",
            },
            "ref": "main",
        }))
        .expect("body parses");

        assert_eq!(body.mode, "land");
        let upload = body
            .objects
            .into_iter()
            .next()
            .unwrap()
            .into_proto()
            .unwrap();
        assert_eq!(upload.id, blob.as_bytes());
        assert_eq!(upload.kind, u32::from(blob.kind().byte()));
        assert_eq!(upload.data, b"hello\n");

        let changeset = body.changeset.into_proto().unwrap();
        assert_eq!(changeset.parents.len(), 0);
        assert_eq!(changeset.delta[0].path, b"hello.txt");
        assert_eq!(changeset.delta[0].op, DELTA_PUT);
        assert_eq!(changeset.delta[0].mode, 1);
        assert_eq!(changeset.delta[0].id, blob.as_bytes());
        assert_eq!(
            changeset.change_id,
            hex::decode("00112233445566778899aabbccddeeff").unwrap()
        );
        assert_eq!(
            changeset.meta_fields.as_ref().unwrap().message,
            b"add hello"
        );
    }

    #[test]
    fn a_put_without_an_object_id_is_rejected_before_the_handler() {
        let delta = DeltaBody {
            path: "a".to_owned(),
            op: 1,
            mode: 1,
            id: String::new(),
            size: 1,
        };
        assert!(delta.into_proto().is_err());
    }

    #[test]
    fn upload_frames_are_readable_grpc_messages() {
        let id = ObjectId::hash_blob(b"x");
        let item = v1::UploadItem {
            id: id.as_bytes().to_vec(),
            kind: u32::from(id.kind().byte()),
            data: b"x".to_vec(),
        };
        let framed = grpc_frames(std::slice::from_ref(&item));
        assert_eq!(framed[0], 0, "uncompressed flag");
        let length = u32::from_be_bytes([framed[1], framed[2], framed[3], framed[4]]) as usize;
        assert_eq!(length, framed.len() - 5);
        let decoded = v1::UploadItem::decode(&framed[5..]).unwrap();
        assert_eq!(decoded.id, item.id);
        assert_eq!(decoded.data, item.data);
    }

    #[test]
    fn statuses_and_encodings_have_stable_names() {
        assert_eq!(item_status(v1::ItemStatus::Ok as i32), "ok");
        assert_eq!(item_status(v1::ItemStatus::Redacted as i32), "redacted");
        assert_eq!(item_status(0), "unspecified");
        assert_eq!(encoding_name(v1::Encoding::Zstd as i32), "zstd");
        assert_eq!(encoding_name(0), "unspecified");
    }
    fn paused_record(status: &str, ref_version: Option<i64>) -> Record {
        Record {
            id: "conflict-id".to_owned(),
            repo: "repo".to_owned(),
            operation: "land".to_owned(),
            status: status.to_owned(),
            ref_name: Some("main".to_owned()),
            ref_version,
            ref_target: Some("c1tip".to_owned()),
            head: "c1head".to_owned(),
            resolve_parent: "c1head".to_owned(),
            base: None,
            ours: "c1tip".to_owned(),
            theirs: "c1head".to_owned(),
            report: serde_json::json!({}),
            sides: serde_json::json!({}),
            created_by: "github:culebraluxe".to_owned(),
            created_at: 1,
            resolved_changeset: Some("c1resolved".to_owned()),
            resolved_by: Some("github:culebraluxe".to_owned()),
            resolved_at: Some(2),
            continued_target: None,
            continued_version: None,
            continued_at: None,
        }
    }

    #[test]
    fn pause_shapes_parent_every_resolution_on_a_landable_changeset() {
        let tip = ObjectId::hash_blob(b"tip");
        let head = ObjectId::hash_blob(b"head");
        let other = ObjectId::hash_blob(b"other");

        // push/land/merge-land: parent on the ref tip, never on the paused head.
        for operation in ["push", "land", "merge-land"] {
            let pause = land_pause(operation, "main".to_owned(), head, tip, head, None);
            assert_eq!(pause.resolve_parent, tip, "{operation}");
            assert_eq!(pause.ours, tip, "{operation}");
            assert_eq!(pause.head, head, "{operation}");
            assert_eq!(pause.theirs, head, "{operation}");
            assert_ne!(pause.resolve_parent, pause.head, "{operation}");
        }
        // merge: parent on `a`; rebase: parent on `onto` (the rebase target / ref tip).
        assert_eq!(merge_pause(tip, head, None).resolve_parent, tip);
        assert_eq!(rebase_pause(head, tip, None).resolve_parent, tip);
        assert_eq!(rebase_pause(head, tip, None).head, head);
        assert_eq!(rebase_pause(head, tip, None).theirs, head);
        assert_eq!(rebase_pause(other, tip, None).ours, tip);
    }

    #[tokio::test]
    async fn a_continue_land_conflict_keeps_the_paused_record() {
        use scion_core::{ErrorCode, ErrorPayload};
        let error =
            scion_core::ScionError::new(ErrorCode::MergeConflict, "rebase: 1 conflicting path(s)")
                .with_payload(ErrorPayload::MergeConflict(Box::default()));
        let api = continue_land_failure(
            status_from_error(&error),
            &paused_record("resolved", Some(7)),
        );
        let response = api.into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["code"], "conflict");
        assert_eq!(body["details"]["kind"], "merge_conflict");
        assert_eq!(body["conflict"]["id"], "conflict-id");
    }

    #[test]
    fn malformed_conflict_ids_are_a_400_before_any_database_cast() {
        assert!(check_conflict_id("not-a-uuid").is_err());
        assert!(check_conflict_id("").is_err());
        let valid = uuid::Uuid::new_v4().to_string();
        assert!(check_conflict_id(&valid).is_ok());
    }

    #[test]
    fn repository_deletion_cleans_the_paused_conflict_rows() {
        // A deleted repository must not leave paused conflicts behind (QA residue finding).
        assert!(
            SERVICE_REPO_TABLES.contains(&"forgevcs_paused_conflicts"),
            "delete_repository_rows must remove paused conflicts: {SERVICE_REPO_TABLES:?}"
        );
    }

    #[test]
    fn a_moved_ref_is_stale_and_an_unknown_version_is_not_provable() {
        let paused = paused_record("resolved", Some(7));
        assert!(stale_ref(&paused, 8), "a newer ref version is stale");
        assert!(!stale_ref(&paused, 7), "the paused version is current");
        let unknown = paused_record("resolved", None);
        assert!(
            !stale_ref(&unknown, 8),
            "an unrecorded version cannot be proven stale"
        );
    }

    #[test]
    fn the_resolved_changeset_must_be_the_single_paused_child() {
        let expected = ObjectId::hash_blob(b"head");
        let other = ObjectId::hash_blob(b"other");
        assert!(valid_resolution_parent(&[expected], &expected.to_text()));
        assert!(!valid_resolution_parent(&[], &expected.to_text()));
        assert!(!valid_resolution_parent(
            &[expected, other],
            &expected.to_text()
        ));
        assert!(!valid_resolution_parent(&[other], &expected.to_text()));
    }

    /// FVCS-02: the push façade feeds every changeset delta path into the authorizer. A regression
    /// that drops the paths would silently fall back to the coarse write-on-`/` gate this work
    /// removed, so the extraction is asserted directly.
    #[test]
    fn a_push_authorizes_every_changeset_delta_path() {
        let body: PushBody = serde_json::from_value(json!({
            "changeset": {
                "delta": [
                    {"path": "lane/a/one.rs"},
                    {"path": "lane/a/two.rs", "op": 2}
                ],
                "meta": {"author": "A <a@example.com>", "message": "two paths"},
                "change_id": "ab".repeat(16)
            }
        }))
        .expect("push body parses");
        assert_eq!(
            push_delta_paths(&body.changeset),
            vec!["lane/a/one.rs".to_owned(), "lane/a/two.rs".to_owned()]
        );
    }

    /// Lane Z: the request `topology` maps onto the two topologies; the deployment default is
    /// used only when the request omits the field, and a typo is a 400 — never a silent fallback
    /// to the other topology.
    #[test]
    fn merge_land_topology_request_maps_or_rejects() {
        use crate::config::{Config, MERGE_TOPOLOGY_MERGE_COMMIT, MERGE_TOPOLOGY_SQUASH_ONLY};

        let squash_default = Config::from_lookup(|_| None);
        assert_eq!(
            resolve_merge_topology(&squash_default, None).unwrap(),
            MERGE_TOPOLOGY_SQUASH_ONLY
        );
        assert_eq!(
            resolve_merge_topology(&squash_default, Some("")).unwrap(),
            MERGE_TOPOLOGY_SQUASH_ONLY
        );
        assert_eq!(
            resolve_merge_topology(&squash_default, Some("squash")).unwrap(),
            MERGE_TOPOLOGY_SQUASH_ONLY
        );
        assert_eq!(
            resolve_merge_topology(&squash_default, Some("merge-commit")).unwrap(),
            MERGE_TOPOLOGY_MERGE_COMMIT
        );
        assert_eq!(
            resolve_merge_topology(&squash_default, Some("merge-commit-v1")).unwrap(),
            MERGE_TOPOLOGY_MERGE_COMMIT
        );

        let merge_commit_default = Config::from_lookup(|key| {
            (key == "FORGEVCS_MERGE_TOPOLOGY").then(|| "merge-commit-v1".to_owned())
        });
        assert_eq!(
            resolve_merge_topology(&merge_commit_default, None).unwrap(),
            MERGE_TOPOLOGY_MERGE_COMMIT
        );
        assert_eq!(
            resolve_merge_topology(&merge_commit_default, Some("squash")).unwrap(),
            MERGE_TOPOLOGY_SQUASH_ONLY
        );

        for bad in ["merge", "dag", "merge_commit", "SQUASH"] {
            assert!(
                matches!(
                    resolve_merge_topology(&squash_default, Some(bad)),
                    Err(ApiError::InvalidArgument(_))
                ),
                "{bad} must be refused"
            );
        }
    }
}
