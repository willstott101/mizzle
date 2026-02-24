use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::{
    serve::{serve_git_protocol_2_2, GitResponse},
    traits::GitServerCallbacks,
};

impl IntoResponse for GitResponse {
    fn into_response(self) -> Response {
        let body = Body::from_stream(tokio_util::io::ReaderStream::new(self.reader.compat()));

        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, self.content_type.parse().unwrap());
        headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());

        (headers, body).into_response()
    }
}

// #[axum::debug_handler]
pub async fn axum_handler<T: GitServerCallbacks>(
    State(config): State<Arc<T>>,
    path: Path<String>,
    req: Request,
) -> Response {
    if req.headers().get("Git-Protocol").unwrap().to_str().unwrap() != "version=2" {
        println!("Only Git Protocol 2 is supported");
        return (
            StatusCode::NOT_IMPLEMENTED,
            format!("Only Git Protocol 2 is supported"),
        )
            .into_response();
    }

    let result = path.rsplit_once(".git/");
    match result {
        Some((git_repo_path, service_path)) => {
            let repo_path_owned: Box<str> = git_repo_path.into();
            let protocol_path_owned: Box<str> = service_path.into();
            let full_repo_path = config.auth(repo_path_owned.as_ref());
            let res = serve_git_protocol_2_2(full_repo_path, protocol_path_owned).await;
            res.into_response()
            // let mut res = Response::new(format!("bob"));
            // res.headers_mut().insert("Cache-Control", "no-cache".parse().unwrap());
            // res
        }
        None => (
            StatusCode::BAD_REQUEST,
            format!("Path doesn't look like a git URL"),
        )
            .into_response(),
    }
}
