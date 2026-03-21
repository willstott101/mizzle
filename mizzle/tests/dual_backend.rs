//! Run integration tests against both FsGitoxide and FsGitCli backends
//! to verify identical behaviour.

mod common;

use std::fs;
use tempfile::tempdir;

use common::Config;
use mizzle::backend::fs_git_cli::FsGitCli;

/// Generate a test for each backend.
macro_rules! dual_backend_test {
    ($name:ident, $body:expr) => {
        paste::paste! {
            #[test]
            fn [< $name _gitoxide >]() -> anyhow::Result<()> {
                let make_server = |config: Config| common::axum_server(config);
                $body(make_server)
            }

            #[test]
            fn [< $name _git_cli >]() -> anyhow::Result<()> {
                let make_server = |config: Config| {
                    common::axum_server_with_backend(config, FsGitCli)
                };
                $body(make_server)
            }
        }
    };
}

dual_backend_test!(test_clone, |make_server: fn(
    Config,
) -> common::ServerHandle| {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path(),
    };
    let server = make_server(config);

    common::run_git(
        tempdir()?.path(),
        [
            "clone",
            format!("http://localhost:{}/test.git", server.port).as_ref(),
        ],
    )?;

    server.stop();
    Ok(())
});

dual_backend_test!(test_clone_v1, |make_server: fn(
    Config,
) -> common::ServerHandle| {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path(),
    };
    let server = make_server(config);

    let clone_dir = tempdir()?;
    common::run_git(
        clone_dir.path(),
        [
            "-c",
            "protocol.version=0",
            "clone",
            "--branch",
            "main",
            format!("http://localhost:{}/test.git", server.port).as_ref(),
        ],
    )?;

    let repo_dir = clone_dir.path().join("test");
    let log = common::run_git(&repo_dir, ["log", "--oneline"])?;
    assert!(
        log.lines().count() >= 2,
        "v1 clone should have at least 2 commits, got:\n{}",
        log
    );

    server.stop();
    Ok(())
});

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

dual_backend_test!(test_fetch, |make_server: fn(
    Config,
) -> common::ServerHandle| {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path().clone(),
    };
    let server = make_server(config);

    let cloned = tempdir()?;
    common::run_git(
        cloned.path(),
        [
            "clone",
            "--branch",
            "main",
            format!("http://localhost:{}/test.git", server.port).as_ref(),
        ],
    )?;

    let clone_dir = cloned.path().join("test");
    let main_before = common::run_git(clone_dir.as_path(), ["rev-parse", "origin/main"])?;

    // Add a new commit to the bare repo
    let server_work = tempdir()?;
    common::run_git(
        server_work.path(),
        ["clone", temprepo.path().to_str().unwrap()],
    )?;
    let server_repo = server_work.path().join("temprepo");
    fs::write(server_repo.join("newfile.txt"), "new content\n")?;
    common::run_git(&server_repo, ["add", "newfile.txt"])?;
    common::run_git(&server_repo, ["commit", "-m", "New commit on server"])?;
    common::run_git(&server_repo, ["push", "origin", "main"])?;
    let new_commit = common::run_git(&server_repo, ["rev-parse", "HEAD"])?;

    common::run_git(clone_dir.as_path(), ["fetch", "origin", "main"])?;

    let main_after = common::run_git(clone_dir.as_path(), ["rev-parse", "origin/main"])?;
    assert_eq!(
        main_after, new_commit,
        "origin/main should be the new commit"
    );
    assert_ne!(main_before, main_after, "origin/main should have advanced");

    server.stop();
    Ok(())
});

dual_backend_test!(
    test_shallow_clone,
    |make_server: fn(Config) -> common::ServerHandle| {
        let temprepo = common::temprepo()?;
        let config = Config {
            bare_repo_path: temprepo.path(),
        };
        let server = make_server(config);

        let clone_dir = tempdir()?;
        common::run_git(
            clone_dir.path(),
            [
                "clone",
                "--depth",
                "1",
                "--branch",
                "main",
                format!("http://localhost:{}/test.git", server.port).as_ref(),
            ],
        )?;

        let repo_dir = clone_dir.path().join("test");

        let log = common::run_git(&repo_dir, ["log", "--oneline"])?;
        let commit_count = log.lines().count();
        assert_eq!(
            commit_count, 1,
            "shallow clone --depth 1 should have exactly 1 commit, got:\n{}",
            log
        );

        let is_shallow = common::run_git(&repo_dir, ["rev-parse", "--is-shallow-repository"])?;
        assert_eq!(is_shallow, "true", "cloned repo should be shallow");

        common::run_git(&repo_dir, ["fsck", "--no-progress"])?;

        server.stop();
        Ok(())
    }
);

dual_backend_test!(test_ls_remote, |make_server: fn(
    Config,
)
    -> common::ServerHandle| {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path(),
    };
    let server = make_server(config);

    let git_output_from_path = common::run_git(
        &temprepo.path(),
        ["ls-remote", temprepo.path().to_str().unwrap()],
    )?;
    let git_output_from_server = common::run_git(
        &temprepo.path(),
        [
            "ls-remote",
            format!("http://localhost:{}/test.git", server.port).as_ref(),
        ],
    )?;

    assert_eq!(git_output_from_path, git_output_from_server);

    server.stop();
    Ok(())
});
