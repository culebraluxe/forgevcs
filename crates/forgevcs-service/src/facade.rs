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
//! - one place that turns a `tonic::Status` into the service's error envelope, shared with the
//!   probes and auth endpoints;
//! - a gRPC-framed in-memory stream for `UploadObjects`, whose handler takes
//!   `Streaming<UploadItem>` rather than a vector. The handler itself is unchanged; only the
//!   wire framing that tonic normally supplies is built here.
//!
//! `POST /repos/{repo}/refs` would be more RESTful as a list, but there is no upstream
//! `ListRefs` RPC. `GET /refs` therefore reads `MetaStore::list_refs` directly after the same
//! `read` on `/` check `GetRefLog` would perform. It is the one read that does not go through a
//! handler, and it is called out in `docs/W4-engine-wiring.md` §8.

// A façade helper returns the HTTP response it would send on failure. Boxing that response
// would only move the allocation to every call site; `axum::Response` is the natural error type
// for these functions.
#![allow(clippy::result_large_err)]

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
use prost::Message as _;
use scion_core::{ConflictReport, EntryInfo, ErrorPayload, ObjectId};
use scion_object::{Canonical, RepoPath};
use scion_proto::{convert, error_from_status, status_from_error, v1};
use scion_server::AppContext;
use scion_server::context::CallerCtx;
use scion_server::handlers;
use scion_store::BlobStore;
use serde::Deserialize;
use serde_json::{Value, json};
use tonic::Status;

use crate::authz::Action;
use crate::repo_acl::ClaimOutcome;

use crate::engine::Engine;
use crate::http::{
    AppState, SERVICE, authenticate, caller_request, error_response, status_response,
};

/// Default ref the push/pull/commits endpoints address.
const DEFAULT_REF: &str = "main";
/// Default page size of `commits`/`pull` (upstream's own `Log` default).
const DEFAULT_LIMIT: u32 = 100;
/// Largest page this façade forwards; upstream clamps to the same value.
const MAX_LIMIT: u32 = 1000;
/// Delta op for a Put (`1`).
const DELTA_PUT: u32 = 1;

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// `POST /repos/{repo}/init` creates a repository, so the name becomes a path segment, a
/// `repos.name` row and a log field. Lowercase keeps it URL- and policy-predictable; the
/// dot rules keep a name from becoming a path traversal if a later layer ever uses it as one.
fn valid_repo_name(name: &str) -> bool {
    crate::repo_acl::valid_repo_name(name)
}

/// The engine for an addressed repository, or 404. Reads never create a repository: a typo in
/// a path must not mint one.
async fn repo_engine(state: &AppState, repo: &str) -> Result<Arc<Engine>, Response> {
    if let Some(engine) = state.engine_for(repo) {
        return Ok(engine);
    }
    // The process may not have booted this repository yet (a different Vercel instance, or a
    // restart). Boot it only if the `repos` row already exists, so a typo still cannot create a
    // repository as a side effect of a read.
    let Some(db) = &state.db else {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "repository registry is unavailable",
        ));
    };
    let client = db.get().await.map_err(|error| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            &format!("repository registry connection: {error}"),
        )
    })?;
    let exists = client
        .query_opt("SELECT 1 FROM repos WHERE name = $1", &[&repo])
        .await
        .map_err(|error| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                &format!("repository registry query: {error}"),
            )
        })?;
    if exists.is_none() {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("repository {repo:?} does not exist; POST /repos/{repo}/init first"),
        ));
    }
    state
        .engines
        .get_or_boot(&state.config, repo)
        .await
        .map_err(|error| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                &format!("booting repository {repo:?}: {error:#}"),
            )
        })
}

fn bad_json(rejection: JsonRejection) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "invalid_argument",
        &rejection.body_text(),
    )
}

fn bad_query(rejection: QueryRejection) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "invalid_argument",
        &rejection.body_text(),
    )
}

/// The JSON name of a `scion.v1` item status.
fn item_status(status: i32) -> &'static str {
    match status {
        x if x == v1::ItemStatus::Ok as i32 => "ok",
        x if x == v1::ItemStatus::NotFound as i32 => "not_found",
        x if x == v1::ItemStatus::Denied as i32 => "denied",
        x if x == v1::ItemStatus::Redacted as i32 => "redacted",
        _ => "unspecified",
    }
}

/// The JSON name of a `scion.v1` payload encoding.
fn encoding_name(encoding: i32) -> &'static str {
    match encoding {
        x if x == v1::Encoding::Raw as i32 => "raw",
        x if x == v1::Encoding::Zstd as i32 => "zstd",
        _ => "unspecified",
    }
}

fn timestamp_json(timestamp: Option<&v1::Timestamp>) -> Value {
    timestamp.map_or(Value::Null, |t| {
        json!({
            "seconds": t.seconds,
            "tz_offset_minutes": t.tz_offset_minutes,
        })
    })
}

fn bytes_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn id_text(bytes: &[u8]) -> String {
    ObjectId::from_bytes(bytes).map_or_else(|_| hex::encode(bytes), |id| id.to_text())
}

fn parse_id(raw: &str, field: &str) -> Result<ObjectId, String> {
    ObjectId::parse_text(raw).map_err(|error| format!("{field}: {}", error.message))
}

fn parse_change_id(raw: &str) -> Result<Vec<u8>, String> {
    let bytes = hex::decode(raw).map_err(|error| format!("change_id is not hex: {error}"))?;
    if bytes.len() != 16 {
        return Err(format!(
            "change_id must be 16 bytes (32 hex digits), got {} bytes",
            bytes.len()
        ));
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------------------------
// POST /repos/{repo}/init
// ---------------------------------------------------------------------------------------------

/// `POST /repos/{repo}/init` — create the repository if this process has not already booted it.
///
/// `Engine::connect` creates the `repos` row and upstream's `ServerApp::build` bootstraps the
/// root changeset and ref `main`, so this is a real initialization rather than a row insert.
/// Idempotent: a second call returns `created:false` with the same refs.
///
/// Authorization is `write` on `/` under the current policy. There is no dedicated
/// `admin.repos` permission yet (ADR-0002 D6), and the check runs on the verifier engine's
/// policy because the target engine does not exist until after it.
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
) -> Result<(), Response> {
    let app = engine.context();
    let policy = app.policy.current();
    match state
        .authz
        .authorize(&policy, &caller.principal, repo, action, path, ref_name)
        .await
    {
        Ok(decision) if decision.allow => Ok(()),
        Ok(decision) => Err(status_response(Status::permission_denied(decision.reason))),
        Err(status) => Err(status_response(status)),
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
) -> Result<(), Response> {
    let app = engine.context();
    let policy = app.policy.current();
    match state
        .authz
        .authorize_provisioning(&policy, &caller.principal, repo, action, Some("/"), None)
        .await
    {
        Ok(decision) if decision.allow => Ok(()),
        Ok(decision) => Err(status_response(Status::permission_denied(decision.reason))),
        Err(status) => Err(status_response(status)),
    }
}

pub(crate) async fn init(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !valid_repo_name(&repo) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "repository names must be 1-100 lowercase [a-z0-9._-] characters, not start with \
             '.', and not contain '..'",
        );
    }
    let (auth_engine, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(response) =
        authorize_provisioning(&state, &auth_engine, &caller, &repo, Action::Write).await
    {
        return response;
    }
    let app = auth_engine.context();
    if !app
        .policy
        .current()
        .can_write(&caller.principal, &RepoPath::root())
    {
        return error_response(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "creating a repository requires write on /",
        );
    }

    let created = state.engine_for(&repo).is_none();
    // Repository lifecycle is a DevOps seat, not a worker permission: workers check in, read
    // and merge-base, but creating (and, when implemented, deleting) a repository needs the
    // single `admin.refs` seat. `admin.refs` is the closest existing upstream permission to a
    // DevOps capability; a dedicated `admin.repos` permit would be an engine-version change.
    if created
        && !app
            .policy
            .current()
            .can_admin(&caller.principal, scion_policy::AdminPerm::Repos)
    {
        return error_response(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "creating a repository requires the DevOps seat (admin.repos)",
        );
    }
    let engine = if created {
        match state.engines.get_or_boot(&state.config, &repo).await {
            Ok(engine) => engine,
            Err(error) => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "unavailable",
                    &format!("{error:#}"),
                );
            }
        }
    } else {
        match state.engine_for(&repo) {
            Some(engine) => engine,
            None => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "engine vanished",
                );
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
                .can_admin(&caller.principal, scion_policy::AdminPerm::Repos)
            {
                return error_response(
                    StatusCode::FORBIDDEN,
                    "permission_denied",
                    &format!("repository {repo:?} is owned by {owner}"),
                );
            }
        }
        Err(status) => return status_response(status),
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
        Err(response) => response,
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
async fn list_refs(engine: &Arc<Engine>, caller: &CallerCtx) -> Result<Vec<Value>, Response> {
    let app = engine.context();
    if !app
        .policy
        .current()
        .can_read(&caller.principal, &RepoPath::root())
    {
        return Err(status_response(Status::permission_denied(
            "repository read (read on /) denied",
        )));
    }
    let rows = app
        .storage
        .meta
        .list_refs()
        .await
        .map_err(|error| status_response(status_from_error(&error)))?;
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    if let Err(response) = authorize(
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
        return response;
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
        Err(response) => response,
    }
}

// ---------------------------------------------------------------------------------------------
// Resolve + Log, shared by GET /commits and POST /pull
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct CommitsQuery {
    #[serde(rename = "ref", default = "default_ref")]
    ref_name: String,
    #[serde(default = "default_limit")]
    limit: u32,
    path: Option<String>,
    #[serde(default)]
    first_parent_only: bool,
    /// Opaque continuation from a previous `next_page_token`, hex-encoded.
    page_token: Option<String>,
}

fn default_ref() -> String {
    DEFAULT_REF.to_owned()
}

fn default_limit() -> u32 {
    DEFAULT_LIMIT
}

/// Resolve `ref_name` and return the first page of its log.
async fn resolve_and_log(
    engine: &Arc<Engine>,
    caller: &CallerCtx,
    ref_name: &str,
    limit: u32,
    path: Option<&str>,
    first_parent_only: bool,
    page_token: Option<&str>,
) -> Result<(String, u64, Vec<Value>, Option<String>), Response> {
    let app = engine.context();
    let page_token = match page_token {
        Some(raw) if !raw.is_empty() => hex::decode(raw).map_err(|error| {
            error_response(
                StatusCode::BAD_REQUEST,
                "invalid_argument",
                &format!("page_token is not hex: {error}"),
            )
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
    .map_err(status_response)?
    .into_inner();
    let target = ObjectId::from_bytes(&resolved.target)
        .map_err(|error| status_response(status_from_error(&error)))?;
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
    .map_err(status_response)?
    .into_inner();
    let commits = log_entries(&log.entries)?;
    let next = (!log.next_page_token.is_empty()).then(|| hex::encode(&log.next_page_token));
    Ok((target.to_text(), resolved.version, commits, next))
}

/// One `Log` entry as JSON. The id, parents and change id come from the canonical changeset
/// when the entry is unfiltered; a filtered view already carries the readable fields.
fn log_entries(entries: &[v1::LogEntry]) -> Result<Vec<Value>, Response> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(item) = &entry.changeset else {
            continue;
        };
        let (parents, root_tree, change_id, filtered) = match &item.view {
            Some(v1::changeset_item::View::Encoded(bytes)) => {
                let changeset = scion_object::Changeset::decode(bytes)
                    .map_err(|error| status_response(status_from_error(&error)))?;
                (
                    changeset
                        .parents
                        .iter()
                        .map(ObjectId::to_text)
                        .collect::<Vec<_>>(),
                    Some(changeset.root_tree.to_text()),
                    Some(hex::encode(changeset.change_id.0)),
                    false,
                )
            }
            Some(v1::changeset_item::View::FilteredView(view)) => (
                view.parents.iter().map(|raw| id_text(raw)).collect(),
                (!view.root_tree.is_empty()).then(|| id_text(&view.root_tree)),
                (!view.change_id.is_empty()).then(|| hex::encode(&view.change_id)),
                view.filtered,
            ),
            None => (Vec::new(), None, None, false),
        };
        let meta = entry.meta.as_ref().map_or(Value::Null, |meta| {
            let author = (!meta.redacted).then(|| bytes_text(&meta.author));
            let committer = if meta.redacted {
                None
            } else {
                meta.committer.as_ref().map(|c| bytes_text(c))
            };
            let message = (!meta.redacted).then(|| bytes_text(&meta.message));
            json!({
                "redacted": meta.redacted,
                "author": author,
                "author_time": timestamp_json(meta.author_time.as_ref()),
                "committer": committer,
                "committer_time": timestamp_json(meta.committer_time.as_ref()),
                "message": message,
                "meta_id": id_text(&meta.meta_id),
            })
        });
        out.push(json!({
            "id": id_text(&item.id),
            "parents": parents,
            "root_tree": root_tree,
            "change_id": change_id,
            "filtered": filtered,
            "meta": meta,
        }));
    }
    Ok(out)
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return bad_query(rejection),
    };
    if let Err(response) = authorize(
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
        return response;
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
        Err(response) => response,
    }
}

// ---------------------------------------------------------------------------------------------
// POST /repos/{repo}/pull
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct PullBody {
    #[serde(rename = "ref", default = "default_ref")]
    ref_name: String,
    #[serde(default = "default_limit")]
    limit: u32,
    path: Option<String>,
    #[serde(default)]
    first_parent_only: bool,
    /// Opaque continuation from a previous `next_page_token`, hex-encoded.
    page_token: Option<String>,
    /// Object ids whose bytes should accompany the history. Empty means metadata only.
    #[serde(default)]
    objects: Vec<String>,
}

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
) -> Result<Vec<Value>, Response> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut parsed = Vec::with_capacity(ids.len());
    for raw in ids {
        parsed.push(parse_id(raw, "objects[]").map_err(|message| {
            error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message)
        })?);
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
    .map_err(status_response)?
    .into_inner();

    let mut chunks: BTreeMap<Vec<u8>, ObjectAccum> = BTreeMap::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(status_response)?;
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    if let Err(response) = authorize(
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
        return response;
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
        Err(response) => return response,
    };
    let objects = match fetch_objects(&engine, &caller, &body.objects).await {
        Ok(objects) => objects,
        Err(response) => return response,
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

fn delta_json(delta: &[v1::DeltaEntryMsg]) -> Vec<Value> {
    delta
        .iter()
        .map(|entry| {
            json!({
                "path": bytes_text(&entry.path),
                "op": entry.op,
                "mode": entry.mode,
                "id": (!entry.id.is_empty()).then(|| id_text(&entry.id)),
                "size": entry.size,
            })
        })
        .collect()
}

fn entry_json(entry: Option<&v1::EntryInfo>) -> Value {
    entry.map_or(Value::Null, |entry| {
        json!({
            "mode": entry.mode,
            "id": id_text(&entry.id),
            "size": entry.size,
        })
    })
}

/// One `ChangesetItem` as JSON. Encoded items are decoded to their canonical fields; a filtered
/// view already carries only the caller-readable delta entries.
fn changeset_json(item: &v1::ChangesetItem) -> Result<Value, Response> {
    let id = id_text(&item.id);
    match &item.view {
        Some(v1::changeset_item::View::Encoded(bytes)) => {
            let changeset = scion_object::Changeset::decode(bytes)
                .map_err(|error| status_response(status_from_error(&error)))?;
            Ok(json!({
                "id": id,
                "parents": changeset.parents.iter().map(ObjectId::to_text).collect::<Vec<_>>(),
                "root_tree": changeset.root_tree.to_text(),
                "change_id": hex::encode(changeset.change_id.0),
                "filtered": false,
                "delta": delta_json(&convert::delta_to_proto(&changeset.delta)),
            }))
        }
        Some(v1::changeset_item::View::FilteredView(view)) => Ok(json!({
            "id": id,
            "parents": view.parents.iter().map(|raw| id_text(raw)).collect::<Vec<_>>(),
            "root_tree": id_text(&view.root_tree),
            "change_id": hex::encode(&view.change_id),
            "filtered": view.filtered,
            "delta": delta_json(&view.delta),
        })),
        None => Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "changeset item has no view",
        )),
    }
}

fn diff_item_json(item: &v1::DiffItem) -> Value {
    json!({
        "path": bytes_text(&item.path),
        "old": entry_json(item.old_entry.as_ref()),
        "new": entry_json(item.new_entry.as_ref()),
        "binary": item.binary,
        "hunks": item.hunks.iter().map(|hunk| json!({
            "old_start": hunk.old_start,
            "old_lines": hunk.old_lines,
            "new_start": hunk.new_start,
            "new_lines": hunk.new_lines,
            "lines": hunk.lines.iter().map(|line| bytes_text(line)).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// `GET /repos/{repo}/changesets/{id}` — one changeset, decoded.
pub(crate) async fn changeset(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let id = match parse_id(&id, "changeset") {
        Ok(id) => id,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
        }
    };
    if let Err(response) = authorize(
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
        return response;
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
        Err(status) => return status_response(status),
    };
    let Some(item) = response.items.first() else {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("changeset {id} not found"),
        );
    };
    if item.status != v1::ItemStatus::Ok as i32 {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("changeset {id} not found"),
        );
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
        Err(response) => response,
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct FileQuery {
    changeset: String,
    #[serde(default)]
    path: String,
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return bad_query(rejection),
    };
    let changeset = match parse_id(&query.changeset, "changeset") {
        Ok(id) => id,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
        }
    };
    let path = query.path;
    if let Err(response) = authorize(
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
        return response;
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
        Err(status) => return status_response(status),
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
            Err(status) => return status_response(status),
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

#[derive(Debug, Deserialize)]
pub(crate) struct TreeQuery {
    changeset: String,
    #[serde(default)]
    path: String,
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return bad_query(rejection),
    };
    if let Err(response) = authorize(
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
        return response;
    }
    let changeset = match parse_id(&query.changeset, "changeset") {
        Ok(id) => id,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
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
        Err(status) => return status_response(status),
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

#[derive(Debug, Deserialize)]
pub(crate) struct DiffQuery {
    a: Option<String>,
    b: String,
    /// Comma-separated paths; empty/absent means the whole tree.
    paths: Option<String>,
    #[serde(default = "default_true")]
    include_hunks: bool,
}

fn default_true() -> bool {
    true
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return bad_query(rejection),
    };
    let a = match query.a.as_deref() {
        Some(raw) if !raw.is_empty() => match parse_id(raw, "a") {
            Ok(id) => Some(id),
            Err(message) => {
                return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
            }
        },
        _ => None,
    };
    let b = match parse_id(&query.b, "b") {
        Ok(id) => id,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
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
    if let Err(response) = authorize(
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
        return response;
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
        Err(status) => return status_response(status),
    };
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(item) => items.push(diff_item_json(&item)),
            Err(status) => return status_response(status),
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

#[derive(Debug, Deserialize)]
pub(crate) struct MergeBaseQuery {
    a: String,
    b: String,
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return bad_query(rejection),
    };
    let a = match parse_id(&query.a, "a") {
        Ok(id) => id,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
        }
    };
    let b = match parse_id(&query.b, "b") {
        Ok(id) => id,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
        }
    };
    if let Err(response) = authorize(
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
        return response;
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
        Err(status) => return status_response(status),
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

#[derive(Debug, Deserialize)]
pub(crate) struct ObjectUpload {
    id: String,
    /// `0x01` blob or `0x02` tree. Defaults to the kind encoded in the id.
    kind: Option<u32>,
    /// Base64 of the raw bytes (a blob's content or a tree's canonical encoding).
    data: String,
}

impl ObjectUpload {
    fn into_proto(self) -> Result<v1::UploadItem, String> {
        let id = parse_id(&self.id, "objects[].id")?;
        let kind = self.kind.unwrap_or(u32::from(id.kind().byte()));
        let data = BASE64
            .decode(self.data.as_bytes())
            .map_err(|error| format!("objects[{}]: data is not base64: {error}", self.id))?;
        Ok(v1::UploadItem {
            id: id.as_bytes().to_vec(),
            kind,
            data,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TimeBody {
    #[serde(default)]
    seconds: i64,
    #[serde(default)]
    tz_offset_minutes: i64,
}

impl TimeBody {
    fn into_proto(self) -> v1::Timestamp {
        v1::Timestamp {
            seconds: self.seconds,
            tz_offset_minutes: self.tz_offset_minutes,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ExtensionBody {
    tag: u64,
    /// The extension value as UTF-8 bytes. Binary extensions are not part of the v1 façade.
    value: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MetaBody {
    /// `author <email>` in UTF-8, matching upstream's bytes field.
    author: String,
    author_time: Option<TimeBody>,
    committer: Option<String>,
    committer_time: Option<TimeBody>,
    message: String,
    #[serde(default)]
    extensions: Vec<ExtensionBody>,
}

impl MetaBody {
    fn into_proto(self) -> v1::MetaFields {
        v1::MetaFields {
            author: self.author.into_bytes(),
            author_time: Some(self.author_time.unwrap_or_default().into_proto()),
            committer: self.committer.map(String::into_bytes),
            committer_time: self.committer_time.map(TimeBody::into_proto),
            message: self.message.into_bytes(),
            extensions: self
                .extensions
                .into_iter()
                .map(|extension| v1::Extension {
                    tag: extension.tag,
                    value: extension.value.into_bytes(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeltaBody {
    /// Repository path, UTF-8 (SPEC 4.3.4; paths are sorted ascending by the caller).
    path: String,
    /// `1` Put, `2` Delete.
    #[serde(default = "put_op")]
    op: u32,
    /// `1` File, `2` Executable, `3` Symlink for a Put.
    #[serde(default = "file_mode")]
    mode: u32,
    /// Blob id for a Put; empty for a Delete.
    #[serde(default)]
    id: String,
    #[serde(default)]
    size: u64,
}

fn put_op() -> u32 {
    DELTA_PUT
}

fn file_mode() -> u32 {
    1
}

impl DeltaBody {
    fn into_proto(self) -> Result<v1::DeltaEntryMsg, String> {
        let id = if self.id.is_empty() {
            Vec::new()
        } else {
            parse_id(&self.id, "changeset.delta[].id")?
                .as_bytes()
                .to_vec()
        };
        if self.op == DELTA_PUT && id.is_empty() {
            return Err("changeset.delta[].id is required for a put".to_owned());
        }
        Ok(v1::DeltaEntryMsg {
            path: self.path.into_bytes(),
            op: self.op,
            mode: self.mode,
            id,
            size: self.size,
        })
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChangesetBody {
    #[serde(default)]
    parents: Vec<String>,
    #[serde(default)]
    delta: Vec<DeltaBody>,
    meta: MetaBody,
    /// 16 bytes as 32 hex digits.
    change_id: String,
    expected_root_tree: Option<String>,
    #[serde(default)]
    import_mode: bool,
}

impl ChangesetBody {
    fn into_proto(self) -> Result<v1::CreateChangesetRequest, String> {
        let parents = self
            .parents
            .iter()
            .map(|raw| parse_id(raw, "changeset.parents[]"))
            .collect::<Result<Vec<_>, _>>()?;
        let delta = self
            .delta
            .into_iter()
            .map(DeltaBody::into_proto)
            .collect::<Result<Vec<_>, _>>()?;
        let expected_root_tree = self
            .expected_root_tree
            .as_deref()
            .map(|raw| parse_id(raw, "changeset.expected_root_tree"))
            .transpose()?
            .map(|id| id.as_bytes().to_vec());
        Ok(v1::CreateChangesetRequest {
            parents: parents.iter().map(convert::id_bytes).collect(),
            delta,
            meta_fields: Some(self.meta.into_proto()),
            change_id: parse_change_id(&self.change_id)?,
            expected_root_tree,
            import_mode: self.import_mode,
        })
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct PushBody {
    #[serde(default)]
    objects: Vec<ObjectUpload>,
    changeset: ChangesetBody,
    #[serde(rename = "ref", default = "default_ref")]
    ref_name: String,
    /// `land` (default), `update` or `none`.
    #[serde(default = "default_push_mode")]
    mode: String,
    /// For `update`: the ref version the caller observed.
    #[serde(default)]
    expected_version: u64,
    #[serde(default)]
    allow_non_fast_forward: bool,
    /// For `land`: tolerate an EMPTY_CHANGE rebase result.
    #[serde(default)]
    allow_empty: bool,
}

fn default_push_mode() -> String {
    "land".to_owned()
}

/// `POST /repos/{repo}/refs` — create a ref at an existing changeset.
#[derive(Debug, Deserialize)]
pub(crate) struct RefCreateBody {
    name: String,
    target: String,
    #[serde(default)]
    reason: String,
}

/// `POST /repos/{repo}/rebase` — rebase one changeset onto another.
#[derive(Debug, Deserialize)]
pub(crate) struct RebaseBody {
    changeset: String,
    onto: String,
}

/// `POST /repos/{repo}/merge` — produce a merge changeset from two changesets.
#[derive(Debug, Deserialize)]
pub(crate) struct MergeBody {
    a: String,
    b: String,
    meta: MetaBody,
    change_id: String,
}

/// `POST /repos/{repo}/land` — publish an existing changeset on a ref.
#[derive(Debug, Deserialize)]
pub(crate) struct LandBody {
    #[serde(rename = "ref", default = "default_ref")]
    ref_name: String,
    head: String,
    #[serde(default)]
    allow_empty: bool,
}

/// `POST /repos/{repo}/merge-land` — merge a head into the ref tip and land the result.
#[derive(Debug, Deserialize)]
pub(crate) struct MergeLandBody {
    #[serde(rename = "ref", default = "default_ref")]
    ref_name: String,
    head: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    change_id: Option<String>,
}

/// `POST /repos/{repo}/conflict-context` — the three readable sides of a refused merge/rebase.
#[derive(Debug, Deserialize)]
pub(crate) struct ConflictContextBody {
    #[serde(default)]
    a: Option<String>,
    #[serde(default)]
    b: Option<String>,
    #[serde(default)]
    changeset: Option<String>,
    #[serde(default)]
    onto: Option<String>,
}

/// `POST /repos/{repo}/conflicts/escalate` — file a stuck merge/rebase for the DevOps seat.
#[derive(Debug, Deserialize)]
pub(crate) struct EscalateBody {
    #[serde(rename = "ref", default = "default_ref")]
    ref_name: String,
    head: String,
    #[serde(default)]
    onto: Option<String>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    report: Option<Value>,
}

/// `GET /repos/{repo}/conflicts?status=…`
#[derive(Debug, Deserialize)]
pub(crate) struct ConflictQueueQuery {
    #[serde(default)]
    status: Option<String>,
}

/// `POST /repos/{repo}/conflicts/{id}/resolve`
#[derive(Debug, Deserialize)]
pub(crate) struct ResolveEscalationBody {
    #[serde(default)]
    resolution: Option<String>,
}

/// `DELETE /repos/{repo}` — the body must repeat the repository name.
#[derive(Debug, Deserialize)]
pub(crate) struct DeleteRepoBody {
    confirm: String,
}

/// `DELETE /repos/{repo}/conflicts?status=…`
#[derive(Debug, Deserialize)]
pub(crate) struct ClearConflictsQuery {
    #[serde(default)]
    status: Option<String>,
}

/// `POST /admin/gc/objects` — dry-run unless `apply` is true.
#[derive(Debug, Deserialize)]
pub(crate) struct GcBody {
    #[serde(default)]
    apply: bool,
    /// Explicit legacy prefix to purge instead of the orphan sweep (e.g. `forgevcs-ci/`).
    #[serde(default)]
    prefix: Option<String>,
}

/// One exported object key and its bytes (base64). Keys are store-relative
/// (`r|q/<repo_id>/<kind>/<fan>/<hex>`).
#[derive(Debug, Deserialize, serde::Serialize)]
pub(crate) struct BlobEntry {
    key: String,
    data: String,
}

/// `POST /repos/{repo}/blobs/import`
#[derive(Debug, Deserialize)]
pub(crate) struct BlobImportBody {
    entries: Vec<BlobEntry>,
}

fn grpc_frames(items: &[v1::UploadItem]) -> Vec<u8> {
    let mut framed = Vec::new();
    for item in items {
        let message = item.encode_to_vec();
        let length = u32::try_from(message.len()).unwrap_or(u32::MAX);
        framed.push(0);
        framed.extend_from_slice(&length.to_be_bytes());
        framed.extend_from_slice(&message);
    }
    framed
}

/// `Streaming<UploadItem>` over the bytes the handler would have received from tonic.
///
/// `UploadObjects` is the one write handler with a streaming request. The façade already holds
/// every item in memory (the REST body is not streamed), so it builds the five-byte gRPC frame
/// header and the prost payloads itself and lets tonic decode them. No upload rule is copied.
fn upload_stream(items: Vec<v1::UploadItem>) -> tonic::Streaming<v1::UploadItem> {
    let body = axum::body::Body::from(grpc_frames(&items));
    tonic::codec::Streaming::new_request(
        tonic_prost::ProstCodec::<v1::UploadItem, v1::UploadItem>::raw_decoder(Default::default()),
        body,
        None,
        None,
    )
}

fn uploads_json(results: &[v1::UploadResult]) -> Vec<Value> {
    results
        .iter()
        .map(|result| {
            json!({
                "id": id_text(&result.id),
                "status": item_status(result.status),
                "error_code": result.error_code,
                "message": result.message,
            })
        })
        .collect()
}

fn upload_failed_response(results: &[v1::UploadResult]) -> Response {
    let failed: Vec<&v1::UploadResult> = results
        .iter()
        .filter(|result| result.status != v1::ItemStatus::Ok as i32)
        .collect();
    let ids: Vec<&str> = failed
        .iter()
        .map(|result| result.error_code.as_str())
        .collect();
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "service": SERVICE,
            "status": "error",
            "code": "upload_failed",
            "message": format!(
                "{} of {} object(s) failed validation: {}",
                failed.len(),
                results.len(),
                ids.join(", ")
            ),
            "uploads": uploads_json(results),
        })),
    )
        .into_response()
}

fn landed_json(landed: &[v1::LandedEntry]) -> Vec<Value> {
    landed
        .iter()
        .map(|entry| {
            json!({
                "change_id": hex::encode(&entry.change_id),
                "source": id_text(&entry.source),
                "result": id_text(&entry.result),
            })
        })
        .collect()
}

/// Advancing the trunk ref is the DevOps seat's entitlement, independent of path writes and of
/// the ref rule: both must allow it. Workers check in on their own refs; they do not land to the
/// trunk.
fn require_devops(
    engine: &Arc<Engine>,
    caller: &CallerCtx,
    permission: scion_policy::AdminPerm,
    message: &str,
) -> Result<(), Response> {
    let app = engine.context();
    if app
        .policy
        .current()
        .can_admin(&caller.principal, permission)
    {
        Ok(())
    } else {
        Err(error_response(
            StatusCode::FORBIDDEN,
            "permission_denied",
            message,
        ))
    }
}

fn require_trunk(engine: &Arc<Engine>, caller: &CallerCtx, ref_name: &str) -> Result<(), Response> {
    if ref_name != DEFAULT_REF {
        return Ok(());
    }
    require_devops(
        engine,
        caller,
        scion_policy::AdminPerm::Trunk,
        "advancing the trunk ref requires the DevOps seat (admin.trunk)",
    )
}

/// The repository paths one push writes: every changeset delta path. The façade authorizes each
/// one (FVCS-02), and upstream's `create_changeset` checks the same paths again (defense in depth).
fn push_delta_paths(changeset: &ChangesetBody) -> Vec<String> {
    changeset
        .delta
        .iter()
        .map(|delta| delta.path.clone())
        .collect()
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    if !matches!(body.mode.as_str(), "land" | "update" | "none") {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "mode must be one of land, update, none",
        );
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
        && let Err(response) = require_trunk(&engine, &caller, &ref_name)
    {
        return response;
    }

    let policy = engine.context().policy.current();
    match state
        .authz
        .authorize_push(&policy, &caller.principal, &repo, &ref_name, &delta_paths)
        .await
    {
        Ok(decision) if decision.allow => {}
        Ok(decision) => return status_response(Status::permission_denied(decision.reason)),
        Err(status) => return status_response(status),
    }

    let mut uploads = Vec::with_capacity(objects.len());
    for object in objects {
        match object.into_proto() {
            Ok(item) => uploads.push(item),
            Err(message) => {
                return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
            }
        }
    }
    let changeset = match changeset.into_proto() {
        Ok(changeset) => changeset,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
        }
    };

    let app = engine.context();
    let mut upload_results = Vec::new();
    if !uploads.is_empty() {
        let request = caller_request(upload_stream(uploads), &caller);
        match handlers::upload::upload_objects(&app, request).await {
            Ok(response) => upload_results = response.into_inner().results,
            Err(status) => return status_response(status),
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
            Err(status) => return status_response(status),
        };
    let changeset_id = match ObjectId::from_bytes(&created.changeset_id) {
        Ok(id) => id,
        Err(error) => return status_response(status_from_error(&error)),
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
                Err(status) => return status_response(status),
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
                }
                Err(status) => return status_response(status),
            }
        }
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_argument",
                "mode must be one of land, update, none",
            );
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    let target = match parse_id(&body.target, "target") {
        Ok(value) => value,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
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
        Err(status) => status_response(status),
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    let changeset = match parse_id(&body.changeset, "changeset") {
        Ok(value) => value,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
        }
    };
    let onto = match parse_id(&body.onto, "onto") {
        Ok(value) => value,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
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
        Err(status) => status_response(status),
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    let a = match parse_id(&body.a, "a") {
        Ok(value) => value,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
        }
    };
    let b = match parse_id(&body.b, "b") {
        Ok(value) => value,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
        }
    };
    let change_id = match parse_change_id(&body.change_id) {
        Ok(value) => value,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
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
        Err(status) => status_response(status),
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    if let Err(response) = require_trunk(&engine, &caller, &body.ref_name) {
        return response;
    }
    let head = match parse_id(&body.head, "head") {
        Ok(value) => value,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
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
            (
                StatusCode::OK,
                Json(json!({
                    "service": SERVICE,
                    "status": "ok",
                    "repo": repo,
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
        Err(status) => status_response(status),
    }
}

fn merge_land_meta(author: &str, message: &str) -> v1::MetaFields {
    v1::MetaFields {
        author: author.as_bytes().to_vec(),
        author_time: Some(v1::Timestamp::default()),
        committer: None,
        committer_time: None,
        message: message.as_bytes().to_vec(),
        extensions: Vec::new(),
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    if let Err(response) = require_trunk(&engine, &caller, &body.ref_name) {
        return response;
    }
    let head = match parse_id(&body.head, "head") {
        Ok(value) => value,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
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
        Err(status) => return status_response(status),
    };
    let tip = match ObjectId::from_bytes(&resolved.target) {
        Ok(id) => id,
        Err(error) => return status_response(status_from_error(&error)),
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
                return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
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
                meta_fields: Some(merge_land_meta(&author, &message)),
                change_id: merge_change_id,
            },
            &caller,
        ),
    )
    .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return status_response(status),
    };
    let merged_id = match ObjectId::from_bytes(&merged.changeset_id) {
        Ok(id) => id,
        Err(error) => return status_response(status_from_error(&error)),
    };

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
        Err(status) => return status_response(status),
    };
    let Some(item) = changesets.items.first() else {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("merge changeset {merged_id} not found"),
        );
    };
    if item.status != v1::ItemStatus::Ok as i32 {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("merge changeset {merged_id} not found"),
        );
    }
    let delta = match &item.view {
        Some(v1::changeset_item::View::FilteredView(view)) => {
            if view.filtered {
                return error_response(
                    StatusCode::CONFLICT,
                    "conflict",
                    "the merge delta includes a path this principal cannot read; refusing to \
                     publish a partial tree",
                );
            }
            view.delta.clone()
        }
        Some(v1::changeset_item::View::Encoded(bytes)) => {
            let changeset = match scion_object::Changeset::decode(bytes) {
                Ok(changeset) => changeset,
                Err(error) => return status_response(status_from_error(&error)),
            };
            changeset
                .delta
                .iter()
                .map(v1::DeltaEntryMsg::from)
                .collect()
        }
        None => {
            return error_response(
                StatusCode::NOT_FOUND,
                "not_found",
                &format!("merge changeset {merged_id} has no view"),
            );
        }
    };

    let linear = match handlers::write::create_changeset(
        &app,
        caller_request(
            v1::CreateChangesetRequest {
                parents: vec![convert::id_bytes(&tip)],
                delta,
                meta_fields: Some(merge_land_meta(&author, &message)),
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
        Err(status) => return status_response(status),
    };
    let linear_id = match ObjectId::from_bytes(&linear.changeset_id) {
        Ok(id) => id,
        Err(error) => return status_response(status_from_error(&error)),
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
        Err(status) => return status_response(status),
    };

    (
        StatusCode::OK,
        Json(json!({
            "service": SERVICE,
            "status": "ok",
            "repo": repo,
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

/// Largest version body embedded in a conflict-context response; larger side is described by
/// `size` and left for a `/file` read.
const MAX_CONTEXT_BYTES: usize = 1 << 20;

fn entry_info_json(entry: Option<&EntryInfo>) -> Value {
    match entry {
        None => Value::Null,
        Some(entry) => json!({
            "mode": entry.mode as u8,
            "id": entry.id.to_text(),
            "size": entry.size,
        }),
    }
}

fn version_json(data: Option<Vec<u8>>) -> Value {
    match data {
        None => json!({ "present": false }),
        Some(bytes) if bytes.len() > MAX_CONTEXT_BYTES => json!({
            "present": true,
            "size": bytes.len(),
            "truncated": true,
        }),
        Some(bytes) => json!({
            "present": true,
            "size": bytes.len(),
            "data": BASE64.encode(&bytes),
        }),
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

fn conflict_report_of(status: &Status) -> Option<ConflictReport> {
    if status.code() != tonic::Code::FailedPrecondition && status.code() != tonic::Code::Aborted {
        return None;
    }
    match error_from_status(status).payload {
        ErrorPayload::MergeConflict(report) => Some(*report),
        _ => None,
    }
}

fn conflict_hunks_json(conflict: &scion_core::Conflict) -> Value {
    json!(
        conflict
            .hunks
            .iter()
            .map(|hunk| json!({
                "base":   { "start": hunk.base.start,   "end": hunk.base.end },
                "ours":   { "start": hunk.ours.start,   "end": hunk.ours.end },
                "theirs": { "start": hunk.theirs.start, "end": hunk.theirs.end },
            }))
            .collect::<Vec<_>>()
    )
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    let app = engine.context();

    let (orientation, base, ours, theirs, report, clean_changeset) =
        if let (Some(raw_a), Some(raw_b)) = (body.a.as_deref(), body.b.as_deref()) {
            let a = match parse_id(raw_a, "a") {
                Ok(id) => id,
                Err(message) => {
                    return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
                }
            };
            let b = match parse_id(raw_b, "b") {
                Ok(id) => id,
                Err(message) => {
                    return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
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
                Err(status) => return status_response(status),
            };
            let base = match bases.as_slice() {
                [] => None,
                [one] => match ObjectId::from_bytes(one) {
                    Ok(id) => Some(id),
                    Err(error) => return status_response(status_from_error(&error)),
                },
                _ => {
                    return error_response(
                        StatusCode::CONFLICT,
                        "conflict",
                        "the two changesets have more than one best common ancestor",
                    );
                }
            };
            let merged = handlers::write::merge(
                &app,
                caller_request(
                    v1::MergeRequest {
                        a: convert::id_bytes(&a),
                        b: convert::id_bytes(&b),
                        meta_fields: Some(merge_land_meta(
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
                    None => return status_response(status),
                },
            }
        } else if let (Some(raw_changeset), Some(raw_onto)) =
            (body.changeset.as_deref(), body.onto.as_deref())
        {
            let changeset = match parse_id(raw_changeset, "changeset") {
                Ok(id) => id,
                Err(message) => {
                    return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
                }
            };
            let onto = match parse_id(raw_onto, "onto") {
                Ok(id) => id,
                Err(message) => {
                    return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
                }
            };
            let parents = match changeset_parents(&app, &caller, &changeset).await {
                Ok(parents) => parents,
                Err(status) => return status_response(status),
            };
            let [parent] = parents.as_slice() else {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_argument",
                    "rebase context requires a single-parent changeset",
                );
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
                    None => return status_response(status),
                },
            }
        } else {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_argument",
                "provide a+b (merge) or changeset+onto (rebase)",
            );
        };

    let mut conflicts = Vec::new();
    let mut clean_entries = Vec::new();
    if let Some(report) = &report {
        for conflict in &report.conflicts {
            let base_data = match read_version(&app, &caller, base.as_ref(), &conflict.path).await {
                Ok(data) => data,
                Err(status) => return status_response(status),
            };
            let ours_data = match read_version(&app, &caller, Some(&ours), &conflict.path).await {
                Ok(data) => data,
                Err(status) => return status_response(status),
            };
            let theirs_data = match read_version(&app, &caller, Some(&theirs), &conflict.path).await
            {
                Ok(data) => data,
                Err(status) => return status_response(status),
            };
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
    }

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
// Conflict escalation queue (DevOps hand-off)
//
// The worker escalates a conflict it cannot resolve; the DevOps seat lists, claims and resolves
// it. The orchestrator polls `GET /repos/{repo}/conflicts?status=open` and launches a DevOps
// agent per item. The queue stores pointers and the conflict report, never content: the DevOps
// agent re-reads the three sides with `/conflict-context`.
// ---------------------------------------------------------------------------------------------

fn queue_unavailable() -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "the conflict queue needs FORGEVCS_DATABASE_URL",
    )
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    let head = match parse_id(&body.head, "head") {
        Ok(id) => id.to_text(),
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
        }
    };
    let optional_id = |raw: Option<&str>, field: &str| -> Result<Option<String>, String> {
        raw.map(|value| parse_id(value, field).map(|id| id.to_text()))
            .transpose()
    };
    let onto = match optional_id(body.onto.as_deref(), "onto") {
        Ok(id) => id,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
        }
    };
    let base = match optional_id(body.base.as_deref(), "base") {
        Ok(id) => id,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_argument", &message);
        }
    };
    if let Err(response) = authorize(
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
        return response;
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable();
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
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", &error),
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return bad_query(rejection),
    };
    if let Err(response) = require_devops(
        &engine,
        &caller,
        scion_policy::AdminPerm::Trunk,
        "reading the conflict queue requires the DevOps seat (admin.trunk)",
    ) {
        return response;
    }
    let status = query.status.unwrap_or_else(|| "open".to_owned());
    if !crate::escalations::valid_status(&status) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "status must be one of open, claimed, resolved, all",
        );
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable();
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
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", &error),
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    if let Err(response) = require_devops(
        &engine,
        &caller,
        scion_policy::AdminPerm::Trunk,
        "claiming a conflict requires the DevOps seat (admin.trunk)",
    ) {
        return response;
    }
    if uuid::Uuid::parse_str(&id).is_err() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "id is not a UUID",
        );
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable();
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
        Ok(None) => error_response(
            StatusCode::CONFLICT,
            "conflict",
            "escalation is not open (already claimed or resolved)",
        ),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", &error),
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    if let Err(response) = require_devops(
        &engine,
        &caller,
        scion_policy::AdminPerm::Trunk,
        "resolving a conflict requires the DevOps seat (admin.trunk)",
    ) {
        return response;
    }
    if uuid::Uuid::parse_str(&id).is_err() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "id is not a UUID",
        );
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable();
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
        Ok(None) => error_response(
            StatusCode::CONFLICT,
            "conflict",
            "escalation is already resolved or absent",
        ),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", &error),
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

async fn delete_repository_rows(pool: &Pool, repo: &str) -> Result<Value, Response> {
    let mut client = pool.get().await.map_err(|error| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            &format!("deleting repository: {error}"),
        )
    })?;
    let tx = client.transaction().await.map_err(|error| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &format!("deleting repository: {error}"),
        )
    })?;
    let row = tx
        .query_opt("SELECT repo_id FROM repos WHERE name = $1", &[&repo])
        .await
        .map_err(|error| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &format!("reading repository: {error}"),
            )
        })?;
    let Some(row) = row else {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("repository {repo} not found"),
        ));
    };
    let repo_id: i64 = row.get(0);
    let mut rows = serde_json::Map::new();
    for table in SCION_REPO_TABLES {
        let sql = format!("DELETE FROM {table} WHERE repo_id = $1");
        let removed = tx.execute(&sql, &[&repo_id]).await.map_err(|error| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &format!("deleting {table}: {error}"),
            )
        })?;
        rows.insert(table.to_owned(), json!(removed));
    }
    for table in ["forgevcs_workspaces", "forgevcs_conflict_escalations"] {
        let sql = format!("DELETE FROM {table} WHERE repo = $1");
        let removed = tx.execute(&sql, &[&repo]).await.map_err(|error| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &format!("deleting {table}: {error}"),
            )
        })?;
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
        .map_err(|error| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &format!("deleting forgevcs_repo_acl: {error}"),
            )
        })?;
    rows.insert("forgevcs_repo_acl".to_owned(), json!(removed_owner));
    let removed = tx
        .execute("DELETE FROM repos WHERE repo_id = $1", &[&repo_id])
        .await
        .map_err(|error| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &format!("deleting the repository row: {error}"),
            )
        })?;
    if removed != 1 {
        return Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "the repository row changed during deletion",
        ));
    }
    tx.commit().await.map_err(|error| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &format!("committing repository deletion: {error}"),
        )
    })?;
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
        Err(response) => return response,
    };
    if let Err(response) = require_devops(
        &auth_engine,
        &caller,
        scion_policy::AdminPerm::Repos,
        "listing repositories requires the DevOps seat (admin.repos)",
    ) {
        return response;
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable();
    };
    let Ok(client) = pool.get().await else {
        return queue_unavailable();
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
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &format!("listing repositories: {error}"),
            );
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
        Err(response) => return response,
    };
    if let Err(response) = require_devops(
        &auth_engine,
        &caller,
        scion_policy::AdminPerm::Repos,
        "deleting a repository requires the DevOps seat (admin.repos)",
    ) {
        return response;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    if body.confirm != repo {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "confirm must equal the repository name",
        );
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable();
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
        Err(response) => response,
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
        Err(response) => return response,
    };
    if let Err(response) = require_devops(
        &auth_engine,
        &caller,
        scion_policy::AdminPerm::Trunk,
        "clearing the conflict queue requires the DevOps seat (admin.trunk)",
    ) {
        return response;
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return bad_query(rejection),
    };
    let status = query.status.unwrap_or_else(|| "resolved".to_owned());
    if !crate::escalations::valid_status(&status) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "status must be one of open, claimed, resolved, all",
        );
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable();
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
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", &error),
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
        Err(response) => return response,
    };
    if let Err(response) = require_devops(
        &auth_engine,
        &caller,
        scion_policy::AdminPerm::Trunk,
        "clearing a conflict requires the DevOps seat (admin.trunk)",
    ) {
        return response;
    }
    if uuid::Uuid::parse_str(&id).is_err() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "id is not a UUID",
        );
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable();
    };
    match crate::escalations::delete(pool, &repo, &id).await {
        Ok(true) => (
            StatusCode::OK,
            Json(json!({ "service": SERVICE, "status": "ok", "repo": repo, "cleared": id })),
        )
            .into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "not_found", "escalation not found"),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", &error),
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
        Err(response) => return response,
    };
    if let Err(response) = require_devops(
        &auth_engine,
        &caller,
        scion_policy::AdminPerm::Repos,
        "object garbage collection requires the DevOps seat (admin.repos)",
    ) {
        return response;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    let Some(s3) = state.config.s3_config() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "object storage is not configured",
        );
    };
    let blobs = match scion_store_s3::S3BlobStore::new(s3) {
        Ok(blobs) => blobs,
        Err(error) => return status_response(status_from_error(&error)),
    };
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable();
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
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", &error),
    }
}

// ---------------------------------------------------------------------------------------------
// Backup transfer (export/import of object bytes)
//
// The metadata lives in Postgres and is exported by the backup client through psql; these two
// routes move the external object bytes, which psql cannot see. Both are DevOps-only and keys
// are confined to the repository's own two areas (`r/<repo_id>/`, `q/<repo_id>/`).
// ---------------------------------------------------------------------------------------------

async fn repo_id_for(pool: &Pool, repo: &str) -> Result<i64, Response> {
    let client = pool.get().await.map_err(|error| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            &format!("backup: database: {error}"),
        )
    })?;
    let row = client
        .query_opt("SELECT repo_id FROM repos WHERE name = $1", &[&repo])
        .await
        .map_err(|error| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &format!("backup: reading repository: {error}"),
            )
        })?;
    row.map(|row| row.get::<_, i64>(0)).ok_or_else(|| {
        error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("repository {repo} not found"),
        )
    })
}

fn store_for_backup(
    config: &crate::config::Config,
) -> Result<scion_store_s3::S3BlobStore, Response> {
    let Some(s3) = config.s3_config() else {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "object storage is not configured",
        ));
    };
    scion_store_s3::S3BlobStore::new(s3).map_err(|error| status_response(status_from_error(&error)))
}

/// `GET /repos/{repo}/blobs/export` — every external object of the repository, base64.
pub(crate) async fn export_blobs(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (auth_engine, caller) = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(response) = require_devops(
        &auth_engine,
        &caller,
        scion_policy::AdminPerm::Repos,
        "exporting object bytes requires the DevOps seat (admin.repos)",
    ) {
        return response;
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable();
    };
    let repo_id = match repo_id_for(pool, &repo).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    let blobs = match store_for_backup(&state.config) {
        Ok(blobs) => blobs,
        Err(response) => return response,
    };
    let mut entries = Vec::new();
    for area in ["r", "q"] {
        let prefix = format!("{area}/{repo_id}/");
        let keys = match blobs.list(&prefix).await {
            Ok(keys) => keys,
            Err(error) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    &format!("listing object keys: {error}"),
                );
            }
        };
        for key in keys {
            let bytes = match blobs.get(&key).await {
                Ok(Some(bytes)) => bytes,
                Ok(None) => continue,
                Err(error) => {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal",
                        &format!("reading {key}: {error}"),
                    );
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
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    if let Err(response) = require_devops(
        &auth_engine,
        &caller,
        scion_policy::AdminPerm::Repos,
        "importing object bytes requires the DevOps seat (admin.repos)",
    ) {
        return response;
    }
    let Some(pool) = state.db.as_ref() else {
        return queue_unavailable();
    };
    let repo_id = match repo_id_for(pool, &repo).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    let live = format!("r/{repo_id}/");
    let quarantine = format!("q/{repo_id}/");
    for entry in &body.entries {
        if !entry.key.starts_with(&live) && !entry.key.starts_with(&quarantine) {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_argument",
                &format!("key {} does not belong to repository {repo}", entry.key),
            );
        }
    }
    let blobs = match store_for_backup(&state.config) {
        Ok(blobs) => blobs,
        Err(response) => return response,
    };
    for entry in &body.entries {
        let Ok(bytes) = BASE64.decode(entry.data.as_bytes()) else {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_argument",
                &format!("entry {} is not base64", entry.key),
            );
        };
        if let Err(error) = blobs.put(&entry.key, bytes.into()).await {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                &format!("writing {}: {error}", entry.key),
            );
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

fn require_root_read(engine: &Arc<Engine>, caller: &CallerCtx) -> Result<(), Response> {
    let app = engine.context();
    let policy = app.policy.current();
    if policy.can_read(&caller.principal, &RepoPath::root()) {
        Ok(())
    } else {
        Err(status_response(Status::permission_denied(
            "repository read (read on /) denied",
        )))
    }
}

fn require_root_write(engine: &Arc<Engine>, caller: &CallerCtx) -> Result<(), Response> {
    let app = engine.context();
    let policy = app.policy.current();
    if policy.can_write(&caller.principal, &RepoPath::root()) {
        Ok(())
    } else {
        Err(status_response(Status::permission_denied(
            "repository write (write on /) denied",
        )))
    }
}

fn workspace_rev(changeset: Option<&str>, ref_name: Option<&str>) -> Result<String, Response> {
    match changeset {
        Some(raw) => match parse_id(raw, "changeset") {
            Ok(id) => Ok(id.to_text()),
            Err(message) => Err(error_response(
                StatusCode::BAD_REQUEST,
                "invalid_argument",
                &message,
            )),
        },
        None => Ok(ref_name.unwrap_or(DEFAULT_REF).to_owned()),
    }
}

fn parse_sparse(sparse: &[String]) -> Result<Vec<RepoPath>, Response> {
    let mut paths = Vec::with_capacity(sparse.len());
    for raw in sparse {
        match RepoPath::parse(raw.as_bytes()) {
            Ok(path) => paths.push(path),
            Err(error) => {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_argument",
                    &format!("sparse path {raw:?}: {}", error.message),
                ));
            }
        }
    }
    Ok(paths)
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceCreateBody {
    #[serde(rename = "ref")]
    ref_name: Option<String>,
    changeset: Option<String>,
    #[serde(default)]
    sparse: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceMaterializeBody {
    #[serde(rename = "ref")]
    ref_name: Option<String>,
    changeset: Option<String>,
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceFileQuery {
    path: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceWriteBody {
    data: String,
    #[serde(default = "default_write_mode")]
    mode: u8,
}

fn default_write_mode() -> u8 {
    1
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceSubmitBody {
    message: String,
    author: Option<String>,
    #[serde(rename = "ref")]
    ref_name: Option<String>,
    #[serde(default = "default_push_mode")]
    mode: String,
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    if let Err(response) = require_root_read(&engine, &caller) {
        return response;
    }
    if let Err(response) = authorize(
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
        return response;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    let rev = match workspace_rev(body.changeset.as_deref(), body.ref_name.as_deref()) {
        Ok(rev) => rev,
        Err(response) => return response,
    };
    let sparse = match parse_sparse(&body.sparse) {
        Ok(sparse) => sparse,
        Err(response) => return response,
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
        Err(status) => status_response(status),
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    if let Err(response) = require_root_read(&engine, &caller) {
        return response;
    }
    if let Err(response) = authorize(
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
        return response;
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    if let Err(response) = require_root_read(&engine, &caller) {
        return response;
    }
    if let Err(response) = authorize(
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
        return response;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    let rev = match workspace_rev(body.changeset.as_deref(), body.ref_name.as_deref()) {
        Ok(rev) => rev,
        Err(response) => return response,
    };
    let result = engine.workspaces().materialize(&id, &rev, body.force).await;
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return bad_query(rejection),
    };
    if let Err(response) = authorize(
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
        return response;
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return bad_query(rejection),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    let data = match BASE64.decode(body.data.as_bytes()) {
        Ok(data) => data,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_argument",
                &format!("data is not base64: {error}"),
            );
        }
    };
    if let Err(response) = authorize(
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
        return response;
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return bad_query(rejection),
    };
    if let Err(response) = authorize(
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
        return response;
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return bad_query(rejection),
    };
    if let Err(response) = require_root_read(&engine, &caller) {
        return response;
    }
    if let Err(response) = authorize(
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
        return response;
    }
    let app = engine.context();
    let policy = app.policy.current();
    let result = engine
        .workspaces()
        .diff(&id, query.context.min(100), &policy, &caller.principal)
        .await;
    workspace_result(repo, result)
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceDiffQuery {
    #[serde(default = "default_context")]
    context: usize,
}

fn default_context() -> usize {
    3
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return bad_json(rejection),
    };
    if body.message.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "message must not be empty",
        );
    }
    if matches!(body.mode.as_str(), "land" | "update")
        && let Err(response) = require_trunk(
            &engine,
            &caller,
            body.ref_name.as_deref().unwrap_or(DEFAULT_REF),
        )
    {
        return response;
    }
    let ref_name = body.ref_name.as_deref().unwrap_or(DEFAULT_REF);
    let author = body
        .author
        .as_deref()
        .unwrap_or("ForgeVCS workspace <workspace@example.test>");
    if let Err(response) = authorize(
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
        return response;
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
        Err(response) => return response,
    };
    let engine = match repo_engine(&state, &repo).await {
        Ok(engine) => engine,
        Err(response) => return response,
    };
    if let Err(response) = require_root_write(&engine, &caller) {
        return response;
    }
    if let Err(response) = authorize(
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
        return response;
    }
    match engine.workspaces().destroy(&id).await {
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
        Err(status) => status_response(status),
    }
}

fn workspace_result(repo: String, result: Result<Value, Status>) -> Response {
    match result {
        Ok(value) => (
            StatusCode::OK,
            Json(json!({
                "service": SERVICE,
                "status": "ok",
                "repo": repo,
                "workspace": value,
            })),
        )
            .into_response(),
        Err(status) => status_response(status),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

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
}
