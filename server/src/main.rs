mod fetch;
mod ls_refs;
mod utils;

use anyhow::{Context, Error, Result};
use futures_lite::AsyncRead;
use gix_packetline::encode::{flush_to_write, text_to_write};
use gix_packetline::{PacketLineRef, StreamingPeekableIter};
use log::info;
use simple_logger::SimpleLogger;
use trillium::{conn_try, Conn};
use trillium_smol;

fn main() {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init()
        .unwrap();

    // port 8080
    trillium_smol::run(|conn: trillium::Conn| async move {
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
                serve_git_protocol_2(conn, repo_path_owned, protocol_path_owned).await
            }
            None => conn
                .with_status(trillium::Status::BadRequest)
                .with_body("Path doesn't look like a git URL")
                .halt(),
        }
    });
}

async fn serve_git_protocol_2(
    mut conn: trillium::Conn,
    repo_path: Box<str>,
    protocol_path: Box<str>,
) -> Conn {
    // The git protocol recommends making sure to prevent any caching
    conn = conn.with_header(trillium::KnownHeaderName::CacheControl, "no-cache");

    info!(
        "REQUEST CONTENT TYPE {:?}",
        conn.headers().get_str("Content-Type")
    );
    info!(
        "HTTP {} {} {}",
        conn.method(),
        conn.path(),
        conn.querystring()
    );
    info!("GIT {} {}", repo_path, protocol_path);

    if protocol_path.as_ref() == "info/refs" {
        // We also expect a query parameter of ?service=git-upload-pack but I don't see a reason to check for it.

        // Part of the V2 handshake
        conn = conn.with_header(
            trillium::KnownHeaderName::ContentType,
            "application/x-git-upload-pack-advertisement",
        );
        let (reader, mut writer) = piper::pipe(4096);
        trillium_smol::spawn((|| async move {
            // Copied from github
            text_to_write(b"# service=git-upload-pack", &mut writer)
                .await
                .expect("to write to output");
            flush_to_write(&mut writer)
                .await
                .expect("to write to output");

            // Understood in the spec
            text_to_write(b"version 2", &mut writer)
                .await
                .expect("to write to output");
            text_to_write(b"agent=mizzle/dev", &mut writer)
                .await
                .expect("to write to output");

            text_to_write(b"ls-refs=unborn", &mut writer)
                .await
                .expect("to write to output");
            text_to_write(b"fetch", &mut writer)
                .await
                .expect("to write to output");
            // Copied from github - yet to be implemented/confirmed
            // text_to_write(b"fetch=shallow wait-for-done filter", &mut writer).await.expect("to write to output");
            // text_to_write(b"server-option", &mut writer).await.expect("to write to output");
            // text_to_write(b"object-format=sha1", &mut writer).await.expect("to write to output");

            // Understood in the spec
            flush_to_write(&mut writer)
                .await
                .expect("to write to output");
        })());
        conn.with_status(trillium::Status::Ok)
            .with_body(trillium::Body::new_streaming(reader, None))
            .halt()
    } else if protocol_path.as_ref() == "git-upload-pack" {
        if conn
            .headers()
            .get_str(trillium::KnownHeaderName::ContentType)
            != Some("application/x-git-upload-pack-request")
        {
            return conn
                .with_status(trillium::Status::BadRequest)
                .with_body("Expected content type application/x-git-upload-pack-request")
                .halt();
        } else {
            // println!("{}", conn.request_body_string().await.unwrap());
            let mut parser = StreamingPeekableIter::new(conn.request_body().await, &[]);
            let command = conn_try!(read_command(&mut parser).await, conn);
            match command {
                Command::ListRefs => {
                    // println!("{}", conn.request_body_string().await.unwrap());
                    let args = conn_try!(ls_refs::read_lsrefs_args(&mut parser).await, conn);
                    // info!("LIST REFS ARGS: {:?}", args);
                    let repo = conn_try!(gix::open("."), conn).into_sync();
                    let (reader, writer) = piper::pipe(4096);
                    trillium_smol::spawn((|| async move {
                        ls_refs::perform_listrefs(&repo, &args, writer)
                            .await
                            .unwrap();
                    })());
                    return conn
                        .with_status(trillium::Status::Ok)
                        .with_body(trillium::Body::new_streaming(reader, None))
                        .halt();
                }
                Command::Empty => (),
                Command::Fetch => {
                    let args = conn_try!(fetch::read_fetch_args(&mut parser).await, conn);
                    info!("FETCH: {:?}", args);
                    // println!("{}", conn.request_body_string().await.unwrap());
                }
            }
        }
        conn
    } else {
        conn
    }
}

#[derive(Debug)]
enum Command {
    Fetch,
    ListRefs,
    Empty,
}

async fn read_command<T>(parser: &mut StreamingPeekableIter<T>) -> Result<Command>
where
    T: AsyncRead + Unpin,
{
    let line = parser
        .read_line()
        .await
        .context("no line when expecting command")???;
    if matches!(line, PacketLineRef::Flush) {
        return Ok(Command::Empty);
    }
    let bstr = line.as_bstr().context("no data when expecting command")?;
    let command = bstr
        .strip_suffix(b"\n")
        .unwrap_or(bstr)
        .strip_prefix(b"command=")
        .context("expected command")?;
    match command {
        b"ls-refs" => Ok(Command::ListRefs),
        b"fetch" => Ok(Command::Fetch),
        command_name => Err(Error::msg(format!(
            "unrecognised command: {:?}",
            command_name
        ))),
    }
}
