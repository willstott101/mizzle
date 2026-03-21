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
use log::info;
use mizzle::serve::ProtocolLimits;
use mizzle::servers::axum::serve;
use mizzle::traits::{PushRef, RepoAccess};
use simple_logger::SimpleLogger;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::timeout::TimeoutLayer;

struct Access {
    repo_path: PathBuf,
}

impl RepoAccess for Access {
    type RepoId = PathBuf;

    fn repo_id(&self) -> &PathBuf {
        &self.repo_path
    }

    fn authorize_push(&self, refs: &[PushRef<'_>]) -> Result<(), String> {
        for r in refs {
            if !r.refname.starts_with("refs/heads/") {
                return Err(format!("pushes to {} are not allowed", r.refname));
            }
        }
        Ok(())
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

    let access = Access {
        repo_path: config.repo_path.clone(),
    };
    serve(access, &path, &config.limits, req).await
}

#[derive(Clone)]
struct Config {
    repo_path: PathBuf,
    limits: ProtocolLimits,
}

#[tokio::main]
async fn main() {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init()
        .unwrap();

    let config = Arc::new(Config {
        repo_path: PathBuf::from("."),
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
