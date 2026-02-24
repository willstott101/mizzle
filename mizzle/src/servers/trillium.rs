use trillium::Conn;

use crate::{serve::serve_git_protocol_2, traits::GitServerCallbacks};

pub async fn trillium_handler<T: GitServerCallbacks + Send + Sync + 'static>(
    conn: trillium::Conn,
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

    let result = conn.path().rsplit_once(".git/");
    match result {
        Some((git_repo_path, service_path)) => {
            let repo_path_owned: Box<str> = git_repo_path.into();
            let protocol_path_owned: Box<str> = service_path.into();
            let full_repo_path = config.auth(repo_path_owned.as_ref());
            serve_git_protocol_2(conn, full_repo_path, protocol_path_owned).await
        }
        None => conn
            .with_status(trillium::Status::BadRequest)
            .with_body("Path doesn't look like a git URL")
            .halt(),
    }
}
