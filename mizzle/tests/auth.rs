mod common;

use std::fs;
use std::path::PathBuf;
use std::thread;
use tempfile::tempdir;

use mizzle::traits::{PushRef, RepoAccess};

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

/// Spin up an axum server that uses DenyPushAccess — reads work, pushes are rejected.
#[cfg(feature = "axum")]
fn deny_push_server(bare_repo_path: PathBuf) -> common::ServerHandle {
    use axum::{
        extract::{Path, Request, State},
        response::Response,
        routing::get,
        Router,
    };
    use std::sync::Arc;

    async fn handler(
        State(repo_path): State<Arc<String>>,
        Path(path): Path<String>,
        req: Request,
    ) -> Response {
        let access = DenyPushAccess {
            repo_path: repo_path.as_str().into(),
        };
        mizzle::servers::axum::serve(access, &path, req).await
    }

    let repo_path = Arc::new(bare_repo_path.to_str().unwrap().to_string());
    let app = Router::new()
        .route("/{*key}", get(handler).post(handler))
        .with_state(repo_path);

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
    let server = deny_push_server(temprepo.path().clone());

    // Clone succeeds (reads are allowed).
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

    // Make a commit and attempt to push.
    fs::write(repo_dir.join("denied.txt"), "this should not land\n").unwrap();
    common::run_git(&repo_dir, ["add", "denied.txt"]).unwrap();
    common::run_git(&repo_dir, ["commit", "-m", "Should be rejected"]).unwrap();

    let result = common::run_git(&repo_dir, ["push", "origin", "main"]);
    assert!(result.is_err(), "push should have been rejected");

    // Verify the bare repo was not updated.
    let bare_head =
        common::run_git(temprepo.path().as_path(), ["rev-parse", "refs/heads/main"]).unwrap();
    let local_head = common::run_git(&repo_dir, ["rev-parse", "HEAD"]).unwrap();
    assert_ne!(
        bare_head, local_head,
        "bare repo should not have been updated"
    );

    server.stop();
}
