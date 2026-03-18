use crate::traits::RepoAccess;
use crate::{fetch, ls_refs, receive};
use futures_lite::AsyncRead;
use gix::ObjectId;
use gix_packetline::async_io::encode::{flush_to_write, text_to_write};
use gix_packetline::async_io::StreamingPeekableIter;
use log::{error, info};
use crate::command::{read_command, Command};
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

/// Shared receive-pack info/refs response (protocol-version-agnostic).
fn recv_pack_info_refs<A: RepoAccess + Send + 'static>(
    spawn: &impl Fn(SpawnFut),
    access: &A,
    repo_path: &str,
) -> GitResponse {
    if access.auto_init() {
        res_try!(receive::init_bare_if_missing(repo_path));
    }
    let refs = res_try!(receive::gather_receive_pack_refs(repo_path));
    let (reader, writer) = piper::pipe(4096);
    spawn(Box::pin(async move {
        let mut w = writer;
        if text_to_write(b"# service=git-receive-pack", &mut w)
            .await
            .is_ok()
            && flush_to_write(&mut w).await.is_ok()
        {
            receive::info_refs_receive_pack_task(refs, w).await;
        }
    }));
    GitResponse {
        status_code: 200,
        content_type: Some("application/x-git-receive-pack-advertisement".to_string()),
        reader: Some(reader),
        body: None,
    }
}

/// Shared receive-pack POST response (protocol-version-agnostic).
async fn recv_pack_post<T, A>(
    spawn: impl Fn(SpawnFut),
    access: A,
    repo_path: Box<str>,
    body: T,
) -> GitResponse
where
    T: AsyncRead + Unpin,
    A: RepoAccess + Send + 'static,
{
    let (ref_updates, pack_data) = res_try!(receive::read_receive_request(body).await);

    // Preliminary auth check before touching the disk.
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

    let written_pack = if !pack_data.is_empty() {
        res_try!(receive::write_pack(repo_path.as_ref(), &pack_data))
    } else {
        None
    };

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
            let result: anyhow::Result<()> = async {
                text_to_write(b"unpack ok", &mut w).await?;
                for (refname, msg) in rejected {
                    let line = format!("ng {} {}", refname, msg);
                    text_to_write(line.as_bytes(), &mut w).await?;
                }
                flush_to_write(&mut w).await?;
                Ok(())
            }
            .await;
            if let Err(e) = result {
                error!("auth rejection write error: {:#}", e);
            }
        }));
    } else {
        let owned_kinds: Vec<(String, crate::traits::PushKind)> = push_refs
            .iter()
            .map(|pr| (pr.refname.to_string(), pr.kind.clone()))
            .collect();
        spawn(Box::pin(async move {
            match receive::update_refs_and_report(repo_path.as_ref(), &ref_updates, writer).await {
                Ok(()) => {
                    let post_refs: Vec<crate::traits::PushRef<'_>> = owned_kinds
                        .iter()
                        .map(|(name, kind)| crate::traits::PushRef {
                            refname: name.as_str(),
                            kind: kind.clone(),
                        })
                        .collect();
                    access.post_receive(&post_refs).await;
                }
                Err(e) => error!("update_refs_and_report error: {:#}", e),
            }
        }));
    }

    GitResponse {
        status_code: 200,
        content_type: Some("application/x-git-receive-pack-result".to_string()),
        reader: Some(reader),
        body: None,
    }
}

/// List all refs (HEAD + refs/*) for a protocol v1 upload-pack advertisement.
fn gather_upload_pack_v1_refs(repo_path: &str) -> anyhow::Result<Vec<(ObjectId, String)>> {
    let repo = gix::open(repo_path)?;
    let mut head: Vec<(ObjectId, String)> = Vec::new();
    let mut refs: Vec<(ObjectId, String)> = Vec::new();
    for r in repo.references()?.all()? {
        let mut r = r.map_err(|e| anyhow::anyhow!("{e}"))?;
        let name = r.name().as_bstr().to_string();
        if let Ok(id) = r.peel_to_id() {
            if name == "HEAD" {
                head.push((id.detach(), name));
            } else if name.starts_with("refs/") {
                refs.push((id.detach(), name));
            }
        }
    }
    // HEAD first so the capabilities NUL goes on the first line.
    head.extend(refs);
    Ok(head)
}

async fn info_refs_upload_pack_v1_task(refs: Vec<(ObjectId, String)>, mut writer: Writer) {
    let caps = b"side-band-64k ofs-delta shallow filter agent=mizzle/dev";
    let result: anyhow::Result<()> = async {
        text_to_write(b"# service=git-upload-pack", &mut writer).await?;
        flush_to_write(&mut writer).await?;
        if refs.is_empty() {
            // Empty repo: capabilities line with null OID.
            let mut line = b"0000000000000000000000000000000000000000 capabilities^{}".to_vec();
            line.push(0);
            line.extend_from_slice(caps);
            text_to_write(&line, &mut writer).await?;
        } else {
            let mut first = true;
            for (oid, name) in &refs {
                let mut line = Vec::new();
                line.extend_from_slice(oid.to_hex().to_string().as_bytes());
                line.push(b' ');
                line.extend_from_slice(name.as_bytes());
                if first {
                    line.push(0);
                    line.extend_from_slice(caps);
                    first = false;
                }
                text_to_write(&line, &mut writer).await?;
            }
        }
        flush_to_write(&mut writer).await?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        error!("info_refs_upload_pack_v1_task error: {:#}", e);
    }
}

/// Serve a git request using protocol v1 (for clients that do not send
/// `Git-Protocol: version=2`).  Handles upload-pack in v1 format; receive-pack
/// is protocol-version-agnostic and is handled identically to v2.
pub async fn serve_git_protocol_1<T, A, S>(
    spawn: S,
    access: A,
    protocol_path: Box<str>,
    query_string: Box<str>,
    content_type: Box<str>,
    body: T,
) -> GitResponse
where
    T: AsyncRead + Unpin,
    A: RepoAccess + Send + 'static,
    S: Fn(SpawnFut),
{
    let repo_path: Box<str> = access.repo_path().into();

    info!("GIT/v1 {} {}", repo_path, protocol_path);

    // Receive-pack discovery: GET /info/refs?service=git-receive-pack
    if protocol_path.as_ref() == "info/refs"
        && query_string
            .split('&')
            .any(|kv| kv == "service=git-receive-pack")
    {
        return recv_pack_info_refs(&spawn, &access, repo_path.as_ref());
    }

    // Upload-pack discovery: GET /info/refs?service=git-upload-pack
    if protocol_path.as_ref() == "info/refs"
        && query_string
            .split('&')
            .any(|kv| kv == "service=git-upload-pack")
    {
        if access.auto_init() {
            res_try!(receive::init_bare_if_missing(repo_path.as_ref()));
        }
        let refs = res_try!(gather_upload_pack_v1_refs(repo_path.as_ref()));
        let (reader, writer) = piper::pipe(4096);
        spawn(Box::pin(info_refs_upload_pack_v1_task(refs, writer)));
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
        return recv_pack_post(spawn, access, repo_path, body).await;
    }

    // Upload-pack: POST /git-upload-pack
    if protocol_path.as_ref() == "git-upload-pack" {
        if content_type.as_ref() != "application/x-git-upload-pack-request" {
            return GitResponse {
                status_code: 400,
                content_type: None,
                reader: None,
                body: Some(
                    "Expected content type application/x-git-upload-pack-request".to_string(),
                ),
            };
        }
        let mut args = res_try!(fetch::read_fetch_args_v1(body).await);
        let repo = res_try!(gix::open(repo_path.as_ref()));
        for refname in &args.want_refs {
            if let Ok(mut r) = repo.find_reference(refname.as_str()) {
                if let Ok(id) = r.peel_to_id() {
                    args.want.push(id.detach());
                }
            }
        }
        let (reader, writer) = piper::pipe(4096);
        spawn(Box::pin(async move {
            if let Err(e) = fetch::perform_fetch_v1(repo.objects, &args, writer).await {
                error!("perform_fetch_v1 error: {:#}", e);
            }
        }));
        return GitResponse {
            status_code: 200,
            content_type: None,
            reader: Some(reader),
            body: None,
        };
    }

    GitResponse {
        status_code: 404,
        content_type: None,
        reader: None,
        body: None,
    }
}

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
    A: RepoAccess + Send + 'static,
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
        return recv_pack_info_refs(&spawn, &access, repo_path.as_ref());
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
        return recv_pack_post(spawn, access, repo_path, body).await;
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
                    if let Err(e) = ls_refs::perform_listrefs(&repo, &args, writer).await {
                        error!("perform_listrefs error: {:#}", e);
                    }
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
                let mut args = res_try!(fetch::read_fetch_args(&mut parser).await);
                let repo = res_try!(gix::open(repo_path.as_ref()));
                // Resolve any want-ref names to OIDs and add to wants.
                for refname in &args.want_refs {
                    if let Ok(mut r) = repo.find_reference(refname.as_str()) {
                        if let Ok(id) = r.peel_to_id() {
                            args.want.push(id.detach());
                        }
                    }
                }
                info!("FETCH: {:?}", args);
                let (reader, writer) = piper::pipe(4096);
                spawn(Box::pin(async move {
                    if let Err(e) = fetch::perform_fetch(repo.objects, &args, writer).await {
                        error!("perform_fetch error: {:#}", e);
                    }
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
    text_to_write(
        b"fetch=ref-in-want wait-for-done shallow filter",
        &mut writer,
    )
    .await
    .expect("to write to output");

    flush_to_write(&mut writer)
        .await
        .expect("to write to output");
}
