use actix_web::{web, App, HttpServer};
use log::info;
use mizzle::servers::actix::actix_handler;
use mizzle::traits::GitServerCallbacks;
use simple_logger::SimpleLogger;

#[derive(Clone)]
struct Config;

impl GitServerCallbacks for Config {
    fn auth(&self, _repo_path: &str) -> Box<str> {
        ".".into()
    }
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
            .app_data(web::Data::new(Config))
            .route("/{tail:.*}", web::get().to(actix_handler::<Config>))
            .route("/{tail:.*}", web::post().to(actix_handler::<Config>))
    })
    .bind("0.0.0.0:8080")?
    .run()
    .await
}
