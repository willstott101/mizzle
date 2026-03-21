//! Filesystem backend that shells out to the `git` CLI.
//!
//! This backend serves as a correctness oracle: the same integration tests can
//! be run against both [`FsGitoxide`](super::fs_gitoxide::FsGitoxide) and
//! `FsGitCli` to verify identical behaviour.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};

use anyhow::{Context, Result};
use gix::ObjectId;

use crate::backend::{
    HeadInfo, PackMetadata, PackOptions, PackOutput, RefInfo, RefUpdate, RefsSnapshot,
    StorageBackend,
};
use crate::traits::PushKind;

/// Filesystem backend using the `git` CLI.
///
/// Every method shells out to a `git` subprocess.  Only used after auth has
/// passed (see `CLAUDE.md` architecture rules).
#[derive(Clone, Copy)]
pub struct FsGitCli;

/// Pack and index files written by [`FsGitCli::ingest_pack`].
pub struct CliWrittenPack {
    pack: PathBuf,
    index: PathBuf,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a `Command` for `git` targeting `repo_path`, with interactive
/// prompts and pager disabled.
fn git_cmd(repo_path: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Run a `git` command and return stdout as a trimmed string.
/// Returns an error on non-zero exit.
fn run_git(repo_path: &Path, args: &[&str]) -> Result<String> {
    let output = git_cmd(repo_path)
        .args(args)
        .output()
        .with_context(|| format!("spawning git {:?} in {}", args, repo_path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {:?} failed (status {}):\n{}",
            args,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// ProgressChildReader — wraps a child process stdout as `impl Read`
// ---------------------------------------------------------------------------

/// Wraps a child process stdout as `impl Read`, with stderr consumed by a
/// separate progress thread.  The thread accumulates raw stderr bytes in a
/// shared buffer so that error messages are available on non-zero exit.
///
/// The `Drop` impl kills the child if it is still running.
struct ProgressChildReader {
    stdout: ChildStdout,
    child: Option<Child>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
}

impl Read for ProgressChildReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.stdout.read(buf)?;
        if n == 0 {
            if let Some(mut child) = self.child.take() {
                let status = child.wait()?;
                if !status.success() {
                    let stderr = self.stderr_buf.lock().unwrap();
                    let msg = String::from_utf8_lossy(&stderr);
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("git process exited with {status}: {msg}"),
                    ));
                }
            }
        }
        Ok(n)
    }
}

impl Drop for ProgressChildReader {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Spawn a thread that reads `stderr` in chunks, sends each chunk as a
/// progress message through `tx`, and accumulates raw bytes in the returned
/// shared buffer.
fn spawn_stderr_progress(
    mut stderr: std::process::ChildStderr,
    tx: mpsc::Sender<String>,
) -> Arc<Mutex<Vec<u8>>> {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let buf2 = buf.clone();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 512];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let data = &chunk[..n];
                    buf2.lock().unwrap().extend_from_slice(data);
                    let _ = tx.send(String::from_utf8_lossy(data).into_owned());
                }
            }
        }
    });
    buf
}

// ---------------------------------------------------------------------------
// StorageBackend implementation
// ---------------------------------------------------------------------------

impl StorageBackend for FsGitCli {
    type RepoId = PathBuf;
    type IngestedPack = CliWrittenPack;

    fn init_repo(&self, repo_path: &PathBuf) -> Result<()> {
        if !repo_path.exists() {
            let output = Command::new("git")
                .args(["init", "--bare"])
                .arg(repo_path)
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .context("spawning git init")?;
            if !output.status.success() {
                anyhow::bail!(
                    "git init --bare failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        Ok(())
    }

    fn list_refs(&self, repo_path: &PathBuf) -> Result<RefsSnapshot> {
        // HEAD
        let head = match run_git(repo_path, &["symbolic-ref", "HEAD"]) {
            Ok(symref_target) => {
                // Resolve HEAD to an OID
                match run_git(repo_path, &["rev-parse", "HEAD"]) {
                    Ok(hex) => {
                        let oid = ObjectId::from_hex(hex.as_bytes()).context("parsing HEAD oid")?;
                        Some(HeadInfo {
                            oid,
                            symref_target: Some(symref_target),
                        })
                    }
                    Err(_) => None, // detached HEAD pointing at nothing
                }
            }
            Err(_) => {
                // Detached HEAD or unborn
                match run_git(repo_path, &["rev-parse", "HEAD"]) {
                    Ok(hex) => {
                        let oid = ObjectId::from_hex(hex.as_bytes()).context("parsing HEAD oid")?;
                        Some(HeadInfo {
                            oid,
                            symref_target: None,
                        })
                    }
                    Err(_) => None,
                }
            }
        };

        // All refs
        let format = "%(objectname) %(refname) %(symref) %(*objectname)";
        let output = run_git(repo_path, &["for-each-ref", &format!("--format={format}")])?;

        let mut refs = Vec::new();
        for line in output.lines() {
            if line.is_empty() {
                continue;
            }
            // Format: "<oid> <refname> <symref> <peeled>"
            // symref and peeled may be empty strings
            let parts: Vec<&str> = line.splitn(4, ' ').collect();
            if parts.len() < 2 {
                continue;
            }

            let oid_hex = parts[0];
            let name = parts[1].to_string();
            let symref = parts.get(2).copied().unwrap_or("");
            let peeled_hex = parts.get(3).copied().unwrap_or("");

            let oid = ObjectId::from_hex(oid_hex.as_bytes()).context("parsing ref oid")?;

            let symref_target = if symref.is_empty() {
                None
            } else {
                Some(symref.to_string())
            };

            let peeled = if peeled_hex.is_empty() || peeled_hex == &"0".repeat(40) {
                None
            } else {
                let peeled_oid =
                    ObjectId::from_hex(peeled_hex.as_bytes()).context("parsing peeled oid")?;
                if peeled_oid == oid {
                    None
                } else {
                    Some(peeled_oid)
                }
            };

            refs.push(RefInfo {
                name,
                oid,
                peeled,
                symref_target,
            });
        }

        Ok(RefsSnapshot { head, refs })
    }

    fn resolve_ref(&self, repo_path: &PathBuf, refname: &str) -> Result<Option<ObjectId>> {
        let arg = format!("{refname}^{{}}");
        match run_git(repo_path, &["rev-parse", "--verify", &arg]) {
            Ok(hex) => {
                let oid = ObjectId::from_hex(hex.as_bytes()).context("parsing resolved oid")?;
                Ok(Some(oid))
            }
            Err(_) => Ok(None), // exit 128 → ref doesn't exist
        }
    }

    fn has_object(&self, repo_path: &PathBuf, oid: &ObjectId) -> Result<bool> {
        let status = git_cmd(repo_path)
            .args(["cat-file", "-e", &oid.to_hex().to_string()])
            .status()
            .context("spawning git cat-file -e")?;
        Ok(status.success())
    }

    fn has_objects(&self, repo_path: &PathBuf, oids: &[ObjectId]) -> Result<Vec<bool>> {
        if oids.is_empty() {
            return Ok(Vec::new());
        }

        let mut child = git_cmd(repo_path)
            .args(["cat-file", "--batch-check"])
            .stdin(Stdio::piped())
            .spawn()
            .context("spawning git cat-file --batch-check")?;

        let mut stdin = child.stdin.take().unwrap();
        for oid in oids {
            writeln!(stdin, "{}", oid.to_hex())?;
        }
        drop(stdin);

        let output = child.wait_with_output().context("waiting for cat-file")?;
        let stdout = String::from_utf8_lossy(&output.stdout);

        let mut results = Vec::with_capacity(oids.len());
        for line in stdout.lines() {
            results.push(!line.contains("missing"));
        }

        // Pad with false if we got fewer lines than expected
        while results.len() < oids.len() {
            results.push(false);
        }

        Ok(results)
    }

    fn compute_push_kind(&self, repo_path: &PathBuf, update: &RefUpdate) -> PushKind {
        if update.old_oid.is_null() {
            return PushKind::Create;
        }
        if update.new_oid.is_null() {
            return PushKind::Delete;
        }

        let status = git_cmd(repo_path)
            .args([
                "merge-base",
                "--is-ancestor",
                &update.old_oid.to_hex().to_string(),
                &update.new_oid.to_hex().to_string(),
            ])
            .status();

        match status {
            Ok(s) if s.success() => PushKind::FastForward,
            _ => PushKind::ForcePush,
        }
    }

    fn update_refs(&self, repo_path: &PathBuf, updates: &[RefUpdate]) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        let mut child = git_cmd(repo_path)
            .args(["update-ref", "--stdin"])
            .stdin(Stdio::piped())
            .spawn()
            .context("spawning git update-ref --stdin")?;

        let mut stdin = child.stdin.take().unwrap();
        for update in updates {
            if update.new_oid.is_null() {
                writeln!(
                    stdin,
                    "delete {} {}",
                    update.refname,
                    update.old_oid.to_hex()
                )?;
            } else if update.old_oid.is_null() {
                writeln!(
                    stdin,
                    "create {} {}",
                    update.refname,
                    update.new_oid.to_hex()
                )?;
            } else {
                writeln!(
                    stdin,
                    "update {} {} {}",
                    update.refname,
                    update.new_oid.to_hex(),
                    update.old_oid.to_hex()
                )?;
            }
        }
        drop(stdin);

        let output = child.wait_with_output().context("waiting for update-ref")?;
        if !output.status.success() {
            anyhow::bail!(
                "git update-ref failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    fn build_pack(
        &self,
        repo_path: &PathBuf,
        want: &[ObjectId],
        have: &[ObjectId],
        opts: &PackOptions,
    ) -> Result<PackOutput> {
        if let Some(depth) = opts.deepen {
            return self.build_pack_shallow(repo_path, want, have, depth, opts);
        }

        let mut args = vec![
            "pack-objects".to_string(),
            "--revs".to_string(),
            "--stdout".to_string(),
            "--progress".to_string(),
        ];
        if opts.thin_pack {
            args.push("--thin".to_string());
        }
        if let Some(ref filter) = opts.filter {
            use crate::backend::Filter;
            let spec = match filter {
                Filter::BlobNone => "blob:none",
                Filter::TreeNone => "tree:0",
            };
            args.push(format!("--filter={spec}"));
        }

        let mut child = git_cmd(repo_path)
            .args(&args)
            .stdin(Stdio::piped())
            .spawn()
            .context("spawning git pack-objects")?;

        let mut stdin = child.stdin.take().unwrap();
        for oid in want {
            writeln!(stdin, "{}", oid.to_hex())?;
        }
        for oid in have {
            writeln!(stdin, "^{}", oid.to_hex())?;
        }
        drop(stdin);

        let (progress_tx, progress_rx) = mpsc::channel();
        let stderr = child.stderr.take().unwrap();
        let stderr_buf = spawn_stderr_progress(stderr, progress_tx);

        let stdout = child.stdout.take().unwrap();
        let reader = ProgressChildReader {
            stdout,
            child: Some(child),
            stderr_buf,
        };

        Ok(PackOutput {
            reader: Box::new(reader),
            shallow: Vec::new(),
            progress: Some(progress_rx),
        })
    }

    fn ingest_pack(
        &self,
        repo_path: &PathBuf,
        staged_pack: &Path,
    ) -> Result<Option<CliWrittenPack>> {
        // Check the object count in the pack header.
        let mut file = std::fs::File::open(staged_pack).context("opening staged pack")?;
        let mut header = [0u8; 12];
        let n = file.read(&mut header).context("reading pack header")?;
        if mizzle_proto::receive::pack_object_count(&header[..n]).unwrap_or(0) == 0 {
            return Ok(None);
        }
        drop(file);

        let pack_dir = repo_path.join("objects").join("pack");
        std::fs::create_dir_all(&pack_dir)?;

        // git index-pack writes .idx alongside the .pack when given --keep.
        // We copy the staged pack into the pack dir first, then index it.
        let output = Command::new("git")
            .args(["index-pack", "--stdin", &format!("--index-version=2")])
            .arg("--fix-thin")
            .current_dir(repo_path)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::fs::File::open(staged_pack)?)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context("running git index-pack")?;

        if !output.status.success() {
            anyhow::bail!(
                "git index-pack failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // git index-pack --stdin writes pack-<hash>.{pack,idx} into
        // .git/objects/pack and prints "pack\t<hash>\n" to stdout.
        let raw = String::from_utf8_lossy(&output.stdout);
        let hash = raw
            .trim()
            .split('\t')
            .last()
            .context("no hash in index-pack output")?;

        let pack_file = pack_dir.join(format!("pack-{hash}.pack"));
        let idx_file = pack_dir.join(format!("pack-{hash}.idx"));

        Ok(Some(CliWrittenPack {
            pack: pack_file,
            index: idx_file,
        }))
    }

    fn inspect_ingested(&self, pack: &CliWrittenPack) -> Result<PackMetadata> {
        crate::inspect::inspect_pack(&pack.pack)
    }

    fn rollback_ingest(&self, pack: CliWrittenPack) {
        let _ = std::fs::remove_file(&pack.index);
        let _ = std::fs::remove_file(&pack.pack);
    }
}

// ---------------------------------------------------------------------------
// Shallow pack building
// ---------------------------------------------------------------------------

impl FsGitCli {
    /// Build a pack for a shallow fetch.
    ///
    /// Two subprocesses total:
    /// 1. `git rev-list --boundary --parents --max-count=<depth>` to identify
    ///    the shallow boundary (no per-commit subprocess needed).
    /// 2. `git pack-objects --revs --stdout` with `--shallow` lines on stdin
    ///    so git limits the walk and builds the correct pack.
    fn build_pack_shallow(
        &self,
        repo_path: &PathBuf,
        want: &[ObjectId],
        have: &[ObjectId],
        depth: u32,
        opts: &PackOptions,
    ) -> Result<PackOutput> {
        // Step 1: find the shallow boundary.
        // --parents makes each commit line: "<oid> <parent1> <parent2> ..."
        // --boundary adds excluded parents prefixed with '-'.
        let mut rev_args: Vec<String> = vec![
            "rev-list".into(),
            "--boundary".into(),
            "--parents".into(),
            format!("--max-count={depth}"),
        ];
        for oid in want {
            rev_args.push(oid.to_hex().to_string());
        }
        for oid in have {
            rev_args.push(format!("^{}", oid.to_hex()));
        }

        let output = git_cmd(repo_path)
            .args(&rev_args)
            .output()
            .context("spawning git rev-list for shallow")?;
        if !output.status.success() {
            anyhow::bail!(
                "git rev-list failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let rev_stdout = String::from_utf8_lossy(&output.stdout);

        // Collect boundary OIDs (excluded parents, prefixed with '-').
        let boundary_set: std::collections::HashSet<ObjectId> = rev_stdout
            .lines()
            .filter_map(|l| l.strip_prefix('-'))
            .filter_map(|l| {
                let hex = l.split_whitespace().next().unwrap_or(l);
                ObjectId::from_hex(hex.as_bytes()).ok()
            })
            .collect();

        // A commit is a shallow root if any of its parents is in the boundary.
        let mut shallow = Vec::new();
        for line in rev_stdout.lines() {
            if line.starts_with('-') || line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let commit_hex = parts.next().unwrap_or("");
            let commit = match ObjectId::from_hex(commit_hex.as_bytes()) {
                Ok(oid) => oid,
                Err(_) => continue,
            };
            if parts.any(|p| {
                ObjectId::from_hex(p.as_bytes())
                    .map(|oid| boundary_set.contains(&oid))
                    .unwrap_or(false)
            }) {
                shallow.push(commit);
            }
        }

        // Step 2: `git pack-objects --revs --stdout` with `--shallow` lines.
        // git's own pack-objects understands `--shallow <oid>` on stdin and
        // limits the traversal accordingly, producing the correct pack.
        let mut pack_args = vec![
            "pack-objects".to_string(),
            "--revs".to_string(),
            "--stdout".to_string(),
            "--progress".to_string(),
        ];
        if opts.thin_pack {
            pack_args.push("--thin".to_string());
        }
        if let Some(ref filter) = opts.filter {
            use crate::backend::Filter;
            let spec = match filter {
                Filter::BlobNone => "blob:none",
                Filter::TreeNone => "tree:0",
            };
            pack_args.push(format!("--filter={spec}"));
        }

        let mut child = git_cmd(repo_path)
            .args(&pack_args)
            .stdin(Stdio::piped())
            .spawn()
            .context("spawning git pack-objects for shallow")?;

        let mut stdin = child.stdin.take().unwrap();
        for oid in want {
            writeln!(stdin, "{}", oid.to_hex())?;
        }
        for oid in have {
            writeln!(stdin, "^{}", oid.to_hex())?;
        }
        for oid in &shallow {
            writeln!(stdin, "--shallow {}", oid.to_hex())?;
        }
        drop(stdin);

        let (progress_tx, progress_rx) = mpsc::channel();
        let stderr = child.stderr.take().unwrap();
        let stderr_buf = spawn_stderr_progress(stderr, progress_tx);

        let stdout = child.stdout.take().unwrap();
        let reader = ProgressChildReader {
            stdout,
            child: Some(child),
            stderr_buf,
        };

        Ok(PackOutput {
            reader: Box::new(reader),
            shallow,
            progress: Some(progress_rx),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// Create a bare repo with known commits and refs for testing.
    ///
    /// Layout mirrors `FsGitoxide::tests::test_bare_repo`.
    fn test_bare_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("test.git");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();

        let git = |cwd: &Path, args: &[&str]| {
            let out = Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "t@t.com")
                .env("GIT_AUTHOR_DATE", "1700000000 +0000")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "t@t.com")
                .env("GIT_COMMITTER_DATE", "1700000000 +0000")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        git(&work, &["init", "-b", "main"]);
        git(&work, &["config", "user.email", "t@t.com"]);
        git(&work, &["config", "user.name", "T"]);
        std::fs::write(work.join("README.md"), "# Demo\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "Initial"]);
        std::fs::write(work.join("hello.txt"), "hello\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "Add hello.txt"]);

        git(&work, &["checkout", "-b", "dev"]);
        std::fs::write(work.join("dev.txt"), "dev\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "Dev commit"]);
        git(&work, &["checkout", "main"]);
        git(&work, &["tag", "v1.0.0"]);

        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--bare"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&work, &["push", "--mirror", "origin"]);
        git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        (dir, bare)
    }

    #[test]
    fn test_init_repo_creates_bare_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("test.git");
        assert!(!repo_path.exists());
        FsGitCli.init_repo(&repo_path).unwrap();
        assert!(repo_path.exists());
        // Calling again is a no-op
        FsGitCli.init_repo(&repo_path).unwrap();
    }

    #[test]
    fn list_refs_returns_head_and_branches() {
        let (_dir, bare) = test_bare_repo();
        let snap = FsGitCli.list_refs(&bare).unwrap();

        let head = snap.head.as_ref().expect("HEAD should exist");
        assert_eq!(
            head.symref_target.as_deref(),
            Some("refs/heads/main"),
            "HEAD should be a symref to main"
        );

        let ref_names: Vec<&str> = snap.refs.iter().map(|r| r.name.as_str()).collect();
        assert!(ref_names.contains(&"refs/heads/main"), "missing main");
        assert!(ref_names.contains(&"refs/heads/dev"), "missing dev");
        assert!(ref_names.contains(&"refs/tags/v1.0.0"), "missing tag");

        let main_ref = snap
            .refs
            .iter()
            .find(|r| r.name == "refs/heads/main")
            .unwrap();
        assert_eq!(head.oid, main_ref.oid, "HEAD oid should match main");

        let tag = snap
            .refs
            .iter()
            .find(|r| r.name == "refs/tags/v1.0.0")
            .unwrap();
        assert!(
            tag.peeled.is_none(),
            "lightweight tag should not have peeled oid"
        );
    }

    #[test]
    fn refs_snapshot_as_upload_pack_v1() {
        let (_dir, bare) = test_bare_repo();
        let snap = FsGitCli.list_refs(&bare).unwrap();
        let v1 = snap.as_upload_pack_v1();

        assert!(!v1.is_empty());
        assert_eq!(v1[0].1, "HEAD");
        for (_, name) in &v1[1..] {
            assert!(
                name.starts_with("refs/"),
                "expected refs/ prefix, got {name}"
            );
        }
    }

    #[test]
    fn refs_snapshot_as_receive_pack() {
        let (_dir, bare) = test_bare_repo();
        let snap = FsGitCli.list_refs(&bare).unwrap();
        let rp = snap.as_receive_pack();

        for (_, name) in &rp {
            assert_ne!(name, "HEAD", "receive-pack should not include HEAD");
            assert!(name.starts_with("refs/"));
        }
    }

    #[test]
    fn resolve_ref_existing_and_nonexistent() {
        let (_dir, bare) = test_bare_repo();

        let main_oid = FsGitCli.resolve_ref(&bare, "refs/heads/main").unwrap();
        assert!(main_oid.is_some(), "main should resolve");

        let dev_oid = FsGitCli.resolve_ref(&bare, "refs/heads/dev").unwrap();
        assert!(dev_oid.is_some(), "dev should resolve");
        assert_ne!(main_oid, dev_oid, "main and dev should differ");

        let none = FsGitCli
            .resolve_ref(&bare, "refs/heads/nonexistent")
            .unwrap();
        assert!(none.is_none(), "nonexistent ref should return None");
    }

    #[test]
    fn has_object_and_has_objects() {
        let (_dir, bare) = test_bare_repo();
        let main_oid = FsGitCli
            .resolve_ref(&bare, "refs/heads/main")
            .unwrap()
            .unwrap();

        assert!(FsGitCli.has_object(&bare, &main_oid).unwrap());

        let fake_oid = ObjectId::from_hex(b"0000000000000000000000000000000000000001").unwrap();
        assert!(!FsGitCli.has_object(&bare, &fake_oid).unwrap());

        let results = FsGitCli.has_objects(&bare, &[main_oid, fake_oid]).unwrap();
        assert_eq!(results, vec![true, false]);
    }

    #[test]
    fn update_refs_creates_new_ref() {
        let (_dir, bare) = test_bare_repo();
        let main_oid = FsGitCli
            .resolve_ref(&bare, "refs/heads/main")
            .unwrap()
            .unwrap();

        FsGitCli
            .update_refs(
                &bare,
                &[RefUpdate {
                    old_oid: ObjectId::null(gix_hash::Kind::Sha1),
                    new_oid: main_oid,
                    refname: "refs/heads/new-branch".to_string(),
                }],
            )
            .unwrap();

        let resolved = FsGitCli
            .resolve_ref(&bare, "refs/heads/new-branch")
            .unwrap();
        assert_eq!(resolved, Some(main_oid));
    }

    #[test]
    fn compute_push_kind_create() {
        let (_dir, bare) = test_bare_repo();
        let main_oid = FsGitCli
            .resolve_ref(&bare, "refs/heads/main")
            .unwrap()
            .unwrap();

        let kind = FsGitCli.compute_push_kind(
            &bare,
            &RefUpdate {
                old_oid: ObjectId::null(gix_hash::Kind::Sha1),
                new_oid: main_oid,
                refname: "refs/heads/new".to_string(),
            },
        );
        assert_eq!(kind, PushKind::Create);
    }

    #[test]
    fn compute_push_kind_delete() {
        let (_dir, bare) = test_bare_repo();
        let main_oid = FsGitCli
            .resolve_ref(&bare, "refs/heads/main")
            .unwrap()
            .unwrap();

        let kind = FsGitCli.compute_push_kind(
            &bare,
            &RefUpdate {
                old_oid: main_oid,
                new_oid: ObjectId::null(gix_hash::Kind::Sha1),
                refname: "refs/heads/main".to_string(),
            },
        );
        assert_eq!(kind, PushKind::Delete);
    }

    #[test]
    fn compute_push_kind_fast_forward() {
        let (_dir, bare) = test_bare_repo();
        let main_oid = FsGitCli
            .resolve_ref(&bare, "refs/heads/main")
            .unwrap()
            .unwrap();
        let dev_oid = FsGitCli
            .resolve_ref(&bare, "refs/heads/dev")
            .unwrap()
            .unwrap();

        let kind = FsGitCli.compute_push_kind(
            &bare,
            &RefUpdate {
                old_oid: main_oid,
                new_oid: dev_oid,
                refname: "refs/heads/main".to_string(),
            },
        );
        assert_eq!(kind, PushKind::FastForward);
    }

    #[test]
    fn compute_push_kind_force_push() {
        let (_dir, bare) = test_bare_repo();
        let main_oid = FsGitCli
            .resolve_ref(&bare, "refs/heads/main")
            .unwrap()
            .unwrap();
        let dev_oid = FsGitCli
            .resolve_ref(&bare, "refs/heads/dev")
            .unwrap()
            .unwrap();

        let kind = FsGitCli.compute_push_kind(
            &bare,
            &RefUpdate {
                old_oid: dev_oid,
                new_oid: main_oid,
                refname: "refs/heads/main".to_string(),
            },
        );
        assert_eq!(kind, PushKind::ForcePush);
    }

    #[test]
    fn build_pack_returns_valid_pack_data() {
        let (_dir, bare) = test_bare_repo();
        let main_oid = FsGitCli
            .resolve_ref(&bare, "refs/heads/main")
            .unwrap()
            .unwrap();

        let mut output = FsGitCli
            .build_pack(
                &bare,
                &[main_oid],
                &[],
                &PackOptions {
                    deepen: None,
                    filter: None,
                    thin_pack: false,
                },
            )
            .unwrap();

        let mut data = Vec::new();
        io::Read::read_to_end(&mut output.reader, &mut data).unwrap();

        assert!(data.len() >= 12, "pack too short: {} bytes", data.len());
        assert_eq!(&data[0..4], b"PACK", "pack should start with PACK magic");
    }

    #[test]
    fn build_pack_with_have_produces_smaller_pack() {
        let (_dir, bare) = test_bare_repo();
        let main_oid = FsGitCli
            .resolve_ref(&bare, "refs/heads/main")
            .unwrap()
            .unwrap();
        let dev_oid = FsGitCli
            .resolve_ref(&bare, "refs/heads/dev")
            .unwrap()
            .unwrap();

        let mut full = FsGitCli
            .build_pack(
                &bare,
                &[dev_oid],
                &[],
                &PackOptions {
                    deepen: None,
                    filter: None,
                    thin_pack: false,
                },
            )
            .unwrap();
        let mut full_data = Vec::new();
        io::Read::read_to_end(&mut full.reader, &mut full_data).unwrap();

        let mut incr = FsGitCli
            .build_pack(
                &bare,
                &[dev_oid],
                &[main_oid],
                &PackOptions {
                    deepen: None,
                    filter: None,
                    thin_pack: false,
                },
            )
            .unwrap();
        let mut incr_data = Vec::new();
        io::Read::read_to_end(&mut incr.reader, &mut incr_data).unwrap();

        assert!(
            incr_data.len() < full_data.len(),
            "incremental pack ({} bytes) should be smaller than full pack ({} bytes)",
            incr_data.len(),
            full_data.len()
        );
    }

    #[test]
    fn ingest_pack_and_rollback() {
        let (_dir, bare) = test_bare_repo();
        let main_oid = FsGitCli
            .resolve_ref(&bare, "refs/heads/main")
            .unwrap()
            .unwrap();

        // Build a pack from the existing repo
        let mut output = FsGitCli
            .build_pack(
                &bare,
                &[main_oid],
                &[],
                &PackOptions {
                    deepen: None,
                    filter: None,
                    thin_pack: false,
                },
            )
            .unwrap();
        let mut pack_data = Vec::new();
        io::Read::read_to_end(&mut output.reader, &mut pack_data).unwrap();

        // Create a fresh bare repo to ingest into
        let target_dir = tempfile::tempdir().unwrap();
        let target = target_dir.path().join("target.git");
        FsGitCli.init_repo(&target).unwrap();

        // Stage the pack to a temp file
        let staged = target_dir.path().join("staged.pack");
        std::fs::write(&staged, &pack_data).unwrap();

        let written = FsGitCli.ingest_pack(&target, &staged).unwrap();
        assert!(written.is_some(), "non-empty pack should return Some");
        let written = written.unwrap();

        assert!(written.pack.exists(), "pack file should exist");
        assert!(written.index.exists(), "index file should exist");

        // The objects should now be accessible
        assert!(FsGitCli.has_object(&target, &main_oid).unwrap());

        // Rollback should remove the files
        FsGitCli.rollback_ingest(written);
    }

    #[test]
    fn ingest_empty_pack_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("test.git");
        FsGitCli.init_repo(&bare).unwrap();

        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&0u32.to_be_bytes());

        let staged = dir.path().join("empty.pack");
        std::fs::write(&staged, &pack).unwrap();

        let result = FsGitCli.ingest_pack(&bare, &staged).unwrap();
        assert!(result.is_none(), "empty pack should return None");
    }
}
