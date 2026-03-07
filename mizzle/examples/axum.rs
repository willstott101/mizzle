use std::sync::Arc;

use axum::{routing::get, Router};

use log::info;
use mizzle::servers::axum::axum_handler;
use mizzle::traits::GitServerCallbacks;
use simple_logger::SimpleLogger;

#[derive(Clone)]
struct Config;

impl GitServerCallbacks for Config {
    fn auth(&self, repo_path: &str) -> Box<str> {
        let repo_root = ".";

        // format!("{}/{}", repo_root, repo_path).into()
        format!("{}", repo_root).into()
    }
}

#[tokio::main]
async fn main() {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init()
        .unwrap();

    let config = Arc::new(Config {});

    // build our application with a single route
    let app = Router::new()
        .route("/{*key}", get(axum_handler).post(axum_handler))
        .with_state(config);

    // run our app with hyper, listening globally on port 8080
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    let address = listener.local_addr().unwrap();
    info!("Server running at http://{}", address);
    axum::serve(listener, app).await.unwrap();
}
