use crate::{fetch, ls_refs};
use anyhow::{Context, Error, Result};
use futures_lite::AsyncRead;
use gix_packetline::async_io::encode::{flush_to_write, text_to_write};
use gix_packetline::async_io::StreamingPeekableIter;
use gix_packetline::PacketLineRef;
use log::info;
use piper::{Reader, Writer};
use trillium::{conn_try, Conn};
use trillium_smol;

pub struct GitResponse {
    pub content_type: Option<String>,
    pub reader: Option<Reader>,
    pub body: Option<String>,
}

#[cfg(feature = "axum")]
pub async fn serve_git_protocol_2_2<T: AsyncRead + Unpin>(
    repo_path: Box<str>,
    protocol_path: Box<str>,
    content_type: Box<str>,
    body: T,
) -> GitResponse {
    info!("GIT {} {}", repo_path, protocol_path);

    if protocol_path.as_ref() == "info/refs" {
        let (reader, writer) = piper::pipe(4096);

        tokio::spawn(info_refs_task(writer));
        GitResponse {
            content_type: Some("application/x-git-upload-pack-advertisement".to_string()),
            reader: Some(reader),
            body: None,
        }
    } else if protocol_path.as_ref() == "git-upload-pack" {
        if content_type.as_ref() != "application/x-git-upload-pack-request" {
            return GitResponse {
                content_type: Some("application/x-git-upload-pack-advertisement".to_string()),
                reader: None,
                body: Some(
                    "Expected content type application/x-git-upload-pack-request".to_string(),
                ),
            };
        } else {
            let mut parser = StreamingPeekableIter::new(body, &[], false);
            let command = read_command(&mut parser).await.unwrap();
            match command {
                Command::ListRefs => {
                    let args = ls_refs::read_lsrefs_args(&mut parser).await.unwrap();
                    // info!("LIST REFS ARGS: {:?}", args);
                    let repo = gix::open(repo_path.as_ref()).unwrap().into_sync();
                    let (reader, writer) = piper::pipe(4096);
                    tokio::spawn(async move {
                        ls_refs::perform_listrefs(&repo, &args, writer)
                            .await
                            .unwrap();
                    });
                    return GitResponse {
                        content_type: None,
                        reader: Some(reader),
                        body: None,
                    };
                }
                Command::Empty => (),
                Command::Fetch => {
                    let args = fetch::read_fetch_args(&mut parser).await.unwrap();
                    info!("FETCH: {:?}", args);
                    // let repo = conn_try!(gix::open("."), conn).into_sync();
                    // let mut handle = repo.clone().objects.into_shared_arc().to_cache_arc();
                    let repo = gix::open(repo_path.as_ref()).unwrap();
                    // let handle = repo.objects;
                    let (reader, writer) = piper::pipe(4096);
                    tokio::spawn(async move {
                        // TODO: What exactly should we pass in here to
                        fetch::perform_fetch(repo.objects, &args, writer)
                            .await
                            .unwrap();
                    });
                    return GitResponse {
                        content_type: None,
                        reader: Some(reader),
                        body: None,
                    };
                }
            }
        }
        GitResponse {
            content_type: None,
            reader: None,
            body: None,
        }
    } else {
        GitResponse {
            content_type: None,
            reader: None,
            body: None,
        }
    }
}

async fn info_refs_task(mut writer: Writer) {
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
}

pub async fn serve_git_protocol_2(
    mut conn: trillium::Conn,
    repo_path: Box<str>,
    protocol_path: Box<str>,
) -> Conn {
    // The git protocol recommends making sure to prevent any caching
    conn = conn.with_response_header(trillium::KnownHeaderName::CacheControl, "no-cache");

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
        conn = conn.with_response_header(
            trillium::KnownHeaderName::ContentType,
            "application/x-git-upload-pack-advertisement",
        );
        let (reader, writer) = piper::pipe(4096);

        trillium_smol::spawn(info_refs_task(writer));

        conn.with_status(trillium::Status::Ok)
            .with_body(trillium::Body::new_streaming(reader, None))
            .halt()
    } else if protocol_path.as_ref() == "git-upload-pack" {
        if conn
            .request_headers()
            .get_str(trillium::KnownHeaderName::ContentType)
            != Some("application/x-git-upload-pack-request")
        {
            return conn
                .with_status(trillium::Status::BadRequest)
                .with_body("Expected content type application/x-git-upload-pack-request")
                .halt();
        } else {
            // println!("{}", conn.request_body_string().await.unwrap());
            let mut parser = StreamingPeekableIter::new(conn.request_body().await, &[], false);
            let command = conn_try!(read_command(&mut parser).await, conn);
            match command {
                Command::ListRefs => {
                    // println!("{}", conn.request_body_string().await.unwrap());
                    let args = conn_try!(ls_refs::read_lsrefs_args(&mut parser).await, conn);
                    // info!("LIST REFS ARGS: {:?}", args);
                    let repo = conn_try!(gix::open(repo_path.as_ref()), conn).into_sync();
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
                    // let repo = conn_try!(gix::open("."), conn).into_sync();
                    // let mut handle = repo.clone().objects.into_shared_arc().to_cache_arc();
                    let repo = conn_try!(gix::open(repo_path.as_ref()), conn);
                    // let handle = repo.objects;
                    let (reader, writer) = piper::pipe(4096);
                    trillium_smol::spawn((|| async move {
                        // TODO: What exactly should we pass in here to
                        fetch::perform_fetch(repo.objects, &args, writer)
                            .await
                            .unwrap();
                    })());
                    return conn
                        .with_status(trillium::Status::Ok)
                        .with_body(trillium::Body::new_streaming(reader, None))
                        .halt();
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
