use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_lite::AsyncRead;
use futures_util::TryStreamExt;
use std::io;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tokio_util::io::StreamReader;

use crate::{
    backend::StorageBackend,
    lfs::{batch, transfer, LfsStore},
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

/// Serve a Git LFS request.
///
/// `path` is the full URL path (e.g. `"myrepo.git/info/lfs/objects/batch"`).
/// Splits on `.git/` to extract the repo prefix and LFS service path.
pub async fn serve_lfs<A, L>(access: A, lfs: L, path: &str, req: Request) -> Response
where
    A: RepoAccess + Send + 'static,
    L: LfsStore<RepoId = A::RepoId> + Clone + Send + 'static,
{
    let Some((repo_prefix, service_path)) = path.rsplit_once(".git/") else {
        return (StatusCode::BAD_REQUEST, "Path doesn't look like a git URL").into_response();
    };

    // Strip the `info/lfs/` prefix.
    let Some(lfs_path) = service_path.strip_prefix("info/lfs/") else {
        return (StatusCode::NOT_FOUND, "Not an LFS path").into_response();
    };

    // Construct the LFS base URL from request headers.
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost")
        .to_string();
    let proto = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http")
        .to_string();
    let lfs_base_url = format!("{}://{}/{}.git/info/lfs", proto, host, repo_prefix);

    let method = req.method().as_str().to_uppercase();
    let repo_id = access.repo_id().clone();

    match (method.as_str(), lfs_path) {
        ("POST", "objects/batch") => {
            let body_bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
                Ok(b) => b,
                Err(_) => return (StatusCode::BAD_REQUEST, "failed to read body").into_response(),
            };
            let (status, json) =
                batch::handle_batch(&access, &lfs, &repo_id, &lfs_base_url, &body_bytes).await;
            let status_code =
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (
                status_code,
                [(
                    header::CONTENT_TYPE,
                    "application/vnd.git-lfs+json"
                        .parse::<axum::http::HeaderValue>()
                        .unwrap(),
                )],
                json,
            )
                .into_response()
        }

        ("GET", p) if p.starts_with("objects/") => {
            let oid_hex = &p["objects/".len()..];
            let (status, reader_opt) =
                transfer::handle_download(&access, &lfs, &repo_id, oid_hex).await;
            let status_code =
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            if let Some(reader) = reader_opt {
                let stream = tokio_util::io::ReaderStream::new(reader.compat());
                (status_code, Body::from_stream(stream)).into_response()
            } else {
                status_code.into_response()
            }
        }

        ("PUT", p) if p.starts_with("objects/") => {
            let oid_hex = p["objects/".len()..].to_string();
            // Get Content-Length from headers before consuming the request.
            let size: u64 = req
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            // Convert the axum body into a futures_lite::AsyncRead.
            let stream = req
                .into_body()
                .into_data_stream()
                .map_err(|err| io::Error::new(io::ErrorKind::Other, err));
            let body_reader: Box<dyn AsyncRead + Send + Unpin> =
                Box::new(StreamReader::new(stream).compat());

            let status =
                transfer::handle_upload(&access, &lfs, &repo_id, &oid_hex, size, body_reader).await;
            StatusCode::from_u16(status)
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }

        ("POST", "objects/verify") => {
            let body_bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
                Ok(b) => b,
                Err(_) => return (StatusCode::BAD_REQUEST, "failed to read body").into_response(),
            };
            #[derive(serde::Deserialize)]
            struct VerifyBody {
                oid: mizzle_proto::lfs::LfsOid,
                size: u64,
            }
            let vb: VerifyBody = match serde_json::from_slice(&body_bytes) {
                Ok(v) => v,
                Err(_) => return (StatusCode::BAD_REQUEST, "invalid verify body").into_response(),
            };
            let status = transfer::handle_verify(&access, &lfs, &repo_id, &vb.oid, vb.size).await;
            StatusCode::from_u16(status)
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }

        _ => StatusCode::NOT_IMPLEMENTED.into_response(),
    }
}

/// Serve a git + LFS request using arbitrary [`StorageBackend`] and [`LfsStore`].
///
/// Routes `info/lfs/…` paths to [`serve_lfs`]; everything else to the existing
/// git protocol dispatch.
pub async fn serve_with_backends<A, B, L>(
    access: A,
    git: B,
    lfs: L,
    path: &str,
    limits: &ProtocolLimits,
    req: Request,
) -> Response
where
    A: RepoAccess + Send + 'static,
    B: StorageBackend<RepoId = A::RepoId> + Clone + Send + 'static,
    L: LfsStore<RepoId = A::RepoId> + Clone + Send + 'static,
{
    let Some((_, service_path)) = path.rsplit_once(".git/") else {
        return (StatusCode::BAD_REQUEST, "Path doesn't look like a git URL").into_response();
    };

    if service_path.starts_with("info/lfs/") {
        return serve_lfs(access, lfs, path, req).await;
    }

    // Fall through to git protocol handling.
    serve_with_backend(access, git, path, limits, req).await
}

/// Serve a git request using an arbitrary [`StorageBackend`].
///
/// This is the generic version of [`serve`] — use it when you want to plug in
/// a backend other than the default [`FsGitoxide`].
pub async fn serve_with_backend<A, B>(
    access: A,
    backend: B,
    path: &str,
    limits: &ProtocolLimits,
    req: Request,
) -> Response
where
    A: RepoAccess + Send + 'static,
    B: StorageBackend<RepoId = A::RepoId> + Clone + Send + 'static,
{
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
