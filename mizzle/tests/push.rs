mod common;

use std::fs;
use tempfile::tempdir;

use common::Config;

// First push to a completely blank repo.  Without the `ofs-delta` capability
// being advertised in `info_refs_receive_pack_task`, git falls back to
// REF_DELTA for intra-pack objects, which the receiver cannot resolve against
// an empty ODB.
dual_backend_test!(
    test_push_to_blank_repo,
    |make_server: fn(Config) -> common::ServerHandle| {
        let blank = common::bare_empty_repo()?;
        let config = Config {
            bare_repo_path: blank.path(),
        };
        let server = make_server(config);

        let work_dir = tempdir()?;
        common::run_git(work_dir.path(), ["init", "-b", "main"])?;
        // Push several commits each rewriting a large file so git generates
        // intra-pack deltas.  Without `ofs-delta` the receiver gets REF_DELTA
        // objects whose bases are also in the incoming pack — not yet in the
        // ODB — causing "object not found".
        for i in 0..5u32 {
            let content: String = (0..500)
                .map(|j| format!("commit {i} line {j:04}: padding for delta compression\n"))
                .collect();
            fs::write(work_dir.path().join("data.txt"), content)?;
            common::run_git(work_dir.path(), ["add", "data.txt"])?;
            common::run_git(work_dir.path(), ["commit", "-m", &format!("commit {i}")])?;
        }
        let local_head = common::run_git(work_dir.path(), ["rev-parse", "HEAD"])?;

        common::run_git(
            work_dir.path(),
            [
                "push",
                &format!("http://localhost:{}/test.git", server.port),
                "main",
            ],
        )?;

        let bare_head = common::run_git(blank.path().as_path(), ["rev-parse", "refs/heads/main"])?;
        assert_eq!(
            bare_head, local_head,
            "blank repo should contain the pushed commit"
        );

        server.stop();
        Ok(())
    }
);

// Push a thin pack that contains REF_DELTA objects referencing blobs already
// in the server ODB.  git push sends thin packs by default; when the client
// modifies a large file it has cloned, git delta-compresses the new blob
// against the existing one and omits the base from the pack.  The receiver
// must resolve the base from its ODB via the thin-pack resolver in
// `ingest_pack_sync`.  Passing `None` there caused "object not found" on any
// repo large enough for git to choose cross-pack deltas.
dual_backend_test!(
    test_push_thin_pack,
    |make_server: fn(Config) -> common::ServerHandle| {
        let temprepo = common::temprepo_with_large_file()?;
        let config = Config {
            bare_repo_path: temprepo.path(),
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

        // Append a line to the large file so git delta-compresses the new blob
        // against the original (already in the server ODB), producing a thin pack.
        let mut content = fs::read_to_string(repo_dir.join("large.txt"))?;
        content.push_str("appended line\n");
        fs::write(repo_dir.join("large.txt"), content)?;
        common::run_git(&repo_dir, ["add", "large.txt"])?;
        common::run_git(&repo_dir, ["commit", "-m", "thin-pack push"])?;
        let pushed_head = common::run_git(&repo_dir, ["rev-parse", "HEAD"])?;

        common::run_git(&repo_dir, ["push", "origin", "main"])?;

        let bare_head =
            common::run_git(temprepo.path().as_path(), ["rev-parse", "refs/heads/main"])?;
        assert_eq!(
            bare_head, pushed_head,
            "server should reflect the pushed commit"
        );

        server.stop();
        Ok(())
    }
);

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
