use crate::{fetch, ls_refs};
use anyhow::{Context, Error, Result};
use futures_lite::AsyncRead;
use gix_packetline::async_io::encode::{flush_to_write, text_to_write};
use gix_packetline::async_io::StreamingPeekableIter;
use gix_packetline::PacketLineRef;
use log::{error, info};
use piper::{Reader, Writer};
use trillium_smol;

pub struct GitResponse {
    pub status_code: u16,
    pub content_type: Option<String>,
    pub reader: Option<Reader>,
    pub body: Option<String>,
}

#[macro_export]
macro_rules! res_try {
    ($expr:expr) => {
        match $expr {
            Ok(value) => value,
            Err(error) => {
                error!("{}:{} res_try error: {}", file!(), line!(), error);
                return GitResponse {
                    reader: None,
                    body: None,
                    status_code: 500,
                    content_type: None,
                };
            }
        }
    };
}

pub enum MizzleRuntime {
    Tokio,
    Smol,
}

pub async fn serve_git_protocol_2<T: AsyncRead + Unpin>(
    runtime: MizzleRuntime,
    repo_path: Box<str>,
    protocol_path: Box<str>,
    content_type: Box<str>,
    body: T,
) -> GitResponse {
    info!("GIT {} {}", repo_path, protocol_path);

    if protocol_path.as_ref() == "info/refs" {
        let (reader, writer) = piper::pipe(4096);

        match runtime {
            MizzleRuntime::Tokio => {
                tokio::spawn(info_refs_task(writer));
            }
            MizzleRuntime::Smol => {
                trillium_smol::spawn(info_refs_task(writer));
            }
        }
        GitResponse {
            status_code: 200,
            content_type: Some("application/x-git-upload-pack-advertisement".to_string()),
            reader: Some(reader),
            body: None,
        }
    } else if protocol_path.as_ref() == "git-upload-pack" {
        if content_type.as_ref() != "application/x-git-upload-pack-request" {
            return GitResponse {
                status_code: 400,
                content_type: Some("application/x-git-upload-pack-advertisement".to_string()),
                reader: None,
                body: Some(
                    "Expected content type application/x-git-upload-pack-request".to_string(),
                ),
            };
        } else {
            let mut parser = StreamingPeekableIter::new(body, &[], false);
            let command = res_try!(read_command(&mut parser).await);
            match command {
                Command::ListRefs => {
                    let args = res_try!(ls_refs::read_lsrefs_args(&mut parser).await);
                    // info!("LIST REFS ARGS: {:?}", args);
                    let repo = res_try!(gix::open(repo_path.as_ref())).into_sync();
                    let (reader, writer) = piper::pipe(4096);
                    match runtime {
                        MizzleRuntime::Tokio => {
                            tokio::spawn(async move {
                                ls_refs::perform_listrefs(&repo, &args, writer)
                                    .await
                                    .unwrap();
                            });
                        }
                        MizzleRuntime::Smol => {
                            trillium_smol::spawn(async move {
                                ls_refs::perform_listrefs(&repo, &args, writer)
                                    .await
                                    .unwrap();
                            });
                        }
                    }
                    return GitResponse {
                        status_code: 200,
                        content_type: None,
                        reader: Some(reader),
                        body: None,
                    };
                }
                Command::Empty => (),
                Command::Fetch => {
                    let args = res_try!(fetch::read_fetch_args(&mut parser).await);
                    info!("FETCH: {:?}", args);
                    // let repo = conn_try!(gix::open("."), conn).into_sync();
                    // let mut handle = repo.clone().objects.into_shared_arc().to_cache_arc();
                    let repo = res_try!(gix::open(repo_path.as_ref()));
                    // let handle = repo.objects;
                    let (reader, writer) = piper::pipe(4096);
                    match runtime {
                        MizzleRuntime::Tokio => {
                            tokio::spawn(async move {
                                // TODO: What exactly should we pass in here to
                                fetch::perform_fetch(repo.objects, &args, writer)
                                    .await
                                    .unwrap();
                            });
                        }
                        MizzleRuntime::Smol => {
                            trillium_smol::spawn(async move {
                                // TODO: What exactly should we pass in here to
                                fetch::perform_fetch(repo.objects, &args, writer)
                                    .await
                                    .unwrap();
                            });
                        }
                    }
                    return GitResponse {
                        status_code: 200,
                        content_type: None,
                        reader: Some(reader),
                        body: None,
                    };
                }
            }
        }
        GitResponse {
            status_code: 404,
            content_type: None,
            reader: None,
            body: None,
        }
    } else {
        GitResponse {
            status_code: 404,
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
