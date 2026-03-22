use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

use anyhow::{anyhow, bail, Result};
use axum::extract::{Path as AxumPath, Request, State};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use mizzle::backend::fs_git_cli::FsGitCli;
use mizzle::backend::fs_gitoxide::FsGitoxide;
use mizzle::backend::StorageBackend;
use mizzle::traits::RepoAccess;
use tempfile::{tempdir, TempDir};

// ── Minimal test infrastructure (mirrors tests/common) ──────────────────────

const AUTHOR_NAME: &str = "Test Author";
const AUTHOR_EMAIL: &str = "author@example.com";
const COMMITTER_NAME: &str = "Test Committer";
const COMMITTER_EMAIL: &str = "committer@example.com";
const FIXED_TIME: &str = "1700000000 +0000";

fn run_git<I, S>(cwd: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
        .env("GIT_AUTHOR_DATE", FIXED_TIME)
        .env("GIT_COMMITTER_NAME", COMMITTER_NAME)
        .env("GIT_COMMITTER_EMAIL", COMMITTER_EMAIL)
        .env("GIT_COMMITTER_DATE", FIXED_TIME)
        .env("TZ", "UTC")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git failed (status {}):\nSTDOUT:\n{}\nSTDERR:\n{}",
            output.status,
            stdout,
            stderr
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

struct TempRepo {
    dir: TempDir,
}

impl TempRepo {
    fn path(&self) -> PathBuf {
        self.dir.path().join("temprepo.git")
    }
}

fn temprepo() -> Result<TempRepo> {
    let dir = tempdir()?;
    let repo = TempRepo { dir };
    create_bare_repo(&repo.path())?;
    Ok(repo)
}

fn create_bare_repo(bare_dir: &Path) -> Result<()> {
    let parent = bare_dir
        .parent()
        .ok_or(anyhow!("bare_dir must have a parent directory"))?;
    let work_dir = parent.join(".work_bench");
    if work_dir.exists() {
        fs::remove_dir_all(&work_dir)?;
    }
    fs::create_dir_all(&work_dir)?;

    run_git(&work_dir, ["init", "-b", "main"])?;
    run_git(&work_dir, ["config", "user.name", "Bench"])?;
    run_git(&work_dir, ["config", "user.email", "b@b.com"])?;
    run_git(&work_dir, ["config", "commit.gpgsign", "false"])?;

    fs::write(work_dir.join("README.md"), "# Bench repo\n")?;
    run_git(&work_dir, ["add", "."])?;
    run_git(&work_dir, ["commit", "-m", "Initial commit"])?;

    fs::write(work_dir.join("hello.txt"), "hello\n")?;
    run_git(&work_dir, ["add", "."])?;
    run_git(&work_dir, ["commit", "-m", "Add hello.txt"])?;

    run_git(&work_dir, ["checkout", "-b", "dev"])?;
    fs::write(work_dir.join("dev.txt"), "dev branch work\n")?;
    run_git(&work_dir, ["add", "."])?;
    run_git(&work_dir, ["commit", "-m", "Dev commit"])?;
    run_git(&work_dir, ["checkout", "main"])?;
    run_git(&work_dir, ["tag", "v1.0.0"])?;

    fs::create_dir_all(bare_dir)?;
    run_git(bare_dir, ["init", "--bare"])?;
    run_git(
        &work_dir,
        ["remote", "add", "origin", bare_dir.to_str().unwrap()],
    )?;
    run_git(&work_dir, ["push", "--mirror", "origin"])?;
    run_git(bare_dir, ["symbolic-ref", "HEAD", "refs/heads/main"])?;
    fs::remove_dir_all(&work_dir)?;

    Ok(())
}

#[derive(Clone)]
struct BenchConfig {
    bare_repo_path: PathBuf,
}

impl RepoAccess for BenchConfig {
    type RepoId = PathBuf;
    fn repo_id(&self) -> &PathBuf {
        &self.bare_repo_path
    }
}

struct ServerHandle {
    port: u16,
    stop: Option<Box<dyn FnOnce()>>,
}

impl ServerHandle {
    fn stop(&mut self) {
        if let Some(f) = self.stop.take() {
            f();
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn start_server<B>(config: BenchConfig, backend: B) -> ServerHandle
where
    B: StorageBackend<RepoId = PathBuf> + Clone + Send + Sync + 'static,
{
    async fn handler<B>(
        State(state): State<Arc<(BenchConfig, B)>>,
        AxumPath(path): AxumPath<String>,
        req: Request,
    ) -> Response
    where
        B: StorageBackend<RepoId = PathBuf> + Clone + Send + Sync + 'static,
    {
        let limits = mizzle::serve::ProtocolLimits::default();
        mizzle::servers::axum::serve_with_backend(
            state.0.clone(),
            state.1.clone(),
            &path,
            &limits,
            req,
        )
        .await
    }

    let state = Arc::new((config, backend));
    let app = Router::new()
        .route("/{*key}", get(handler::<B>).post(handler::<B>))
        .with_state(state);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    thread::spawn(move || {
        rt.block_on(async {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    rx.await.ok();
                })
                .await
        })
        .unwrap()
    });

    ServerHandle {
        port,
        stop: Some(Box::new(move || {
            let _ = tx.send(());
        })),
    }
}

// ── Benchmarks ──────────────────────────────────────────────────────────────

fn bench_clone(c: &mut Criterion) {
    let mut group = c.benchmark_group("clone");

    for (name, server) in make_servers() {
        group.bench_with_input(BenchmarkId::new("clone", &name), &server, |b, srv| {
            b.iter(|| {
                let clone_dir = tempdir().unwrap();
                run_git(
                    clone_dir.path(),
                    ["clone", &format!("http://localhost:{}/test.git", srv.port)],
                )
                .unwrap();
            });
        });
    }

    group.finish();
}

fn bench_push(c: &mut Criterion) {
    let mut group = c.benchmark_group("push");

    for (name, server) in make_servers() {
        group.bench_with_input(BenchmarkId::new("push", &name), &server, |b, srv| {
            b.iter(|| {
                let clone_dir = tempdir().unwrap();
                run_git(
                    clone_dir.path(),
                    [
                        "clone",
                        "--branch",
                        "main",
                        &format!("http://localhost:{}/test.git", srv.port),
                    ],
                )
                .unwrap();
                let repo_dir = clone_dir.path().join("test");
                fs::write(repo_dir.join("bench.txt"), "bench\n").unwrap();
                run_git(&repo_dir, ["add", "bench.txt"]).unwrap();
                run_git(&repo_dir, ["commit", "-m", "bench commit"]).unwrap();
                run_git(&repo_dir, ["push", "origin", "main"]).unwrap();
            });
        });
    }

    group.finish();
}

fn bench_fetch(c: &mut Criterion) {
    let mut group = c.benchmark_group("fetch");

    for (name, server) in make_servers() {
        // Pre-clone once outside the benchmark loop.
        let clone_dir = tempdir().unwrap();
        run_git(
            clone_dir.path(),
            [
                "clone",
                "--branch",
                "main",
                &format!("http://localhost:{}/test.git", server.port),
            ],
        )
        .unwrap();
        let repo_dir = clone_dir.path().join("test");

        group.bench_with_input(BenchmarkId::new("fetch", &name), &server, |b, srv| {
            b.iter(|| {
                run_git(
                    repo_dir.as_path(),
                    [
                        "fetch",
                        &format!("http://localhost:{}/test.git", srv.port),
                        "main",
                    ],
                )
                .unwrap();
            });
        });
    }

    group.finish();
}

fn bench_ls_remote(c: &mut Criterion) {
    let mut group = c.benchmark_group("ls_remote");

    for (name, server) in make_servers() {
        let tmp = tempdir().unwrap();
        group.bench_with_input(BenchmarkId::new("ls_remote", &name), &server, |b, srv| {
            b.iter(|| {
                run_git(
                    tmp.path(),
                    [
                        "ls-remote",
                        &format!("http://localhost:{}/test.git", srv.port),
                    ],
                )
                .unwrap();
            });
        });
    }

    group.finish();
}

/// Create one server per backend, each backed by its own temprepo.
fn make_servers() -> Vec<(String, ServerHandle)> {
    let gitoxide_repo = temprepo().unwrap();
    let gitcli_repo = temprepo().unwrap();

    let gitoxide = start_server(
        BenchConfig {
            bare_repo_path: gitoxide_repo.path(),
        },
        FsGitoxide,
    );
    let gitcli = start_server(
        BenchConfig {
            bare_repo_path: gitcli_repo.path(),
        },
        FsGitCli,
    );

    // Leak the temprepo TempDirs so they survive for the benchmark lifetime.
    std::mem::forget(gitoxide_repo);
    std::mem::forget(gitcli_repo);

    vec![
        ("FsGitoxide".to_string(), gitoxide),
        ("FsGitCli".to_string(), gitcli),
    ]
}

criterion_group!(
    benches,
    bench_clone,
    bench_push,
    bench_fetch,
    bench_ls_remote
);
criterion_main!(benches);
