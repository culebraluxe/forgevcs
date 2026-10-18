//! SPEC 8.3 `Merge(a, b)` two-parent merge changesets (P19).
//!
//! The base is the unique best common ancestor of `a` and `b` (the empty tree when there is
//! none, PLAN 6.14; AMBIGUOUS_MERGE_BASE when there are several, e.g. a criss-cross,
//! SPEC Appendix A). The examined paths are those of `FileDiff(base, a)` and
//! `FileDiff(base, b)`, both computed with identical-subtree pruning, and each is resolved by
//! the same per-path SPEC 8.3 rules as rebase with ours = `a` and theirs = `b`.

use crate::conflict::{PathInput, TreeReader};
use crate::history::History;
use crate::hooks::WriteAuthorizer;
use crate::rename;
use crate::types::{FileChange, MetaFields, MetaSource};
use scion_core::{
    ChangeId, Conflict, ConflictKind, ConflictReport, EntryInfo, ErrorCode, ErrorPayload, ObjectId,
    Principal, Result, ScionError, empty_tree_id,
};
use scion_object::RepoPath;
use std::collections::{BTreeMap, BTreeSet};

/// Base, ours and theirs entries of one examined path, and whether theirs changed it.
struct Sides {
    b: Option<EntryInfo>,
    o: Option<EntryInfo>,
    t: Option<EntryInfo>,
    theirs_changed: bool,
}

impl History {
    /// SPEC 8.3 `Merge(a, b)`: merge `b` into `a` over their unique merge base.
    ///
    /// - AMBIGUOUS_MERGE_BASE when more than one best common ancestor exists; no common
    ///   ancestor (unrelated imported roots) merges against the empty tree (PLAN 6.14).
    /// - Every path of `FileDiff(base, a)` and `FileDiff(base, b)` is resolved by the SPEC 8.3
    ///   rules (structural `file_directory` for paths `b` changed, the table, `redacted`,
    ///   `binary`, content merge); all conflicts are returned as MERGE_CONFLICT with a
    ///   `ConflictReport`.
    /// - PERMISSION_DENIED unless `authz.can_write` holds for every path of the resulting delta
    ///   against `a` (SPEC 12.4).
    /// - The result has parents `[a, b]`, a new Meta built from `meta` (creator = `who`), and
    ///   `change_id`; it is persisted by the CreateChangeset path (SPEC 5.3).
    pub async fn merge(
        &self,
        a: &ObjectId,
        b: &ObjectId,
        meta: MetaFields,
        change_id: ChangeId,
        who: &Principal,
        authz: &dyn WriteAuthorizer,
    ) -> Result<ObjectId> {
        let ra = self.changeset(a).await?;
        let rb = self.changeset(b).await?;
        let bases = self.merge_bases(a, b).await?;
        let base_root = match bases.as_slice() {
            [] => empty_tree_id(),
            [base] => self.changeset(base).await?.changeset.root_tree,
            many => {
                return Err(ScionError::new(
                    ErrorCode::AmbiguousMergeBase,
                    format!("{a} and {b} have {} best common ancestors", many.len()),
                ));
            }
        };
        let (o_root, t_root) = (ra.changeset.root_tree, rb.changeset.root_tree);

        let ours = self.file_diff(Some(&base_root), &o_root, None).await?;
        let theirs = self.file_diff(Some(&base_root), &t_root, None).await?;
        let mut sides: BTreeMap<RepoPath, Sides> = BTreeMap::new();
        for c in &ours {
            sides.insert(
                c.path.clone(),
                Sides {
                    b: c.old,
                    o: c.new,
                    t: c.old,
                    theirs_changed: false,
                },
            );
        }
        for c in &theirs {
            match sides.get_mut(&c.path) {
                Some(s) => {
                    s.t = c.new;
                    s.theirs_changed = true;
                }
                None => {
                    sides.insert(
                        c.path.clone(),
                        Sides {
                            b: c.old,
                            o: c.old,
                            t: c.new,
                            theirs_changed: true,
                        },
                    );
                }
            }
        }

        let mut reader = TreeReader::new(self);
        let mut inputs = Vec::with_capacity(sides.len());
        for (path, s) in sides {
            let structural = if s.theirs_changed {
                reader
                    .probe(&o_root, &path)
                    .await?
                    .structural_conflict(s.t.as_ref())
            } else {
                false
            };
            inputs.push(PathInput {
                path,
                b: s.b,
                o: s.o,
                t: s.t,
                structural,
            });
        }
        drop(reader);

        // Opt-in rename engine: re-anchor edits across exact and similar renames on either side.
        let (inputs, forced_adds) = if rename::rename_engine_enabled() {
            self.apply_merge_renames(&ours, &theirs, &o_root, inputs)
                .await?
        } else {
            (inputs, BTreeSet::new())
        };

        let mut resolution = self.resolve_paths(inputs).await?;
        for path in forced_adds {
            resolution.ours.insert(path, None);
        }
        if let Some(err) = resolution.conflict_error("merge") {
            return Err(err);
        }
        let delta = resolution.delta();
        if let Some(i) = delta.iter().position(|e| !authz.can_write(&e.path)) {
            return Err(ScionError::permission_denied(format!(
                "no write permission for merge delta entry {i}"
            )));
        }
        let created = self
            .create_internal(vec![*a, *b], delta, MetaSource::New(meta), change_id, who)
            .await?;
        tracing::debug!(ours = %a, theirs = %b, result = %created.changeset_id, "merged");
        Ok(created.changeset_id)
    }
}

impl History {
    /// Stage 2 of the rename engine for `Merge(a, b)`: use the two sides' diffs to move edits
    /// onto rename destinations (see `rename.rs`). The transform mirrors the rebase one with the
    /// merge orientation: `ours = a`, `theirs = b`.
    async fn apply_merge_renames(
        &self,
        ours_diff: &[FileChange],
        theirs_diff: &[FileChange],
        o_root: &ObjectId,
        mut inputs: Vec<PathInput>,
    ) -> Result<(Vec<PathInput>, BTreeSet<RepoPath>)> {
        let (our_deleted, our_added) = rename::deletions_and_additions(ours_diff);
        let (their_deleted, their_added) = rename::deletions_and_additions(theirs_diff);
        let max = self.cfg.limits.merge_max_text_bytes;
        let mut ids = BTreeSet::new();
        for (_, entry) in our_deleted.iter().chain(&their_deleted) {
            if entry.size <= max {
                ids.insert(entry.id);
            }
        }
        for (_, entry) in our_added.iter().chain(&their_added) {
            if entry.size <= max {
                ids.insert(entry.id);
            }
        }
        let contents = self.rename_contents(&ids).await?;
        let our = rename::detect_renames(&our_deleted, &our_added, &contents);
        let their = rename::detect_renames(&their_deleted, &their_added, &contents);

        let base_of = |inputs: &[PathInput], path: &RepoPath| {
            inputs
                .iter()
                .find(|input| &input.path == path)
                .and_then(|input| input.b)
        };

        for (old, (ours_target, ours_entry)) in &our {
            if let Some((theirs_target, theirs_entry)) = their.get(old)
                && ours_target != theirs_target
            {
                let base_entry = base_of(&inputs, old);
                let mut conflicts = Vec::new();
                for (path, ours, theirs) in [
                    (ours_target, ours_entry, theirs_entry),
                    (theirs_target, ours_entry, theirs_entry),
                ] {
                    conflicts.push(Conflict {
                        path: path.as_bytes().to_vec(),
                        kind: ConflictKind::AddAdd,
                        base: base_entry,
                        ours: Some(*ours),
                        theirs: Some(*theirs),
                        hunks: Vec::new(),
                    });
                }
                return Err(ScionError::new(
                    ErrorCode::MergeConflict,
                    "merge: rename/rename conflict",
                )
                .with_payload(ErrorPayload::MergeConflict(Box::new(
                    ConflictReport {
                        clean_entries: Vec::new(),
                        conflicts,
                    },
                ))));
            }
        }

        let position: BTreeMap<RepoPath, usize> = inputs
            .iter()
            .enumerate()
            .map(|(index, input)| (input.path.clone(), index))
            .collect();

        // Theirs renamed: delete P relative to a and resolve the edit at Q with base = P.
        let mut forced_adds = BTreeSet::new();
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

        // Ours renamed: P disappears relative to a (no delta) and b's edit moves to Q.
        let mut reader = TreeReader::new(self);
        for (old, (target, _)) in &our {
            let (Some(&old_index), Some(&target_index)) = (position.get(old), position.get(target))
            else {
                continue;
            };
            let (Some(base_entry), Some(theirs_entry)) = (inputs[old_index].b, inputs[old_index].t)
            else {
                continue;
            };
            let target_o = inputs[target_index].o;
            let probe = reader.probe(o_root, target).await?;
            let structural = probe.structural_conflict(Some(&theirs_entry));
            inputs[old_index] = PathInput {
                path: old.clone(),
                b: None,
                o: None,
                t: None,
                structural: false,
            };
            inputs[target_index] = PathInput {
                path: target.clone(),
                b: Some(base_entry),
                o: target_o,
                t: Some(theirs_entry),
                structural,
            };
        }
        drop(reader);
        Ok((inputs, forced_adds))
    }
}
