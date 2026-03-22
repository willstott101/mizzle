mod common;

use std::fs;
use tempfile::tempdir;

use common::Config;

dual_backend_test!(test_push, |make_server: fn(
    Config,
) -> common::ServerHandle| {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path().clone(),
    };
    let server = make_server(config);

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

    fs::write(repo_dir.join("push_test.txt"), "pushed content\n")?;
    common::run_git(&repo_dir, ["add", "push_test.txt"])?;
    common::run_git(&repo_dir, ["commit", "-m", "Push test commit"])?;
    let pushed_commit = common::run_git(&repo_dir, ["rev-parse", "HEAD"])?;

    common::run_git(&repo_dir, ["push", "origin", "main"])?;

    let bare_head = common::run_git(temprepo.path().as_path(), ["rev-parse", "refs/heads/main"])?;
    assert_eq!(
        bare_head, pushed_commit,
        "bare repo main should point to the pushed commit"
    );

    server.stop();
    Ok(())
});
