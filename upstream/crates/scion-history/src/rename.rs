//! Exact-content rename detection (`FORGEVCS_MERGE_ENGINE=exact-rename-v1`).
//!
//! Scion v1 stores a rename as delete(`P`) + add(`Q`). When the other side edits `P`, rebase
//! sees a modify/delete candidate at `P` (`o == None`, `b` and `t` present) and reports a
//! conflict the agent did not create. This module recognizes the common case **exactly**:
//! `P`'s base content id appears unchanged at exactly one added path `Q`.
//!
//! The rules are opt-in through the `FORGEVCS_MERGE_ENGINE` environment variable so the
//! vendored default (`line-diff3-v1`) is untouched and the upstream gate keeps its behaviour.
//! Similarity-based renames are a later engine version (see docs/M1-rename-detection.md).

use std::collections::BTreeMap;

use scion_core::{EntryInfo, ObjectId};
use scion_object::RepoPath;

use crate::types::FileChange;

/// Exact rename pairs of one side's file diff: base path `P` -> added path `Q`, plus `Q`'s entry.
///
/// A candidate is ambiguous (two added paths with the same content id) and is left alone; the
/// safer delete + add behaviour is kept for it.
pub(crate) fn exact_renames(changes: &[FileChange]) -> BTreeMap<RepoPath, (RepoPath, EntryInfo)> {
    let mut added: BTreeMap<ObjectId, Vec<(RepoPath, EntryInfo)>> = BTreeMap::new();
    for change in changes {
        if let (None, Some(new)) = (&change.old, &change.new) {
            added
                .entry(new.id)
                .or_default()
                .push((change.path.clone(), *new));
        }
    }
    let mut out = BTreeMap::new();
    for change in changes {
        if let (Some(old), None) = (&change.old, &change.new)
            && let Some(candidates) = added.get(&old.id)
            && let [only] = candidates.as_slice()
        {
            out.insert(change.path.clone(), only.clone());
        }
    }
    out
}

/// Whether this process runs the exact-rename merge engine.
pub(crate) fn rename_engine_enabled() -> bool {
    matches!(
        std::env::var("FORGEVCS_MERGE_ENGINE").as_deref(),
        Ok("exact-rename-v1")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::Mode;

    fn id(byte: u8) -> ObjectId {
        // Raw layout: [BLAKE3 algorithm][kind byte (0x01 = blob)][32-byte digest].
        let mut bytes = vec![scion_core::ALGO_BLAKE3, 0x01];
        bytes.extend(std::iter::repeat_n(0u8, 32));
        bytes[2] = byte;
        ObjectId::from_bytes(&bytes).expect("blob id")
    }

    fn entry(byte: u8) -> EntryInfo {
        EntryInfo {
            mode: Mode::File,
            id: id(byte),
            size: 1,
        }
    }

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw.as_bytes()).expect("path")
    }

    fn change(raw: &str, old: Option<u8>, new: Option<u8>) -> FileChange {
        FileChange {
            path: path(raw),
            old: old.map(entry),
            new: new.map(entry),
        }
    }

    #[test]
    fn exact_rename_is_detected() {
        let diff = vec![
            change("old.txt", Some(1), None),
            change("new.txt", None, Some(1)),
        ];
        let renames = exact_renames(&diff);
        assert_eq!(
            renames.get(&path("old.txt")),
            Some(&(path("new.txt"), entry(1)))
        );
    }

    #[test]
    fn edited_rename_is_not_detected() {
        let diff = vec![
            change("old.txt", Some(1), None),
            change("new.txt", None, Some(2)),
        ];
        assert!(exact_renames(&diff).is_empty());
    }

    #[test]
    fn ambiguous_additions_are_left_alone() {
        let diff = vec![
            change("old.txt", Some(1), None),
            change("a.txt", None, Some(1)),
            change("b.txt", None, Some(1)),
        ];
        assert!(exact_renames(&diff).is_empty());
    }
}
