mod common;

use std::fs;
use std::path::PathBuf;
use std::thread;
use tempfile::tempdir;

use mizzle::traits::{PushKind, PushRef, RepoAccess};

// An access type that allows reads but rejects all pushes.
#[derive(Clone)]
struct DenyPushAccess {
    repo_path: Box<str>,
}

impl RepoAccess for DenyPushAccess {
    fn repo_path(&self) -> &str {
        &self.repo_path
    }

    fn authorize_push(&self, _refs: &[PushRef<'_>]) -> Result<(), String> {
        Err("permission denied".into())
    }
}

// An access type that denies one specific push kind and allows the rest.
#[derive(Clone)]
struct KindFilterAccess {
    repo_path: Box<str>,
    denied: PushKind,
}

impl RepoAccess for KindFilterAccess {
    fn repo_path(&self) -> &str {
        &self.repo_path
    }

    fn authorize_push(&self, refs: &[PushRef<'_>]) -> Result<(), String> {
        for r in refs {
            if r.kind == self.denied {
                return Err(format!("{:?} pushes are not allowed", self.denied));
            }
        }
        Ok(())
    }
}

/// Spin up an axum server whose handler always returns 403 before calling mizzle.
#[cfg(feature = "axum")]
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

/// Spin up an axum server that uses DenyPushAccess — reads work, all pushes rejected.
#[cfg(feature = "axum")]
fn deny_push_server(bare_repo_path: PathBuf) -> common::ServerHandle {
    axum_access_server(bare_repo_path, |repo_path| DenyPushAccess { repo_path })
}

/// Spin up an axum server that uses KindFilterAccess — reads work, one push kind rejected.
#[cfg(feature = "axum")]
fn kind_filter_server(bare_repo_path: PathBuf, denied: PushKind) -> common::ServerHandle {
    axum_access_server(bare_repo_path, move |repo_path| KindFilterAccess {
        repo_path,
        denied: denied.clone(),
    })
}

/// Generic axum server builder: constructs an access object from the repo path per request.
#[cfg(feature = "axum")]
fn axum_access_server<A, F>(bare_repo_path: PathBuf, make_access: F) -> common::ServerHandle
where
    A: RepoAccess + Send + 'static,
    F: Fn(Box<str>) -> A + Send + Sync + 'static,
{
    use axum::{
        extract::{Path, Request, State},
        response::Response,
        routing::get,
        Router,
    };
    use std::sync::Arc;

    async fn handler<
        A: RepoAccess + Send + 'static,
        F: Fn(Box<str>) -> A + Send + Sync + 'static,
    >(
        State(state): State<Arc<(String, F)>>,
        Path(path): Path<String>,
        req: Request,
    ) -> Response {
        let access = state.1(state.0.as_str().into());
        mizzle::servers::axum::serve(access, &path, req).await
    }

    let state = Arc::new((bare_repo_path.to_str().unwrap().to_string(), make_access));
    let app = Router::new()
        .route("/{*key}", get(handler::<A, F>).post(handler::<A, F>))
        .with_state(state);

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

#[cfg(feature = "axum")]
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

#[cfg(feature = "axum")]
#[test]
fn test_push_denied() {
    let temprepo = common::temprepo().unwrap();
    let server = deny_push_server(temprepo.path());

    let clone_dir = tempdir().unwrap();
    common::run_git(
        clone_dir.path(),
        [
            "clone",
            "--branch",
            "main",
            &format!("http://localhost:{}/test.git", server.port),
        ],
    )
    .unwrap();
    let repo_dir = clone_dir.path().join("test");

    fs::write(repo_dir.join("denied.txt"), "this should not land\n").unwrap();
    common::run_git(&repo_dir, ["add", "denied.txt"]).unwrap();
    common::run_git(&repo_dir, ["commit", "-m", "Should be rejected"]).unwrap();

    let err = common::run_git(&repo_dir, ["push", "origin", "main"]).unwrap_err();
    assert!(
        err.to_string().contains("permission denied"),
        "expected 'permission denied' in error, got: {err}"
    );

    let bare_head =
        common::run_git(temprepo.path().as_path(), ["rev-parse", "refs/heads/main"]).unwrap();
    let local_head = common::run_git(&repo_dir, ["rev-parse", "HEAD"]).unwrap();
    assert_ne!(
        bare_head, local_head,
        "bare repo should not have been updated"
    );

    server.stop();
}

#[cfg(feature = "axum")]
#[test]
fn test_force_push_denied() {
    let temprepo = common::temprepo().unwrap();
    let server = kind_filter_server(temprepo.path(), PushKind::ForcePush);

    let clone_dir = tempdir().unwrap();
    common::run_git(
        clone_dir.path(),
        [
            "clone",
            "--branch",
            "main",
            &format!("http://localhost:{}/test.git", server.port),
        ],
    )
    .unwrap();
    let repo_dir = clone_dir.path().join("test");

    // A normal fast-forward push should succeed.
    fs::write(repo_dir.join("ff.txt"), "fast forward\n").unwrap();
    common::run_git(&repo_dir, ["add", "ff.txt"]).unwrap();
    common::run_git(&repo_dir, ["commit", "-m", "FF commit"]).unwrap();
    common::run_git(&repo_dir, ["push", "origin", "main"]).unwrap();

    // Reset to before the FF commit and make a diverging commit — this is a force push.
    common::run_git(&repo_dir, ["reset", "--hard", "HEAD~1"]).unwrap();
    fs::write(repo_dir.join("diverge.txt"), "diverging\n").unwrap();
    common::run_git(&repo_dir, ["add", "diverge.txt"]).unwrap();
    common::run_git(&repo_dir, ["commit", "-m", "Diverging commit"]).unwrap();

    let err = common::run_git(&repo_dir, ["push", "--force", "origin", "main"]).unwrap_err();
    assert!(
        err.to_string().contains("ForcePush"),
        "expected 'ForcePush' in error, got: {err}"
    );

    server.stop();
}

#[cfg(feature = "axum")]
#[test]
fn test_create_denied() {
    let temprepo = common::temprepo().unwrap();
    let server = kind_filter_server(temprepo.path(), PushKind::Create);

    let clone_dir = tempdir().unwrap();
    common::run_git(
        clone_dir.path(),
        [
            "clone",
            "--branch",
            "main",
            &format!("http://localhost:{}/test.git", server.port),
        ],
    )
    .unwrap();
    let repo_dir = clone_dir.path().join("test");

    // Pushing to an existing branch (fast-forward) should succeed.
    fs::write(repo_dir.join("ok.txt"), "ok\n").unwrap();
    common::run_git(&repo_dir, ["add", "ok.txt"]).unwrap();
    common::run_git(&repo_dir, ["commit", "-m", "FF commit"]).unwrap();
    common::run_git(&repo_dir, ["push", "origin", "main"]).unwrap();

    // Pushing a new branch should be rejected.
    common::run_git(&repo_dir, ["checkout", "-b", "new-branch"]).unwrap();
    let err = common::run_git(&repo_dir, ["push", "origin", "new-branch"]).unwrap_err();
    assert!(
        err.to_string().contains("Create"),
        "expected 'Create' in error, got: {err}"
    );

    server.stop();
}

#[cfg(feature = "axum")]
#[test]
fn test_delete_denied() {
    let temprepo = common::temprepo().unwrap();
    let server = kind_filter_server(temprepo.path(), PushKind::Delete);

    let clone_dir = tempdir().unwrap();
    common::run_git(
        clone_dir.path(),
        [
            "clone",
            &format!("http://localhost:{}/test.git", server.port),
        ],
    )
    .unwrap();
    let repo_dir = clone_dir.path().join("test");

    // Pushing a fast-forward commit should succeed.
    fs::write(repo_dir.join("ok.txt"), "ok\n").unwrap();
    common::run_git(&repo_dir, ["add", "ok.txt"]).unwrap();
    common::run_git(&repo_dir, ["commit", "-m", "FF commit"]).unwrap();
    common::run_git(&repo_dir, ["push", "origin", "main"]).unwrap();

    // Deleting a remote branch should be rejected.
    let err = common::run_git(&repo_dir, ["push", "origin", "--delete", "dev"]).unwrap_err();
    assert!(
        err.to_string().contains("Delete"),
        "expected 'Delete' in error, got: {err}"
    );

    server.stop();
}
