mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use tempfile::tempdir;

use mizzle::traits::{PostReceiveFut, PushKind, PushRef, RepoAccess};

// ── Access types ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AutoInitAccess {
    repo_path: PathBuf,
    enabled: bool,
}

impl RepoAccess for AutoInitAccess {
    type RepoId = PathBuf;

    fn repo_id(&self) -> &PathBuf {
        &self.repo_path
    }
    fn auto_init(&self) -> bool {
        self.enabled
    }
}

#[derive(Clone)]
struct RecordingAccess {
    repo_path: PathBuf,
    // None = hook not yet called; Some(vec) = hook was called with these refs.
    received: Arc<Mutex<Option<Vec<(String, PushKind)>>>>,
    // When Some, authorize_push will return this error (simulates a rejection).
    reject_with: Option<String>,
}

impl RepoAccess for RecordingAccess {
    type RepoId = PathBuf;

    fn repo_id(&self) -> &PathBuf {
        &self.repo_path
    }

    fn authorize_push(&self, _refs: &[PushRef<'_>]) -> Result<(), String> {
        match &self.reject_with {
            Some(msg) => Err(msg.clone()),
            None => Ok(()),
        }
    }

    fn post_receive<'a>(&'a self, refs: &'a [PushRef<'a>]) -> PostReceiveFut<'a> {
        let data = refs
            .iter()
            .map(|r| (r.refname.to_string(), r.kind.clone()))
            .collect();
        let received = self.received.clone();
        Box::pin(async move {
            *received.lock().unwrap() = Some(data);
        })
    }
}

// ── Server helpers ────────────────────────────────────────────────────────────

fn axum_access_server<A, F>(repo_path: PathBuf, make_access: F) -> common::ServerHandle
where
    A: RepoAccess<RepoId = PathBuf> + Send + 'static,
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
        A: RepoAccess<RepoId = PathBuf> + Send + 'static,
        F: Fn(Box<str>) -> A + Send + Sync + 'static,
    >(
        State(state): State<Arc<(String, F)>>,
        Path(path): Path<String>,
        req: Request,
    ) -> Response {
        let access = state.1(state.0.as_str().into());
        let limits = mizzle::serve::ProtocolLimits::default();
        mizzle::servers::axum::serve(access, &path, &limits, req).await
    }

    let state = Arc::new((repo_path.to_str().unwrap().to_string(), make_access));
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

// ── auto-init tests ───────────────────────────────────────────────────────────

/// Push to a repo path that does not exist yet.  With auto_init enabled the
/// push should succeed and leave the repo in place.
#[test]
fn test_auto_init_creates_repo_on_push() {
    let temp = tempdir().unwrap();
    let repo_path = temp.path().join("new.git"); // does not exist yet
    assert!(!repo_path.exists());

    let server = axum_access_server(repo_path.clone(), |rp| AutoInitAccess {
        repo_path: PathBuf::from(rp.as_ref()),
        enabled: true,
    });

    // Build a fresh local working repo with no remote-tracking history so
    // git will push all objects rather than deciding "Everything up-to-date".
    let work = tempdir().unwrap();
    common::run_git(work.path(), ["init", "-b", "main"]).unwrap();
    common::run_git(work.path(), ["config", "user.email", "t@t.com"]).unwrap();
    common::run_git(work.path(), ["config", "user.name", "T"]).unwrap();
    std::fs::write(work.path().join("f.txt"), "hello\n").unwrap();
    common::run_git(work.path(), ["add", "."]).unwrap();
    common::run_git(work.path(), ["commit", "-m", "init"]).unwrap();
    common::run_git(
        work.path(),
        [
            "push",
            &format!("http://localhost:{}/new.git", server.port),
            "main",
        ],
    )
    .unwrap();

    assert!(repo_path.exists(), "repo should have been initialised");
    server.stop();
}

/// With auto_init disabled, pushing to a nonexistent path returns a 500.
#[test]
fn test_auto_init_disabled_returns_error() {
    let temp = tempdir().unwrap();
    let repo_path = temp.path().join("nonexistent.git");

    let server = axum_access_server(repo_path, |rp| AutoInitAccess {
        repo_path: PathBuf::from(rp.as_ref()),
        enabled: false,
    });

    let src = common::temprepo().unwrap();
    let err = common::run_git(
        &src.path(),
        [
            "push",
            &format!("http://localhost:{}/nonexistent.git", server.port),
            "main",
        ],
    );
    assert!(err.is_err(), "push to nonexistent repo should fail");

    server.stop();
}

// ── post-receive tests ────────────────────────────────────────────────────────

/// post_receive is called after a successful push and receives the correct
/// ref names and push kinds.
///
/// Note: post_receive runs inside the spawn that writes the git response, so
/// the HTTP response stream only closes (and git considers the push complete)
/// after post_receive returns.  No sleep or extra synchronisation is needed.
#[test]
fn test_post_receive_called_after_push() {
    let temprepo = common::temprepo().unwrap();
    let received: Arc<Mutex<Option<Vec<(String, PushKind)>>>> = Arc::new(Mutex::new(None));
    let received_clone = received.clone();

    let server = axum_access_server(temprepo.path(), move |rp| RecordingAccess {
        repo_path: PathBuf::from(rp.as_ref()),
        received: received_clone.clone(),
        reject_with: None,
    });

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

    std::fs::write(repo_dir.join("hook-test.txt"), "hook test\n").unwrap();
    common::run_git(&repo_dir, ["add", "hook-test.txt"]).unwrap();
    common::run_git(&repo_dir, ["commit", "-m", "Hook test commit"]).unwrap();
    common::run_git(&repo_dir, ["push", "origin", "main"]).unwrap();

    let guard = received.lock().unwrap();
    let data = guard
        .as_ref()
        .expect("post_receive should have been called");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].0, "refs/heads/main");
    assert_eq!(data[0].1, PushKind::FastForward);

    server.stop();
}

/// post_receive must NOT be called when authorize_push rejects the push.
#[test]
fn test_post_receive_not_called_on_rejection() {
    let temprepo = common::temprepo().unwrap();
    let received: Arc<Mutex<Option<Vec<(String, PushKind)>>>> = Arc::new(Mutex::new(None));
    let received_clone = received.clone();

    let server = axum_access_server(temprepo.path(), move |rp| RecordingAccess {
        repo_path: PathBuf::from(rp.as_ref()),
        received: received_clone.clone(),
        reject_with: Some("not allowed".to_string()),
    });

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

    std::fs::write(repo_dir.join("rejected.txt"), "rejected\n").unwrap();
    common::run_git(&repo_dir, ["add", "rejected.txt"]).unwrap();
    common::run_git(&repo_dir, ["commit", "-m", "Should be rejected"]).unwrap();
    let _ = common::run_git(&repo_dir, ["push", "origin", "main"]);

    assert!(
        received.lock().unwrap().is_none(),
        "post_receive should not be called when push is rejected"
    );

    server.stop();
}
