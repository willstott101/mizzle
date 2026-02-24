use std::sync::Arc;

use axum::{routing::get, Router};

use mizzle::servers::axum::axum_handler;
use mizzle::traits::{GitServerCallbacks};

#[derive(Clone)]
struct Config;

impl GitServerCallbacks for Config {
    fn auth(&self, repo_path: &str) -> Box<str> {
        let repo_root = ".";

        format!("{}/{}", repo_root, repo_path).into()
    }
}

#[tokio::main]
async fn main() {
    let config = Arc::new(Config {});

    // build our application with a single route
    let app = Router::new()
        .route("/", get(axum_handler))
        .with_state(config);

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
