//! SPEC 8.2 / 18.3 deterministic rebase (P19).
//!
//! `Rebase(C, X)` with `P = C.parents[0]`, `B = tree(P)`, `O = tree(X)`, `T = tree(C)`:
//! only the paths of `C.delta` can differ between the result and `O`, so exactly those paths
//! and their ancestor directories in `B` and `O` are read (O(|delta| x depth) trees; no blob is
//! read unless a path needs a content merge). The result reuses `C`'s Meta and change id and
//! gets the recomputed delta against `O`; nothing time-dependent enters it, so identical
//! requests produce identical identifiers.

use std::collections::BTreeMap;

use crate::conflict::{PathInput, TreeReader, op_entry};
use crate::history::History;
use crate::hooks::Rebaser;
use crate::rename;
use crate::types::MetaSource;
use async_trait::async_trait;
use scion_core::{
    Conflict, ConflictKind, ConflictReport, EntryInfo, ErrorCode, ErrorPayload, ObjectId,
    Principal, Result, ScionError, subcode,
};
use scion_object::{DeltaEntry, DeltaOp, RepoPath};

impl History {
    /// SPEC 8.2 / 18.3: rebase `c` onto `onto`.
    ///
    /// - INVALID_ARGUMENT/multi_parent unless `c` has exactly one parent (v1).
    /// - Fast path: `onto == c.parents[0]` returns `c` itself.
    /// - Per delta path the SPEC 8.3 rules (structural `file_directory` first, then the table,
    ///   `redacted`, `binary` and the `line-diff3-v1` content merge). Every conflict is
    ///   collected and returned as MERGE_CONFLICT with a `ConflictReport` (`clean_entries` for
    ///   every non-conflicting delta path). Clean content merges are stored as new blobs;
    ///   conflict markers are never stored.
    /// - EMPTY_CHANGE when the recomputed delta against `tree(onto)` is empty.
    /// - Otherwise the changeset `{parents: [onto], meta: c.meta, change_id: c.change_id}` is
    ///   created through the internal CreateChangeset path. A pure function of
    ///   `(c, onto, merge engine version)`: no wall-clock value enters the result.
    pub async fn rebase(&self, c: &ObjectId, onto: &ObjectId, who: &Principal) -> Result<ObjectId> {
        let rec = self.changeset(c).await?;
        let cs = rec.changeset;
        let p = match cs.parents.as_slice() {
            [p] => *p,
            _ => {
                return Err(ScionError::invalid(
                    subcode::MULTI_PARENT,
                    format!(
                        "rebase requires exactly one parent; the changeset has {}",
                        cs.parents.len()
                    ),
                ));
            }
        };
        if *onto == p {
            return Ok(*c);
        }
        let x = self.changeset(onto).await?;
        let base = self.changeset(&p).await?;
        let (b_root, o_root) = (base.changeset.root_tree, x.changeset.root_tree);

        let mut reader = TreeReader::new(self);
        let paths: Vec<&RepoPath> = cs.delta.iter().map(|e| &e.path).collect();
        reader.prefetch(&[b_root, o_root], &paths).await?;
        let mut inputs = Vec::with_capacity(cs.delta.len());
        for e in &cs.delta {
            let t = op_entry(&e.op);
            let b = reader.probe(&b_root, &e.path).await?.file;
            let o_probe = reader.probe(&o_root, &e.path).await?;
            inputs.push(PathInput {
                path: e.path.clone(),
                b,
                o: o_probe.file,
                t,
                structural: o_probe.structural_conflict(t.as_ref()),
            });
        }
        drop(reader);

        // Exact-content rename awareness (opt-in engine version). A rename on the onto side or
        // in this changeset would otherwise turn an edit into a false modify/delete conflict.
        let (inputs, forced_adds) = if rename::rename_engine_enabled() {
            self.apply_renames(&b_root, &o_root, &cs.delta, inputs)
                .await?
        } else {
            (inputs, std::collections::BTreeSet::new())
        };

        let mut resolution = self.resolve_paths(inputs).await?;
        // A re-anchored path that does not exist on the onto side is an add relative to it; the
        // synthesized content is the merged entry, so the delta baseline must be absent.
        for path in forced_adds {
            resolution.ours.insert(path, None);
        }
        if let Some(err) = resolution.conflict_error("rebase") {
            return Err(err);
        }
        let delta = resolution.delta();
        if delta.is_empty() {
            return Err(ScionError::new(
                ErrorCode::EmptyChange,
                format!("the change of {c} is already present in {onto}"),
            ));
        }
        let created = self
            .create_internal(
                vec![*onto],
                delta,
                MetaSource::Existing(cs.meta),
                cs.change_id,
                who,
            )
            .await?;
        tracing::debug!(source = %c, onto = %onto, result = %created.changeset_id, "rebased");
        Ok(created.changeset_id)
    }
}

impl History {
    /// The opt-in rename engine: move edits onto rename destinations (exact and similar
    /// renames, see `rename.rs`).
    ///
    /// Two orientations are handled:
    ///
    /// - **theirs renamed** (`C` deletes `P` and adds `Q`): the edit at `P` is resolved at `Q`
    ///   with `base = P`, and `P` is deleted relative to the onto tree.
    /// - **ours renamed** (`onto` deleted `P` and added `Q`): the edit at `P` is re-anchored to
    ///   `Q`, where the onto side supplies the current content.
    ///
    /// A rename/rename to different destinations is an explicit `add_add` conflict rather than a
    /// silent duplicate. Everything else is left to the SPEC 8.3 rules.
    async fn apply_renames(
        &self,
        b_root: &ObjectId,
        o_root: &ObjectId,
        delta: &[DeltaEntry],
        mut inputs: Vec<PathInput>,
    ) -> Result<(Vec<PathInput>, std::collections::BTreeSet<RepoPath>)> {
        let mut base: BTreeMap<RepoPath, EntryInfo> = BTreeMap::new();
        for input in &inputs {
            if let Some(entry) = input.b {
                base.insert(input.path.clone(), entry);
            }
        }

        // Theirs: a Delete + an added Put are rename candidates.
        let mut their_deleted = Vec::new();
        let mut their_added = Vec::new();
        for entry in delta {
            match entry.op {
                DeltaOp::Delete => {
                    if let Some(base_entry) = base.get(&entry.path) {
                        their_deleted.push((entry.path.clone(), *base_entry));
                    }
                }
                DeltaOp::Put { mode, id, size } => {
                    if !base.contains_key(&entry.path) {
                        their_added.push((entry.path.clone(), EntryInfo { mode, id, size }));
                    }
                }
            }
        }

        let ours_candidate = inputs
            .iter()
            .any(|input| input.b.is_some() && input.o.is_none() && input.t.is_some());
        let needs_ours = ours_candidate
            || their_added
                .iter()
                .any(|(target, _)| inputs.iter().any(|i| &i.path == target && i.o.is_some()));
        let (mut our_deleted, mut our_added) = (Vec::new(), Vec::new());
        if needs_ours {
            let diff = self.file_diff(Some(b_root), o_root, None).await?;
            (our_deleted, our_added) = rename::deletions_and_additions(&diff);
        }

        let max = self.cfg.limits.merge_max_text_bytes;
        let mut ids = std::collections::BTreeSet::new();
        for (_, entry) in their_deleted.iter().chain(&our_deleted) {
            if entry.size <= max {
                ids.insert(entry.id);
            }
        }
        for (_, entry) in their_added.iter().chain(&our_added) {
            if entry.size <= max {
                ids.insert(entry.id);
            }
        }
        let contents = self.rename_contents(&ids).await?;
        let their = rename::detect_renames(&their_deleted, &their_added, &contents);
        let our = rename::detect_renames(&our_deleted, &our_added, &contents);

        for (old, (ours_target, ours_entry)) in &our {
            if let Some((theirs_target, theirs_entry)) = their.get(old)
                && ours_target != theirs_target
            {
                let base_entry = base.get(old).copied();
                let conflict = |path: &RepoPath, ours: &EntryInfo, theirs: &EntryInfo| Conflict {
                    path: path.as_bytes().to_vec(),
                    kind: ConflictKind::AddAdd,
                    base: base_entry,
                    ours: Some(*ours),
                    theirs: Some(*theirs),
                    hunks: Vec::new(),
                };
                let report = ConflictReport {
                    clean_entries: Vec::new(),
                    conflicts: vec![
                        conflict(ours_target, ours_entry, theirs_entry),
                        conflict(theirs_target, ours_entry, theirs_entry),
                    ],
                };
                return Err(ScionError::new(
                    ErrorCode::MergeConflict,
                    "rebase: rename/rename conflict",
                )
                .with_payload(ErrorPayload::MergeConflict(Box::new(report))));
            }
        }

        let position: BTreeMap<RepoPath, usize> = inputs
            .iter()
            .enumerate()
            .map(|(index, input)| (input.path.clone(), index))
            .collect();

        let mut forced_adds = std::collections::BTreeSet::new();
        for (old, (target, target_entry)) in &their {
            let (Some(&old_index), Some(&target_index)) = (position.get(old), position.get(target))
            else {
                continue;
            };
            let (Some(base_entry), Some(ours_entry)) = (inputs[old_index].b, inputs[old_index].o)
            else {
                continue;
            };
            if inputs[target_index].o.is_none() {
                forced_adds.insert(target.clone());
            }
            inputs[old_index] = PathInput {
                path: old.clone(),
                b: Some(ours_entry),
                o: Some(ours_entry),
                t: None,
                structural: false,
            };
            inputs[target_index] = PathInput {
                path: target.clone(),
                b: Some(base_entry),
                o: Some(ours_entry),
                t: Some(*target_entry),
                structural: false,
            };
        }

        let mut reader = TreeReader::new(self);
        for (old, (target, _)) in &our {
            let Some(&index) = position.get(old) else {
                continue;
            };
            let (Some(base_entry), None, Some(theirs_entry)) =
                (inputs[index].b, inputs[index].o, inputs[index].t)
            else {
                continue;
            };
            if position.contains_key(target) {
                continue;
            }
            let probe = reader.probe(o_root, target).await?;
            inputs[index] = PathInput {
                path: target.clone(),
                b: Some(base_entry),
                o: probe.file,
                t: Some(theirs_entry),
                structural: probe.structural_conflict(Some(&theirs_entry)),
            };
        }
        drop(reader);
        Ok((inputs, forced_adds))
    }
}

/// SPEC 8.4: Land rebases stack members through this seam; it is [`History::rebase`].
#[async_trait]
impl Rebaser for History {
    async fn rebase(&self, c: &ObjectId, onto: &ObjectId, who: &Principal) -> Result<ObjectId> {
        History::rebase(self, c, onto, who).await
    }
}
