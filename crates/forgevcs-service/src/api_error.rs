//! The service's single typed HTTP error.
//!
//! One enum decides the status code, the stable `code` slug and the JSON envelope for every
//! failure the probes and the business façade return. Façade helpers return [`ApiResult`] and use
//! `?`; a handler, whose axum signature returns `Response`, converts once at its boundary via
//! [`IntoResponse`]. No helper carries an `axum::Response` as its error type, which is what the
//! old `result_large_err` allowance was hiding.
//!
//! The envelope is the same one the probes use:
//! `{"service":"forgevcs","status":"error","code":…,"message":…}`, plus `details` when an
//! upstream `tonic::Status` carries a `scion.v1.ErrorDetail` payload (a merge conflict report,
//! the current ref state for a version mismatch, …). That mapping is identical to the gRPC
//! surface's, so the two transports cannot disagree about what a failure means.

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use scion_core::{EntryInfo, ErrorPayload};
use scion_proto::error_from_status;
use serde_json::{Value, json};

use crate::http::SERVICE;

/// A façade/handler step that fails with the service's typed error.
pub(crate) type ApiResult<T> = Result<T, ApiError>;

/// One failed request.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ApiError {
    /// 400 — the request is malformed or fails a façade-level validation.
    InvalidArgument(String),
    /// 401 — the supplied credentials are not accepted (human token mint).
    Unauthenticated(String),
    /// 403 — authentication succeeded but the principal is not allowed to act.
    PermissionDenied(String),
    /// 404 — the addressed object/repository does not exist.
    NotFound(String),
    /// 409 — a merge/rebase/ref precondition failed; the caller can retry or resolve.
    Conflict(String),
    /// 503 — storage/the registry is not configured or not reachable right now.
    Unavailable(String),
    /// 500 — an invariant of the service itself failed.
    Internal(String),
    /// A failure mapped from an upstream `tonic::Status`. The HTTP status and envelope `code`
    /// are already mapped; `details` is the decoded `scion.v1.ErrorDetail` payload, when any.
    /// `extra` carries additive top-level fields (e.g. the paused `conflict` record) without
    /// changing the error envelope's existing shape.
    Upstream {
        status: StatusCode,
        code: &'static str,
        message: String,
        details: Option<Box<Value>>,
        extra: Option<Box<Value>>,
    },
    /// 409 — another live owner holds the workspace write lease (R4). Carries the current owner,
    /// fencing token and expiry so the caller can back off or surface the conflict instead of
    /// retrying blindly.
    WorkspaceLeaseConflict(Box<LeaseConflictDetails>),
}

/// The current holder reported by a workspace write-lease conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeaseConflictDetails {
    /// Workspace that is leased.
    pub workspace_id: String,
    /// Repository the workspace belongs to.
    pub repo: String,
    /// Principal that holds the lease.
    pub owner: String,
    /// Fencing token of the current lease.
    pub lease_id: String,
    /// Unix seconds when the current lease expires (a stale lease may be taken over after this).
    pub expires_at_unix: i64,
}

impl ApiError {
    // The constructors exist so call sites spell intent, not a status/code pair.
    pub(crate) fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument(message.into())
    }

    pub(crate) fn unauthenticated(message: impl Into<String>) -> Self {
        Self::Unauthenticated(message.into())
    }

    pub(crate) fn permission_denied(message: impl Into<String>) -> Self {
        Self::PermissionDenied(message.into())
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    /// Decode the `Aborted` status the workspace layer returns for a held lease into the typed
    /// 409. Returns `None` for any status that is not a lease conflict (e.g. an upstream abort).
    pub(crate) fn lease_conflict_from_status(status: &tonic::Status) -> Option<Self> {
        if status.code() != tonic::Code::Aborted {
            return None;
        }
        let owner = metadata_string(status, crate::lease::METADATA_OWNER)?;
        Some(Self::WorkspaceLeaseConflict(Box::new(
            LeaseConflictDetails {
                workspace_id: metadata_string(status, crate::lease::METADATA_WORKSPACE_ID)
                    .unwrap_or_default(),
                repo: metadata_string(status, crate::lease::METADATA_REPO).unwrap_or_default(),
                owner,
                lease_id: metadata_string(status, crate::lease::METADATA_LEASE_ID)
                    .unwrap_or_default(),
                expires_at_unix: metadata_string(status, crate::lease::METADATA_EXPIRES_AT)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_default(),
            },
        )))
    }

    /// Attach additive top-level fields to an upstream error envelope (e.g. `conflict`).
    pub(crate) fn with_extra(self, extra: Value) -> Self {
        match self {
            Self::Upstream {
                status,
                code,
                message,
                details,
                ..
            } => Self::Upstream {
                status,
                code,
                message,
                details,
                extra: Some(Box::new(extra)),
            },
            other => other,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message, details, extra) = match self {
            Self::InvalidArgument(message) => (
                StatusCode::BAD_REQUEST,
                "invalid_argument",
                message,
                None,
                None,
            ),
            Self::Unauthenticated(message) => (
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                message,
                None,
                None,
            ),
            Self::PermissionDenied(message) => (
                StatusCode::FORBIDDEN,
                "permission_denied",
                message,
                None,
                None,
            ),
            Self::NotFound(message) => (StatusCode::NOT_FOUND, "not_found", message, None, None),
            Self::Conflict(message) => (StatusCode::CONFLICT, "conflict", message, None, None),
            Self::Unavailable(message) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                message,
                None,
                None,
            ),
            Self::Internal(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                message,
                None,
                None,
            ),
            Self::Upstream {
                status,
                code,
                message,
                details,
                extra,
            } => (status, code, message, details, extra),
            Self::WorkspaceLeaseConflict(boxed) => {
                let details = *boxed;
                (
                    StatusCode::CONFLICT,
                    "workspace_lease_conflict",
                    format!(
                        "workspace {} is leased by {} until unix {}",
                        details.workspace_id, details.owner, details.expires_at_unix
                    ),
                    Some(Box::new(json!({
                        "kind": "workspace_lease",
                        "workspace_id": details.workspace_id,
                        "repo": details.repo,
                        "owner": details.owner,
                        "lease_id": details.lease_id,
                        "expires_at_unix": details.expires_at_unix,
                    }))),
                    None,
                )
            }
        };
        let mut body = json!({
            "service": SERVICE,
            "status": "error",
            "code": code,
            "message": message,
        });
        if let Some(details) = details
            && let Some(object) = body.as_object_mut()
        {
            object.insert("details".to_owned(), *details);
        }
        if let Some(extra) = extra
            && let (Some(object), Some(fields)) = (body.as_object_mut(), extra.as_object())
        {
            for (key, value) in fields {
                object.insert(key.clone(), value.clone());
            }
        }
        (status, Json(body)).into_response()
    }
}

impl From<tonic::Status> for ApiError {
    fn from(status: tonic::Status) -> Self {
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
        Self::Upstream {
            status: http,
            code: slug,
            message: status.message().to_owned(),
            details: error_payload_json(&error.payload).map(Box::new),
            extra: None,
        }
    }
}

impl From<&scion_core::ScionError> for ApiError {
    fn from(error: &scion_core::ScionError) -> Self {
        Self::from(scion_proto::status_from_error(error))
    }
}

impl From<scion_core::ScionError> for ApiError {
    fn from(error: scion_core::ScionError) -> Self {
        Self::from(&error)
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        Self::InvalidArgument(rejection.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        Self::InvalidArgument(rejection.body_text())
    }
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
/// Also used when a paused conflict is persisted, so `show` returns the same report shape the
/// initial `409 details.report` carried.
pub(crate) fn conflict_report_json(report: &scion_core::ConflictReport) -> Value {
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

/// One metadata value as UTF-8 text, when present and printable.
fn metadata_string(status: &tonic::Status, key: &str) -> Option<String> {
    status
        .metadata()
        .get(key)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The `scion.v1.ErrorDetail` payload as JSON, when the status carries one.
fn error_payload_json(payload: &ErrorPayload) -> Option<Value> {
    match payload {
        ErrorPayload::None => None,
        ErrorPayload::Subcode(subcode) => Some(json!({ "kind": "subcode", "subcode": subcode })),
        ErrorPayload::Limit {
            name,
            limit,
            actual,
        } => Some(json!({
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    async fn body_of(error: ApiError) -> (StatusCode, Value) {
        let response = error.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[test]
    fn merge_conflict_payload_is_exposed_as_details() {
        use scion_core::{Conflict, ConflictHunk, ConflictKind};
        let report = scion_core::ConflictReport {
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

    /// `clippy::result_large_err` fires above 128 bytes. The façade removed its allowance by
    /// making the error a small enum (upstream details are boxed); this guards the redesign.
    #[test]
    fn the_error_type_stays_below_the_large_err_limit() {
        assert!(
            std::mem::size_of::<ApiError>() <= 128,
            "ApiError grew past clippy's large-error threshold: {} bytes",
            std::mem::size_of::<ApiError>()
        );
    }

    #[tokio::test]
    async fn constructors_keep_the_canonical_status_and_code() {
        let cases = [
            (
                ApiError::invalid_argument("x"),
                StatusCode::BAD_REQUEST,
                "invalid_argument",
            ),
            (
                ApiError::permission_denied("x"),
                StatusCode::FORBIDDEN,
                "permission_denied",
            ),
            (
                ApiError::unauthenticated("x"),
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
            ),
            (ApiError::not_found("x"), StatusCode::NOT_FOUND, "not_found"),
            (ApiError::conflict("x"), StatusCode::CONFLICT, "conflict"),
            (
                ApiError::unavailable("x"),
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
            ),
            (
                ApiError::internal("x"),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
            ),
        ];
        for (error, status, code) in cases {
            let (actual_status, body) = body_of(error).await;
            assert_eq!(actual_status, status);
            assert_eq!(body["service"], SERVICE);
            assert_eq!(body["status"], "error");
            assert_eq!(body["code"], code);
        }
    }

    #[tokio::test]
    async fn an_upstream_status_keeps_its_details() {
        let error =
            scion_core::ScionError::new(scion_core::ErrorCode::MergeConflict, "merge conflict")
                .with_payload(scion_core::ErrorPayload::MergeConflict(Box::default()));
        let (http, body) = body_of(ApiError::from(&error)).await;
        assert_eq!(http, StatusCode::CONFLICT);
        assert_eq!(body["code"], "conflict");
        assert_eq!(body["details"]["kind"], "merge_conflict");
    }

    /// R4: a workspace write-lease conflict is its own typed 409. The body names the current
    /// owner, fencing token and expiry, so the losing writer can back off instead of retrying.
    #[tokio::test]
    async fn a_workspace_lease_conflict_is_a_typed_409_with_the_current_owner() {
        let workspace = "00000000-0000-0000-0000-0000000000aa";
        let lease = "00000000-0000-0000-0000-0000000000bb";
        let mut status = tonic::Status::aborted("workspace is leased");
        {
            let metadata = status.metadata_mut();
            for (key, value) in [
                (crate::lease::METADATA_OWNER, "service:agent-a"),
                (crate::lease::METADATA_WORKSPACE_ID, workspace),
                (crate::lease::METADATA_REPO, "pilot"),
                (crate::lease::METADATA_LEASE_ID, lease),
                (crate::lease::METADATA_EXPIRES_AT, "1700000300"),
            ] {
                metadata.insert(
                    key,
                    tonic::metadata::MetadataValue::try_from(value).expect("metadata value"),
                );
            }
        }
        let error = ApiError::lease_conflict_from_status(&status).expect("typed lease conflict");
        let (http, body) = body_of(error).await;
        assert_eq!(http, StatusCode::CONFLICT);
        assert_eq!(body["code"], "workspace_lease_conflict");
        assert_eq!(body["details"]["kind"], "workspace_lease");
        assert_eq!(body["details"]["owner"], "service:agent-a");
        assert_eq!(body["details"]["workspace_id"], workspace);
        assert_eq!(body["details"]["repo"], "pilot");
        assert_eq!(body["details"]["lease_id"], lease);
        assert_eq!(body["details"]["expires_at_unix"], 1_700_000_300);
    }

    #[test]
    fn a_non_lease_abort_is_not_misread_as_a_lease_conflict() {
        assert!(
            ApiError::lease_conflict_from_status(&tonic::Status::aborted("plain abort")).is_none()
        );
        assert!(
            ApiError::lease_conflict_from_status(&tonic::Status::failed_precondition("merge"))
                .is_none()
        );
    }
}
