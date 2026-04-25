use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

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

mod support;
use support::git_runner::run_git;
use support::repo_builder::RepoBuilder;
use support::span_totals;

/// Number of linear commits in the `deep` reference repo — large enough that
/// `build_have_set` walks meaningful history, small enough that the cold
/// build completes in ~30s on commodity hardware.
const DEEP_LINEAR_COMMITS: usize = 10_000;

// ── Minimal test infrastructure (mirrors tests/common) ──────────────────────

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

// ── Deep-repo fetch-incremental bench ───────────────────────────────────────

/// Two versions of the `deep` repo: one plain (`bare`), one post-processed
/// with `git repack -adb` so it has `.bitmap` + `.rev` side files
/// (`bare_bitmap`).  Cold build takes several seconds so both must live
/// outside the iteration loop.
static DEEP_REPO: OnceLock<DeepRepo> = OnceLock::new();

struct DeepRepo {
    bare: PathBuf,
    bare_bitmap: PathBuf,
    /// Held to keep the TempDir alive for the process lifetime.
    _dir: TempDir,
}

fn deep_repo() -> &'static DeepRepo {
    DEEP_REPO.get_or_init(|| {
        let dir = tempdir().expect("tempdir for deep repo");
        let bare = dir.path().join("deep.git");
        let bare_bitmap = dir.path().join("deep-bitmap.git");
        RepoBuilder::new(bare.clone())
            .linear_commits(DEEP_LINEAR_COMMITS)
            .build()
            .expect("build deep repo");
        RepoBuilder::new(bare_bitmap.clone())
            .linear_commits(DEEP_LINEAR_COMMITS)
            .with_bitmap()
            .build()
            .expect("build deep-bitmap repo");
        DeepRepo {
            bare,
            bare_bitmap,
            _dir: dir,
        }
    })
}

/// Resolve a rev to an `ObjectId` via git CLI.
fn rev_parse(repo: &Path, rev: &str) -> gix::ObjectId {
    let hex = run_git(repo, ["rev-parse", rev]).unwrap();
    gix::ObjectId::from_hex(hex.as_bytes()).unwrap()
}

/// Drive `build_pack` on a backend and fully consume its streaming reader.
///
/// Measures pack computation directly rather than through an HTTP `git fetch`
/// because iterative fetch short-circuits once the client already has the
/// target tip, and the HTTP transport layer is orthogonal to have-set cost.
fn drive_build_pack<B>(backend: &B, repo: &B::Repo, want: &[gix::ObjectId], have: &[gix::ObjectId])
where
    B: StorageBackend,
{
    use std::io::Read;
    let opts = mizzle::backend::PackOptions {
        deepen: None,
        filter: None,
        thin_pack: false,
    };
    let mut out = backend.build_pack(repo, want, have, &opts).unwrap();
    let mut sink = [0u8; 64 * 1024];
    while out.reader.read(&mut sink).unwrap() > 0 {}
}

fn bench_fetch_incremental(c: &mut Criterion) {
    span_totals::install();

    // Snapshot JSON lives next to criterion's output so it's easy to diff
    // across runs.  Deleted at start so each bench run produces a fresh file.
    let out_path = PathBuf::from("target/criterion/bitmap-spans.jsonl");
    let _ = fs::remove_file(&out_path);

    let repos = deep_repo();
    // Re-parse tips against the non-bitmap repo; both variants have
    // identical commit OIDs by construction (same RepoBuilder input).
    let tip = rev_parse(&repos.bare, "HEAD");
    let have_oids: Vec<(usize, gix::ObjectId)> = [1usize, 100]
        .into_iter()
        .map(|n| (n, rev_parse(&repos.bare, &format!("HEAD~{n}"))))
        .collect();

    let mut group = c.benchmark_group("fetch_incremental");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    // Run each backend against both the plain and bitmap-enabled `deep`
    // repo so we can read off bitmap-vs-walker deltas directly from
    // criterion's change report.  `FsGitoxide` uses our in-tree bitmap
    // reader (see `src/bitmap.rs`); `FsGitCli` uses git's native bitmap
    // support, providing an external reference for the gitoxide path.
    let variants: [(&str, &PathBuf); 2] =
        [("nobitmap", &repos.bare), ("bitmap", &repos.bare_bitmap)];

    for (variant, bare_path) in variants {
        {
            let backend = FsGitoxide;
            let repo = backend.open(bare_path).unwrap();
            for &(behind, have_oid) in &have_oids {
                let case = format!("{variant}/{behind}_behind");
                span_totals::reset();
                group.bench_with_input(
                    BenchmarkId::new("FsGitoxide", &case),
                    &have_oid,
                    |b, &have_oid| {
                        b.iter(|| drive_build_pack(&backend, &repo, &[tip], &[have_oid]));
                    },
                );
                let snap = span_totals::snapshot(format!("deep/FsGitoxide/{case}"));
                let _ = span_totals::append_snapshot(&out_path, &snap);
            }
        }

        // FsGitCli shells out to `git upload-pack`, so `pack::*` spans are
        // never entered and its span totals are always empty.  Wall-clock
        // is the only useful output for it; keep it as a cross-check.
        {
            let backend = FsGitCli;
            let repo = backend.open(bare_path).unwrap();
            for &(behind, have_oid) in &have_oids {
                let case = format!("{variant}/{behind}_behind");
                span_totals::reset();
                group.bench_with_input(
                    BenchmarkId::new("FsGitCli", &case),
                    &have_oid,
                    |b, &have_oid| {
                        b.iter(|| drive_build_pack(&backend, &repo, &[tip], &[have_oid]));
                    },
                );
                let snap = span_totals::snapshot(format!("deep/FsGitCli/{case}"));
                let _ = span_totals::append_snapshot(&out_path, &snap);
            }
        }
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_clone,
    bench_push,
    bench_fetch,
    bench_ls_remote,
    bench_fetch_incremental
);
criterion_main!(benches);
