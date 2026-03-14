use std::collections::HashSet;
use std::ops::ControlFlow;

use anyhow::anyhow;
use gix::bstr::BStr;
use gix::traverse::tree::{visit::Action, Visit};
use gix::{objs::Find, ObjectId};

/// The result of [`objects_for_fetch`].
pub struct PackObjects {
    /// Objects to include in the pack: every commit, tree, and blob reachable
    /// from the `want` tips that is not already reachable from the `have` tips.
    pub objects: Vec<ObjectId>,
    /// Every object reachable from the `have` tips. These are safe delta bases
    /// for thin-pack creation, because the client is guaranteed to have them.
    pub have_set: HashSet<ObjectId>,
}

/// Collects all object IDs (commits, trees, blobs) needed to pack for a fetch:
/// every object reachable from any commit in `want` that is not also reachable
/// from any commit in `have`.
///
/// Unlike a commit-graph hide + `TreeContents` expansion, this performs full
/// object-level deduplication. Trees and blobs shared across the want/have
/// boundary are excluded, producing a minimal pack. The tree-skip optimisation
/// means that when a tree root is already known the entire subtree beneath it
/// is skipped without further descent.
///
/// The returned [`PackObjects::have_set`] can be used by the caller to
/// implement thin-pack creation, where delta bases are drawn from objects the
/// client already holds.
pub fn objects_for_fetch(
    odb: impl Find + Clone,
    want: &[ObjectId],
    have: &[ObjectId],
) -> anyhow::Result<PackObjects> {
    let have_set = build_have_set(odb.clone(), have)?;

    let want_commits: Vec<ObjectId> =
        gix::traverse::commit::Simple::new(want.iter().copied(), odb.clone())
            .hide(have.iter().copied())?
            .map(|r| r.map(|info| info.id))
            .collect::<Result<_, _>>()
            .map_err(|e| anyhow!("commit traversal: {e}"))?;

    let mut objects: HashSet<ObjectId> = HashSet::new();
    let mut state = gix::traverse::tree::breadthfirst::State::default();
    let mut commit_buf = Vec::new();
    let mut tree_buf = Vec::new();

    for commit_id in want_commits {
        objects.insert(commit_id);

        let tree_id = {
            let obj = odb
                .try_find(&commit_id, &mut commit_buf)
                .map_err(|e| anyhow!("find commit {commit_id}: {e}"))?
                .ok_or_else(|| anyhow!("commit {commit_id} not found"))?;
            gix::objs::CommitRefIter::from_bytes(obj.data)
                .tree_id()
                .map_err(|e| anyhow!("read tree id from commit: {e}"))?
        };

        if objects.contains(&tree_id) || have_set.contains(&tree_id) {
            continue;
        }
        objects.insert(tree_id);

        {
            let obj = odb
                .try_find(&tree_id, &mut tree_buf)
                .map_err(|e| anyhow!("find tree {tree_id}: {e}"))?
                .ok_or_else(|| anyhow!("tree {tree_id} not found"))?;
            let root = gix::objs::TreeRefIter::from_bytes(obj.data);
            let mut visitor = WantVisitor {
                have_set: &have_set,
                result: &mut objects,
            };
            gix::traverse::tree::breadthfirst(root, &mut state, odb.clone(), &mut visitor)
                .map_err(|e| anyhow!("tree walk: {e}"))?;
        }
    }

    Ok(PackObjects {
        objects: objects.into_iter().collect(),
        have_set,
    })
}

/// Builds a set of all object IDs reachable from the `have` commits. Used to
/// exclude already-known objects when building a pack for the want side.
fn build_have_set(
    odb: impl Find + Clone,
    have: &[ObjectId],
) -> anyhow::Result<HashSet<ObjectId>> {
    let mut have_set: HashSet<ObjectId> = HashSet::new();
    let mut state = gix::traverse::tree::breadthfirst::State::default();
    let mut commit_buf = Vec::new();
    let mut tree_buf = Vec::new();

    let have_commits: Vec<ObjectId> =
        gix::traverse::commit::Simple::new(have.iter().copied(), odb.clone())
            .map(|r| r.map(|info| info.id))
            .collect::<Result<_, _>>()
            .map_err(|e| anyhow!("have commit traversal: {e}"))?;

    for commit_id in have_commits {
        have_set.insert(commit_id);

        let tree_id = {
            let obj = odb
                .try_find(&commit_id, &mut commit_buf)
                .map_err(|e| anyhow!("find have commit {commit_id}: {e}"))?
                .ok_or_else(|| anyhow!("have commit {commit_id} not found"))?;
            gix::objs::CommitRefIter::from_bytes(obj.data)
                .tree_id()
                .map_err(|e| anyhow!("read tree id from have commit: {e}"))?
        };

        if have_set.contains(&tree_id) {
            continue;
        }
        have_set.insert(tree_id);

        {
            let obj = odb
                .try_find(&tree_id, &mut tree_buf)
                .map_err(|e| anyhow!("find have tree {tree_id}: {e}"))?
                .ok_or_else(|| anyhow!("have tree {tree_id} not found"))?;
            let root = gix::objs::TreeRefIter::from_bytes(obj.data);
            let mut visitor = HaveVisitor {
                have_set: &mut have_set,
            };
            gix::traverse::tree::breadthfirst(root, &mut state, odb.clone(), &mut visitor)
                .map_err(|e| anyhow!("have tree walk: {e}"))?;
        }
    }

    Ok(have_set)
}

/// Visitor for the have-side tree walk. Inserts every object it encounters
/// into `have_set`, using the tree-skip optimisation to avoid re-descending
/// into trees already recorded.
struct HaveVisitor<'a> {
    have_set: &'a mut HashSet<ObjectId>,
}

impl Visit for HaveVisitor<'_> {
    fn pop_back_tracked_path_and_set_current(&mut self) {}
    fn pop_front_tracked_path_and_set_current(&mut self) {}
    fn push_back_tracked_path_component(&mut self, _: &BStr) {}
    fn push_path_component(&mut self, _: &BStr) {}
    fn pop_path_component(&mut self) {}

    fn visit_tree(&mut self, entry: &gix::objs::tree::EntryRef<'_>) -> Action {
        if self.have_set.insert(entry.oid.to_owned()) {
            ControlFlow::Continue(true) // new tree, descend
        } else {
            ControlFlow::Continue(false) // already recorded, skip subtree
        }
    }

    fn visit_nontree(&mut self, entry: &gix::objs::tree::EntryRef<'_>) -> Action {
        self.have_set.insert(entry.oid.to_owned());
        ControlFlow::Continue(true)
    }
}

/// Visitor for the want-side tree walk. Adds only objects not already in
/// `have_set` to `result`, using the tree-skip optimisation to avoid
/// descending into subtrees the client already has.
struct WantVisitor<'a> {
    have_set: &'a HashSet<ObjectId>,
    result: &'a mut HashSet<ObjectId>,
}

impl Visit for WantVisitor<'_> {
    fn pop_back_tracked_path_and_set_current(&mut self) {}
    fn pop_front_tracked_path_and_set_current(&mut self) {}
    fn push_back_tracked_path_component(&mut self, _: &BStr) {}
    fn push_path_component(&mut self, _: &BStr) {}
    fn pop_path_component(&mut self) {}

    fn visit_tree(&mut self, entry: &gix::objs::tree::EntryRef<'_>) -> Action {
        let id = entry.oid.to_owned();
        if self.have_set.contains(&id) || !self.result.insert(id) {
            ControlFlow::Continue(false) // client has it or already queued, skip subtree
        } else {
            ControlFlow::Continue(true) // new tree, descend
        }
    }

    fn visit_nontree(&mut self, entry: &gix::objs::tree::EntryRef<'_>) -> Action {
        let id = entry.oid.to_owned();
        if !self.have_set.contains(&id) {
            self.result.insert(id);
        }
        ControlFlow::Continue(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, fs, path::Path, process::Command};
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

    fn open_odb(path: &Path) -> gix::OdbHandle {
        gix::open(path).unwrap().objects
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.name", "T"]);
        git(dir, &["config", "user.email", "t@t.com"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
    }

    /// Runs `git rev-list --objects <want> ^<have>` and returns the set of
    /// object IDs, used as ground truth for comparison.
    fn rev_list_objects(cwd: &Path, want: &[ObjectId], have: &[ObjectId]) -> HashSet<ObjectId> {
        let mut args: Vec<String> = vec!["rev-list".into(), "--objects".into()];
        for w in want {
            args.push(format!("{}", w));
        }
        for h in have {
            args.push(format!("^{}", h));
        }
        let out = Command::new("git")
            .current_dir(cwd)
            .args(&args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git rev-list failed");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let oid_str = line.split_whitespace().next()?;
                ObjectId::from_hex(oid_str.as_bytes()).ok()
            })
            .collect()
    }

    // ── clone: no haves ──────────────────────────────────────────────────────

    #[test]
    fn clone_returns_all_objects() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        init_repo(p);
        fs::write(p.join("README.md"), "# hi\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "init"]);

        let tip = rev_parse(p, "HEAD");
        let result: HashSet<_> = objects_for_fetch(open_odb(p), &[tip], &[])
            .unwrap()
            .objects
            .into_iter()
            .collect();
        assert_eq!(result, rev_list_objects(p, &[tip], &[]));
    }

    // ── incremental fetch ────────────────────────────────────────────────────

    // The client already has C1. C2 adds a new file on top. Only C2, C2's
    // root tree, and the new blob should be sent — not the unchanged blob.
    #[test]
    fn incremental_fetch_excludes_known_objects() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        init_repo(p);

        fs::write(p.join("README.md"), "# hi\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "C1"]);
        let c1 = rev_parse(p, "HEAD");
        let readme_blob = rev_parse(p, "HEAD:README.md");

        fs::write(p.join("hello.txt"), "hello\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "C2"]);
        let c2 = rev_parse(p, "HEAD");
        let hello_blob = rev_parse(p, "HEAD:hello.txt");

        let result: HashSet<_> = objects_for_fetch(open_odb(p), &[c2], &[c1])
            .unwrap()
            .objects
            .into_iter()
            .collect();
        assert_eq!(result, rev_list_objects(p, &[c2], &[c1]));

        assert!(result.contains(&c2));
        assert!(result.contains(&hello_blob));
        assert!(!result.contains(&c1), "have-side commit must be excluded");
        assert!(!result.contains(&readme_blob), "unchanged blob must be excluded");
    }

    // ── tree-skip optimisation ────────────────────────────────────────────────

    // C2 adds a top-level file; subdir/ is identical to C1. The subtree object
    // and everything beneath it should not appear in the result.
    #[test]
    fn unchanged_subtree_excluded_without_descent() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        init_repo(p);

        fs::create_dir(p.join("subdir")).unwrap();
        fs::write(p.join("subdir/deep.txt"), "deep\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "C1"]);
        let c1 = rev_parse(p, "HEAD");
        let subdir_tree = rev_parse(p, "HEAD:subdir");
        let deep_blob = rev_parse(p, "HEAD:subdir/deep.txt");

        fs::write(p.join("new.txt"), "new\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "C2"]);
        let c2 = rev_parse(p, "HEAD");

        let result: HashSet<_> = objects_for_fetch(open_odb(p), &[c2], &[c1])
            .unwrap()
            .objects
            .into_iter()
            .collect();
        assert_eq!(result, rev_list_objects(p, &[c2], &[c1]));

        assert!(!result.contains(&subdir_tree), "unchanged subtree must be excluded");
        assert!(!result.contains(&deep_blob), "blob under unchanged subtree must be excluded");
    }

    // ── want == have ─────────────────────────────────────────────────────────

    #[test]
    fn want_equals_have_is_empty() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        init_repo(p);
        fs::write(p.join("f.txt"), "x\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "C1"]);
        let c1 = rev_parse(p, "HEAD");

        let result = objects_for_fetch(open_odb(p), &[c1], &[c1]).unwrap();
        assert!(result.objects.is_empty());
    }

    // ── cross-branch shared blob ──────────────────────────────────────────────

    // Branch A: file_a.txt = "shared\n"
    // Branch B (diverged from root): file_b.txt = "shared\n" + new.txt = "new\n"
    // The blob for "shared\n" is identical in both branches. Because the client
    // already has it via branch A, it must not be resent for branch B.
    //
    // Note: `git rev-list --objects` uses lazy object flagging and may include
    // the shared blob in its output in this topology — our algorithm is more
    // conservative and correctly omits it, so we don't compare against rev-list
    // here.
    #[test]
    fn shared_blob_across_branches_excluded() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        init_repo(p);

        // Common root
        fs::write(p.join("base.txt"), "base\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "root"]);

        // Branch A
        git(p, &["checkout", "-b", "branch-a"]);
        fs::write(p.join("file_a.txt"), "shared\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "C1"]);
        let c1 = rev_parse(p, "HEAD");
        let shared_blob = rev_parse(p, "HEAD:file_a.txt");

        // Branch B from root
        git(p, &["checkout", "main"]);
        git(p, &["checkout", "-b", "branch-b"]);
        fs::write(p.join("file_b.txt"), "shared\n").unwrap(); // same content → same blob
        fs::write(p.join("new.txt"), "new\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "C2"]);
        let c2 = rev_parse(p, "HEAD");
        let new_blob = rev_parse(p, "HEAD:new.txt");

        let result: HashSet<_> = objects_for_fetch(open_odb(p), &[c2], &[c1])
            .unwrap()
            .objects
            .into_iter()
            .collect();

        assert!(result.contains(&new_blob));
        assert!(!result.contains(&shared_blob), "blob known via have branch must be excluded");
    }
}
