use actix_web::{web, HttpRequest, HttpResponse};
use futures_util::TryStreamExt;
use std::io;
use std::sync::Arc;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tokio_util::io::StreamReader;

use crate::{
    serve::{serve_git_protocol_2, GitResponse},
    traits::GitServerCallbacks,
};

impl GitResponse {
    fn into_http_response(self) -> HttpResponse {
        let status = actix_web::http::StatusCode::from_u16(self.status_code)
            .unwrap_or(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR);

        let mut builder = HttpResponse::build(status);
        builder.insert_header(("Cache-Control", "no-cache"));
        if let Some(ct) = self.content_type {
            builder.insert_header(("Content-Type", ct));
        }

        match self.reader {
            Some(reader) => builder.streaming(tokio_util::io::ReaderStream::new(reader.compat())),
            None => builder.body(self.body.unwrap_or_default()),
        }
    }
}

pub async fn actix_handler<T: GitServerCallbacks + Send + Sync + 'static>(
    req: HttpRequest,
    payload: web::Payload,
    config: web::Data<T>,
) -> HttpResponse {
    let git_protocol = req
        .headers()
        .get("Git-Protocol")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("version=2");

    if git_protocol != "version=2" {
        return HttpResponse::NotImplemented().body("Only Git Protocol 2 is supported");
    }

    let content_type: Box<str> = req
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .into();

    let query_string: Box<str> = req.query_string().into();

    // Strip the leading '/' from the path.
    let path = req.path().trim_start_matches('/');

    let stream = payload.map_err(|e| io::Error::new(io::ErrorKind::Other, e));
    let reader = StreamReader::new(stream).compat(); // tokio AsyncRead → futures AsyncRead

    match path.rsplit_once(".git/") {
        Some((git_repo_path, service_path)) => {
            let repo_path_owned: Box<str> = git_repo_path.into();
            let protocol_path_owned: Box<str> = service_path.into();
            let full_repo_path = config.auth(repo_path_owned.as_ref());
            let callbacks = Arc::new(config.get_ref().clone());
            serve_git_protocol_2(
                |fut| {
                    tokio::spawn(fut);
                },
                callbacks,
                full_repo_path,
                protocol_path_owned,
                query_string,
                content_type,
                reader,
            )
            .await
            .into_http_response()
        }
        None => HttpResponse::BadRequest().body("Path doesn't look like a git URL"),
    }
}
