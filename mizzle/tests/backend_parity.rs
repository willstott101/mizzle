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
    use futures_lite::future::block_on;

    let repo_tmp = common::temprepo().unwrap();
    let bare = repo_tmp.path();
    let repo = block_on(backend.open(&bare)).unwrap();

    let main_oid = block_on(backend.resolve_ref(&repo, "refs/heads/main"))
        .unwrap()
        .unwrap();
    let dev_oid = block_on(backend.resolve_ref(&repo, "refs/heads/dev"))
        .unwrap()
        .unwrap();

    // Claim old_oid = dev_oid, but main actually points to main_oid.
    let result = block_on(backend.update_refs(
        &repo,
        &[RefUpdate {
            old_oid: dev_oid,
            new_oid: dev_oid,
            refname: "refs/heads/main".to_string(),
        }],
    ));
    assert!(
        result.is_err(),
        "update_refs with wrong old_oid must fail; main is at {main_oid}, claimed {dev_oid}"
    );

    let after = block_on(backend.resolve_ref(&repo, "refs/heads/main"))
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
    use futures_lite::future::block_on;

    let repo_tmp = common::temprepo().unwrap();
    let bare = repo_tmp.path();
    let repo = block_on(backend.open(&bare)).unwrap();

    let main_oid = block_on(backend.resolve_ref(&repo, "refs/heads/main"))
        .unwrap()
        .unwrap();
    let dev_oid = block_on(backend.resolve_ref(&repo, "refs/heads/dev"))
        .unwrap()
        .unwrap();

    // Batch of two edits:
    //   1. Create refs/heads/feature → dev_oid  (valid)
    //   2. Update refs/heads/main: dev_oid → dev_oid  (stale; main is at main_oid)
    let result = block_on(backend.update_refs(
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
    ));
    assert!(result.is_err(), "batch with a stale edit must fail");

    let main_after = block_on(backend.resolve_ref(&repo, "refs/heads/main"))
        .unwrap()
        .unwrap();
    assert_eq!(main_after, main_oid, "main must be unchanged");

    let feature_after = block_on(backend.resolve_ref(&repo, "refs/heads/feature")).unwrap();
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
    use futures_lite::future::block_on;

    const RACERS: usize = 8;

    let repo_tmp = common::temprepo().unwrap();
    let bare = repo_tmp.path();

    let setup_backend = make_backend();
    let setup_repo = block_on(setup_backend.open(&bare)).unwrap();
    let main_oid = block_on(setup_backend.resolve_ref(&setup_repo, "refs/heads/main"))
        .unwrap()
        .unwrap();
    let dev_oid = block_on(setup_backend.resolve_ref(&setup_repo, "refs/heads/dev"))
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
                let repo = block_on(backend.open(&bare)).unwrap();
                let result = block_on(backend.update_refs(
                    &repo,
                    &[RefUpdate {
                        old_oid: main_oid,
                        new_oid: dev_oid,
                        refname: "refs/heads/main".to_string(),
                    }],
                ));
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

// ─── SQL backend tests ───────────────────────────────────────────────────────

#[cfg(feature = "sql")]
mod sql_tests {
    use super::*;

    /// SQL-specific helper: scenario functions create their own temprepo, but
    /// for SQL the backend must already have that repo registered.  We solve
    /// this by passing the repo path alongside the backend.
    fn sql_setup() -> (mizzle::backend::sql::SqlBackend, common::TempRepo) {
        let repo_tmp = common::temprepo().unwrap();
        let backend = common::sql_backend_from_fs(&repo_tmp.path());
        (backend, repo_tmp)
    }

    #[test]
    fn stale_oid_rejected_sql() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let (backend, _repo_tmp) = sql_setup();
        // scenario_stale_oid_rejected creates its own temprepo, so we
        // duplicate the scenario inline using the pre-registered path.
        use futures_lite::future::block_on;
        let bare = _repo_tmp.path();
        let repo = block_on(backend.open(&bare)).unwrap();

        let main_oid = block_on(backend.resolve_ref(&repo, "refs/heads/main"))
            .unwrap()
            .unwrap();
        let dev_oid = block_on(backend.resolve_ref(&repo, "refs/heads/dev"))
            .unwrap()
            .unwrap();

        let result = block_on(backend.update_refs(
            &repo,
            &[RefUpdate {
                old_oid: dev_oid,
                new_oid: dev_oid,
                refname: "refs/heads/main".to_string(),
            }],
        ));
        assert!(
            result.is_err(),
            "update_refs with wrong old_oid must fail; main is at {main_oid}, claimed {dev_oid}"
        );

        let after = block_on(backend.resolve_ref(&repo, "refs/heads/main"))
            .unwrap()
            .unwrap();
        assert_eq!(
            after, main_oid,
            "main must be unchanged after stale-CAS failure"
        );
    }

    #[test]
    fn multi_ref_atomicity_sql() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let (backend, _repo_tmp) = sql_setup();
        use futures_lite::future::block_on;
        let bare = _repo_tmp.path();
        let repo = block_on(backend.open(&bare)).unwrap();

        let main_oid = block_on(backend.resolve_ref(&repo, "refs/heads/main"))
            .unwrap()
            .unwrap();
        let dev_oid = block_on(backend.resolve_ref(&repo, "refs/heads/dev"))
            .unwrap()
            .unwrap();

        let result = block_on(backend.update_refs(
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
        ));
        assert!(result.is_err(), "batch with a stale edit must fail");

        let main_after = block_on(backend.resolve_ref(&repo, "refs/heads/main"))
            .unwrap()
            .unwrap();
        assert_eq!(main_after, main_oid, "main must be unchanged");

        let feature_after = block_on(backend.resolve_ref(&repo, "refs/heads/feature")).unwrap();
        assert!(
            feature_after.is_none(),
            "feature must not have been created; got {feature_after:?}"
        );
    }

    /// build_pack must write a cache file on miss, and serve it on a
    /// subsequent call with the same wants/haves.
    #[test]
    fn pack_cache_miss_then_hit_sql() {
        use futures_lite::future::block_on;
        use mizzle::backend::{PackOptions, StorageBackend};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let (backend, repo_tmp) = sql_setup();
        let bare = repo_tmp.path();
        let repo = block_on(backend.open(&bare)).unwrap();

        let main_oid = block_on(backend.resolve_ref(&repo, "refs/heads/main"))
            .unwrap()
            .unwrap();

        let opts = PackOptions {
            deepen: None,
            filter: None,
            thin_pack: false,
        };

        // Cache dir should be empty before the first call.
        let cache_dir = backend.pack_cache_dir();
        let cache_files_before: Vec<_> = walkdir(cache_dir);
        assert!(
            cache_files_before.is_empty(),
            "cache dir should be empty before build_pack, found: {cache_files_before:?}"
        );

        // First call: cache miss → builds pack and writes cache.
        let mut output1 = block_on(backend.build_pack(&repo, &[main_oid], &[], &opts)).unwrap();
        let mut bytes1 = Vec::new();
        std::io::Read::read_to_end(&mut output1.reader, &mut bytes1).unwrap();
        assert!(!bytes1.is_empty(), "pack must not be empty");

        // A .pack file should now exist in the cache dir.
        let cache_files_after: Vec<_> = walkdir(cache_dir);
        assert_eq!(
            cache_files_after.len(),
            1,
            "expected exactly 1 cache file, found: {cache_files_after:?}"
        );
        assert!(
            cache_files_after[0]
                .extension()
                .map(|e| e == "pack")
                .unwrap_or(false),
            "cache file should have .pack extension: {:?}",
            cache_files_after[0]
        );

        // Second call: same wants/haves → cache hit.
        let mut output2 = block_on(backend.build_pack(&repo, &[main_oid], &[], &opts)).unwrap();
        let mut bytes2 = Vec::new();
        std::io::Read::read_to_end(&mut output2.reader, &mut bytes2).unwrap();

        assert_eq!(bytes1, bytes2, "cache hit must return identical pack bytes");

        // No new cache files should have been created.
        let cache_files_final: Vec<_> = walkdir(cache_dir);
        assert_eq!(
            cache_files_final.len(),
            1,
            "cache should still have exactly 1 file"
        );
    }

    /// Recursively list all files under `dir`.
    fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    files.extend(walkdir(&path));
                } else {
                    files.push(path);
                }
            }
        }
        files
    }

    #[test]
    fn concurrent_pushes_only_one_wins_sql() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let (backend, repo_tmp) = sql_setup();
        let bare = repo_tmp.path();

        use futures_lite::future::block_on;
        const RACERS: usize = 8;

        let repo = block_on(backend.open(&bare)).unwrap();
        let main_oid = block_on(backend.resolve_ref(&repo, "refs/heads/main"))
            .unwrap()
            .unwrap();
        let dev_oid = block_on(backend.resolve_ref(&repo, "refs/heads/dev"))
            .unwrap()
            .unwrap();
        drop(repo);

        let shared = std::sync::Arc::new(backend);
        let barrier = std::sync::Arc::new(Barrier::new(RACERS));
        let successes = std::sync::Arc::new(Mutex::new(0usize));
        let bare = std::sync::Arc::new(bare);
        let rt_handle = rt.handle().clone();

        let mut join_handles = Vec::with_capacity(RACERS);
        for _ in 0..RACERS {
            let shared = shared.clone();
            let barrier = barrier.clone();
            let successes = successes.clone();
            let bare = bare.clone();
            let rt_handle = rt_handle.clone();
            join_handles.push(std::thread::spawn(move || {
                let _guard = rt_handle.enter();
                barrier.wait();
                let b = (*shared).clone();
                let repo = block_on(b.open(&bare)).unwrap();
                let result = block_on(b.update_refs(
                    &repo,
                    &[RefUpdate {
                        old_oid: main_oid,
                        new_oid: dev_oid,
                        refname: "refs/heads/main".to_string(),
                    }],
                ));
                if result.is_ok() {
                    *successes.lock().unwrap() += 1;
                }
            }));
        }
        for h in join_handles {
            h.join().expect("racer thread panicked");
        }

        let wins = *successes.lock().unwrap();
        assert_eq!(
            wins, 1,
            "exactly 1 push must win; got {wins}/{RACERS} successes"
        );
    }
}

// ─── KV backend tests ────────────────────────────────────────────────────────
//
// Gated on the `tikv` feature AND `$MIZZLE_TIKV_PD_ADDR`.  Without the env
// var the tests skip silently so contributors can `cargo test --features tikv`
// without a running cluster — CI provisions TiKV via `tiup playground` and
// sets the env var.

#[cfg(feature = "tikv")]
mod kv_tests {
    use super::*;

    /// One-stop fixture: a tokio `Runtime` that outlives the backend (tikv-client
    /// spawns background tasks on whatever runtime is current at construction
    /// time, so the runtime must stay alive for every subsequent operation),
    /// the backend itself, the on-disk temp repo we ingested into it, and the
    /// `TempDir` that owns the pack cache directory (so it's cleaned up when
    /// the fixture drops instead of leaking on every test run).
    struct KvFixture {
        rt: tokio::runtime::Runtime,
        backend: mizzle::backend::kv::KvBackend,
        repo_tmp: common::TempRepo,
        #[allow(dead_code)] // held for its Drop side effect (rmdir on exit)
        cache_dir: tempfile::TempDir,
    }

    fn kv_setup() -> Option<KvFixture> {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let repo_tmp = common::temprepo().unwrap();
        let (backend, cache_dir) = common::kv_backend_from_fs(&rt, &repo_tmp.path())?;
        Some(KvFixture {
            rt,
            backend,
            repo_tmp,
            cache_dir,
        })
    }

    #[test]
    fn stale_oid_rejected_kv() {
        let Some(fx) = kv_setup() else {
            eprintln!("skipping: MIZZLE_TIKV_PD_ADDR not set");
            return;
        };
        let bare = fx.repo_tmp.path();
        let repo = fx.rt.block_on(fx.backend.open(&bare)).unwrap();

        let main_oid = fx
            .rt
            .block_on(fx.backend.resolve_ref(&repo, "refs/heads/main"))
            .unwrap()
            .unwrap();
        let dev_oid = fx
            .rt
            .block_on(fx.backend.resolve_ref(&repo, "refs/heads/dev"))
            .unwrap()
            .unwrap();

        let result = fx.rt.block_on(fx.backend.update_refs(
            &repo,
            &[RefUpdate {
                old_oid: dev_oid,
                new_oid: dev_oid,
                refname: "refs/heads/main".to_string(),
            }],
        ));
        assert!(
            result.is_err(),
            "update_refs with wrong old_oid must fail; main is at {main_oid}, claimed {dev_oid}"
        );

        let after = fx
            .rt
            .block_on(fx.backend.resolve_ref(&repo, "refs/heads/main"))
            .unwrap()
            .unwrap();
        assert_eq!(
            after, main_oid,
            "main must be unchanged after stale-CAS failure"
        );
    }

    /// End-to-end fetch path: build_pack must hit the cache on the second
    /// call, and (more importantly here) actually produce a valid pack on the
    /// first call — exercising reachable_excluding, tree_oids collection, the
    /// temp-gitoxide-repo assembly, and pack_cache write.
    #[test]
    fn pack_cache_miss_then_hit_kv() {
        use mizzle::backend::{PackOptions, StorageBackend};

        let Some(fx) = kv_setup() else {
            eprintln!("skipping: MIZZLE_TIKV_PD_ADDR not set");
            return;
        };
        let bare = fx.repo_tmp.path();
        let repo = fx.rt.block_on(fx.backend.open(&bare)).unwrap();

        let main_oid = fx
            .rt
            .block_on(fx.backend.resolve_ref(&repo, "refs/heads/main"))
            .unwrap()
            .unwrap();

        let opts = PackOptions {
            deepen: None,
            filter: None,
            thin_pack: false,
        };

        let cache_dir = fx.backend.pack_cache_dir();
        assert!(
            walkdir(cache_dir).is_empty(),
            "cache should be empty before first build_pack"
        );

        let mut output1 = fx
            .rt
            .block_on(fx.backend.build_pack(&repo, &[main_oid], &[], &opts))
            .unwrap();
        let mut bytes1 = Vec::new();
        std::io::Read::read_to_end(&mut output1.reader, &mut bytes1).unwrap();
        assert!(!bytes1.is_empty(), "pack must not be empty");

        let after_first = walkdir(cache_dir);
        assert_eq!(
            after_first.len(),
            1,
            "expected exactly 1 cache file, found: {after_first:?}"
        );

        let mut output2 = fx
            .rt
            .block_on(fx.backend.build_pack(&repo, &[main_oid], &[], &opts))
            .unwrap();
        let mut bytes2 = Vec::new();
        std::io::Read::read_to_end(&mut output2.reader, &mut bytes2).unwrap();
        assert_eq!(bytes1, bytes2, "cache hit must return identical pack bytes");

        assert_eq!(
            walkdir(cache_dir).len(),
            1,
            "cache hit must not write a new file"
        );
    }

    fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    files.extend(walkdir(&path));
                } else {
                    files.push(path);
                }
            }
        }
        files
    }
}
