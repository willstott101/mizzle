use trillium_smol;
use trillium::Conn;
use simple_logger::SimpleLogger;
use log::info;
use form_urlencoded;
use std::collections::HashMap;

const NAME: &str = "mizzle";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    SimpleLogger::new().init().unwrap();

    // port 8080
    trillium_smol::run(|mut conn: trillium::Conn| async move {
        if conn.headers().get_str("Git-Protocol").unwrap_or("version=2") != "version=2" {
            println!("Only Git Protocol 2 is supported");
            return conn.with_status(trillium::Status::NotImplemented).with_body("Only Git Protocol 2 is supported").halt();
        }

        let result = conn.path().rsplit_once(".git/");
        match result {
            Some((git_repo_path, service_path)) => {
                let repo_path_owned: Box<str> = git_repo_path.into();
                let protocol_path_owned: Box<str> = service_path.into();
                serve_git_protocol_2(conn, repo_path_owned, protocol_path_owned).await
            },
            None => {
                conn.with_status(trillium::Status::BadRequest).with_body("Path doesn't look like a git URL").halt()
            },
        }
    });
}

async fn serve_git_protocol_2(mut conn: trillium::Conn, repo_path: Box<str>, protocol_path: Box<str>) -> Conn {
    // The git protocol recommends making sure to prevent any caching
    conn = conn.with_header(trillium::KnownHeaderName::CacheControl, "no-cache");

    // /info/refs

    // let params: HashMap<_, _> = form_urlencoded::parse(conn.querystring().as_bytes()).collect();
    // if params.get("service").map(|p| (*p).as_ref()) != Some("git-upload-pack") {
    //     println!("Only git-upload-pack is supported");
    //     return conn.with_status(501).with_body("Only git-upload-pack is supported").halt();
    // }

    // StreamingPeekableIter
    info!("REQUEST CONTENT TYPE {:?}", conn.headers().get_str("Content-Type"));
    info!("HTTP {} {} {}", conn.method(), conn.path(), conn.querystring());
    info!("GIT {} {}", repo_path, protocol_path);
    info!("RESPONDING {}", format!("000eversion 2
agent={}/{}
0000", NAME, VERSION));

    // Missing leading length i think
    // use gix-packetline text_to_write

    // 001e# service=git-upload-pack
    // 0000000eversion 2
    // 0022agent=git/github-0ecc5b5f94fa
    // 0013ls-refs=unborn
    // 0027fetch=shallow wait-for-done filter
    // 0012server-option
    // 0017object-format=sha1
    // 0000

    conn.with_status(trillium::Status::Ok).with_body(format!("000eversion 2\nagent={}/{}\n0000", NAME, VERSION))

    // info!("BODY: {:#?}", conn.request_body_string().await);
}
