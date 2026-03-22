mod common;

use tempfile::tempdir;

use common::Config;

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

// When --depth covers the full history (2 commits on main), both backends
// should recognise there is nothing to cut off and tell the client it has a
// complete clone rather than marking it shallow.
dual_backend_test!(
    test_shallow_clone_depth_covers_full_history,
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
                "2",
                "--branch",
                "main",
                format!("http://localhost:{}/test.git", server.port).as_ref(),
            ],
        )?;

        let repo_dir = clone_dir.path().join("test");

        let log = common::run_git(&repo_dir, ["log", "--oneline"])?;
        let commit_count = log.lines().count();
        assert_eq!(
            commit_count, 2,
            "clone --depth 2 should have exactly 2 commits, got:\n{}",
            log
        );

        let is_shallow = common::run_git(&repo_dir, ["rev-parse", "--is-shallow-repository"])?;
        assert_eq!(
            is_shallow, "false",
            "depth covers full history — client should see a full clone"
        );

        common::run_git(&repo_dir, ["fsck", "--no-progress"])?;

        server.stop();
        Ok(())
    }
);

dual_backend_test!(
    test_partial_clone_tree_none,
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
                "--filter=tree:0",
                "--branch",
                "main",
                format!("http://localhost:{}/test.git", server.port).as_ref(),
            ],
        )?;

        let repo_dir = clone_dir.path().join("test");

        let log = common::run_git(&repo_dir, ["log", "--oneline"])?;
        assert!(
            !log.is_empty(),
            "partial clone should have at least one commit"
        );

        let is_partial = common::run_git(&repo_dir, ["config", "--get", "remote.origin.promisor"])?;
        assert_eq!(is_partial, "true", "should be a promisor/partial clone");

        server.stop();
        Ok(())
    }
);

dual_backend_test!(
    test_partial_clone_blob_none,
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
                "--filter=blob:none",
                "--branch",
                "main",
                format!("http://localhost:{}/test.git", server.port).as_ref(),
            ],
        )?;

        let repo_dir = clone_dir.path().join("test");

        let log = common::run_git(&repo_dir, ["log", "--oneline"])?;
        assert!(
            !log.is_empty(),
            "partial clone should have at least one commit"
        );

        let is_partial = common::run_git(&repo_dir, ["config", "--get", "remote.origin.promisor"])?;
        assert_eq!(is_partial, "true", "should be a promisor/partial clone");

        server.stop();
        Ok(())
    }
);
