use trillium_smol;
use trillium::Conn;
use simple_logger::SimpleLogger;
use log::info;
// use form_urlencoded;
// use std::collections::HashMap;
use gix_packetline::encode::{text_to_write, flush_to_write};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    SimpleLogger::new().init().unwrap();

    // port 8080
    trillium_smol::run(|conn: trillium::Conn| async move {
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

    let (reader, mut writer) = piper::pipe(4096);
    trillium_smol::spawn((|| async move {
        // Copied from github
        text_to_write(b"# service=git-upload-pack", &mut writer).await.expect("to write to output");
        flush_to_write(&mut writer).await.expect("to write to output");

        // Understood in the spec
        text_to_write(b"version 2", &mut writer).await.expect("to write to output");
        text_to_write(b"agent=mizzle/dev", &mut writer).await.expect("to write to output");

        // Copied from github
        text_to_write(b"ls-refs=unborn", &mut writer).await.expect("to write to output");
        text_to_write(b"fetch=shallow wait-for-done filter", &mut writer).await.expect("to write to output");
        text_to_write(b"server-option", &mut writer).await.expect("to write to output");
        text_to_write(b"object-format=sha1", &mut writer).await.expect("to write to output");

        // Understood in the spec
        flush_to_write(&mut writer).await.expect("to write to output");
    })());

    // 001e# service=git-upload-pack
    // 0000000eversion 2
    // 0022agent=git/github-0ecc5b5f94fa
    // 0013ls-refs=unborn
    // 0027fetch=shallow wait-for-done filter
    // 0012server-option
    // 0017object-format=sha1
    // 0000

    conn.with_status(trillium::Status::Ok).with_body(trillium::Body::new_streaming(reader, None)).halt()
    // conn.with_status(trillium::Status::Ok).with_body(format!("000eversion 2\nagent={}/{}\n0000", NAME, VERSION))

    // info!("BODY: {:#?}", conn.request_body_string().await);
}
