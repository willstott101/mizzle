use trillium_smol;
use trillium::{Conn, conn_try};
use simple_logger::SimpleLogger;
use log::info;
// use form_urlencoded;
// use std::collections::HashMap;
use gix_packetline::encode::{text_to_write, flush_to_write};
use gix_packetline::{StreamingPeekableIter, PacketLineRef};

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

    info!("REQUEST CONTENT TYPE {:?}", conn.headers().get_str("Content-Type"));
    info!("HTTP {} {} {}", conn.method(), conn.path(), conn.querystring());
    info!("GIT {} {}", repo_path, protocol_path);

    if protocol_path.as_ref() == "info/refs" {
        // let params: HashMap<_, _> = form_urlencoded::parse(conn.querystring().as_bytes()).collect();
        // if params.get("service").map(|p| (*p).as_ref()) != Some("git-upload-pack") {
        //     println!("Only git-upload-pack is supported");
        //     return conn.with_status(501).with_body("Only git-upload-pack is supported").halt();
        // }

        // Part of the V2 handshake
        conn = conn.with_header(trillium::KnownHeaderName::ContentType, "application/x-git-upload-pack-advertisement");
        let (reader, mut writer) = piper::pipe(4096);
        trillium_smol::spawn((|| async move {
            // Copied from github
            text_to_write(b"# service=git-upload-pack", &mut writer).await.expect("to write to output");
            flush_to_write(&mut writer).await.expect("to write to output");

            // Understood in the spec
            text_to_write(b"version 2", &mut writer).await.expect("to write to output");
            text_to_write(b"agent=mizzle/dev", &mut writer).await.expect("to write to output");

            // Copied from github
            text_to_write(b"ls-refs", &mut writer).await.expect("to write to output");
            // text_to_write(b"ls-refs=unborn", &mut writer).await.expect("to write to output");
            // text_to_write(b"fetch=shallow wait-for-done filter", &mut writer).await.expect("to write to output");
            // text_to_write(b"server-option", &mut writer).await.expect("to write to output");
            // text_to_write(b"object-format=sha1", &mut writer).await.expect("to write to output");

            // Understood in the spec
            flush_to_write(&mut writer).await.expect("to write to output");
        })());
        conn.with_status(trillium::Status::Ok).with_body(trillium::Body::new_streaming(reader, None)).halt()
    } else if protocol_path.as_ref() == "git-upload-pack" {
        if conn.headers().get_str(trillium::KnownHeaderName::ContentType) != Some("application/x-git-upload-pack-request") {
            return conn.with_status(trillium::Status::BadRequest).with_body("Expected content type application/x-git-upload-pack-request").halt();
        } else {
            info!("PARSING LINES");
            let mut parser = StreamingPeekableIter::new(conn.request_body().await, &[]);
            loop {
                let line = parser.read_line().await;
                match line {
                    Some(found_line) => {
                        let parsed_line = conn_try!(conn_try!(found_line, conn), conn);
                        if matches!(parsed_line, PacketLineRef::ResponseEnd | PacketLineRef::Flush) {
                            break;
                        }
                        info!("LINE: {:?}", parsed_line);
                        info!("LINE: {:#?}", parsed_line.as_bstr());
                    },
                    None => {
                        break
                    },
                }
            }
        }
        conn
    } else {
        conn
    }
}
