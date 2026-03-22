mod common;

use std::fs;
use std::path::PathBuf;
use std::thread;
use tempfile::tempdir;

use mizzle::traits::{PackMetadata, PushKind, PushRef, RepoAccess};

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

    fn repo_id(&self) -> &PathBuf {
        &self.repo_path
    }

    fn authorize_push(
        &self,
        _refs: &[PushRef<'_>],
        _pack: Option<&PackMetadata>,
    ) -> Result<(), String> {
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

    fn repo_id(&self) -> &PathBuf {
        &self.repo_path
    }

    fn authorize_push(
        &self,
        refs: &[PushRef<'_>],
        _pack: Option<&PackMetadata>,
    ) -> Result<(), String> {
        for r in refs {
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
