//! Pure response mappers: `scion.v1` messages → the JSON the REST façade returns.
//!
//! These functions read proto replies, ids and timestamps; they never call a handler or change
//! engine state. Decoding a canonical changeset can fail, and that failure is the same typed
//! upstream error the handler would have returned, so the fallible mappers return
//! [`ApiResult`] and the handler propagates it (`?`) or converts it once at the HTTP boundary.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use scion_core::{ConflictReport, EntryInfo, ErrorPayload, ObjectId};
use scion_object::Canonical;
use scion_proto::{convert, error_from_status, v1};
use serde_json::{Value, json};
use tonic::Status;

use crate::api_error::{ApiError, ApiResult};
use crate::http::SERVICE;

/// Largest version body embedded in a conflict-context response; larger side is described by
/// `size` and left for a `/file` read.
const MAX_CONTEXT_BYTES: usize = 1 << 20;

/// The JSON name of a `scion.v1` item status.
pub(crate) fn item_status(status: i32) -> &'static str {
    match status {
        x if x == v1::ItemStatus::Ok as i32 => "ok",
        x if x == v1::ItemStatus::NotFound as i32 => "not_found",
        x if x == v1::ItemStatus::Denied as i32 => "denied",
        x if x == v1::ItemStatus::Redacted as i32 => "redacted",
        _ => "unspecified",
    }
}

/// The JSON name of a `scion.v1` payload encoding.
pub(crate) fn encoding_name(encoding: i32) -> &'static str {
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

pub(crate) fn bytes_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// An object id in its canonical text form, falling back to hex for bytes that are not one.
pub(crate) fn id_text(bytes: &[u8]) -> String {
    ObjectId::from_bytes(bytes).map_or_else(|_| hex::encode(bytes), |id| id.to_text())
}

/// One `Log` entry as JSON. The id, parents and change id come from the canonical changeset
/// when the entry is unfiltered; a filtered view already carries the readable fields.
pub(crate) fn log_entries(entries: &[v1::LogEntry]) -> ApiResult<Vec<Value>> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(item) = &entry.changeset else {
            continue;
        };
        let (parents, root_tree, change_id, filtered) = match &item.view {
            Some(v1::changeset_item::View::Encoded(bytes)) => {
                let changeset = scion_object::Changeset::decode(bytes).map_err(ApiError::from)?;
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

pub(crate) fn entry_json(entry: Option<&v1::EntryInfo>) -> Value {
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
pub(crate) fn changeset_json(item: &v1::ChangesetItem) -> ApiResult<Value> {
    let id = id_text(&item.id);
    match &item.view {
        Some(v1::changeset_item::View::Encoded(bytes)) => {
            let changeset = scion_object::Changeset::decode(bytes).map_err(ApiError::from)?;
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
        None => Err(ApiError::internal("changeset item has no view")),
    }
}

pub(crate) fn diff_item_json(item: &v1::DiffItem) -> Value {
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

pub(crate) fn uploads_json(results: &[v1::UploadResult]) -> Vec<Value> {
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

pub(crate) fn upload_failed_response(results: &[v1::UploadResult]) -> Response {
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

pub(crate) fn landed_json(landed: &[v1::LandedEntry]) -> Vec<Value> {
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

pub(crate) fn entry_info_json(entry: Option<&EntryInfo>) -> Value {
    match entry {
        None => Value::Null,
        Some(entry) => json!({
            "mode": entry.mode as u8,
            "id": entry.id.to_text(),
            "size": entry.size,
        }),
    }
}

pub(crate) fn version_json(data: Option<Vec<u8>>) -> Value {
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

pub(crate) fn conflict_report_of(status: &Status) -> Option<ConflictReport> {
    if status.code() != tonic::Code::FailedPrecondition && status.code() != tonic::Code::Aborted {
        return None;
    }
    match error_from_status(status).payload {
        ErrorPayload::MergeConflict(report) => Some(*report),
        _ => None,
    }
}

pub(crate) fn conflict_hunks_json(conflict: &scion_core::Conflict) -> Value {
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

/// Render a workspace-layer failure, preferring the typed R4 lease conflict over the generic
/// upstream mapping. Used by the workspace envelope and by handlers that build their own success
/// body (`destroy`), so a lease conflict reads the same on every workspace endpoint.
pub(crate) fn workspace_error(status: Status) -> Response {
    match ApiError::lease_conflict_from_status(&status) {
        Some(error) => error.into_response(),
        None => ApiError::from(status).into_response(),
    }
}

/// Render one WorkspaceService reply as the workspace endpoint's envelope.
///
/// A workspace write lease conflict (R4) arrives as an `Aborted` status carrying the current
/// owner in metadata. It is rendered as the typed `workspace_lease_conflict` 409 rather than the
/// generic upstream conflict, so a writer learns who holds the lease instead of guessing why the
/// write failed.
pub(crate) fn workspace_result(repo: String, result: Result<Value, Status>) -> Response {
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
        Err(status) => workspace_error(status),
    }
}
