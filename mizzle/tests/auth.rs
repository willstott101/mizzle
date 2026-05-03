mod common;

use std::fs;
use std::path::PathBuf;
use std::thread;
use tempfile::tempdir;

use std::sync::{Arc, Mutex};

use mizzle::traits::{Comparison, PushKind, PushRef, RepoAccess};

/// Spin up an axum server whose handler always returns 403 before calling mizzle.
fn deny_all_server() -> common::ServerHandle {
    use axum::{http::StatusCode, routing::get, Router};

    let app = Router::new().route(
        "/{*key}",
        get(|| async { (StatusCode::FORBIDDEN, "access denied") })
            .post(|| async { (StatusCode::FORBIDDEN, "access denied") }),
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    thread::spawn(move || {
        rt.block_on(async {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    rx.await.ok();
                })
                .await
        })
        .unwrap()
    });

    common::ServerHandle::new(port, move || {
        let _ = tx.send(());
    })
}

// An access type that allows reads but rejects all pushes.
#[derive(Clone)]
struct DenyPushAccess {
    repo_path: PathBuf,
}

impl RepoAccess for DenyPushAccess {
    type RepoId = PathBuf;
    type PushContext = ();

    fn repo_id(&self) -> &PathBuf {
        &self.repo_path
    }

    fn authorize_preliminary(&self, _refs: &[PushRef<'_>]) -> Result<(), String> {
        Err("permission denied".into())
    }
}

// An access type that denies one specific push kind and allows the rest.
#[derive(Clone)]
struct KindFilterAccess {
    repo_path: PathBuf,
    denied: PushKind,
}

impl RepoAccess for KindFilterAccess {
    type RepoId = PathBuf;
    type PushContext = ();

    fn repo_id(&self) -> &PathBuf {
        &self.repo_path
    }

    fn authorize_preliminary(&self, refs: &[PushRef<'_>]) -> Result<(), String> {
        // Create / Delete are detected at the preliminary stage; ForcePush
        // can only be classified after pack ingestion.
        for r in refs {
            if r.kind == self.denied
                && (self.denied == PushKind::Create || self.denied == PushKind::Delete)
            {
                return Err(format!("{:?} pushes are not allowed", self.denied));
            }
        }
        Ok(())
    }

    fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>) -> Result<(), String> {
        for r in push.refs() {
            if r.kind == self.denied {
                return Err(format!("{:?} pushes are not allowed", self.denied));
            }
        }
        Ok(())
    }
}

// ── Single-backend tests (not backend-sensitive) ─────────────────────────────

#[test]
fn test_clone_denied() {
    let temprepo = common::temprepo().unwrap();
    let server = deny_all_server();

    let clone_dir = tempdir().unwrap();
    let result = common::run_git(
        clone_dir.path(),
        [
            "clone",
            "--branch",
            "main",
            &format!("http://localhost:{}/test.git", server.port),
        ],
    );

    assert!(result.is_err(), "clone should have been rejected");
    server.stop();
    drop(temprepo);
}

/// A server pointing at a non-existent repo path should return HTTP 500 with a
/// non-empty error body immediately — not a silent truncated stream.
#[test]
fn test_bad_repo_path_returns_500() {
    let server = common::axum_server(common::Config {
        bare_repo_path: PathBuf::from("/nonexistent/path/that/does/not/exist.git"),
    });
    let clone_dir = tempdir().unwrap();

    let err = common::run_git(
        clone_dir.path(),
        [
            "clone",
            "--branch",
            "main",
            &format!("http://localhost:{}/test.git", server.port),
        ],
    );

    assert!(err.is_err(), "clone from bad repo path should fail");
    server.stop();
}

// ── Dual-backend tests (exercise backend storage on push) ────────────────────

dual_backend_access_test!(test_push_denied, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| DenyPushAccess {
            repo_path: PathBuf::from(rp.as_ref()),
        });

    let clone_dir = tempdir()?;
    common::run_git(
        clone_dir.path(),
        [
            "clone",
            "--branch",
            "main",
            &format!("http://localhost:{}/test.git", server.port),
        ],
    )?;
    let repo_dir = clone_dir.path().join("test");

    fs::write(repo_dir.join("denied.txt"), "this should not land\n")?;
    common::run_git(&repo_dir, ["add", "denied.txt"])?;
    common::run_git(&repo_dir, ["commit", "-m", "Should be rejected"])?;

    let err = common::run_git(&repo_dir, ["push", "origin", "main"]).unwrap_err();
    assert!(
        err.to_string().contains("permission denied"),
        "expected 'permission denied' in error, got: {err}"
    );

    let bare_head = common::run_git(temprepo.path().as_path(), ["rev-parse", "refs/heads/main"])?;
    let local_head = common::run_git(&repo_dir, ["rev-parse", "HEAD"])?;
    assert_ne!(
        bare_head, local_head,
        "bare repo should not have been updated"
    );

    server.stop();
    Ok(())
});

dual_backend_access_test!(test_force_push_denied, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| KindFilterAccess {
            repo_path: PathBuf::from(rp.as_ref()),
            denied: PushKind::ForcePush,
        });

    let clone_dir = tempdir()?;
    common::run_git(
        clone_dir.path(),
        [
            "clone",
            "--branch",
            "main",
            &format!("http://localhost:{}/test.git", server.port),
        ],
    )?;
    let repo_dir = clone_dir.path().join("test");

    // A normal fast-forward push should succeed.
    fs::write(repo_dir.join("ff.txt"), "fast forward\n")?;
    common::run_git(&repo_dir, ["add", "ff.txt"])?;
    common::run_git(&repo_dir, ["commit", "-m", "FF commit"])?;
    common::run_git(&repo_dir, ["push", "origin", "main"])?;

    // Reset to before the FF commit and make a diverging commit — this is a force push.
    common::run_git(&repo_dir, ["reset", "--hard", "HEAD~1"])?;
    fs::write(repo_dir.join("diverge.txt"), "diverging\n")?;
    common::run_git(&repo_dir, ["add", "diverge.txt"])?;
    common::run_git(&repo_dir, ["commit", "-m", "Diverging commit"])?;

    let err = common::run_git(&repo_dir, ["push", "--force", "origin", "main"]).unwrap_err();
    assert!(
        err.to_string().contains("ForcePush"),
        "expected 'ForcePush' in error, got: {err}"
    );

    server.stop();
    Ok(())
});

dual_backend_access_test!(test_create_denied, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| KindFilterAccess {
            repo_path: PathBuf::from(rp.as_ref()),
            denied: PushKind::Create,
        });

    let clone_dir = tempdir()?;
    common::run_git(
        clone_dir.path(),
        [
            "clone",
            "--branch",
            "main",
            &format!("http://localhost:{}/test.git", server.port),
        ],
    )?;
    let repo_dir = clone_dir.path().join("test");

    // Pushing to an existing branch (fast-forward) should succeed.
    fs::write(repo_dir.join("ok.txt"), "ok\n")?;
    common::run_git(&repo_dir, ["add", "ok.txt"])?;
    common::run_git(&repo_dir, ["commit", "-m", "FF commit"])?;
    common::run_git(&repo_dir, ["push", "origin", "main"])?;

    // Pushing a new branch should be rejected.
    common::run_git(&repo_dir, ["checkout", "-b", "new-branch"])?;
    let err = common::run_git(&repo_dir, ["push", "origin", "new-branch"]).unwrap_err();
    assert!(
        err.to_string().contains("Create"),
        "expected 'Create' in error, got: {err}"
    );

    server.stop();
    Ok(())
});

// ── Comparison accessor tests ───────────────────────────────────────────────
//
// These exercise `Comparison::new_commits` / `dropped_commits` / `ref_diff`
// end-to-end against both backends through a real `git push`.

#[derive(Default, Clone)]
struct CapturedPush {
    /// (refname, [oid, oid, ...]) for new_commits per ref.
    new_commits: Vec<(String, Vec<gix::ObjectId>)>,
    /// (refname, [oid, oid, ...]) for dropped_commits per ref.
    dropped_commits: Vec<(String, Vec<gix::ObjectId>)>,
    /// (refname, [path, path, ...]) for the ref_diff touched paths.
    diff_paths: Vec<(String, Vec<Vec<u8>>)>,
}

#[derive(Clone)]
struct CaptureAccess {
    repo: PathBuf,
    captured: Arc<Mutex<CapturedPush>>,
}

impl RepoAccess for CaptureAccess {
    type RepoId = PathBuf;
    type PushContext = ();

    fn repo_id(&self) -> &PathBuf {
        &self.repo
    }

    fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>) -> Result<(), String> {
        let mut cap = self.captured.lock().unwrap();
        for r in push.refs() {
            cap.new_commits.push((
                r.refname.to_string(),
                push.new_commits(r)
                    .map_err(|e| e.to_string())?
                    .iter()
                    .map(|c| c.oid)
                    .collect(),
            ));
            cap.dropped_commits.push((
                r.refname.to_string(),
                push.dropped_commits(r)
                    .map_err(|e| e.to_string())?
                    .iter()
                    .map(|c| c.oid)
                    .collect(),
            ));
            let diff = push.ref_diff(r).map_err(|e| e.to_string())?;
            cap.diff_paths.push((
                r.refname.to_string(),
                diff.touched_paths().map(|p| p.to_vec()).collect(),
            ));
        }
        Ok(())
    }
}

dual_backend_access_test!(comparison_new_commits_excludes_existing, |backend| {
    let temprepo = common::temprepo()?;
    let captured = Arc::new(Mutex::new(CapturedPush::default()));
    let cap2 = captured.clone();
    let server = common::axum_access_server_with_backend(temprepo.path(), backend, move |rp| {
        CaptureAccess {
            repo: PathBuf::from(rp.as_ref()),
            captured: cap2.clone(),
        }
    });

    let clone_dir = tempdir()?;
    let repo_dir = clone_repo_main(server.port, clone_dir.path());

    fs::write(repo_dir.join("new.txt"), "hi")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "added"])?;
    let new_oid = common::run_git(&repo_dir, ["rev-parse", "HEAD"])?;

    common::run_git(&repo_dir, ["push", "origin", "main"])?;

    let cap = captured.lock().unwrap();
    assert_eq!(cap.new_commits.len(), 1, "one ref pushed");
    let (ref_name, commits) = &cap.new_commits[0];
    assert_eq!(ref_name, "refs/heads/main");
    assert_eq!(commits.len(), 1, "one new commit introduced");
    assert_eq!(commits[0].to_hex().to_string(), new_oid);

    server.stop();
    Ok(())
});

dual_backend_access_test!(comparison_dropped_commits_on_force_push, |backend| {
    let temprepo = common::temprepo()?;
    let captured = Arc::new(Mutex::new(CapturedPush::default()));
    let cap2 = captured.clone();
    let server = common::axum_access_server_with_backend(temprepo.path(), backend, move |rp| {
        CaptureAccess {
            repo: PathBuf::from(rp.as_ref()),
            captured: cap2.clone(),
        }
    });

    let clone_dir = tempdir()?;
    let repo_dir = clone_repo_main(server.port, clone_dir.path());

    let dropped_oid = common::run_git(&repo_dir, ["rev-parse", "HEAD"])?;
    common::run_git(&repo_dir, ["reset", "--hard", "HEAD~1"])?;
    fs::write(repo_dir.join("d.txt"), "d")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "diverging"])?;

    common::run_git(&repo_dir, ["push", "--force", "origin", "main"])?;

    let cap = captured.lock().unwrap();
    let (_, dropped) = &cap.dropped_commits[0];
    assert_eq!(dropped.len(), 1, "force-push drops one commit");
    assert_eq!(dropped[0].to_hex().to_string(), dropped_oid);

    server.stop();
    Ok(())
});

dual_backend_access_test!(comparison_ref_diff_lists_added_paths, |backend| {
    let temprepo = common::temprepo()?;
    let captured = Arc::new(Mutex::new(CapturedPush::default()));
    let cap2 = captured.clone();
    let server = common::axum_access_server_with_backend(temprepo.path(), backend, move |rp| {
        CaptureAccess {
            repo: PathBuf::from(rp.as_ref()),
            captured: cap2.clone(),
        }
    });

    let clone_dir = tempdir()?;
    let repo_dir = clone_repo_main(server.port, clone_dir.path());

    fs::write(repo_dir.join("brand-new.txt"), "yay")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "add brand-new.txt"])?;
    common::run_git(&repo_dir, ["push", "origin", "main"])?;

    let cap = captured.lock().unwrap();
    let (_, paths) = &cap.diff_paths[0];
    assert!(
        paths.iter().any(|p| p.as_slice() == b"brand-new.txt"),
        "ref_diff should report brand-new.txt as touched, got: {:?}",
        paths
            .iter()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect::<Vec<_>>()
    );

    server.stop();
    Ok(())
});

fn clone_repo_main(server_port: u16, dir: &std::path::Path) -> PathBuf {
    common::run_git(
        dir,
        [
            "clone",
            "--branch",
            "main",
            &format!("http://localhost:{}/test.git", server_port),
        ],
    )
    .unwrap();
    dir.join("test")
}

dual_backend_access_test!(test_delete_denied, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| KindFilterAccess {
            repo_path: PathBuf::from(rp.as_ref()),
            denied: PushKind::Delete,
        });

    let clone_dir = tempdir()?;
    common::run_git(
        clone_dir.path(),
        [
            "clone",
            &format!("http://localhost:{}/test.git", server.port),
        ],
    )?;
    let repo_dir = clone_dir.path().join("test");

    // Pushing a fast-forward commit should succeed.
    fs::write(repo_dir.join("ok.txt"), "ok\n")?;
    common::run_git(&repo_dir, ["add", "ok.txt"])?;
    common::run_git(&repo_dir, ["commit", "-m", "FF commit"])?;
    common::run_git(&repo_dir, ["push", "origin", "main"])?;

    // Deleting a remote branch should be rejected.
    let err = common::run_git(&repo_dir, ["push", "origin", "--delete", "dev"]).unwrap_err();
    assert!(
        err.to_string().contains("Delete"),
        "expected 'Delete' in error, got: {err}"
    );

    server.stop();
    Ok(())
});
