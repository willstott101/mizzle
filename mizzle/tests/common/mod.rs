use std::path::{PathBuf, Path};
use std::fs;
use std::ffi::OsStr;
use std::process::{Command, Stdio};
use anyhow::{bail, anyhow, Result};

use tempfile::{tempdir, TempDir};

pub struct TempRepo {
	dir: TempDir,
}

impl TempRepo {
	pub fn path(&self) -> PathBuf {
		self.dir.path().join("temprepo.git")
	}
}


pub fn temprepo() -> Result<TempRepo> {
	let dir = tempdir()?;
	let repo = TempRepo {
		dir,
	};

	create_bare_repo_with_refs(&repo.path())?;

	Ok(repo)
}

const AUTHOR_NAME: &str = "Test Author";
const AUTHOR_EMAIL: &str = "author@example.com";
const COMMITTER_NAME: &str = "Test Committer";
const COMMITTER_EMAIL: &str = "committer@example.com";
const FIXED_TIME: &str = "1700000000 +0000";

/// Creates a bare repo at `bare_dir` that contains several refs (branches/tags/custom refs).
///
/// Strategy:
/// 1) Create a temporary working repo
/// 2) Create commits + refs in the working repo
/// 3) Initialize bare repo
/// 4) Push refs into the bare repo (including custom refs) via `git push --mirror` + explicit pushes
fn create_bare_repo_with_refs(bare_dir: &Path) -> Result<()> {
    // Ensure target doesn't already exist (or is empty).
    if bare_dir.exists() {
        bail!(
            "Target bare repo path already exists: {}",
            bare_dir.display()
        );
    }

    // Make a temp-ish workspace dir next to the bare repo path.
    // (You can replace this with the `tempfile` crate if you want true OS temp dirs.)
    let parent = bare_dir
        .parent()
        .ok_or(anyhow!("bare_dir must have a parent directory"))?;
    fs::create_dir_all(parent)?;

    let work_dir: PathBuf = parent.join(format!(
        ".work_{}",
        bare_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("repo")
    ));

    if work_dir.exists() {
        fs::remove_dir_all(&work_dir)?;
    }
    fs::create_dir_all(&work_dir)?;

    // 1) Init working repo
    run_git(&work_dir, ["init", "-b", "main"])?;

    // Set an identity so commits succeed even without global git config
    run_git(&work_dir, ["config", "user.name", "Example Bot"])?;
    run_git(&work_dir, ["config", "user.email", "bot@example.invalid"])?;

    // Disable features that can introduce nondeterminism
    run_git(&work_dir, ["config", "commit.gpgsign", "false"])?;
    run_git(&work_dir, ["config", "core.autocrlf", "false"])?;
    run_git(&work_dir, ["config", "core.filemode", "false"])?;

    // Create initial commit
    fs::write(work_dir.join("README.md"), "# Demo repo\n")?;
    run_git(&work_dir, ["add", "."])?;
    run_git(&work_dir, ["commit", "-m", "Initial commit"])?;

    // Second commit on main
    fs::write(work_dir.join("hello.txt"), "hello\n")?;
    run_git(&work_dir, ["add", "."])?;
    run_git(&work_dir, ["commit", "-m", "Add hello.txt"])?;

    // Create a dev branch with an extra commit
    run_git(&work_dir, ["checkout", "-b", "dev"])?;
    fs::write(work_dir.join("dev.txt"), "dev branch work\n")?;
    run_git(&work_dir, ["add", "."])?;
    run_git(&work_dir, ["commit", "-m", "Dev commit"])?;

    // Back to main
    run_git(&work_dir, ["checkout", "main"])?;

    // Create a tag on main (lightweight)
    run_git(&work_dir, ["tag", "v1.0.0"])?;

    // Create a custom ref pointing at the current HEAD (main)
    let head_oid = run_git(&work_dir, ["rev-parse", "HEAD"])?;
    run_git(
        &work_dir,
        ["update-ref", "refs/custom/demo", head_oid.as_str()],
    )?;

    // Create another custom ref pointing at dev tip
    let dev_oid = run_git(&work_dir, ["rev-parse", "dev"])?;
    run_git(
        &work_dir,
        ["update-ref", "refs/custom/dev-tip", dev_oid.as_str()],
    )?;

    // 2) Init bare repo
    fs::create_dir_all(bare_dir)?;
    run_git(bare_dir, ["init", "--bare"])?;

    // 3) Add bare as a remote and push everything
    run_git(&work_dir, ["remote", "add", "origin", bare_dir.to_str().unwrap()])?;

    // Push branches + tags + "normal" refs
    // --mirror pushes refs under refs/* (including custom ones) and deletes remote refs not present locally.
    run_git(&work_dir, ["push", "--mirror", "origin"])?;

    // Create a symbolic ref in the bare repo so HEAD points to main.
    // (Some tooling expects HEAD to reference the default branch.)
    run_git(bare_dir, ["symbolic-ref", "HEAD", "refs/heads/main"])?;

    // Cleanup working dir
    fs::remove_dir_all(&work_dir)?;

    Ok(())
}


pub fn run_git<I, S>(cwd: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        // Specified for determinism
        .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
        .env("GIT_AUTHOR_DATE", FIXED_TIME)
        .env("GIT_COMMITTER_NAME", COMMITTER_NAME)
        .env("GIT_COMMITTER_EMAIL", COMMITTER_EMAIL)
        .env("GIT_COMMITTER_DATE", FIXED_TIME)
        .env("TZ", "UTC")
        .stdin(Stdio::null())
        .output()?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git failed (status {}):\nSTDOUT:\n{}\nSTDERR:\n{}",
            output.status, stdout, stderr
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}