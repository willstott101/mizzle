mod fetch;
mod ls_refs;
mod utils;
mod serve;

use simple_logger::SimpleLogger;
use trillium_smol;

fn main() {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init()
        .unwrap();

    // port 8080
    trillium_smol::run(|conn: trillium::Conn| async move {
        if conn
            .headers()
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
                serve::serve_git_protocol_2(conn, repo_path_owned, protocol_path_owned).await
            }
            None => conn
                .with_status(trillium::Status::BadRequest)
                .with_body("Path doesn't look like a git URL")
                .halt(),
        }
    });
}
