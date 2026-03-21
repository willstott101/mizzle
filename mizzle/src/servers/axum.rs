use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::TryStreamExt;
use std::io;
use std::path::PathBuf;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tokio_util::io::StreamReader;

use crate::{
    backend::fs_gitoxide::FsGitoxide,
    serve::{serve_git_protocol_1, serve_git_protocol_2, GitResponse, ProtocolLimits},
    traits::RepoAccess,
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

/// Serve a git request using the default filesystem (gitoxide) backend.
///
/// Call this from your own handler after performing whatever authentication you
/// need.  `path` is the full URL path (e.g. `"myrepo.git/info/refs"`).
pub async fn serve<A: RepoAccess<RepoId = PathBuf> + Send + 'static>(
    access: A,
    path: &str,
    limits: &ProtocolLimits,
    req: Request,
) -> Response {
    let git_protocol = req
        .headers()
        .get("Git-Protocol")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("version=1")
        .to_string();

    let content_type: Box<str> = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .into();

    let query_string: Box<str> = req.uri().query().unwrap_or("").into();

    let stream = req
        .into_body()
        .into_data_stream()
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err));
    let reader = StreamReader::new(stream).compat();

    let Some((_, service_path)) = path.rsplit_once(".git/") else {
        return (StatusCode::BAD_REQUEST, "Path doesn't look like a git URL").into_response();
    };

    let backend = FsGitoxide;

    if git_protocol.as_str() == "version=2" {
        serve_git_protocol_2(
            |fut| {
                tokio::spawn(fut);
            },
            access,
            backend,
            service_path.into(),
            query_string,
            content_type,
            limits,
            reader,
        )
        .await
        .into_response()
    } else {
        serve_git_protocol_1(
            |fut| {
                tokio::spawn(fut);
            },
            access,
            backend,
            service_path.into(),
            query_string,
            content_type,
            limits,
            reader,
        )
        .await
        .into_response()
    }
}
