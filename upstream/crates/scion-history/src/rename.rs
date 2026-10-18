//! Rename detection for the opt-in rename engine (`FORGEVCS_MERGE_ENGINE=exact-rename-v1`).
//!
//! Scion v1 stores a rename as delete(`P`) + add(`Q`). This module recognizes renames on one
//! side of a merge or rebase so the other side's edit can be re-anchored instead of turning into
//! a false modify/delete conflict:
//!
//! 1. **Exact** — `Q` carries `P`'s base content id (no content read).
//! 2. **Similar** — stage 2: line-based similarity over the two contents, at or above
//!    [`SIMILARITY_THRESHOLD`]. Binary or oversized contents never match.
//!
//! Detection is one-to-one and deterministic: exact matches are assigned first (ascending path),
//! then similar pairs by descending similarity (ties by ascending paths). Ambiguous candidates
//! fall back to delete + add, the conservative behaviour.
//!
//! The rules are opt-in through the `FORGEVCS_MERGE_ENGINE` environment variable so the
//! vendored default (`line-diff3-v1`) is untouched and the upstream gate keeps its behaviour.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use scion_core::{EntryInfo, ObjectId, Result};
use scion_object::RepoPath;

use crate::history::History;
use crate::types::FileChange;

/// Minimum line-set Jaccard similarity for a similarity rename.
pub(crate) const SIMILARITY_THRESHOLD: f64 = 0.5;

/// Renames on one side: base path -> (new path, new entry).
pub(crate) type Renames = BTreeMap<RepoPath, (RepoPath, EntryInfo)>;

/// Whether this process runs the opt-in rename engine.
pub(crate) fn rename_engine_enabled() -> bool {
    matches!(
        std::env::var("FORGEVCS_MERGE_ENGINE").as_deref(),
        Ok("exact-rename-v1")
    )
}

/// Deletions and additions of one side's diff, in ascending path order.
pub(crate) fn deletions_and_additions(
    changes: &[FileChange],
) -> (Vec<(RepoPath, EntryInfo)>, Vec<(RepoPath, EntryInfo)>) {
    let mut deleted = Vec::new();
    let mut added = Vec::new();
    for change in changes {
        match (&change.old, &change.new) {
            (Some(old), None) => deleted.push((change.path.clone(), *old)),
            (None, Some(new)) => added.push((change.path.clone(), *new)),
            _ => {}
        }
    }
    (deleted, added)
}

/// Detect exact and similar renames between one side's deletions and additions.
///
/// `contents` holds the text candidates' bytes (`None` = binary/redacted/too large); it only
/// needs entries for ids the caller wants considered for similarity.
pub(crate) fn detect_renames(
    deleted: &[(RepoPath, EntryInfo)],
    added: &[(RepoPath, EntryInfo)],
    contents: &BTreeMap<ObjectId, Option<Vec<u8>>>,
) -> Renames {
    let mut out = Renames::new();
    let mut used: BTreeSet<RepoPath> = BTreeSet::new();
    for (old_path, old) in deleted {
        let candidates: Vec<&(RepoPath, EntryInfo)> = added
            .iter()
            .filter(|(path, new)| new.id == old.id && !used.contains(path))
            .collect();
        // Only an unambiguous exact match is a rename; two adds with the same content stay
        // delete + add (the conservative behaviour).
        if let [only] = candidates.as_slice() {
            used.insert(only.0.clone());
            out.insert(old_path.clone(), (only.0.clone(), only.1));
        }
    }
    let mut similar: Vec<(f64, RepoPath, RepoPath, EntryInfo)> = Vec::new();
    for (old_path, old) in deleted {
        if out.contains_key(old_path) {
            continue;
        }
        let Some(Some(old_bytes)) = contents.get(&old.id) else {
            continue;
        };
        for (new_path, new) in added {
            if used.contains(new_path) {
                continue;
            }
            let Some(Some(new_bytes)) = contents.get(&new.id) else {
                continue;
            };
            let score = line_similarity(old_bytes, new_bytes);
            if score >= SIMILARITY_THRESHOLD {
                similar.push((score, old_path.clone(), new_path.clone(), *new));
            }
        }
    }
    similar.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    for (_, old_path, new_path, new) in similar {
        if out.contains_key(&old_path) || used.contains(&new_path) {
            continue;
        }
        used.insert(new_path.clone());
        out.insert(old_path, (new_path, new));
    }
    out
}

/// Line-set Jaccard similarity of two texts (0.0 for an empty union or NUL bytes).
fn line_similarity(a: &[u8], b: &[u8]) -> f64 {
    if is_binary(a) || is_binary(b) {
        return 0.0;
    }
    let lines = |bytes: &[u8]| -> BTreeSet<u64> {
        let mut set = BTreeSet::new();
        for line in bytes.split(|byte| *byte == b'\n') {
            let mut hasher = DefaultHasher::new();
            line.hash(&mut hasher);
            set.insert(hasher.finish());
        }
        set
    };
    let (left, right) = (lines(a), lines(b));
    let union = left.union(&right).count();
    if union == 0 {
        return 0.0;
    }
    left.intersection(&right).count() as f64 / union as f64
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|byte| *byte == 0)
}

impl History {
    /// Read the contents for similarity candidates: `None` when redacted, missing, binary or
    /// larger than the merge text limit.
    pub(crate) async fn rename_contents(
        &self,
        ids: &BTreeSet<ObjectId>,
    ) -> Result<BTreeMap<ObjectId, Option<Vec<u8>>>> {
        let max = self.cfg.limits.merge_max_text_bytes;
        let mut out = BTreeMap::new();
        for id in ids {
            let value = match self.objects.get(id).await {
                Ok(bytes) => {
                    if bytes.len() as u64 > max || is_binary(&bytes) {
                        None
                    } else {
                        Some(bytes.to_vec())
                    }
                }
                Err(_) => None,
            };
            out.insert(*id, value);
        }
        Ok(out)
    }
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

    fn contents(pairs: &[(u8, &[u8])]) -> BTreeMap<ObjectId, Option<Vec<u8>>> {
        pairs
            .iter()
            .map(|(byte, bytes)| (id(*byte), Some(bytes.to_vec())))
            .collect()
    }

    #[test]
    fn exact_rename_is_detected() {
        let deleted = vec![(path("old.txt"), entry(1))];
        let added = vec![(path("new.txt"), entry(1))];
        let renames = detect_renames(&deleted, &added, &BTreeMap::new());
        assert_eq!(
            renames.get(&path("old.txt")),
            Some(&(path("new.txt"), entry(1)))
        );
    }

    #[test]
    fn similar_rename_is_detected() {
        let deleted = vec![(path("old.txt"), entry(1))];
        let added = vec![(path("new.txt"), entry(2))];
        let contents = contents(&[
            (
                1,
                b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\niota\nkappa\n",
            ),
            (
                2,
                b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\niota\nkappa edited\n",
            ),
        ]);
        let renames = detect_renames(&deleted, &added, &contents);
        assert_eq!(
            renames.get(&path("old.txt")),
            Some(&(path("new.txt"), entry(2)))
        );
    }

    #[test]
    fn unrelated_contents_are_not_renames() {
        let deleted = vec![(path("old.txt"), entry(1))];
        let added = vec![(path("new.txt"), entry(2))];
        let contents = contents(&[(1, b"one\ntwo\nthree\n"), (2, b"red\ngreen\nblue\n")]);
        assert!(detect_renames(&deleted, &added, &contents).is_empty());
    }

    #[test]
    fn binary_candidates_are_not_renames() {
        let deleted = vec![(path("old.bin"), entry(1))];
        let added = vec![(path("new.bin"), entry(2))];
        let contents = contents(&[(1, b"\x00\x01\x02"), (2, b"\x00\x01\x02")]);
        assert!(detect_renames(&deleted, &added, &contents).is_empty());
    }

    #[test]
    fn ambiguous_additions_are_left_alone() {
        let deleted = vec![(path("old.txt"), entry(1))];
        let added = vec![(path("a.txt"), entry(1)), (path("b.txt"), entry(1))];
        assert!(detect_renames(&deleted, &added, &BTreeMap::new()).is_empty());
    }
}
