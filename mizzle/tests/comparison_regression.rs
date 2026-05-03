//! Regression tests that exercise the `Comparison` handle and `StorageBackend`
//! directly, without going through the HTTP server.
//!
//! These cover bypass scenarios that are awkward to trigger through real
//! `git push` flows but represent real authorization risks.

mod common;

use std::path::PathBuf;

use gix::ObjectId;
use mizzle::auth::{run_comparison, ComparisonOptions};
use mizzle::backend::{PackMetadata, StorageBackend};
use mizzle::traits::{PushKind, PushRef, RepoAccess};

#[derive(Clone)]
struct NoopAccess {
    repo: PathBuf,
}

impl RepoAccess for NoopAccess {
    type RepoId = PathBuf;
    type PushContext = ();
    fn repo_id(&self) -> &PathBuf {
        &self.repo
    }
}

// ── Bug 1: `new_commits` must surface commits that are in the ODB but not
// in the staged pack ──────────────────────────────────────────────────────────
//
// If a push points a ref at a commit already in the ODB and the staged pack
// does not contain it (empty pack, or thin-pack omission), `new_commits` must
// still include that commit.  Previously the OnceCell cached only OIDs and
// the `find_commit` lookup against the staged pack returned `None`, silently
// dropping the commit and letting any commit-content rule (committer email,
// DCO, merges-only, …) be bypassed.

#[test]
fn new_commits_includes_odb_only_commit_gitoxide() -> anyhow::Result<()> {
    new_commits_includes_odb_only_commit(mizzle::backend::fs_gitoxide::FsGitoxide)
}

#[test]
fn new_commits_includes_odb_only_commit_git_cli() -> anyhow::Result<()> {
    new_commits_includes_odb_only_commit(mizzle::backend::fs_git_cli::FsGitCli)
}

fn new_commits_includes_odb_only_commit<B: StorageBackend<RepoId = PathBuf>>(
    backend: B,
) -> anyhow::Result<()> {
    let temprepo = common::temprepo()?;
    let repo = backend.open(&temprepo.path())?;

    // The temprepo has main (2 commits) and dev (3 commits, sharing 2 with main).
    // Pick the dev tip — it's in the ODB but not reachable from main.
    let dev_oid: ObjectId =
        common::run_git(temprepo.path().as_path(), ["rev-parse", "refs/heads/dev"])?.parse()?;
    let main_oid: ObjectId =
        common::run_git(temprepo.path().as_path(), ["rev-parse", "refs/heads/main"])?.parse()?;

    // Empty pack: simulates a push that points a ref at an existing commit
    // without sending any new objects.
    let pack = PackMetadata {
        objects: Vec::new(),
    };
    let refs = vec![PushRef {
        refname: "refs/heads/resurrected",
        kind: PushKind::Create,
        old_oid: ObjectId::null(gix_hash::Kind::Sha1),
        new_oid: dev_oid,
    }];

    // existing_ref_tips=[main]: dev_oid is unreachable from advertised refs.
    let access = NoopAccess {
        repo: temprepo.path(),
    };
    let oids: Vec<ObjectId> = run_comparison(
        &access,
        &backend,
        &repo,
        &pack,
        refs,
        vec![main_oid],
        ComparisonOptions::default(),
        |comp| {
            let r = comp.refs()[0].clone();
            comp.new_commits(&r)
                .map(|cs| cs.iter().map(|c| c.oid).collect())
        },
    )?;

    assert!(
        oids.contains(&dev_oid),
        "new_commits must include {dev_oid} even though the staged pack is empty; got {oids:?}"
    );
    Ok(())
}

// ── Bug 2: `read_blob` must reject non-blob OIDs ─────────────────────────────
//
// `FsGitCli::read_blob` already returns `Ok(None)` for non-blob kinds; the
// gitoxide implementation previously returned the raw object bytes regardless
// of kind, causing a silent behavioural divergence between backends.

#[test]
fn read_blob_returns_none_for_commit_oid_gitoxide() -> anyhow::Result<()> {
    read_blob_returns_none_for_commit_oid(mizzle::backend::fs_gitoxide::FsGitoxide)
}

#[test]
fn read_blob_returns_none_for_commit_oid_git_cli() -> anyhow::Result<()> {
    read_blob_returns_none_for_commit_oid(mizzle::backend::fs_git_cli::FsGitCli)
}

fn read_blob_returns_none_for_commit_oid<B: StorageBackend<RepoId = PathBuf>>(
    backend: B,
) -> anyhow::Result<()> {
    let temprepo = common::temprepo()?;
    let repo = backend.open(&temprepo.path())?;
    let head: ObjectId =
        common::run_git(temprepo.path().as_path(), ["rev-parse", "HEAD"])?.parse()?;
    let result = backend.read_blob(&repo, head, 16 * 1024 * 1024)?;
    assert!(
        result.is_none(),
        "read_blob on a commit OID must return None, got {} bytes",
        result.as_ref().map(|b| b.len()).unwrap_or(0)
    );
    Ok(())
}

#[test]
fn read_blob_returns_none_for_tree_oid_gitoxide() -> anyhow::Result<()> {
    read_blob_returns_none_for_tree_oid(mizzle::backend::fs_gitoxide::FsGitoxide)
}

#[test]
fn read_blob_returns_none_for_tree_oid_git_cli() -> anyhow::Result<()> {
    read_blob_returns_none_for_tree_oid(mizzle::backend::fs_git_cli::FsGitCli)
}

fn read_blob_returns_none_for_tree_oid<B: StorageBackend<RepoId = PathBuf>>(
    backend: B,
) -> anyhow::Result<()> {
    let temprepo = common::temprepo()?;
    let repo = backend.open(&temprepo.path())?;
    let tree: ObjectId =
        common::run_git(temprepo.path().as_path(), ["rev-parse", "HEAD^{tree}"])?.parse()?;
    let result = backend.read_blob(&repo, tree, 16 * 1024 * 1024)?;
    assert!(
        result.is_none(),
        "read_blob on a tree OID must return None, got {} bytes",
        result.as_ref().map(|b| b.len()).unwrap_or(0)
    );
    Ok(())
}
