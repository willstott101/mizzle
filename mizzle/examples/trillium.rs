use mizzle::servers::trillium::serve;
use mizzle::traits::{PushRef, RepoAccess};
use simple_logger::SimpleLogger;
use trillium::Conn;
use trillium::State;
use trillium_smol;

#[derive(Clone)]
struct Config {
    repo_path: String,
}

struct Access {
    repo_path: String,
}

impl RepoAccess for Access {
    fn repo_path(&self) -> &str {
        &self.repo_path
    }

    fn authorize_push(&self, refs: &[PushRef<'_>]) -> Result<(), String> {
        for r in refs {
            if !r.refname.starts_with("refs/heads/") {
                return Err(format!("pushes to {} are not allowed", r.refname));
            }
        }
        Ok(())
    }
}

async fn git_handler(conn: Conn) -> Conn {
    let token = conn.request_headers().get_str("Authorization");
    if token != Some("Bearer secret") {
        return conn
            .with_status(trillium::Status::Unauthorized)
            .with_body("unauthorized")
            .halt();
    }

    let config = conn.state::<Config>().unwrap();
    let access = Access {
        repo_path: config.repo_path.clone(),
    };
    serve(access, conn).await
}

fn main() {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init()
        .unwrap();

    let config = Config {
        repo_path: ".".to_string(),
    };

    trillium_smol::run((State::new(config), git_handler));
}
