mod fetch;
mod ls_refs;
mod serve;
mod utils;

use simple_logger::SimpleLogger;
use trillium_smol;

pub trait GitServerCallbacks {
    fn auth(&self, repo_path: &str) -> Box<str>;
}

#[derive(Clone)]
struct Config {}

impl GitServerCallbacks for Config {
    fn auth(&self, repo_path: &str) -> Box<str> {
        // TODO: check if user has access to this repo

        let repo_root = ".";

        format!("{}/{}", repo_root, repo_path).into()
    }
}

fn main() {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init()
        .unwrap();

    let config = Config {};

    // port 8080
    trillium_smol::run(move |conn: trillium::Conn| {
        let config = config.clone();
        async move {
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
                    let full_repo_path = config.auth(repo_path_owned.as_ref());
                    serve::serve_git_protocol_2(conn, full_repo_path, protocol_path_owned).await
                }
                None => conn
                    .with_status(trillium::Status::BadRequest)
                    .with_body("Path doesn't look like a git URL")
                    .halt(),
            }
        }
    });
}
