// Backend parity and concurrency tests.
//
// These tests express the *correct* post-fix behaviour for the concurrency
// gaps documented in design/concurrency.md.  Tests that currently fail on
// FsGitoxide are the regression baseline for Phase 1 implementation work.
//
// Failing tests:
//   stale_oid_rejected_gitoxide          — PreviousValue::Any ignores old_oid
//   multi_ref_atomicity_gitoxide         — per-ref transactions are not atomic
//   concurrent_pushes_only_one_wins_gitoxide — lost-update under concurrent push
mod common;

use std::path::PathBuf;
use std::sync::{Barrier, Mutex};

use gix_hash::ObjectId;
use mizzle::backend::StorageBackend;
use mizzle_proto::receive::RefUpdate;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn null_oid() -> ObjectId {
    ObjectId::null(gix_hash::Kind::Sha1)
}

// ─── CAS correctness scenarios ───────────────────────────────────────────────

/// Updating a ref with a wrong `old_oid` must be rejected.
///
/// Currently **fails** on FsGitoxide: `PreviousValue::Any` accepts any value.
/// Passes on FsGitCli: `git update-ref` enforces CAS under the per-ref lock.
fn scenario_stale_oid_rejected<B: StorageBackend<RepoId = PathBuf>>(backend: B) {
    let repo_tmp = common::temprepo().unwrap();
    let bare = repo_tmp.path();
    let repo = backend.open(&bare).unwrap();

    let main_oid = backend
        .resolve_ref(&repo, "refs/heads/main")
        .unwrap()
        .unwrap();
    let dev_oid = backend
        .resolve_ref(&repo, "refs/heads/dev")
        .unwrap()
        .unwrap();

    // Claim old_oid = dev_oid, but main actually points to main_oid.
    let result = backend.update_refs(
        &repo,
        &[RefUpdate {
            old_oid: dev_oid,
            new_oid: dev_oid,
            refname: "refs/heads/main".to_string(),
        }],
    );
    assert!(
        result.is_err(),
        "update_refs with wrong old_oid must fail; main is at {main_oid}, claimed {dev_oid}"
    );

    let after = backend
        .resolve_ref(&repo, "refs/heads/main")
        .unwrap()
        .unwrap();
    assert_eq!(
        after, main_oid,
        "main must be unchanged after stale-CAS failure"
    );
}

/// A multi-ref batch where one edit has a stale `old_oid` must leave *all*
/// refs unchanged (all-or-nothing).
///
/// Currently **fails** on FsGitoxide: the first ref in the batch commits
/// before the second fails, leaving the repo in a partial state.
/// Passes on FsGitCli: `git update-ref --stdin` is a single transaction.
fn scenario_multi_ref_atomicity<B: StorageBackend<RepoId = PathBuf>>(backend: B) {
    let repo_tmp = common::temprepo().unwrap();
    let bare = repo_tmp.path();
    let repo = backend.open(&bare).unwrap();

    let main_oid = backend
        .resolve_ref(&repo, "refs/heads/main")
        .unwrap()
        .unwrap();
    let dev_oid = backend
        .resolve_ref(&repo, "refs/heads/dev")
        .unwrap()
        .unwrap();

    // Batch of two edits:
    //   1. Create refs/heads/feature → dev_oid  (valid)
    //   2. Update refs/heads/main: dev_oid → dev_oid  (stale; main is at main_oid)
    let result = backend.update_refs(
        &repo,
        &[
            RefUpdate {
                old_oid: null_oid(),
                new_oid: dev_oid,
                refname: "refs/heads/feature".to_string(),
            },
            RefUpdate {
                old_oid: dev_oid,
                new_oid: dev_oid,
                refname: "refs/heads/main".to_string(),
            },
        ],
    );
    assert!(result.is_err(), "batch with a stale edit must fail");

    let main_after = backend
        .resolve_ref(&repo, "refs/heads/main")
        .unwrap()
        .unwrap();
    assert_eq!(main_after, main_oid, "main must be unchanged");

    let feature_after = backend.resolve_ref(&repo, "refs/heads/feature").unwrap();
    assert!(
        feature_after.is_none(),
        "feature must not have been created; got {feature_after:?}"
    );
}

dual_backend_access_test!(stale_oid_rejected, |backend| {
    scenario_stale_oid_rejected(backend);
    Ok(())
});

dual_backend_access_test!(multi_ref_atomicity, |backend| {
    scenario_multi_ref_atomicity(backend);
    Ok(())
});

// ─── concurrency test ─────────────────────────────────────────────────────────

/// Eight threads simultaneously attempt to push `main: A → B` with the same
/// `old_oid = A`.  Exactly one must succeed; the remaining seven must receive
/// a CAS error.
///
/// Currently **fails** on FsGitoxide: all eight calls return `Ok(())` because
/// `PreviousValue::Any` never checks `old_oid`.
/// Passes on FsGitCli: `git update-ref` enforces the CAS under the per-ref lock.
fn concurrent_pushes_only_one_wins<B, F>(make_backend: F)
where
    B: StorageBackend<RepoId = PathBuf>,
    F: Fn() -> B + Sync,
{
    const RACERS: usize = 8;

    let repo_tmp = common::temprepo().unwrap();
    let bare = repo_tmp.path();

    let setup_backend = make_backend();
    let setup_repo = setup_backend.open(&bare).unwrap();
    let main_oid = setup_backend
        .resolve_ref(&setup_repo, "refs/heads/main")
        .unwrap()
        .unwrap();
    let dev_oid = setup_backend
        .resolve_ref(&setup_repo, "refs/heads/dev")
        .unwrap()
        .unwrap();
    drop(setup_repo);

    let barrier = Barrier::new(RACERS);
    let successes = Mutex::new(0usize);

    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(RACERS);
        for _ in 0..RACERS {
            handles.push(s.spawn(|| {
                barrier.wait();
                let backend = make_backend();
                let repo = backend.open(&bare).unwrap();
                let result = backend.update_refs(
                    &repo,
                    &[RefUpdate {
                        old_oid: main_oid,
                        new_oid: dev_oid,
                        refname: "refs/heads/main".to_string(),
                    }],
                );
                if result.is_ok() {
                    *successes.lock().unwrap() += 1;
                }
            }));
        }
        for h in handles {
            h.join().expect("racer thread panicked");
        }
    });

    let wins = *successes.lock().unwrap();
    assert_eq!(
        wins, 1,
        "exactly 1 push must win; got {wins}/{RACERS} successes"
    );
}

#[test]
fn concurrent_pushes_only_one_wins_gitoxide() {
    concurrent_pushes_only_one_wins(|| mizzle::backend::fs_gitoxide::FsGitoxide);
}

#[test]
fn concurrent_pushes_only_one_wins_git_cli() {
    concurrent_pushes_only_one_wins(|| mizzle::backend::fs_git_cli::FsGitCli);
}
