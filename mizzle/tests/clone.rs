mod common;

use tempfile::tempdir;

use common::Config;

#[test]
fn test_clone_protocol_v1() -> anyhow::Result<()> {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path(),
    };
    let server = common::axum_server(config);

    let clone_dir = tempdir()?;
    // -c protocol.version=0 forces the client to use the old dumb HTTP
    // discovery path (GET /info/refs?service=git-upload-pack) with no
    // Git-Protocol header, which exercises our v1 upload-pack handler.
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
}

#[test]
fn test_clone() -> anyhow::Result<()> {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path(),
    };
    let server = common::axum_server(config);

    let git_output = common::run_git(
        tempdir()?.path(),
        [
            "clone",
            format!("http://localhost:{}/test.git", server.port).as_ref(),
        ],
    )?;
    println!("{}", git_output);

    server.stop();
    Ok(())
}

#[test]
fn test_shallow_clone() -> anyhow::Result<()> {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path(),
    };
    let server = common::axum_server(config);

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

    // With --depth 1, `git log` should show only the single tip commit.
    let log = common::run_git(&repo_dir, ["log", "--oneline"])?;
    let commit_count = log.lines().count();
    assert_eq!(
        commit_count, 1,
        "shallow clone --depth 1 should have exactly 1 commit, got:\n{}",
        log
    );

    // `git rev-parse --is-shallow-repository` confirms it's shallow.
    let is_shallow = common::run_git(&repo_dir, ["rev-parse", "--is-shallow-repository"])?;
    assert_eq!(is_shallow, "true", "cloned repo should be shallow");

    // fsck should pass — the shallow grafts are well-formed.
    common::run_git(&repo_dir, ["fsck", "--no-progress"])?;

    server.stop();
    Ok(())
}

// The temprepo has 3 commits (2 on main, 1 on dev).  --depth 2 on main
// should return exactly 2 commits, catching off-by-one boundary errors.
#[test]
fn test_shallow_clone_depth_2() -> anyhow::Result<()> {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path(),
    };
    let server = common::axum_server(config);

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
        "shallow clone --depth 2 should have exactly 2 commits, got:\n{}",
        log
    );

    let is_shallow = common::run_git(&repo_dir, ["rev-parse", "--is-shallow-repository"])?;
    assert_eq!(is_shallow, "true", "cloned repo should be shallow");

    common::run_git(&repo_dir, ["fsck", "--no-progress"])?;

    server.stop();
    Ok(())
}

#[test]
fn test_partial_clone_tree_none() -> anyhow::Result<()> {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path(),
    };
    let server = common::axum_server(config);

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

    // The clone should succeed and have commits.
    let log = common::run_git(&repo_dir, ["log", "--oneline"])?;
    assert!(
        !log.is_empty(),
        "partial clone should have at least one commit"
    );

    // Verify by checking that git recognizes it as a partial clone.
    let is_partial = common::run_git(&repo_dir, ["config", "--get", "remote.origin.promisor"])?;
    assert_eq!(is_partial, "true", "should be a promisor/partial clone");

    server.stop();
    Ok(())
}

#[test]
fn test_partial_clone_blob_none() -> anyhow::Result<()> {
    let temprepo = common::temprepo()?;
    let config = Config {
        bare_repo_path: temprepo.path(),
    };
    let server = common::axum_server(config);

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

    // The clone should succeed and have commits.
    let log = common::run_git(&repo_dir, ["log", "--oneline"])?;
    assert!(
        !log.is_empty(),
        "partial clone should have at least one commit"
    );

    // Verify by checking that git recognizes it as a partial clone.
    let is_partial = common::run_git(&repo_dir, ["config", "--get", "remote.origin.promisor"])?;
    assert_eq!(is_partial, "true", "should be a promisor/partial clone");

    server.stop();
    Ok(())
}
