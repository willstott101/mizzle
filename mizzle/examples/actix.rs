use actix_web::{web, App, HttpRequest, HttpServer};
use log::info;
use mizzle::servers::actix::serve;
use mizzle::traits::{PushRef, RepoAccess};
use simple_logger::SimpleLogger;

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

async fn git_handler(
    req: HttpRequest,
    payload: web::Payload,
    config: web::Data<Config>,
) -> actix_web::HttpResponse {
    let token = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok());
    if token != Some("Bearer secret") {
        return actix_web::HttpResponse::Unauthorized().body("unauthorized");
    }

    let access = Access {
        repo_path: config.repo_path.clone(),
    };
    serve(access, req, payload).await
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init()
        .unwrap();

    info!("Server running at http://0.0.0.0:8080");

    HttpServer::new(|| {
        App::new()
            .app_data(web::Data::new(Config {
                repo_path: ".".to_string(),
            }))
            .route("/{tail:.*}", web::get().to(git_handler))
            .route("/{tail:.*}", web::post().to(git_handler))
    })
    .bind("0.0.0.0:8080")?
    .run()
    .await
}
