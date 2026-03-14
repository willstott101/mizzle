mod common;

use std::fs;

use anyhow::Result;
use tempfile::tempdir;

use common::{axum_server, Config};

#[test]
fn test_fetch_axum() -> Result<()> {
    let temprepo = common::temprepo()?;

    let config = Config {
        bare_repo_path: temprepo.path().clone(),
    };

    let (port, tx) = axum_server(config);

    let cloned = tempdir()?;

    // Clone from the axum server
    common::run_git(
        cloned.path(),
        [
            "clone",
            "--branch",
            "main",
            format!("http://localhost:{}/test.git", port).as_ref(),
        ],
    )?;

    let clone_dir = cloned.path().join("test");
    let main_before = common::run_git(clone_dir.as_path(), ["rev-parse", "origin/main"])?;

    // Add a new commit to the bare repo so the client can fetch it. We push via the
    // filesystem to the same bare repo the server serves—not through HTTP (server
    // doesn't support push).
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

    // Fetch the new commit from the server (via HTTP)
    common::run_git(clone_dir.as_path(), ["fetch", "origin", "main"])?;

    // Verify we got the new commit
    let main_after = common::run_git(clone_dir.as_path(), ["rev-parse", "origin/main"])?;
    assert_eq!(main_after, new_commit, "origin/main should be the new commit");
    assert_ne!(
        main_before, main_after,
        "origin/main should have advanced"
    );

    let _ = tx.send(());

    Ok(())
}

// Verifies that the server correctly honours the `thin-pack` fetch capability.
//
// For a thin pack to actually contain RefDelta entries the objects being sent
// must be stored as OfsDelta in the server's pack file, with their bases outside
// the output set.  We arrange this by:
//   1. Repacking the server before the client clones (so existing objects live in
//      a pack with delta chains).
//   2. Pushing a new commit that modifies an existing file — the updated blob is a
//      good delta candidate against the original.
//   3. Repacking again so the new objects are also packed and delta-compressed
//      against the objects the client already holds.
//
// git automatically includes `thin-pack` in its fetch capabilities whenever the
// client has existing objects, so no special client flags are needed.
//
// We verify correctness with `git fsck`: git thickens thin packs on receipt
// (resolving all RefDelta bases from the local repo), and fsck would catch any
// pack that references a base the client doesn't have.
#[test]
fn fetch_with_thin_pack() -> anyhow::Result<()> {
    let temprepo = common::temprepo()?;
    let server = temprepo.path();

    // Pack all existing server objects so delta chains are built.
    common::run_git(&server, ["repack", "-a", "-d"])?;

    let config = Config {
        bare_repo_path: server.clone(),
    };
    let (port, tx) = axum_server(config);

    let cloned = tempdir()?;
    common::run_git(
        cloned.path(),
        [
            "clone",
            "--branch",
            "main",
            format!("http://localhost:{}/test.git", port).as_ref(),
        ],
    )?;
    let clone_dir = cloned.path().join("test");
    let main_before = common::run_git(&clone_dir, ["rev-parse", "origin/main"])?;

    // Push a new commit that modifies an existing file.  Using a file that
    // already exists (README.md) means the new blob is a near-identical delta
    // candidate against the blob the client received in the clone.
    let server_work = tempdir()?;
    common::run_git(
        server_work.path(),
        ["clone", server.to_str().unwrap()],
    )?;
    let server_repo = server_work.path().join("temprepo");
    let mut readme = fs::read_to_string(server_repo.join("README.md"))?;
    readme.push_str("\n## Thin-pack test section\n\nAdded to exercise thin-pack delta compression.\n");
    fs::write(server_repo.join("README.md"), &readme)?;
    common::run_git(&server_repo, ["add", "README.md"])?;
    common::run_git(&server_repo, ["commit", "-m", "Update README for thin-pack test"])?;
    common::run_git(&server_repo, ["push", "origin", "main"])?;
    let new_commit = common::run_git(&server_repo, ["rev-parse", "HEAD"])?;

    // Repack again so the new objects end up in a pack file and git can build
    // deltas between the new and old README blobs.
    common::run_git(&server, ["repack", "-a", "-d"])?;

    // Fetch — git automatically sends `thin-pack` since the client has existing
    // objects, so the server will now produce RefDelta entries for any blobs
    // whose delta base is in the client's repository.
    common::run_git(&clone_dir, ["fetch", "origin", "main"])?;

    let main_after = common::run_git(&clone_dir, ["rev-parse", "origin/main"])?;
    assert_eq!(main_after, new_commit, "origin/main should point to the new commit");
    assert_ne!(main_before, main_after, "origin/main should have advanced");

    // fsck verifies that git successfully thickened the thin pack (resolved all
    // RefDelta bases from the local objects) and that the result is consistent.
    common::run_git(&clone_dir, ["fsck", "--no-progress"])?;

    // Confirm the updated file content is readable.
    let fetched_readme = common::run_git(&clone_dir, ["show", "origin/main:README.md"])?;
    assert!(fetched_readme.contains("Thin-pack test section"));

    let _ = tx.send(());
    Ok(())
}

