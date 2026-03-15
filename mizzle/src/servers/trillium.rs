use trillium::Conn;

use crate::{
    serve::{serve_git_protocol_2, GitResponse},
    traits::GitServerCallbacks,
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

pub async fn trillium_handler<T: GitServerCallbacks + Send + Sync + 'static>(
    mut conn: trillium::Conn,
) -> Conn {
    let config = conn.state::<T>().unwrap();
    if conn
        .request_headers()
        .get_str("Git-Protocol")
        .unwrap_or("version=2")
        != "version=2"
    {
        println!("Only Git Protocol 2 is supported");
        return conn
            .with_status(trillium::Status::NotImplemented)
            .with_body("Only Git Protocol 2 is supported")
            .halt();
    }

    let content_type = match conn
        .request_headers()
        .get_str(trillium::KnownHeaderName::ContentType)
    {
        Some(header) => header.into(),
        None => "".into(),
    };

    let result = conn.path().rsplit_once(".git/");
    match result {
        Some((git_repo_path, service_path)) => {
            let repo_path_owned: Box<str> = git_repo_path.into();
            let protocol_path_owned: Box<str> = service_path.into();
            let query_string: Box<str> = conn.querystring().into();
            let full_repo_path = config.auth(repo_path_owned.as_ref());
            let callbacks = std::sync::Arc::new(config.clone());
            let res = serve_git_protocol_2(
                |fut| {
                    trillium_smol::spawn(fut);
                },
                callbacks,
                full_repo_path,
                protocol_path_owned,
                query_string,
                content_type,
                conn.request_body().await,
            )
            .await;

            res.with_conn(conn)
        }
        None => conn
            .with_status(trillium::Status::BadRequest)
            .with_body("Path doesn't look like a git URL")
            .halt(),
    }
}
