use trillium::Conn;

use crate::{
    serve::{serve_git_protocol_1, serve_git_protocol_2, GitResponse},
    traits::RepoAccess,
};

impl GitResponse {
    fn with_conn(self, mut conn: trillium::Conn) -> trillium::Conn {
        conn = conn.with_status(self.status_code);
        conn = conn.with_response_header(trillium::KnownHeaderName::CacheControl, "no-cache");
        if let Some(content_type) = self.content_type {
            conn = conn.with_response_header(trillium::KnownHeaderName::ContentType, content_type);
        }

        let conn = match self.reader {
            Some(reader) => conn.with_body(trillium::Body::new_streaming(reader, None)),
            None => conn.with_body(self.body.unwrap_or("".to_string())),
        };

        conn.halt()
    }
}

/// Serve a git request.  Call this from your own handler after performing
/// whatever authentication you need.
pub async fn serve<A: RepoAccess + Send + 'static>(access: A, mut conn: Conn) -> Conn {
    let git_protocol: String = conn
        .request_headers()
        .get_str("Git-Protocol")
        .unwrap_or("version=1")
        .to_string();

    let content_type: Box<str> = conn
        .request_headers()
        .get_str(trillium::KnownHeaderName::ContentType)
        .map(Into::into)
        .unwrap_or_else(|| "".into());

    let query_string: Box<str> = conn.querystring().into();
    let path = conn.path().to_string();

    let Some((_, service_path)) = path.rsplit_once(".git/") else {
        return conn
            .with_status(trillium::Status::BadRequest)
            .with_body("Path doesn't look like a git URL")
            .halt();
    };
    let protocol_path: Box<str> = service_path.into();
    let body = conn.request_body().await;

    let res = if git_protocol == "version=2" {
        serve_git_protocol_2(
            |fut| {
                trillium_smol::spawn(fut);
            },
            access,
            protocol_path,
            query_string,
            content_type,
            body,
        )
        .await
    } else {
        serve_git_protocol_1(
            |fut| {
                trillium_smol::spawn(fut);
            },
            access,
            protocol_path,
            query_string,
            content_type,
            body,
        )
        .await
    };
    res.with_conn(conn)
}
