mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tempfile::tempdir;

use mizzle::traits::{Comparison, PostReceiveFut, PushKind, RepoAccess};

// ── Access types ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AutoInitAccess {
    repo_path: PathBuf,
    enabled: bool,
}

impl RepoAccess for AutoInitAccess {
    type RepoId = PathBuf;
    type PushContext = ();

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
    received: Arc<Mutex<Option<Vec<(String, PushKind)>>>>,
    reject_with: Option<String>,
}

impl RepoAccess for RecordingAccess {
    type RepoId = PathBuf;
    type PushContext = ();

    fn repo_id(&self) -> &PathBuf {
        &self.repo_path
    }

    fn authorize_push(&self, _ctx: &(), _push: &dyn Comparison<'_>) -> Result<(), String> {
        match &self.reject_with {
            Some(msg) => Err(msg.clone()),
            None => Ok(()),
        }
    }

    fn post_receive(&self, push: &dyn Comparison<'_>) -> PostReceiveFut {
        let data: Vec<(String, PushKind)> = push
            .refs()
            .iter()
            .map(|r| (r.refname.to_string(), r.kind.clone()))
            .collect();
        let received = self.received.clone();
        Box::pin(async move {
            *received.lock().unwrap() = Some(data);
        })
    }
}

// ── Dual-backend tests ──────────────────────────────────────────────────────

dual_backend_access_test!(test_auto_init_creates_repo_on_push, |backend| {
    let temp = tempdir()?;
    let repo_path = temp.path().join("new.git");
    assert!(!repo_path.exists());

    let server =
        common::axum_access_server_with_backend(repo_path.clone(), backend, |rp| AutoInitAccess {
            repo_path: PathBuf::from(rp.as_ref()),
            enabled: true,
        });

    let work = tempdir()?;
    common::run_git(work.path(), ["init", "-b", "main"])?;
    common::run_git(work.path(), ["config", "user.email", "t@t.com"])?;
    common::run_git(work.path(), ["config", "user.name", "T"])?;
    std::fs::write(work.path().join("f.txt"), "hello\n")?;
    common::run_git(work.path(), ["add", "."])?;
    common::run_git(work.path(), ["commit", "-m", "init"])?;
    common::run_git(
        work.path(),
        [
            "push",
            &format!("http://localhost:{}/new.git", server.port),
            "main",
        ],
    )?;

    assert!(repo_path.exists(), "repo should have been initialised");
    server.stop();
    Ok(())
});

dual_backend_access_test!(test_post_receive_called_after_push, |backend| {
    let temprepo = common::temprepo()?;
    let received: Arc<Mutex<Option<Vec<(String, PushKind)>>>> = Arc::new(Mutex::new(None));
    let received_clone = received.clone();

    let server = common::axum_access_server_with_backend(temprepo.path(), backend, move |rp| {
        RecordingAccess {
            repo_path: PathBuf::from(rp.as_ref()),
            received: received_clone.clone(),
            reject_with: None,
        }
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

    std::fs::write(repo_dir.join("hook-test.txt"), "hook test\n")?;
    common::run_git(&repo_dir, ["add", "hook-test.txt"])?;
    common::run_git(&repo_dir, ["commit", "-m", "Hook test commit"])?;
    common::run_git(&repo_dir, ["push", "origin", "main"])?;

    let guard = received.lock().unwrap();
    let data = guard
        .as_ref()
        .expect("post_receive should have been called");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].0, "refs/heads/main");
    assert_eq!(data[0].1, PushKind::FastForward);

    server.stop();
    Ok(())
});

// ── Single-backend tests (error paths, not backend-sensitive) ────────────────

#[test]
fn test_auto_init_disabled_returns_error() {
    let temp = tempdir().unwrap();
    let repo_path = temp.path().join("nonexistent.git");

    let server = common::axum_access_server(repo_path, |rp| AutoInitAccess {
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

#[test]
fn test_post_receive_not_called_on_rejection() {
    let temprepo = common::temprepo().unwrap();
    let received: Arc<Mutex<Option<Vec<(String, PushKind)>>>> = Arc::new(Mutex::new(None));
    let received_clone = received.clone();

    let server = common::axum_access_server(temprepo.path(), move |rp| RecordingAccess {
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
