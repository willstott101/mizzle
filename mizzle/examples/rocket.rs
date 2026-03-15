use mizzle::servers::rocket::{handle_git_request, GitRequestMeta, RocketGitResponse};
use mizzle::traits::{PushRef, RepoAccess};
use rocket::data::ToByteUnit;
use rocket::tokio::io::AsyncReadExt;
use rocket::{get, post, routes, Data, State};

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

fn check_auth(req: &rocket::Request<'_>) -> Option<RocketGitResponse> {
    let token = req.headers().get_one("Authorization");
    if token != Some("Bearer secret") {
        Some(RocketGitResponse::error(401, "unauthorized"))
    } else {
        None
    }
}

#[get("/<path..>")]
async fn git_get(
    path: std::path::PathBuf,
    meta: GitRequestMeta,
    config: &State<Config>,
    req: &rocket::Request<'_>,
) -> RocketGitResponse {
    if let Some(err) = check_auth(req) {
        return err;
    }
    let access = Access {
        repo_path: config.repo_path.clone(),
    };
    handle_git_request(access, &path.to_string_lossy(), meta, Box::pin(futures_lite::io::empty())).await
}

#[post("/<path..>", data = "<data>")]
async fn git_post(
    path: std::path::PathBuf,
    meta: GitRequestMeta,
    config: &State<Config>,
    data: Data<'_>,
    req: &rocket::Request<'_>,
) -> RocketGitResponse {
    if let Some(err) = check_auth(req) {
        return err;
    }
    let access = Access {
        repo_path: config.repo_path.clone(),
    };
    let mut buf = Vec::new();
    let _ = data.open(512.mebibytes()).read_to_end(&mut buf).await;
    let reader = Box::pin(futures_lite::io::Cursor::new(buf));
    handle_git_request(access, &path.to_string_lossy(), meta, reader).await
}

#[rocket::main]
async fn main() -> Result<(), rocket::Error> {
    let _ = rocket::build()
        .manage(Config {
            repo_path: ".".to_string(),
        })
        .mount("/", routes![git_get, git_post])
        .launch()
        .await?;
    Ok(())
}
