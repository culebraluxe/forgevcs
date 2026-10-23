//! SPEC 19.5 / 19.7 / 19.12 / 15.2 via RPC: Land (P26).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod write_support;

use scion_core::{ErrorCode, ErrorPayload, FaultAction, ObjectId, points, subcode};
use scion_history::{LandMergeRequest, LandResult};
use scion_object::RefName;
use scion_proto::{ScionClient, v1};
use scion_server::CallerCtx;
use scion_server::authz::PolicyLandAuthzSource;
use scion_server::handlers;
use scion_server::testing::{InProcessServer, ServerTestOpts};
use std::sync::Arc;
use std::time::{Duration, Instant};
use write_support::*;

async fn land(
    c: &ScionClient,
    r: &str,
    head: &ObjectId,
    allow_empty: bool,
) -> Result<v1::LandResponse, tonic::Status> {
    c.write()
        .land(v1::LandRequest {
            r#ref: r.into(),
            head: head.as_bytes().to_vec(),
            allow_empty,
        })
        .await
        .map(|r| r.into_inner())
}

fn landed_ids(resp: &v1::LandResponse) -> Vec<(Vec<u8>, ObjectId, ObjectId)> {
    resp.landed
        .iter()
        .map(|l| (l.change_id.clone(), id(&l.source), id(&l.result)))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn land_happy_path_and_ref_log() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let metrics = srv.ctx().metrics.clone();
    let committed_before = metrics
        .land_attempts
        .with_label_values(&["committed"])
        .get();
    let durations_before = metrics.land_duration.get_sample_count();

    // A change on the tip lands as itself (rebase fast path).
    let c1 = commit(&srv, &c, &r, &[("one", b"1\n")], 1).await;
    let resp = land(&c, "main", &c1, false).await.unwrap();
    assert_eq!(id(&resp.target), c1);
    assert_eq!(resp.version, 2);
    assert_eq!(landed_ids(&resp), vec![(cid(1), c1, c1)]);

    // A two-member stack based on the old tip is rebased onto the new tip.
    let s1 = commit(&srv, &c, &r, &[("two", b"2\n")], 2).await;
    let s2 = commit(&srv, &c, &s1, &[("three", b"3\n")], 3).await;
    let resp = land(&c, "main", &s2, false).await.unwrap();
    assert_eq!(resp.version, 3);
    let landed = landed_ids(&resp);
    assert_eq!(landed.len(), 2);
    assert_eq!(landed[0].0, cid(2));
    assert_eq!(landed[0].1, s1);
    assert_eq!(landed[1].1, s2);
    assert_eq!(id(&resp.target), landed[1].2);
    let tip = srv
        .ctx()
        .history
        .changeset(&id(&resp.target))
        .await
        .unwrap();
    assert_eq!(tip.changeset.parents, vec![landed[0].2]);
    let h = &srv.ctx().history;
    for p in ["one", "two", "three"] {
        let path = scion_object::RepoPath::parse(p.as_bytes()).unwrap();
        assert!(
            h.lookup(&tip.changeset.root_tree, &path)
                .await
                .unwrap()
                .is_some()
        );
    }
    assert_eq!(ref_state(&srv, "main").await, (id(&resp.target), 3));

    // The ref log records both lands with their landed rows.
    let log = c
        .refs()
        .get_ref_log(v1::GetRefLogRequest {
            name: "main".into(),
            from_version: 2,
            limit: 0,
        })
        .await
        .unwrap()
        .into_inner()
        .entries;
    assert_eq!(
        log.iter().map(|e| e.version).collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(log[0].old_target, r.as_bytes().to_vec());
    assert_eq!(log[0].new_target, c1.as_bytes().to_vec());
    assert_eq!(log[1].landed.len(), 2);
    assert_eq!(log[1].principal, "github:alice");

    // The land metrics moved (SPEC 15.2).
    assert_eq!(
        metrics
            .land_attempts
            .with_label_values(&["committed"])
            .get(),
        committed_before + 2
    );
    assert_eq!(
        metrics.land_duration.get_sample_count(),
        durations_before + 2
    );
}

/// SPEC 19.5 idempotency: the CAS commits but the response is lost; the retry returns the
/// recorded result and does not create a second ref version.
#[tokio::test(flavor = "multi_thread")]
async fn land_retry_after_lost_response_returns_recorded_result() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let first = commit(&srv, &c, &r, &[("first", b"f\n")], 1).await;
    land(&c, "main", &first, false).await.unwrap();
    let mine = commit(&srv, &c, &r, &[("mine", b"m\n")], 2).await;

    srv.storage
        .faults
        .arm("meta.cas_ref.after", FaultAction::FailAfter, Some(1));
    let e = land(&c, "main", &mine, false).await.unwrap_err();
    let got = expect_err(&e, ErrorCode::Unavailable, Some(subcode::INJECTED));
    assert!(got.retryable());
    srv.storage.faults.disarm_all();
    let (tip, version) = ref_state(&srv, "main").await;
    assert_eq!(version, 3, "the CAS committed before the failure");

    let idempotent_before = srv
        .ctx()
        .metrics
        .land_attempts
        .with_label_values(&["idempotent"])
        .get();
    let retry = land(&c, "main", &mine, false).await.unwrap();
    assert_eq!(id(&retry.target), tip);
    assert_eq!(retry.version, 3);
    assert_eq!(landed_ids(&retry), vec![(cid(2), mine, tip)]);
    assert_eq!(ref_state(&srv, "main").await, (tip, 3));
    let log = srv
        .ctx()
        .history
        .ref_log(&RefName::main(), 0, 100)
        .await
        .unwrap();
    assert_eq!(log.len(), 3);
    assert_eq!(
        srv.ctx()
            .metrics
            .land_attempts
            .with_label_values(&["idempotent"])
            .get(),
        idempotent_before + 1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn amended_change_gives_already_landed_payload() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let orig = commit(&srv, &c, &r, &[("f", b"v1\n")], 7).await;
    let landed = land(&c, "main", &orig, false).await.unwrap();
    let amended = commit(&srv, &c, &r, &[("f", b"v2\n")], 7).await;
    let e = land(&c, "main", &amended, false).await.unwrap_err();
    let got = expect_err(&e, ErrorCode::AlreadyLanded, None);
    assert!(!got.retryable());
    assert_eq!(
        got.payload,
        ErrorPayload::AlreadyLanded {
            source: orig,
            result: id(&landed.target),
            version: landed.version,
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn land_conflict_empty_change_and_allow_empty() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let a = commit(&srv, &c, &r, &[("f", b"a\n"), ("same", b"s\n")], 1).await;
    land(&c, "main", &a, false).await.unwrap();
    let (tip, v) = ref_state(&srv, "main").await;

    // MERGE_CONFLICT (add/add on f), with clean entries.
    let conflicting = commit(&srv, &c, &r, &[("f", b"b\n"), ("g", b"g\n")], 2).await;
    let e = land(&c, "main", &conflicting, false).await.unwrap_err();
    let got = expect_err(&e, ErrorCode::MergeConflict, None);
    let ErrorPayload::MergeConflict(report) = got.payload else {
        panic!("expected MergeConflict payload");
    };
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.clean_entries.len(), 1);

    // EMPTY_CHANGE: the content is already on the ref.
    let dup = commit(&srv, &c, &r, &[("same", b"s\n")], 3).await;
    let e = land(&c, "main", &dup, false).await.unwrap_err();
    expect_err(&e, ErrorCode::EmptyChange, None);
    assert_eq!(ref_state(&srv, "main").await, (tip, v));

    // allow_empty: recorded with result = the current tip (PLAN 6.14).
    let resp = land(&c, "main", &dup, true).await.unwrap();
    assert_eq!(landed_ids(&resp), vec![(cid(3), dup, tip)]);
    assert_eq!(id(&resp.target), tip);
    // A retry is idempotent.
    let again = land(&c, "main", &dup, true).await.unwrap();
    assert_eq!(again.version, resp.version);
    assert_eq!(landed_ids(&again), landed_ids(&resp));
}

#[tokio::test(flavor = "multi_thread")]
async fn land_enforces_ref_update_and_path_write() {
    let srv = InProcessServer::start(opts_with_acl(RESTRICTED_ACL)).await;
    let r = root(&srv).await;
    let admin_c = srv.client(Some(&admin())).await;
    let alice_c = srv.client(Some(&alice())).await;

    // Write on every delta path of every rebased member.
    let secret = commit(&srv, &admin_c, &r, &[("secrets/k", b"k\n")], 1).await;
    let e = land(&alice_c, "main", &secret, false).await.unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    assert_eq!(ref_state(&srv, "main").await, (r, 1));
    land(&admin_c, "main", &secret, false).await.unwrap();

    // `update` on the ref: release/* is admins-only.
    admin_c
        .refs()
        .create_ref(v1::CreateRefRequest {
            name: "release/1".into(),
            target: r.as_bytes().to_vec(),
            reason: "branch cut".into(),
        })
        .await
        .unwrap();
    let pubc = commit(&srv, &alice_c, &r, &[("pub", b"p\n")], 2).await;
    let e = land(&alice_c, "release/1", &pubc, false).await.unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    land(&admin_c, "release/1", &pubc, false).await.unwrap();

    // A reader without update is denied on main.
    let reader = srv.client(Some(&gh("reader"))).await;
    let e = land(&reader, "main", &pubc, false).await.unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
}

/// SPEC 8.4: the policy is re-read on each land; a revoked grant applies to the next land.
#[tokio::test(flavor = "multi_thread")]
async fn land_uses_the_live_policy() {
    let srv = InProcessServer::start(opts_with_acl(RESTRICTED_ACL)).await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let x = commit(&srv, &c, &r, &[("x", b"x\n")], 1).await;
    srv.set_policy(&RESTRICTED_ACL.replace(
        "[refs.\"main\"]\nupdate = [\"group:eng\"]",
        "[refs.\"main\"]\nupdate = [\"group:admins\"]",
    ))
    .unwrap();
    let e = land(&c, "main", &x, false).await.unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    srv.set_policy(RESTRICTED_ACL).unwrap();
    land(&c, "main", &x, false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn land_argument_errors() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let x = commit(&srv, &c, &r, &[("x", b"x\n")], 1).await;
    let e = land(&c, "", &x, false).await.unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::MISSING_FIELD));
    let e = land(&c, "bad..name", &x, false).await.unwrap_err();
    expect_err(
        &e,
        ErrorCode::InvalidArgument,
        Some(subcode::INVALID_REF_NAME),
    );
    let e = c
        .write()
        .land(v1::LandRequest {
            r#ref: "main".into(),
            head: Vec::new(),
            allow_empty: false,
        })
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::MISSING_FIELD));
    let e = land(&c, "no-such-ref", &x, false).await.unwrap_err();
    expect_err(&e, ErrorCode::NotFound, None);
    // A stack containing a merge changeset.
    let y = commit(&srv, &c, &r, &[("y", b"y\n")], 2).await;
    let m = c
        .write()
        .merge(v1::MergeRequest {
            a: x.as_bytes().to_vec(),
            b: y.as_bytes().to_vec(),
            meta_fields: Some(meta("merge")),
            change_id: cid(3),
        })
        .await
        .unwrap()
        .into_inner();
    let e = land(&c, "main", &id(&m.changeset_id), false)
        .await
        .unwrap_err();
    expect_err(
        &e,
        ErrorCode::InvalidArgument,
        Some(subcode::MERGE_IN_STACK),
    );
}

/// Concurrent non-overlapping lands through the RPC all land (lost CAS races retry); the ref
/// log has one entry per land and no gaps (SPEC 19.5, scaled down; the N = 10/100/1000 suite
/// is P30's).
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_lands_all_succeed() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let n = 16u32;
    let mut heads = Vec::new();
    for i in 0..n {
        let path = format!("file{i}");
        heads.push(commit(&srv, &c, &r, &[(path.as_str(), b"content\n")], 100 + i).await);
    }
    let mut tasks = Vec::new();
    for h in heads {
        let c = c.clone();
        tasks.push(tokio::spawn(
            async move { land(&c, "main", &h, false).await },
        ));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    let (_, v) = ref_state(&srv, "main").await;
    assert_eq!(v, 1 + u64::from(n));
    let log = srv
        .ctx()
        .history
        .ref_log(&RefName::main(), 0, 1000)
        .await
        .unwrap();
    let versions: Vec<u64> = log.iter().map(|e| e.version).collect();
    assert_eq!(versions, (1..=1 + u64::from(n)).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread")]
async fn land_rpc_event_has_land_fields() {
    let capture = scion_obs::capture::LogCapture::install();
    let repo = format!("landlog-{}", uuid::Uuid::new_v4().simple());
    let repo2 = repo.clone();
    let base = opts();
    let base_tweak = base.tweak.clone().unwrap();
    let srv = InProcessServer::start(ServerTestOpts {
        tweak: Some(Arc::new(move |cfg| {
            base_tweak(cfg);
            cfg.server.repo = repo2.clone();
        })),
        ..base
    })
    .await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let x = commit(&srv, &c, &r, &[("x", b"x\n")], 1).await;
    let resp = land(&c, "main", &x, false).await.unwrap();
    let find = || {
        capture.events().into_iter().find(|e| {
            e["target"] == "scion::rpc" && e["repo"] == repo.as_str() && e["rpc"] == "Land"
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let ev = loop {
        if let Some(ev) = find() {
            break ev;
        }
        assert!(Instant::now() < deadline, "no Land event");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(ev["status"], "OK");
    assert_eq!(ev["ref"], "main");
    assert_eq!(ev["ref_version"], resp.version);
    assert_eq!(ev["land_attempt"], 1);
    assert_eq!(ev["changeset_id"], id(&resp.target).to_text());
    assert_eq!(ev["principal"], "github:alice");
}

/// Pause the next land for 1.5 s after its rebase step, before its CAS, so a concurrent writer
/// can move the ref (pair with [`wait_for_hits`]).
fn park_next_land_after_rebase(srv: &InProcessServer) {
    srv.storage.faults.arm(
        points::LAND_AFTER_REBASE,
        FaultAction::Delay(Duration::from_millis(1500)),
        Some(1),
    );
}

async fn wait_for_hits(srv: &InProcessServer, point: &str, at_least: u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while srv.storage.faults.hits(point) < at_least {
        assert!(Instant::now() < deadline, "checkpoint {point} not reached");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Move `main` from version 1 to `target` (a fast-forward) through UpdateRef.
async fn advance_main(c: &ScionClient, target: &ObjectId) {
    c.refs()
        .update_ref(v1::UpdateRefRequest {
            name: "main".into(),
            expected_version: 1,
            new_target: target.as_bytes().to_vec(),
            allow_non_fast_forward: false,
        })
        .await
        .unwrap();
}

/// SPEC 19.5 / 8.4: authorization is re-checked on every attempt. A land loses its CAS to a
/// concurrent update, the policy revoking the lander's `update` grant is applied meanwhile, and
/// the retry fails PERMISSION_DENIED without touching the ref.
#[tokio::test(flavor = "multi_thread")]
async fn policy_revoked_mid_retry_fails_the_land() {
    let srv = InProcessServer::start(opts_with_acl(RESTRICTED_ACL)).await;
    let r = root(&srv).await;
    let alice_c = srv.client(Some(&alice())).await;
    let bob_c = srv.client(Some(&bob())).await;
    let mine = commit(&srv, &alice_c, &r, &[("mine", b"m\n")], 1).await;
    let other = commit(&srv, &bob_c, &r, &[("other", b"o\n")], 2).await;
    let metrics = srv.ctx().metrics.clone();
    let cas_before = metrics
        .land_attempts
        .with_label_values(&["cas_mismatch"])
        .get();
    let failed_before = metrics.land_attempts.with_label_values(&["failed"]).get();

    let hits = srv.storage.faults.hits(points::LAND_AFTER_REBASE);
    park_next_land_after_rebase(&srv);
    let task = {
        let c = alice_c.clone();
        tokio::spawn(async move { land(&c, "main", &mine, false).await })
    };
    wait_for_hits(&srv, points::LAND_AFTER_REBASE, hits + 1).await;
    advance_main(&bob_c, &other).await;
    srv.set_policy(&RESTRICTED_ACL.replace(
        "[refs.\"main\"]\nupdate = [\"group:eng\"]",
        "[refs.\"main\"]\nupdate = [\"github:bob\"]",
    ))
    .unwrap();

    let e = task.await.unwrap().unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    assert_eq!(ref_state(&srv, "main").await, (other, 2));
    assert_eq!(
        metrics
            .land_attempts
            .with_label_values(&["cas_mismatch"])
            .get(),
        cas_before + 1,
        "the first attempt lost the CAS"
    );
    assert_eq!(
        metrics.land_attempts.with_label_values(&["failed"]).get(),
        failed_before + 1
    );
    // Restoring the grant lets the same land through.
    srv.set_policy(RESTRICTED_ACL).unwrap();
    land(&alice_c, "main", &mine, false).await.unwrap();
}

/// SPEC 19.5 / PLAN 6.7: `land.max_attempts` exhaustion returns UNAVAILABLE/land_contention
/// (retryable) and leaves the ref as the competing writer left it.
#[tokio::test(flavor = "multi_thread")]
async fn max_attempts_exhaustion_is_retryable_and_leaves_ref_unchanged() {
    let base = opts();
    let base_tweak = base.tweak.clone().unwrap();
    let srv = InProcessServer::start(ServerTestOpts {
        tweak: Some(Arc::new(move |cfg| {
            base_tweak(cfg);
            cfg.land.max_attempts = 1;
        })),
        ..base
    })
    .await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let mine = commit(&srv, &c, &r, &[("mine", b"m\n")], 1).await;
    let other = commit(&srv, &c, &r, &[("other", b"o\n")], 2).await;
    let cas_failures_before = srv
        .ctx()
        .metrics
        .ref_cas_failures
        .with_label_values(&["main"])
        .get();

    let hits = srv.storage.faults.hits(points::LAND_AFTER_REBASE);
    park_next_land_after_rebase(&srv);
    let task = {
        let c = c.clone();
        tokio::spawn(async move { land(&c, "main", &mine, false).await })
    };
    wait_for_hits(&srv, points::LAND_AFTER_REBASE, hits + 1).await;
    advance_main(&c, &other).await;

    let e = task.await.unwrap().unwrap_err();
    let got = expect_err(&e, ErrorCode::Unavailable, Some(subcode::LAND_CONTENTION));
    assert!(got.retryable());
    assert_eq!(ref_state(&srv, "main").await, (other, 2));
    assert!(
        srv.ctx()
            .history
            .ref_log(&RefName::main(), 3, 10)
            .await
            .unwrap()
            .is_empty(),
        "no ref version beyond the competing update"
    );
    assert_eq!(
        srv.ctx()
            .metrics
            .ref_cas_failures
            .with_label_values(&["main"])
            .get(),
        cas_failures_before + 1
    );
    // The client's retry lands on the new tip.
    let resp = land(&c, "main", &mine, false).await.unwrap();
    assert_eq!(resp.version, 3);
}

/// SPEC 12.6 applied to conflict reports, local ForgeVCS patch (task-87): a lander that can
/// WRITE a conflicting path but cannot READ it must not receive a partial MERGE_CONFLICT report
/// (it could not resolve what it cannot see), so the operation fails closed with a generic
/// PERMISSION_DENIED that names neither path nor content. A principal that can read everything
/// still gets the complete report.
#[tokio::test(flavor = "multi_thread")]
async fn land_conflict_fails_closed_when_a_writable_path_is_unreadable() {
    // Write on /secrets for alice, read only for admin; additionally let admin read/write root
    // so the read-all assertion below sees the complete report.
    let acl = RESTRICTED_ACL
        .replace(
            "prefix = \"/secrets\"\nread = [\"github:admin\"]\nwrite = [\"github:admin\"]",
            "prefix = \"/secrets\"\nread = [\"github:admin\"]\nwrite = [\"github:admin\", \"github:alice\"]",
        )
        .replace(
            "prefix = \"/\"\nread = [\"group:readers\"]\nwrite = [\"group:eng\"]",
            "prefix = \"/\"\nread = [\"group:readers\", \"github:admin\"]\nwrite = [\"group:eng\", \"github:admin\"]",
        )
        .replace(
            "[refs.\"main\"]\nupdate = [\"group:eng\"]\nforce = []",
            "[refs.\"main\"]\nupdate = [\"group:eng\", \"github:admin\"]\nforce = []",
        );
    assert_ne!(acl, RESTRICTED_ACL);
    let srv = InProcessServer::start(opts_with_acl(&acl)).await;
    let r = root(&srv).await;
    let admin_c = srv.client(Some(&admin())).await;
    let alice_c = srv.client(Some(&alice())).await;
    let theirs = commit(
        &srv,
        &admin_c,
        &r,
        &[("pub", b"theirs\n"), ("secrets/k", b"theirs\n")],
        1,
    )
    .await;
    land(&admin_c, "main", &theirs, false).await.unwrap();
    let mine = commit(
        &srv,
        &alice_c,
        &r,
        &[
            ("clean", b"c\n"),
            ("pub", b"mine\n"),
            ("secrets/k", b"mine\n"),
            ("secrets/new", b"n\n"),
        ],
        2,
    )
    .await;
    // Alice may write /secrets but not read it: fail closed, no partial report, no path leak.
    let e = land(&alice_c, "main", &mine, false).await.unwrap_err();
    let got = expect_err(&e, ErrorCode::PermissionDenied, None);
    assert!(
        got.message.contains("write but not read"),
        "the refusal explains the policy problem: {}",
        got.message
    );
    for leaked in ["secrets", "pub", "clean", "mine"] {
        assert!(
            !got.message.contains(leaked),
            "the refusal must not reveal the hidden path/entry {leaked:?}: {}",
            got.message
        );
    }
    assert!(
        matches!(got.payload, ErrorPayload::None),
        "the refusal carries no conflict payload: {:?}",
        got.payload
    );

    // A principal that can read the whole tree still sees the complete conflict report.
    let e = land(&admin_c, "main", &mine, false).await.unwrap_err();
    let got = expect_err(&e, ErrorCode::MergeConflict, None);
    let ErrorPayload::MergeConflict(report) = got.payload else {
        panic!("expected MergeConflict payload");
    };
    let conflict_paths: Vec<_> = report.conflicts.iter().map(|c| c.path.clone()).collect();
    let clean_paths: Vec<_> = report
        .clean_entries
        .iter()
        .map(|c| c.path.clone())
        .collect();
    assert_eq!(conflict_paths, vec![b"pub".to_vec(), b"secrets/k".to_vec()]);
    assert_eq!(
        clean_paths,
        vec![b"clean".to_vec(), b"secrets/new".to_vec()]
    );
}

// ---------------------------------------------------------------------------------------------
// ForgeVCS merge-commit topology (Lane Z, local patch task-88): tip-parented merge landing.
//
// These exercise `History::land_merge` and the narrow public handler. The topology is opt-in and
// only publishes a clean two-parent merge changeset; a stale first parent is a typed refusal and
// nothing is rebased, squashed or paused.
// ---------------------------------------------------------------------------------------------

/// `Merge(a, b)` over the RPC; the caller keeps the changeset id.
async fn make_merge(
    c: &ScionClient,
    a: &ObjectId,
    b: &ObjectId,
    change_id: u32,
) -> Result<ObjectId, tonic::Status> {
    c.write()
        .merge(v1::MergeRequest {
            a: a.as_bytes().to_vec(),
            b: b.as_bytes().to_vec(),
            meta_fields: Some(meta("merge")),
            change_id: cid(change_id),
        })
        .await
        .map(|r| id(&r.into_inner().changeset_id))
}

/// `History::land_merge` with the live policy adapter, as the handler calls it.
async fn land_merge(
    srv: &InProcessServer,
    r: &str,
    merge: &ObjectId,
    who: &scion_core::Principal,
) -> scion_core::Result<LandResult> {
    let ctx = srv.ctx();
    let source = PolicyLandAuthzSource::new(ctx.policy.clone(), who.clone());
    ctx.history
        .land_merge(
            LandMergeRequest {
                ref_name: RefName::parse(r).unwrap(),
                merge: *merge,
            },
            who,
            &source,
        )
        .await
}

/// Land the base commit so `main` is at version 2, returning the tip.
async fn main_at_base(srv: &InProcessServer, c: &ScionClient) -> (ObjectId, ObjectId) {
    let r = root(srv).await;
    let base = commit(srv, c, &r, &[("one", b"1\n")], 1).await;
    let resp = land(c, "main", &base, false).await.unwrap();
    assert_eq!(resp.version, 2);
    assert_eq!(id(&resp.target), base);
    (r, base)
}

/// Clean tip-parented merge lands as itself: ref target is the merge changeset, exactly one
/// landed row maps its change id to itself, parents stay `[tip, side]`, and both trees are
/// visible. Nothing is rebased or squashed.
#[tokio::test(flavor = "multi_thread")]
async fn land_merge_publishes_the_tip_parented_merge_changeset() {
    let srv = start().await;
    let c = srv.client(Some(&alice())).await;
    let (r, tip) = main_at_base(&srv, &c).await;
    let side = commit(&srv, &c, &r, &[("two", b"2\n")], 2).await;
    let m = make_merge(&c, &tip, &side, 3).await.unwrap();

    let result = land_merge(&srv, "main", &m, &alice()).await.unwrap();
    assert_eq!(result.target, m, "the merge changeset is the new target");
    assert_eq!(result.version, 3);
    assert_eq!(result.landed.len(), 1, "one landed row for the merge");
    assert_eq!(result.landed[0].source, m);
    assert_eq!(result.landed[0].result, m);
    assert_eq!(
        result.landed[0].change_id,
        srv.ctx()
            .history
            .changeset(&m)
            .await
            .unwrap()
            .changeset
            .change_id
    );

    assert_eq!(ref_state(&srv, "main").await, (m, 3));
    let rec = srv.ctx().history.changeset(&m).await.unwrap();
    assert_eq!(
        rec.changeset.parents,
        vec![tip, side],
        "parents are [tip, side]"
    );
    for path in ["one", "two"] {
        let path = scion_object::RepoPath::parse(path.as_bytes()).unwrap();
        assert!(
            srv.ctx()
                .history
                .lookup(&rec.changeset.root_tree, &path)
                .await
                .unwrap()
                .is_some(),
            "{path:?} must be in the merge tree"
        );
    }

    // The landed index has the row `(main, change_id) -> (merge, merge)`, version 3.
    let landed = srv
        .ctx()
        .storage
        .meta
        .get_landed(&RefName::main(), &rec.changeset.change_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(landed.source, m);
    assert_eq!(landed.result, m);
    assert_eq!(landed.version, 3);

    // One ref-log entry (the land), carrying the single landed row.
    let log = srv
        .ctx()
        .history
        .ref_log(&RefName::main(), 0, 100)
        .await
        .unwrap();
    assert_eq!(log.len(), 3, "bootstrap + base land + merge land");
    assert_eq!(log[2].version, 3);
    assert_eq!(log[2].new_target, Some(m));
    assert_eq!(log[2].landed.len(), 1);
}

/// A merge whose first parent is no longer the tip is refused as stale: typed
/// REF_VERSION_MISMATCH with the current target/version, ref unchanged, no landed row. The
/// caller must re-merge; the merge changeset is never rewritten or force-landed.
#[tokio::test(flavor = "multi_thread")]
async fn land_merge_stale_first_parent_is_a_typed_refusal() {
    let srv = start().await;
    let c = srv.client(Some(&alice())).await;
    let (r, tip) = main_at_base(&srv, &c).await;
    let side = commit(&srv, &c, &r, &[("two", b"2\n")], 2).await;
    let stale_merge = make_merge(&c, &tip, &side, 3).await.unwrap();

    // Another writer advances main after the merge was created.
    let newer = commit(&srv, &c, &tip, &[("three", b"3\n")], 4).await;
    let resp = land(&c, "main", &newer, false).await.unwrap();
    assert_eq!(resp.version, 3);
    let new_tip = id(&resp.target);

    let error = land_merge(&srv, "main", &stale_merge, &alice())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::RefVersionMismatch);
    match error.payload {
        ErrorPayload::RefMismatch {
            current_version,
            current_target,
        } => {
            assert_eq!(current_version, 3);
            assert_eq!(current_target, Some(new_tip));
        }
        other => panic!("expected RefMismatch, got {other:?}"),
    }
    assert_eq!(ref_state(&srv, "main").await, (new_tip, 3));
    let change_id = srv
        .ctx()
        .history
        .changeset(&stale_merge)
        .await
        .unwrap()
        .changeset
        .change_id;
    assert!(
        srv.ctx()
            .storage
            .meta
            .get_landed(&RefName::main(), &change_id)
            .await
            .unwrap()
            .is_none(),
        "a stale merge must not be recorded in the landed index"
    );
}

/// Lost-response retry: the CAS committed, the response was lost, and the retry sees the merge
/// already landed. It returns the recorded result idempotently and creates no second ref version.
#[tokio::test(flavor = "multi_thread")]
async fn land_merge_retry_after_lost_response_is_idempotent() {
    let srv = start().await;
    let c = srv.client(Some(&alice())).await;
    let (r, tip) = main_at_base(&srv, &c).await;
    let side = commit(&srv, &c, &r, &[("two", b"2\n")], 2).await;
    let m = make_merge(&c, &tip, &side, 3).await.unwrap();

    srv.storage
        .faults
        .arm("meta.cas_ref.after", FaultAction::FailAfter, Some(1));
    let error = land_merge(&srv, "main", &m, &alice()).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable, "{error:?}");
    assert_eq!(error.subcode(), Some(subcode::INJECTED), "{error:?}");
    assert!(error.retryable());
    srv.storage.faults.disarm_all();
    assert_eq!(ref_state(&srv, "main").await, (m, 3), "the CAS committed");

    let retry = land_merge(&srv, "main", &m, &alice()).await.unwrap();
    assert_eq!(retry.target, m);
    assert_eq!(retry.version, 3);
    assert_eq!(retry.landed.len(), 1);
    assert_eq!(retry.landed[0].source, m);
    assert_eq!(ref_state(&srv, "main").await, (m, 3), "no second version");
    let log = srv
        .ctx()
        .history
        .ref_log(&RefName::main(), 0, 100)
        .await
        .unwrap();
    assert_eq!(
        log.len(),
        3,
        "bootstrap + land + merge; the retry writes no ref-log row"
    );
}

/// The topology gate: only a two-parent changeset may be merge-landed.
#[tokio::test(flavor = "multi_thread")]
async fn land_merge_rejects_non_merge_changesets() {
    let srv = start().await;
    let c = srv.client(Some(&alice())).await;
    let (r, _tip) = main_at_base(&srv, &c).await;
    let single_parent = commit(&srv, &c, &r, &[("two", b"2\n")], 2).await;
    let error = land_merge(&srv, "main", &single_parent, &alice())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_eq!(ref_state(&srv, "main").await.1, 2, "ref unchanged");
}

/// A change id already landed from another merge is ALREADY_LANDED, not a second landing and not
/// an idempotent success for the wrong source.
#[tokio::test(flavor = "multi_thread")]
async fn land_merge_same_change_id_from_another_merge_is_already_landed() {
    let srv = start().await;
    let c = srv.client(Some(&alice())).await;
    let (r, tip) = main_at_base(&srv, &c).await;
    let side = commit(&srv, &c, &r, &[("two", b"2\n")], 2).await;
    let first = make_merge(&c, &tip, &side, 3).await.unwrap();
    land_merge(&srv, "main", &first, &alice()).await.unwrap();

    // Same change id, different changeset (merged onto the new tip with another side commit).
    let side2 = commit(&srv, &c, &r, &[("three", b"3\n")], 4).await;
    let second = make_merge(&c, &first, &side2, 3).await.unwrap();
    assert_ne!(first, second);
    let error = land_merge(&srv, "main", &second, &alice())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::AlreadyLanded);
    match error.payload {
        ErrorPayload::AlreadyLanded { source, result, .. } => {
            assert_eq!(source, first);
            assert_eq!(result, first);
        }
        other => panic!("expected AlreadyLanded, got {other:?}"),
    }
    assert_eq!(ref_state(&srv, "main").await.0, first);
}

/// Policy: `update` on the ref is required (the same adapter as `Land`), and `write` on every
/// published merge delta path; denial leaves the ref unchanged and writes nothing to `landed`.
#[tokio::test(flavor = "multi_thread")]
async fn land_merge_enforces_ref_and_path_policy() {
    let srv = start().await;
    let c = srv.client(Some(&alice())).await;
    let (r, tip) = main_at_base(&srv, &c).await;
    let side = commit(&srv, &c, &r, &[("two", b"2\n")], 2).await;
    let m = make_merge(&c, &tip, &side, 3).await.unwrap();

    // bob cannot update main.
    let error = land_merge(&srv, "main", &m, &mallory()).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    assert_eq!(ref_state(&srv, "main").await, (tip, 2));

    // alice may update main but not write /secrets; a merge that publishes /secrets is refused.
    let restricted = InProcessServer::start(opts_with_acl(RESTRICTED_ACL)).await;
    let admin_c = restricted.client(Some(&admin())).await;
    let rr = root(&restricted).await;
    let base = commit(&restricted, &admin_c, &rr, &[("pub", b"p\n")], 10).await;
    land(&admin_c, "main", &base, false).await.unwrap();
    let tip_r = base;
    let (secret_blob, size) = blob(&restricted, b"secret\n").await;
    let secret_change = admin_c
        .write()
        .create_changeset(create_req(
            &[rr],
            vec![put_entry("secrets/k", &secret_blob, size)],
            11,
        ))
        .await
        .unwrap()
        .into_inner();
    let secret_side = id(&secret_change.changeset_id);
    let merge = admin_c
        .write()
        .merge(v1::MergeRequest {
            a: tip_r.as_bytes().to_vec(),
            b: secret_side.as_bytes().to_vec(),
            meta_fields: Some(meta("merge secrets")),
            change_id: cid(12),
        })
        .await
        .unwrap()
        .into_inner();
    let merge_id = id(&merge.changeset_id);
    let error = land_merge(&restricted, "main", &merge_id, &alice())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    assert_eq!(ref_state(&restricted, "main").await.1, 2);
}

/// A conflicting merge is refused explicitly at merge creation — no merge changeset exists to
/// land, and the merge-commit path never turns a conflict into a squash or a paused conflict.
#[tokio::test(flavor = "multi_thread")]
async fn conflicting_merge_is_refused_and_publishes_nothing() {
    let srv = start().await;
    let c = srv.client(Some(&alice())).await;
    let (r, tip) = main_at_base(&srv, &c).await;
    let ours = commit(&srv, &c, &tip, &[("same", b"ours\n")], 2).await;
    let theirs = commit(&srv, &c, &r, &[("same", b"theirs\n")], 3).await;
    let error = make_merge(&c, &ours, &theirs, 4).await.unwrap_err();
    let got = expect_err(&error, ErrorCode::MergeConflict, None);
    let ErrorPayload::MergeConflict(report) = got.payload else {
        panic!("expected MergeConflict payload");
    };
    assert_eq!(report.conflicts[0].path, b"same".to_vec());
    // Nothing was published: the ref is untouched and no landed row exists for the refused
    // change id (there is no merge changeset at all).
    assert_eq!(ref_state(&srv, "main").await, (tip, 2));
    let rows = srv
        .ctx()
        .history
        .ref_log(&RefName::main(), 0, 100)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "bootstrap + base land only");
}

/// The narrow public handler is the façade's entry point: it authorizes through the request's
/// `CallerCtx`, records the attempt/log fields and returns the merge as the landed target.
#[tokio::test(flavor = "multi_thread")]
async fn land_merge_public_handler_publishes_the_merge() {
    let srv = start().await;
    let c = srv.client(Some(&alice())).await;
    let (r, tip) = main_at_base(&srv, &c).await;
    let side = commit(&srv, &c, &r, &[("two", b"2\n")], 2).await;
    let m = make_merge(&c, &tip, &side, 3).await.unwrap();

    let ctx = srv.ctx();
    let (_, claims) = ctx
        .keyring
        .issue(&alice(), Duration::from_secs(60), ctx.clock.now())
        .unwrap();
    let caller = CallerCtx {
        principal: claims.sub.clone(),
        claims,
        request_id: "test-land-merge".to_owned(),
    };
    let mut request = tonic::Request::new(v1::LandRequest {
        r#ref: "main".into(),
        head: m.as_bytes().to_vec(),
        allow_empty: false,
    });
    request.extensions_mut().insert(caller);
    let resp = handlers::write::land_merge(&ctx, request)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(id(&resp.target), m);
    assert_eq!(resp.version, 3);
    assert_eq!(resp.landed.len(), 1);
    assert_eq!(id(&resp.landed[0].source), m);
}

/// A merge whose delta is empty (its side is already contained in the ref) is refused rather than
/// publishing a no-op merge commit; a re-merge of an already-merged side is not a second landing.
#[tokio::test(flavor = "multi_thread")]
async fn land_merge_refuses_an_empty_merge_delta() {
    let srv = start().await;
    let c = srv.client(Some(&alice())).await;
    let (r, tip) = main_at_base(&srv, &c).await;
    let side = commit(&srv, &c, &r, &[("two", b"2\n")], 2).await;
    let first = make_merge(&c, &tip, &side, 3).await.unwrap();
    land_merge(&srv, "main", &first, &alice()).await.unwrap();

    // Re-merge the same side on the new tip: the tree already contains it, so the delta is empty.
    let redundant = make_merge(&c, &first, &side, 4).await.unwrap();
    let record = srv.ctx().history.changeset(&redundant).await.unwrap();
    assert!(
        record.changeset.delta.is_empty(),
        "expected an empty merge delta"
    );
    let error = land_merge(&srv, "main", &redundant, &alice())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::EmptyChange, "{error:?}");
    assert_eq!(ref_state(&srv, "main").await, (first, 3), "ref unchanged");
}
