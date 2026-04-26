//! Pack-reuse utilities for storage backends.
//!
//! When a pack file already on disk contains *exactly* the closure of a
//! fetch request, it can be streamed verbatim to the client, skipping the
//! count → delta-compute → stream pipeline.  This module provides building
//! blocks for backends to detect that case using git reachability bitmaps.
//!
//! Used by [`FsGitoxide`](crate::backend::fs_gitoxide::FsGitoxide)'s
//! `build_pack` fast path.  Other backends with locally-cached packs (e.g.
//! a networked backend with an SSD pack cache) can call these helpers
//! directly to decide whether they can short-circuit pack generation.
//!
//! The check is conservative on purpose: a pack is reusable only when it
//! contains exactly the requested closure — no extras (which would waste
//! client bandwidth) and no missing objects.  Trading bandwidth for CPU is
//! a separate optimisation and out of scope here.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gix::ObjectId;

use crate::bitmap::PackBitmap;

/// Find a pack in `pack_dir` whose bitmap proves it contains *exactly* the
/// closure of `want` minus the closure of `have`.
///
/// Returns `Ok(Some(pack_path))` pointing at the `.pack` file when reuse is
/// safe.  Returns `Ok(None)` when no pack qualifies — the caller must fall
/// back to the regular pack-generation pipeline.
///
/// This is the directory-scanning convenience wrapper around
/// [`pack_is_exactly_reusable`].  Backends with a single known pack should
/// call that lower-level helper directly instead.
#[tracing::instrument(skip_all, fields(want = want.len(), have = have.len()))]
pub fn find_reusable_pack(
    pack_dir: &Path,
    want: &[ObjectId],
    have: &[ObjectId],
) -> Result<Option<PathBuf>> {
    if want.is_empty() {
        return Ok(None);
    }
    let entries = match std::fs::read_dir(pack_dir) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("reading pack directory"),
    };
    for entry in entries.flatten() {
        let idx_path = entry.path();
        if idx_path.extension().and_then(|s| s.to_str()) != Some("idx") {
            continue;
        }
        if pack_is_exactly_reusable(&idx_path, want, have)? {
            tracing::debug!(idx = %idx_path.display(), "reusable pack found");
            return Ok(Some(idx_path.with_extension("pack")));
        }
    }
    Ok(None)
}

/// Check whether the pack identified by `idx_path` contains exactly the
/// closure of `want` minus the closure of `have`.
///
/// Returns `false` (not an error) when:
/// - the index can't be opened,
/// - no `.bitmap` / `.rev` sidecar is present,
/// - the bitmap uses unsupported features,
/// - any `want` or `have` OID is not a bitmap entry,
/// - the pack contents don't match the request closure exactly.
///
/// All of those are "no fast-path available" conditions, and the caller
/// falls back to the walker-driven pipeline.
#[tracing::instrument(skip_all, fields(idx = %idx_path.display(), want = want.len(), have = have.len()))]
pub fn pack_is_exactly_reusable(
    idx_path: &Path,
    want: &[ObjectId],
    have: &[ObjectId],
) -> Result<bool> {
    let pack_idx = match gix_pack::index::File::at(idx_path, gix_hash::Kind::Sha1) {
        Ok(idx) => idx,
        Err(_) => return Ok(false),
    };
    let obj_count = pack_idx.num_objects();
    let mut bitmap = match PackBitmap::load(idx_path, obj_count)? {
        Some(b) => b,
        None => return Ok(false),
    };
    bitmap.build_oid_index(|pos| pack_idx.oid_at_index(pos).try_into().ok());
    Ok(bitmap.covers_exactly(want, have).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t.com")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t.com")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn rev_parse(cwd: &Path, rev: &str) -> ObjectId {
        ObjectId::from_hex(git(cwd, &["rev-parse", rev]).as_bytes()).unwrap()
    }

    /// `git repack -adb` produces a single closure-complete pack with bitmaps.
    /// A clone-style request (want=tip, no haves) must find it as reusable.
    #[test]
    fn finds_pack_for_clone_of_repacked_repo() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        for i in 0..5 {
            std::fs::write(p.join("f.txt"), format!("c{i}\n")).unwrap();
            git(p, &["add", "f.txt"]);
            git(p, &["commit", "-q", "-m", &format!("c{i}")]);
        }
        let tip = rev_parse(p, "HEAD");
        git(p, &["repack", "-adb"]);

        let pack_dir = p.join(".git/objects/pack");
        let result = find_reusable_pack(&pack_dir, &[tip], &[]).unwrap();
        let pack = result.expect("repacked single-pack repo should be reusable");
        assert_eq!(pack.extension().and_then(|s| s.to_str()), Some("pack"));
        assert!(pack.is_file());
    }

    /// Without `repack -adb` there is no `.bitmap`, so reuse must not fire.
    /// (gix's default pack writer doesn't emit bitmaps.)
    #[test]
    fn no_bitmap_means_no_reuse() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        std::fs::write(p.join("f.txt"), "x\n").unwrap();
        git(p, &["add", "f.txt"]);
        git(p, &["commit", "-q", "-m", "c0"]);
        let tip = rev_parse(p, "HEAD");
        // Plain `repack -ad` (no -b) — produces a pack but no bitmap.
        git(p, &["repack", "-ad"]);

        let pack_dir = p.join(".git/objects/pack");
        let result = find_reusable_pack(&pack_dir, &[tip], &[]).unwrap();
        assert!(result.is_none(), "no .bitmap → no reuse");
    }

    /// Incremental fetch: closure(want) ⊃ closure(want) \ closure(have).
    /// The pack contains the full closure, so reuse must refuse — sending
    /// the whole pack would waste bandwidth.
    #[test]
    fn incremental_fetch_does_not_reuse() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        let mut tips = Vec::new();
        for i in 0..5 {
            std::fs::write(p.join("f.txt"), format!("c{i}\n")).unwrap();
            git(p, &["add", "f.txt"]);
            git(p, &["commit", "-q", "-m", &format!("c{i}")]);
            tips.push(rev_parse(p, "HEAD"));
        }
        git(p, &["repack", "-adb"]);

        let pack_dir = p.join(".git/objects/pack");
        let result = find_reusable_pack(&pack_dir, &[*tips.last().unwrap()], &[tips[2]]).unwrap();
        assert!(result.is_none(), "incremental fetch must not reuse pack");
    }

    /// Missing pack directory must not be an error — backends may call this
    /// against a freshly-initialised repo where `objects/pack/` doesn't yet
    /// exist.
    #[test]
    fn missing_pack_dir_returns_none() {
        let dir = tempdir().unwrap();
        let result = find_reusable_pack(
            &dir.path().join("does-not-exist"),
            &[ObjectId::null(gix_hash::Kind::Sha1)],
            &[],
        )
        .unwrap();
        assert!(result.is_none());
    }

    /// Empty wants is a degenerate request (no objects requested) — return
    /// `None` so the caller's normal pipeline can produce an empty pack.
    #[test]
    fn empty_want_returns_none() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        std::fs::write(p.join("f.txt"), "x\n").unwrap();
        git(p, &["add", "f.txt"]);
        git(p, &["commit", "-q", "-m", "c"]);
        git(p, &["repack", "-adb"]);

        let pack_dir = p.join(".git/objects/pack");
        let result = find_reusable_pack(&pack_dir, &[], &[]).unwrap();
        assert!(result.is_none());
    }
}
