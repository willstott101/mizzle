use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::TryStreamExt;
use std::io;
use std::sync::Arc;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tokio_util::io::StreamReader;

use crate::{
    serve::{serve_git_protocol_2, GitResponse, MizzleRuntime},
    traits::GitServerCallbacks,
};

impl IntoResponse for GitResponse {
    fn into_response(self) -> Response {
        let mut headers = HeaderMap::new();
        headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
        if let Some(content_type) = self.content_type {
            headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
        }

        let body = match self.reader {
            Some(reader) => Body::from_stream(tokio_util::io::ReaderStream::new(reader.compat())),
            None => Body::from(self.body.unwrap_or("".to_string())),
        };
        let status_code =
            StatusCode::from_u16(self.status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status_code, headers, body).into_response()
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

    let content_type = match req.headers().get(header::CONTENT_TYPE) {
        Some(header) => header.to_str().unwrap().into(),
        None => "".into(),
    };

    let stream = req
        .into_body()
        .into_data_stream()
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err));
    let reader = StreamReader::new(stream);

    let reader = reader.compat();

    let result = path.rsplit_once(".git/");
    match result {
        Some((git_repo_path, service_path)) => {
            let repo_path_owned: Box<str> = git_repo_path.into();
            let protocol_path_owned: Box<str> = service_path.into();
            let full_repo_path = config.auth(repo_path_owned.as_ref());
            let res = serve_git_protocol_2(
                MizzleRuntime::Tokio,
                full_repo_path,
                protocol_path_owned,
                content_type,
                reader,
            )
            .await;
            res.into_response()
        }
        None => (
            StatusCode::BAD_REQUEST,
            format!("Path doesn't look like a git URL"),
        )
            .into_response(),
    }
}
