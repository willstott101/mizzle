mod common;

use std::fs;
use tempfile::tempdir;

use common::{test_with_servers, Config};

test_with_servers!(test_push, |start_server| {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path().clone(),
    };
    let server = start_server(config);

    // Clone the repo from the server.
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

    // Make a new commit in the clone.
    fs::write(repo_dir.join("push_test.txt"), "pushed content\n")?;
    common::run_git(&repo_dir, ["add", "push_test.txt"])?;
    common::run_git(&repo_dir, ["commit", "-m", "Push test commit"])?;
    let pushed_commit = common::run_git(&repo_dir, ["rev-parse", "HEAD"])?;

    // Push to the server via HTTP.
    common::run_git(&repo_dir, ["push", "origin", "main"])?;

    // Verify the bare repo was updated.
    let bare_head = common::run_git(temprepo.path().as_path(), ["rev-parse", "refs/heads/main"])?;
    assert_eq!(
        bare_head, pushed_commit,
        "bare repo main should point to the pushed commit"
    );

    server.stop();
    Ok(())
});
