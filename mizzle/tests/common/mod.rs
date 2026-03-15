#![allow(dead_code, unused_macros, unused_imports)]

use anyhow::{anyhow, bail, Result};
#[cfg(feature = "axum")]
use axum::extract::{Path, Request, State};
#[cfg(feature = "axum")]
use axum::response::Response;
#[cfg(feature = "axum")]
use axum::routing::get;
#[cfg(feature = "axum")]
use axum::Router;
use mizzle::traits::RepoAccess;
use simple_logger::SimpleLogger;
use std::ffi::OsStr;
use std::path::{Path as FsPath, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Once};
use std::{fs, thread};
#[cfg(feature = "trillium_smol")]
use trillium::State as TrilliumState;

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
    let repo = TempRepo { dir };

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
fn create_bare_repo_with_refs(bare_dir: &FsPath) -> Result<()> {
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
    _run_git(&work_dir, ["init", "-b", "main"])?;

    // Set an identity so commits succeed even without global git config
    _run_git(&work_dir, ["config", "user.name", "Example Bot"])?;
    _run_git(&work_dir, ["config", "user.email", "bot@example.invalid"])?;

    // Disable features that can introduce nondeterminism
    _run_git(&work_dir, ["config", "commit.gpgsign", "false"])?;
    _run_git(&work_dir, ["config", "core.autocrlf", "false"])?;
    _run_git(&work_dir, ["config", "core.filemode", "false"])?;

    // Create initial commit
    fs::write(work_dir.join("README.md"), "# Demo repo\n")?;
    _run_git(&work_dir, ["add", "."])?;
    _run_git(&work_dir, ["commit", "-m", "Initial commit"])?;

    // Second commit on main
    fs::write(work_dir.join("hello.txt"), "hello\n")?;
    _run_git(&work_dir, ["add", "."])?;
    _run_git(&work_dir, ["commit", "-m", "Add hello.txt"])?;

    // Create a dev branch with an extra commit
    _run_git(&work_dir, ["checkout", "-b", "dev"])?;
    fs::write(work_dir.join("dev.txt"), "dev branch work\n")?;
    _run_git(&work_dir, ["add", "."])?;
    _run_git(&work_dir, ["commit", "-m", "Dev commit"])?;

    // Back to main
    _run_git(&work_dir, ["checkout", "main"])?;

    // Create a tag on main (lightweight)
    _run_git(&work_dir, ["tag", "v1.0.0"])?;

    // Create a custom ref pointing at the current HEAD (main)
    let head_oid = _run_git(&work_dir, ["rev-parse", "HEAD"])?;
    _run_git(
        &work_dir,
        ["update-ref", "refs/custom/demo", head_oid.as_str()],
    )?;

    // Create another custom ref pointing at dev tip
    let dev_oid = _run_git(&work_dir, ["rev-parse", "dev"])?;
    _run_git(
        &work_dir,
        ["update-ref", "refs/custom/dev-tip", dev_oid.as_str()],
    )?;

    // 2) Init bare repo
    fs::create_dir_all(bare_dir)?;
    _run_git(bare_dir, ["init", "--bare"])?;

    // 3) Add bare as a remote and push everything
    _run_git(
        &work_dir,
        ["remote", "add", "origin", bare_dir.to_str().unwrap()],
    )?;

    // Push branches + tags + "normal" refs
    // --mirror pushes refs under refs/* (including custom ones) and deletes remote refs not present locally.
    _run_git(&work_dir, ["push", "--mirror", "origin"])?;

    // Create a symbolic ref in the bare repo so HEAD points to main.
    // (Some tooling expects HEAD to reference the default branch.)
    _run_git(bare_dir, ["symbolic-ref", "HEAD", "refs/heads/main"])?;

    // Cleanup working dir
    fs::remove_dir_all(&work_dir)?;

    Ok(())
}

pub fn run_git<I, S>(cwd: &FsPath, args: I) -> Result<String>
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
        .env("GIT_TRACE_PACKET", "1")
        .env("GIT_TRACE", "2")
        .env("GIT_CURL_VERBOSE", "1")
        .stdin(Stdio::null())
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

fn _run_git<I, S>(cwd: &FsPath, args: I) -> Result<String>
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
            output.status,
            stdout,
            stderr
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[derive(Clone)]
pub struct Config {
    pub bare_repo_path: PathBuf,
}

impl RepoAccess for Config {
    fn repo_path(&self) -> &str {
        self.bare_repo_path.to_str().unwrap()
    }
}

static INIT: Once = Once::new();

pub fn init_logging() {
    INIT.call_once(|| {
        SimpleLogger::new()
            .with_level(log::LevelFilter::Info)
            .init()
            .unwrap();
    });
}

pub struct ServerHandle {
    pub port: u16,
    stop: Box<dyn FnOnce()>,
}

impl ServerHandle {
    pub fn new(port: u16, stop: impl FnOnce() + 'static) -> Self {
        ServerHandle {
            port,
            stop: Box::new(stop),
        }
    }

    pub fn stop(self) {
        (self.stop)();
    }
}

// Concrete axum handler for the test Config type.
#[cfg(feature = "axum")]
async fn axum_git_handler(
    State(config): State<Arc<Config>>,
    Path(path): Path<String>,
    req: Request,
) -> Response {
    mizzle::servers::axum::serve((*config).clone(), &path, req).await
}

#[cfg(feature = "axum")]
pub fn axum_server(config: Config) -> ServerHandle {
    init_logging();

    let config = Arc::new(config);

    let app = Router::new()
        .route("/{*key}", get(axum_git_handler).post(axum_git_handler))
        .with_state(config);

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

    ServerHandle::new(port, move || {
        let _ = tx.send(());
    })
}

// Concrete trillium handler for the test Config type.
#[cfg(feature = "trillium_smol")]
async fn trillium_git_handler(conn: trillium::Conn) -> trillium::Conn {
    let config = conn.state::<Config>().unwrap().clone();
    mizzle::servers::trillium::serve(config, conn).await
}

#[cfg(feature = "trillium_smol")]
pub fn trillium_server(config: Config) -> ServerHandle {
    init_logging();

    let stopper = trillium_smol::Stopper::new();

    let listener = smol::block_on(async_net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = trillium_smol::config()
        .with_stopper(stopper.clone())
        .with_prebound_server(listener);

    thread::spawn(move || {
        server.run((TrilliumState::new(config), trillium_git_handler));
    });

    ServerHandle::new(port, move || stopper.stop())
}

#[cfg(feature = "actix")]
async fn actix_git_handler(
    req: actix_web::HttpRequest,
    payload: actix_web::web::Payload,
    config: actix_web::web::Data<Config>,
) -> actix_web::HttpResponse {
    mizzle::servers::actix::serve(config.get_ref().clone(), req, payload).await
}

#[cfg(feature = "actix")]
pub fn actix_server(config: Config) -> ServerHandle {
    use actix_web::{web, App, HttpServer};

    init_logging();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let data = actix_web::web::Data::new(config);
    let server = HttpServer::new(move || {
        App::new()
            .app_data(data.clone())
            .route("/{tail:.*}", web::get().to(actix_git_handler))
            .route("/{tail:.*}", web::post().to(actix_git_handler))
    })
    .listen(listener)
    .unwrap()
    .run();

    let actix_handle = server.handle();
    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(server).unwrap();
    });

    ServerHandle::new(port, move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(actix_handle.stop(false));
    })
}

// Concrete rocket route handlers for the test Config type.
// rocket's #[get]/#[post] macros don't support generic functions, so we define
// them here with a concrete type and call through to the generic library helper.
#[cfg(feature = "rocket")]
mod rocket_handlers {
    use super::Config;
    use mizzle::servers::rocket as mr;
    use rocket::data::ToByteUnit;
    use rocket::tokio::io::AsyncReadExt;
    use rocket::{get, post, Data, State};

    #[get("/<path..>")]
    pub async fn git_get(
        path: std::path::PathBuf,
        meta: mr::GitRequestMeta,
        config: &State<Config>,
    ) -> mr::RocketGitResponse {
        mr::handle_git_request(
            config.inner().clone(),
            &path.to_string_lossy(),
            meta,
            Box::pin(futures_lite::io::empty()),
        )
        .await
    }

    #[post("/<path..>", data = "<data>")]
    pub async fn git_post(
        path: std::path::PathBuf,
        meta: mr::GitRequestMeta,
        config: &State<Config>,
        data: Data<'_>,
    ) -> mr::RocketGitResponse {
        let mut buf = Vec::new();
        let _ = data.open(512.mebibytes()).read_to_end(&mut buf).await;
        let reader = Box::pin(futures_lite::io::Cursor::new(buf));
        mr::handle_git_request(
            config.inner().clone(),
            &path.to_string_lossy(),
            meta,
            reader,
        )
        .await
    }
}

// Note: rocket handlers can't be generic, so rocket_server only accepts the
// concrete test Config. Auth tests that use custom config types stay axum-only.
#[cfg(feature = "rocket")]
pub fn rocket_server(config: Config) -> ServerHandle {
    init_logging();

    // Briefly bind to port 0 to find a free port, then release it for rocket.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<rocket::Shutdown>();

    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rocket_config = rocket::Config {
                port,
                address: "127.0.0.1".parse().unwrap(),
                log_level: rocket::config::LogLevel::Off,
                ..rocket::Config::default()
            };
            let ignited = rocket::custom(rocket_config)
                .manage(config)
                .mount(
                    "/",
                    rocket::routes![rocket_handlers::git_get, rocket_handlers::git_post],
                )
                .ignite()
                .await
                .unwrap();

            let shutdown = ignited.shutdown();
            ready_tx.send(shutdown).unwrap();
            let _ = ignited.launch().await;
        });
    });

    let shutdown = ready_rx.recv().unwrap();

    ServerHandle::new(port, move || shutdown.notify())
}

/// Generates sub-tests from a single body, one per supported server framework.
/// The body receives a `start_server: impl Fn(Config) -> ServerHandle` closure.
///
/// Usage:
/// ```
/// test_with_servers!(my_test, |start_server| {
///     let server = start_server(Config { ... });
///     // ... test using server.port
///     server.stop();
///     Ok(())
/// });
/// ```
macro_rules! test_with_servers {
    ($name:ident, |$start:ident| $body:block) => {
        mod $name {
            use super::common;
            use super::*;

            fn run($start: impl Fn(common::Config) -> common::ServerHandle) -> anyhow::Result<()> {
                $body
            }

            #[cfg(feature = "axum")]
            #[test]
            fn axum() -> anyhow::Result<()> {
                run(|c| common::axum_server(c))
            }

            #[cfg(feature = "trillium_smol")]
            #[test]
            fn trillium() -> anyhow::Result<()> {
                run(|c| common::trillium_server(c))
            }

            #[cfg(feature = "actix")]
            #[test]
            fn actix() -> anyhow::Result<()> {
                run(|c| common::actix_server(c))
            }

            #[cfg(feature = "rocket")]
            #[test]
            fn rocket() -> anyhow::Result<()> {
                run(|c| common::rocket_server(c))
            }
        }
    };
}
pub(super) use test_with_servers;
