mod common;

use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

use mizzle::traits::{GitServerCallbacks, PushRef};

// A config whose auth() always denies by returning an empty path.
#[derive(Clone)]
struct DenyAuthConfig;

impl GitServerCallbacks for DenyAuthConfig {
    fn auth(&self, _repo_path: &str) -> Box<str> {
        "".into()
    }
}

// A config that allows reads but rejects all pushes.
#[derive(Clone)]
struct DenyPushConfig {
    bare_repo_path: PathBuf,
}

impl GitServerCallbacks for DenyPushConfig {
    fn auth(&self, _repo_path: &str) -> Box<str> {
        self.bare_repo_path.to_str().unwrap().into()
    }

    fn authorize_push(&self, _repo_path: &str, _refs: &[PushRef<'_>]) -> Result<(), String> {
        Err("permission denied".to_string())
    }
}

#[test]
fn test_clone_denied() {
    let temprepo = common::temprepo().unwrap();
    let (port, tx) = common::axum_server(DenyAuthConfig);

    let clone_dir = tempdir().unwrap();
    let result = common::run_git(
        clone_dir.path(),
        [
            "clone",
            "--branch",
            "main",
            &format!("http://localhost:{}/test.git", port),
        ],
    );

    assert!(result.is_err(), "clone should have been rejected");
    let _ = tx.send(());
    drop(temprepo);
}

#[test]
fn test_push_denied() {
    let temprepo = common::temprepo().unwrap();
    let (port, tx) = common::axum_server(DenyPushConfig {
        bare_repo_path: temprepo.path().clone(),
    });

    // Clone succeeds (auth allows reads).
    let clone_dir = tempdir().unwrap();
    common::run_git(
        clone_dir.path(),
        [
            "clone",
            "--branch",
            "main",
            &format!("http://localhost:{}/test.git", port),
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

    let _ = tx.send(());
}
