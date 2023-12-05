use trillium_smol;
use trillium::{Conn, conn_try};
use simple_logger::SimpleLogger;
use log::info;
use anyhow::Result;
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
        // We also expect a query parameter of ?service=git-upload-pack but I don't see a reason to check for it.

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
            let lines = conn_try!(read_message_as_unicode_lines(&mut conn).await, conn);
            info!("LINES: {:?}", lines);
        }
        conn
    } else {
        conn
    }
}

async fn read_message_as_unicode_lines(conn: &mut trillium::Conn) -> Result<Vec<Option<String>>> {
    let mut parser = StreamingPeekableIter::new(conn.request_body().await, &[]);
    let mut v = Vec::new();
    loop {
        let line = parser.read_line().await;
        match line {
            Some(found_line) => {
                let parsed_line = found_line??;
                if matches!(parsed_line, PacketLineRef::ResponseEnd | PacketLineRef::Flush) {
                    break;
                }
                match parsed_line.as_bstr() {
                    Some(bstr) => v.push(Some(String::from_utf8(bstr.strip_suffix(b"\n").unwrap_or(bstr).to_owned())?)),
                    None => v.push(None),
                }
            },
            None => {
                break
            },
        }
    }
    return Ok(v);
}
