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
use base64::Engine;
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
    let auth_header = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok());

    let authorized = if let Some(header) = auth_header {
        if let Some(credentials) = header.strip_prefix("Basic ") {
            if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(credentials) {
                if let Ok(creds_str) = String::from_utf8(decoded) {
                    // Check for username:password (default is "user:pass")
                    creds_str == "user:pass"
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    if !authorized {
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
        repos_base: PathBuf::from("./data"),
        limits: ProtocolLimits::default(),
    });

    let app = Router::new()
        .route(
            "/{*key}",
            get(git_handler).post(git_handler).put(git_handler),
        )
        .layer(DefaultBodyLimit::max(5 * 1024 * 1024 * 1024)) // 5 GB
        // NOTE: Do not set a short timeout here if you serve LFS uploads.
        // TimeoutLayer applies to the entire request/response cycle, so a
        // large upload that takes longer than the deadline will be killed
        // mid-transfer and the client will see a broken pipe or 504.
        // Either omit this layer or set a value larger than your worst-case
        // upload time.  For non-LFS workloads a 300 s timeout is fine.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            Duration::from_secs(3600), // 1 h — accommodates large LFS uploads
        ))
        .layer(ConcurrencyLimitLayer::new(64))
        .with_state(config);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    let address = listener.local_addr().unwrap();
    info!("Server running at http://{}", address);
    axum::serve(listener, app).await.unwrap();
}
