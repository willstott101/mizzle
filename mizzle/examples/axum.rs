use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{DefaultBodyLimit, Path, Request, State},
    response::Response,
    routing::get,
    Router,
};

use axum::http::StatusCode;
use axum::response::IntoResponse;
use mizzle::backend::fs_gitoxide::FsGitoxide;
use mizzle::lfs::fs::FsLfs;
use mizzle::serve::ProtocolLimits;
use mizzle::servers::axum::serve_with_backends;
use mizzle::traits::{PushRef, RepoAccess};
use tower::limit::ConcurrencyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tracing::info;

struct Access {
    repo_path: PathBuf,
}

impl RepoAccess for Access {
    type RepoId = PathBuf;
    type PushContext = ();

    fn repo_id(&self) -> &PathBuf {
        &self.repo_path
    }

    fn authorize_preliminary(&self, refs: &[PushRef<'_>]) -> Result<(), String> {
        for r in refs {
            if !r.refname.starts_with("refs/heads/") {
                return Err(format!("pushes to {} are not allowed", r.refname));
            }
        }
        Ok(())
    }

    fn auto_init(&self) -> bool {
        true
    }
}

async fn git_handler(
    State(config): State<Arc<Config>>,
    Path(path): Path<String>,
    req: Request,
) -> Response {
    let token = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok());
    if token != Some("Bearer secret") {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    // Derive the per-repo path from the URL: everything before `.git/`.
    let repo_name = path
        .split_once(".git/")
        .map(|(prefix, _)| prefix)
        .unwrap_or(path.trim_end_matches('/'));
    let repo_path = config.repos_base.join(repo_name);

    let access = Access { repo_path };
    serve_with_backends(access, FsGitoxide, FsLfs, &path, &config.limits, req).await
}

#[derive(Clone)]
struct Config {
    repos_base: PathBuf,
    limits: ProtocolLimits,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Arc::new(Config {
        repos_base: PathBuf::from("."),
        limits: ProtocolLimits::default(),
    });

    let app = Router::new()
        .route("/{*key}", get(git_handler).post(git_handler))
        .layer(DefaultBodyLimit::max(5 * 1024 * 1024 * 1024)) // 5 GB
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            Duration::from_secs(300),
        ))
        .layer(ConcurrencyLimitLayer::new(64))
        .with_state(config);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    let address = listener.local_addr().unwrap();
    info!("Server running at http://{}", address);
    axum::serve(listener, app).await.unwrap();
}
