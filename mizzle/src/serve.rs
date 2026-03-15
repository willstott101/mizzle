use crate::traits::RepoAccess;
use crate::{fetch, ls_refs, receive};
use anyhow::{Context, Error, Result};
use futures_lite::AsyncRead;
use gix_packetline::async_io::encode::{flush_to_write, text_to_write};
use gix_packetline::async_io::StreamingPeekableIter;
use gix_packetline::PacketLineRef;
use log::{error, info};
use piper::{Reader, Writer};
use std::future::Future;
use std::pin::Pin;

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
                    body: Some(format!("internal error: {:#}", error)),
                    status_code: 500,
                    content_type: Some("text/plain".to_string()),
                };
            }
        }
    };
}

pub type SpawnFut = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub async fn serve_git_protocol_2<T, A, S>(
    spawn: S,
    access: A,
    protocol_path: Box<str>,
    query_string: Box<str>,
    content_type: Box<str>,
    body: T,
) -> GitResponse
where
    T: AsyncRead + Unpin,
    A: RepoAccess + Send,
    S: Fn(SpawnFut),
{
    let repo_path: Box<str> = access.repo_path().into();

    info!("GIT {} {}", repo_path, protocol_path);

    // Receive-pack discovery: GET /info/refs?service=git-receive-pack
    if protocol_path.as_ref() == "info/refs"
        && query_string
            .split('&')
            .any(|kv| kv == "service=git-receive-pack")
    {
        let (reader, writer) = piper::pipe(4096);
        let rp = repo_path.clone();
        spawn(Box::pin(async move {
            let mut w = writer;
            text_to_write(b"# service=git-receive-pack", &mut w)
                .await
                .expect("write");
            flush_to_write(&mut w).await.expect("flush");
            receive::info_refs_receive_pack_task(rp, w).await;
        }));
        return GitResponse {
            status_code: 200,
            content_type: Some("application/x-git-receive-pack-advertisement".to_string()),
            reader: Some(reader),
            body: None,
        };
    }

    // Upload-pack discovery: GET /info/refs
    if protocol_path.as_ref() == "info/refs" {
        let (reader, writer) = piper::pipe(4096);
        spawn(Box::pin(info_refs_task(writer)));
        return GitResponse {
            status_code: 200,
            content_type: Some("application/x-git-upload-pack-advertisement".to_string()),
            reader: Some(reader),
            body: None,
        };
    }

    // Receive-pack: POST /git-receive-pack
    if protocol_path.as_ref() == "git-receive-pack" {
        if content_type.as_ref() != "application/x-git-receive-pack-request" {
            return GitResponse {
                status_code: 400,
                content_type: None,
                reader: None,
                body: Some(
                    "Expected content type application/x-git-receive-pack-request".to_string(),
                ),
            };
        }

        let (ref_updates, pack_data) = res_try!(receive::read_receive_request(body).await);

        // Preliminary auth check before touching the disk.  Create and Delete
        // are definitive; FastForward is optimistic (may be upgraded to
        // ForcePush once the pack is in the odb).  This lets us reject cheap
        // denials without writing anything.
        let preliminary_refs: Vec<crate::traits::PushRef<'_>> = ref_updates
            .iter()
            .map(|u| crate::traits::PushRef {
                refname: &u.refname,
                kind: receive::preliminary_push_kind(u),
            })
            .collect();
        if let Err(msg) = access.authorize_push(&preliminary_refs) {
            let (reader, writer) = piper::pipe(4096);
            spawn(Box::pin(async move {
                let mut w = writer;
                text_to_write(b"unpack ok", &mut w).await.unwrap();
                for update in &ref_updates {
                    let line = format!("ng {} {}", update.refname, msg);
                    text_to_write(line.as_bytes(), &mut w).await.unwrap();
                }
                flush_to_write(&mut w).await.unwrap();
            }));
            return GitResponse {
                status_code: 200,
                content_type: Some("application/x-git-receive-pack-result".to_string()),
                reader: Some(reader),
                body: None,
            };
        }

        // Write the pack into objects/pack/ via a temp dir so the move is
        // atomic.  We get back the file paths so we can delete them if the
        // final auth check fails — rejected pushes won't leave orphaned objects.
        let written_pack = if !pack_data.is_empty() {
            res_try!(receive::write_pack(repo_path.as_ref(), &pack_data))
        } else {
            None
        };

        // Now that the pack is in the odb we can classify every ref correctly,
        // including distinguishing FastForward from ForcePush.
        let repo = res_try!(gix::open(repo_path.as_ref()));
        let odb = repo.objects;
        let push_refs: Vec<crate::traits::PushRef<'_>> = ref_updates
            .iter()
            .map(|u| crate::traits::PushRef {
                refname: &u.refname,
                kind: receive::compute_push_kind(odb.clone().into_inner(), u),
            })
            .collect();
        let rejected: Vec<(String, String)> = match access.authorize_push(&push_refs) {
            Err(msg) => {
                if let Some(pack) = written_pack {
                    pack.delete();
                }
                ref_updates
                    .iter()
                    .map(|u| (u.refname.clone(), msg.clone()))
                    .collect()
            }
            Ok(()) => Vec::new(),
        };

        let (reader, writer) = piper::pipe(4096);

        if !rejected.is_empty() {
            spawn(Box::pin(async move {
                let mut w = writer;
                text_to_write(b"unpack ok", &mut w).await.unwrap();
                for (refname, msg) in rejected {
                    let line = format!("ng {} {}", refname, msg);
                    text_to_write(line.as_bytes(), &mut w).await.unwrap();
                }
                flush_to_write(&mut w).await.unwrap();
            }));
        } else {
            spawn(Box::pin(async move {
                receive::update_refs_and_report(repo_path.as_ref(), ref_updates, writer)
                    .await
                    .unwrap();
            }));
        }

        return GitResponse {
            status_code: 200,
            content_type: Some("application/x-git-receive-pack-result".to_string()),
            reader: Some(reader),
            body: None,
        };
    }

    // Upload-pack: POST /git-upload-pack
    if protocol_path.as_ref() == "git-upload-pack" {
        if content_type.as_ref() != "application/x-git-upload-pack-request" {
            return GitResponse {
                status_code: 400,
                content_type: Some("application/x-git-upload-pack-advertisement".to_string()),
                reader: None,
                body: Some(
                    "Expected content type application/x-git-upload-pack-request".to_string(),
                ),
            };
        }

        let mut parser = StreamingPeekableIter::new(body, &[], false);
        let command = res_try!(read_command(&mut parser).await);
        match command {
            Command::ListRefs => {
                let args = res_try!(ls_refs::read_lsrefs_args(&mut parser).await);
                let repo = res_try!(gix::open(repo_path.as_ref())).into_sync();
                let (reader, writer) = piper::pipe(4096);
                spawn(Box::pin(async move {
                    ls_refs::perform_listrefs(&repo, &args, writer)
                        .await
                        .unwrap();
                }));
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
                let repo = res_try!(gix::open(repo_path.as_ref()));
                let (reader, writer) = piper::pipe(4096);
                spawn(Box::pin(async move {
                    fetch::perform_fetch(repo.objects, &args, writer)
                        .await
                        .unwrap();
                }));
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
}

async fn info_refs_task(mut writer: Writer) {
    text_to_write(b"# service=git-upload-pack", &mut writer)
        .await
        .expect("to write to output");
    flush_to_write(&mut writer)
        .await
        .expect("to write to output");

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
