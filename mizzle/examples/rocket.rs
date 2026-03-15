use mizzle::servers::rocket::{handle_git_request, GitRequestMeta, RocketGitResponse};
use mizzle::traits::GitServerCallbacks;
use rocket::data::ToByteUnit;
use rocket::tokio::io::AsyncReadExt;
use rocket::{get, post, routes, Data, State};
use std::sync::Arc;

#[derive(Clone)]
struct Config;

impl GitServerCallbacks for Config {
    fn auth(&self, _repo_path: &str) -> Box<str> {
        ".".into()
    }
}

#[get("/<path..>")]
async fn git_get(
    path: std::path::PathBuf,
    meta: GitRequestMeta,
    config: &State<Config>,
) -> RocketGitResponse {
    let config = Arc::new(config.inner().clone());
    let path_str = path.to_string_lossy().into_owned();
    handle_git_request(
        &path_str,
        meta,
        config,
        Box::pin(futures_lite::io::empty()),
    )
    .await
}

#[post("/<path..>", data = "<data>")]
async fn git_post(
    path: std::path::PathBuf,
    meta: GitRequestMeta,
    config: &State<Config>,
    data: Data<'_>,
) -> RocketGitResponse {
    let config = Arc::new(config.inner().clone());
    let path_str = path.to_string_lossy().into_owned();

    // Buffer the body to avoid tying the reader's lifetime to `data`.
    let mut buf = Vec::new();
    let _ = data.open(512.mebibytes()).read_to_end(&mut buf).await;
    let reader = Box::pin(futures_lite::io::Cursor::new(buf));

    handle_git_request(&path_str, meta, config, reader).await
}

#[rocket::main]
async fn main() -> Result<(), rocket::Error> {
    let _ = rocket::build()
        .manage(Config)
        .mount("/", routes![git_get, git_post])
        .launch()
        .await?;
    Ok(())
}
