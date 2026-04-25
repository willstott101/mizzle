//! Deterministic bare-repository builder for performance benchmarks.
//!
//! Content is derived from the commit index so the resulting packs are
//! byte-for-byte reproducible across runs.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};

use super::git_runner::{run_git, AUTHOR_EMAIL, AUTHOR_NAME};

/// Builder for a deterministic bare git repository used by performance benches.
pub struct RepoBuilder {
    bare_path: PathBuf,
    linear: Option<usize>,
    with_bitmap: bool,
}

impl RepoBuilder {
    /// `bare_path` is the on-disk location of the bare repository that will
    /// be created.  Its parent must exist.
    pub fn new(bare_path: PathBuf) -> Self {
        Self {
            bare_path,
            linear: None,
            with_bitmap: false,
        }
    }

    /// Produce `n` linear commits on `main`, each modifying a single file
    /// with content derived from the commit index.
    pub fn linear_commits(mut self, n: usize) -> Self {
        self.linear = Some(n);
        self
    }

    /// After pushing to the bare repo, run `git repack -adb` to consolidate
    /// everything into a single pack and generate `.bitmap` + `.rev` side
    /// files.  Required for benchmarking the bitmap-accelerated have-set.
    pub fn with_bitmap(mut self) -> Self {
        self.with_bitmap = true;
        self
    }

    /// Build the repository and return the bare path.
    pub fn build(self) -> Result<PathBuf> {
        let parent = self
            .bare_path
            .parent()
            .ok_or_else(|| anyhow!("bare_path must have a parent"))?;
        fs::create_dir_all(parent)?;
        let work = parent.join(format!(
            ".work_{}",
            self.bare_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("repo")
        ));
        if work.exists() {
            fs::remove_dir_all(&work)?;
        }
        fs::create_dir_all(&work)?;

        run_git(&work, ["init", "-b", "main"])?;
        run_git(&work, ["config", "user.name", AUTHOR_NAME])?;
        run_git(&work, ["config", "user.email", AUTHOR_EMAIL])?;
        run_git(&work, ["config", "commit.gpgsign", "false"])?;
        run_git(&work, ["config", "core.autocrlf", "false"])?;
        run_git(&work, ["config", "core.filemode", "false"])?;

        if let Some(n) = self.linear {
            if n == 0 {
                bail!("linear_commits(0) produces no commits; need at least one");
            }
            for i in 0..n {
                // One file, one line per commit — keeps tree churn small
                // so the bench exercises deep *commit* history rather than
                // huge per-commit trees.
                fs::write(work.join("history.txt"), format!("commit {i}\n"))?;
                run_git(&work, ["add", "history.txt"])?;
                run_git(&work, ["commit", "-m", &format!("c{i}")])?;
            }
        }

        fs::create_dir_all(&self.bare_path)?;
        run_git(&self.bare_path, ["init", "--bare"])?;
        run_git(
            &work,
            ["remote", "add", "origin", self.bare_path.to_str().unwrap()],
        )?;
        run_git(&work, ["push", "--mirror", "origin"])?;
        run_git(&self.bare_path, ["symbolic-ref", "HEAD", "refs/heads/main"])?;
        fs::remove_dir_all(&work)?;

        if self.with_bitmap {
            run_git(&self.bare_path, ["repack", "-adb"])?;
        }

        Ok(self.bare_path)
    }
}
