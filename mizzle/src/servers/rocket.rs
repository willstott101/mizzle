use rocket::http::{ContentType, Header, Status};
use rocket::request::{FromRequest, Outcome, Request};
use rocket::response::{self, Responder, Response};
use std::io::Cursor;
use std::pin::Pin;
use std::sync::Arc;

use crate::{
    serve::{serve_git_protocol_2, GitResponse},
    traits::GitServerCallbacks,
};

/// Drop-in `Responder` wrapper around `GitResponse`.
pub struct RocketGitResponse(pub GitResponse);

impl<'r> Responder<'r, 'static> for RocketGitResponse {
    fn respond_to(self, _req: &'r Request<'_>) -> response::Result<'static> {
        use tokio_util::compat::FuturesAsyncReadCompatExt;

        let r = self.0;
        let status = Status::from_code(r.status_code).unwrap_or(Status::InternalServerError);
        let mut builder = Response::build();
        builder.status(status);
        builder.header(Header::new("Cache-Control", "no-cache"));
        if let Some(ct) = r.content_type {
            if let Ok(parsed) = ct.parse::<ContentType>() {
                builder.header(parsed);
            }
        }
        match r.reader {
            Some(reader) => {
                builder.streamed_body(reader.compat());
            }
            None => {
                let body = r.body.unwrap_or_default();
                builder.sized_body(body.len(), Cursor::new(body));
            }
        }
        builder.ok()
    }
}

/// Request guard that extracts git-relevant metadata in one shot.
pub struct GitRequestMeta {
    pub query_string: Box<str>,
    pub content_type: Box<str>,
    pub git_protocol: Box<str>,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for GitRequestMeta {
    type Error = ();

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, ()> {
        Outcome::Success(GitRequestMeta {
            query_string: req
                .uri()
                .query()
                .map(|q| q.as_str())
                .unwrap_or("")
                .into(),
            content_type: req
                .content_type()
                .map(|ct| ct.to_string())
                .unwrap_or_default()
                .into(),
            git_protocol: req
                .headers()
                .get_one("Git-Protocol")
                .unwrap_or("version=2")
                .into(),
        })
    }
}

type BoxBody = Pin<Box<dyn futures_lite::AsyncRead + Send + Unpin>>;

/// Core handler logic — call this from your concrete `#[get]` / `#[post]` routes.
///
/// For GET requests pass `futures_lite::io::empty()` (boxed) as `body`.
/// For POST requests open the `Data` with your size limit and pass the reader.
pub async fn handle_git_request<T: GitServerCallbacks + Send + Sync + 'static>(
    path: &str,
    meta: GitRequestMeta,
    config: Arc<T>,
    body: BoxBody,
) -> RocketGitResponse {
    if meta.git_protocol.as_ref() != "version=2" {
        return RocketGitResponse(GitResponse {
            status_code: 501,
            content_type: None,
            reader: None,
            body: Some("Only Git Protocol 2 is supported".to_string()),
        });
    }

    let Some((git_repo_path, service_path)) = path.rsplit_once(".git/") else {
        return RocketGitResponse(GitResponse {
            status_code: 400,
            content_type: None,
            reader: None,
            body: Some("Path doesn't look like a git URL".to_string()),
        });
    };

    let repo_path_owned: Box<str> = git_repo_path.into();
    let protocol_path_owned: Box<str> = service_path.into();
    let full_repo_path = config.auth(repo_path_owned.as_ref());

    RocketGitResponse(
        serve_git_protocol_2(
            |fut| {
                tokio::spawn(fut);
            },
            config,
            full_repo_path,
            protocol_path_owned,
            meta.query_string,
            meta.content_type,
            body,
        )
        .await,
    )
}
