use crate::backend::{PackMetadata, StorageBackend};
use crate::command::{read_command, Command};
use crate::traits::RepoAccess;
use crate::{fetch, ls_refs, receive};
use futures_lite::{AsyncRead, AsyncWrite};
use gix::ObjectId;
use gix_packetline::async_io::encode::{flush_to_write, text_to_write};
use gix_packetline::async_io::StreamingPeekableIter;
use log::{error, info};
pub use mizzle_proto::limits::ProtocolLimits;
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
fn recv_pack_info_refs<A, B>(
    spawn: &impl Fn(SpawnFut),
    access: &A,
    backend: &B,
    repo_id: &A::RepoId,
) -> GitResponse
where
    A: RepoAccess + Send + 'static,
    B: StorageBackend<RepoId = A::RepoId>,
{
    if access.auto_init() {
        res_try!(backend.init_repo(repo_id));
    }
    let snapshot = res_try!(backend.list_refs(repo_id));
    let refs = snapshot.as_receive_pack();
    let (reader, writer) = piper::pipe(4096);
    spawn(Box::pin(async move {
        let mut w = writer;
        let result: anyhow::Result<()> = async {
            text_to_write(b"# service=git-receive-pack", &mut w).await?;
            flush_to_write(&mut w).await?;
            receive::info_refs_receive_pack_task(refs, &mut w).await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            error!("recv_pack_info_refs write error: {:#}", e);
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
async fn recv_pack_post<T, A, B>(
    spawn: impl Fn(SpawnFut),
    access: A,
    backend: B,
    repo_id: A::RepoId,
    limits: &ProtocolLimits,
    body: T,
) -> GitResponse
where
    T: AsyncRead + Unpin,
    A: RepoAccess + Send + 'static,
    B: StorageBackend<RepoId = A::RepoId> + Clone,
{
    let (ref_updates, body) = res_try!(receive::read_receive_request(body, limits).await);

    // Preliminary auth check before touching the disk.
    let preliminary_refs: Vec<crate::traits::PushRef<'_>> = ref_updates
        .iter()
        .map(|u| crate::traits::PushRef {
            refname: &u.refname,
            kind: receive::preliminary_push_kind(u),
        })
        .collect();
    if let Err(msg) = access.authorize_push(&preliminary_refs, None) {
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

    // Stage pack data to a temp file (streamed, not buffered in memory).
    let staged = res_try!(receive::stage_pack(body, None).await);
    let written_pack = if let Some(ref staged) = staged {
        res_try!(backend.ingest_pack(&repo_id, staged.path()))
    } else {
        None
    };

    // Inspect the ingested pack for auth metadata.  If inspection fails,
    // reject the push — proceeding without metadata could let a crafted
    // pack bypass metadata-based auth checks.
    let pack_meta: Option<PackMetadata> = if let Some(ref pack) = written_pack {
        match backend.inspect_ingested(pack) {
            Ok(meta) => Some(meta),
            Err(e) => {
                error!("pack inspection failed: {:#}", e);
                if let Some(pack) = written_pack {
                    backend.rollback_ingest(pack);
                }
                let msg = format!("pack inspection failed: {e:#}");
                let (reader, writer) = piper::pipe(4096);
                spawn(Box::pin(async move {
                    let mut w = writer;
                    let result: anyhow::Result<()> = async {
                        text_to_write(b"unpack ok", &mut w).await?;
                        for update in &ref_updates {
                            let line = format!("ng {} {}", update.refname, msg);
                            text_to_write(line.as_bytes(), &mut w).await?;
                        }
                        flush_to_write(&mut w).await?;
                        Ok(())
                    }
                    .await;
                    if let Err(e) = result {
                        error!("inspection rejection write error: {:#}", e);
                    }
                }));
                return GitResponse {
                    status_code: 200,
                    content_type: Some("application/x-git-receive-pack-result".to_string()),
                    reader: Some(reader),
                    body: None,
                };
            }
        }
    } else {
        None
    };

    let push_refs: Vec<crate::traits::PushRef<'_>> = ref_updates
        .iter()
        .map(|u| crate::traits::PushRef {
            refname: &u.refname,
            kind: backend.compute_push_kind(&repo_id, u),
        })
        .collect();
    let rejected: Vec<(String, String)> =
        match access.authorize_push(&push_refs, pack_meta.as_ref()) {
            Err(msg) => {
                if let Some(pack) = written_pack {
                    backend.rollback_ingest(pack);
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
        let b = backend.clone();
        let rid = repo_id.clone();
        spawn(Box::pin(async move {
            let mut w = writer;
            match receive::update_refs_and_report(&b, &rid, &ref_updates, &mut w).await {
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

/// Writes the v1 upload-pack ref advertisement (refs + capabilities + flush).
/// Does NOT write the HTTP preamble (`# service=git-upload-pack` + flush).
pub async fn upload_pack_v1_refs(
    refs: &[(ObjectId, String)],
    writer: &mut (impl AsyncWrite + Unpin),
) -> anyhow::Result<()> {
    let caps = b"side-band-64k ofs-delta shallow filter agent=mizzle/dev";
    if refs.is_empty() {
        let mut line = b"0000000000000000000000000000000000000000 capabilities^{}".to_vec();
        line.push(0);
        line.extend_from_slice(caps);
        text_to_write(&line, &mut *writer).await?;
    } else {
        let mut first = true;
        for (oid, name) in refs {
            let mut line = Vec::new();
            line.extend_from_slice(oid.to_hex().to_string().as_bytes());
            line.push(b' ');
            line.extend_from_slice(name.as_bytes());
            if first {
                line.push(0);
                line.extend_from_slice(caps);
                first = false;
            }
            text_to_write(&line, &mut *writer).await?;
        }
    }
    flush_to_write(&mut *writer).await?;
    Ok(())
}

async fn info_refs_upload_pack_v1_task(refs: Vec<(ObjectId, String)>, mut writer: Writer) {
    let result: anyhow::Result<()> = async {
        text_to_write(b"# service=git-upload-pack", &mut writer).await?;
        flush_to_write(&mut writer).await?;
        upload_pack_v1_refs(&refs, &mut writer).await?;
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
pub async fn serve_git_protocol_1<T, A, B, S>(
    spawn: S,
    access: A,
    backend: B,
    protocol_path: Box<str>,
    query_string: Box<str>,
    content_type: Box<str>,
    limits: &ProtocolLimits,
    body: T,
) -> GitResponse
where
    T: AsyncRead + Unpin,
    A: RepoAccess + Send + 'static,
    B: StorageBackend<RepoId = A::RepoId> + Clone,
    S: Fn(SpawnFut),
{
    let repo_id = access.repo_id().clone();

    info!("GIT/v1 {:?} {}", repo_id, protocol_path);

    // Receive-pack discovery: GET /info/refs?service=git-receive-pack
    if protocol_path.as_ref() == "info/refs"
        && query_string
            .split('&')
            .any(|kv| kv == "service=git-receive-pack")
    {
        return recv_pack_info_refs(&spawn, &access, &backend, &repo_id);
    }

    // Upload-pack discovery: GET /info/refs?service=git-upload-pack
    if protocol_path.as_ref() == "info/refs"
        && query_string
            .split('&')
            .any(|kv| kv == "service=git-upload-pack")
    {
        if access.auto_init() {
            res_try!(backend.init_repo(&repo_id));
        }
        let snapshot = res_try!(backend.list_refs(&repo_id));
        let refs = snapshot.as_upload_pack_v1();
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
        return recv_pack_post(spawn, access, backend, repo_id, limits, body).await;
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
        let mut args = res_try!(fetch::read_fetch_args_v1(body, limits).await);
        for refname in &args.want_refs {
            if let Ok(Some(oid)) = backend.resolve_ref(&repo_id, refname.as_str()) {
                args.want.push(oid);
            }
        }
        let b = backend.clone();
        let rid = repo_id.clone();
        let (reader, writer) = piper::pipe(4096);
        spawn(Box::pin(async move {
            let mut w = writer;
            if let Err(e) = fetch::perform_fetch_v1(&b, &rid, &args, &mut w).await {
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

pub async fn serve_git_protocol_2<T, A, B, S>(
    spawn: S,
    access: A,
    backend: B,
    protocol_path: Box<str>,
    query_string: Box<str>,
    content_type: Box<str>,
    limits: &ProtocolLimits,
    body: T,
) -> GitResponse
where
    T: AsyncRead + Unpin,
    A: RepoAccess + Send + 'static,
    B: StorageBackend<RepoId = A::RepoId> + Clone,
    S: Fn(SpawnFut),
{
    let repo_id = access.repo_id().clone();

    info!("GIT {:?} {}", repo_id, protocol_path);

    // Receive-pack discovery: GET /info/refs?service=git-receive-pack
    if protocol_path.as_ref() == "info/refs"
        && query_string
            .split('&')
            .any(|kv| kv == "service=git-receive-pack")
    {
        return recv_pack_info_refs(&spawn, &access, &backend, &repo_id);
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
        return recv_pack_post(spawn, access, backend, repo_id, limits, body).await;
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
                let args = res_try!(ls_refs::read_lsrefs_args(&mut parser, limits).await);
                let snapshot = res_try!(backend.list_refs(&repo_id));
                let (reader, writer) = piper::pipe(4096);
                spawn(Box::pin(async move {
                    let mut w = writer;
                    if let Err(e) = ls_refs::perform_listrefs(&snapshot, &args, &mut w).await {
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
                let mut args = res_try!(fetch::read_fetch_args(&mut parser, limits).await);
                // Resolve any want-ref names to OIDs and add to wants.
                for refname in &args.want_refs {
                    if let Ok(Some(oid)) = backend.resolve_ref(&repo_id, refname.as_str()) {
                        args.want.push(oid);
                    }
                }
                info!("FETCH: {:?}", args);
                let b = backend.clone();
                let rid = repo_id.clone();
                let (reader, writer) = piper::pipe(4096);
                spawn(Box::pin(async move {
                    let mut w = writer;
                    if let Err(e) = fetch::perform_fetch(&b, &rid, &args, &mut w).await {
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

/// Writes the v2 capability advertisement (version 2, agent, ls-refs, fetch).
/// Does NOT write the HTTP preamble (`# service=git-upload-pack` + flush).
pub async fn capability_advertisement_v2(
    writer: &mut (impl AsyncWrite + Unpin),
) -> anyhow::Result<()> {
    text_to_write(b"version 2", &mut *writer).await?;
    text_to_write(b"agent=mizzle/dev", &mut *writer).await?;
    text_to_write(b"ls-refs=unborn", &mut *writer).await?;
    text_to_write(
        b"fetch=ref-in-want wait-for-done shallow filter",
        &mut *writer,
    )
    .await?;
    flush_to_write(&mut *writer).await?;
    Ok(())
}

async fn info_refs_task(mut writer: Writer) {
    let result: anyhow::Result<()> = async {
        text_to_write(b"# service=git-upload-pack", &mut writer).await?;
        flush_to_write(&mut writer).await?;
        capability_advertisement_v2(&mut writer).await?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        error!("info_refs_task error: {:#}", e);
    }
}

/// Serve an upload-pack session over separate read/write halves.
///
/// This is the core logic used by the SSH transport. The caller is responsible
/// for providing the read and write sides and for any post-protocol cleanup
/// (e.g. sending SSH exit-status).
pub async fn serve_upload_pack<R, W, A, B>(
    access: A,
    backend: &B,
    reader: R,
    writer: &mut W,
    version: u32,
    limits: &ProtocolLimits,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    A: RepoAccess + Send + 'static,
    B: StorageBackend<RepoId = A::RepoId>,
{
    let repo_id = access.repo_id().clone();

    if access.auto_init() {
        backend.init_repo(&repo_id)?;
    }

    if version >= 2 {
        info!("upload-pack v2 {:?}", repo_id);
        capability_advertisement_v2(writer).await?;

        let mut parser = StreamingPeekableIter::new(reader, &[], false);
        loop {
            let command = match read_command(&mut parser).await {
                Ok(cmd) => cmd,
                Err(_) => break, // EOF — client disconnected after final command
            };
            match command {
                Command::ListRefs => {
                    let args = ls_refs::read_lsrefs_args(&mut parser, limits).await?;
                    let snapshot = backend.list_refs(&repo_id)?;
                    ls_refs::perform_listrefs(&snapshot, &args, writer).await?;
                }
                Command::Fetch => {
                    let mut args = fetch::read_fetch_args(&mut parser, limits).await?;
                    for refname in &args.want_refs {
                        if let Ok(Some(oid)) = backend.resolve_ref(&repo_id, refname.as_str()) {
                            args.want.push(oid);
                        }
                    }
                    info!("FETCH: {:?}", args);
                    fetch::perform_fetch(backend, &repo_id, &args, writer).await?;
                }
                Command::Empty => break,
            }
        }
    } else {
        info!("upload-pack v1 {:?}", repo_id);
        let snapshot = backend.list_refs(&repo_id)?;
        let refs = snapshot.as_upload_pack_v1();
        upload_pack_v1_refs(&refs, writer).await?;

        let mut args = fetch::read_fetch_args_v1(reader, limits).await?;
        for refname in &args.want_refs {
            if let Ok(Some(oid)) = backend.resolve_ref(&repo_id, refname.as_str()) {
                args.want.push(oid);
            }
        }
        fetch::perform_fetch_v1(backend, &repo_id, &args, writer).await?;
    }

    Ok(())
}

/// Serve a receive-pack session over separate read/write halves.
///
/// This is the core logic used by the SSH transport. The caller is responsible
/// for providing the read and write sides and for any post-protocol cleanup
/// (e.g. sending SSH exit-status).
pub async fn serve_receive_pack<R, W, A, B>(
    access: A,
    backend: &B,
    reader: R,
    writer: &mut W,
    limits: &ProtocolLimits,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    A: RepoAccess + Send + 'static,
    B: StorageBackend<RepoId = A::RepoId>,
{
    let repo_id = access.repo_id().clone();
    info!("receive-pack {:?}", repo_id);

    if access.auto_init() {
        backend.init_repo(&repo_id)?;
    }

    // Advertise refs (no HTTP preamble).
    let snapshot = backend.list_refs(&repo_id)?;
    let refs = snapshot.as_receive_pack();
    receive::info_refs_receive_pack_task(refs, writer).await?;

    // Read the ref update commands (the reader is left positioned at the pack).
    let (ref_updates, reader) = receive::read_receive_request(reader, limits).await?;

    // Preliminary auth check.
    let preliminary_refs: Vec<crate::traits::PushRef<'_>> = ref_updates
        .iter()
        .map(|u| crate::traits::PushRef {
            refname: &u.refname,
            kind: receive::preliminary_push_kind(u),
        })
        .collect();
    if let Err(msg) = access.authorize_push(&preliminary_refs, None) {
        text_to_write(b"unpack ok", &mut *writer).await?;
        for update in &ref_updates {
            let line = format!("ng {} {}", update.refname, msg);
            text_to_write(line.as_bytes(), &mut *writer).await?;
        }
        flush_to_write(&mut *writer).await?;
        return Ok(());
    }

    // Stage pack data to a temp file (streamed, not buffered in memory).
    let staged = receive::stage_pack(reader, None).await?;
    let written_pack = if let Some(ref staged) = staged {
        backend.ingest_pack(&repo_id, staged.path())?
    } else {
        None
    };

    // Inspect the ingested pack for auth metadata.  If inspection fails,
    // reject the push rather than proceeding without metadata.
    let pack_meta: Option<PackMetadata> = if let Some(ref pack) = written_pack {
        match backend.inspect_ingested(pack) {
            Ok(meta) => Some(meta),
            Err(e) => {
                error!("pack inspection failed: {:#}", e);
                if let Some(pack) = written_pack {
                    backend.rollback_ingest(pack);
                }
                let msg = format!("pack inspection failed: {e:#}");
                text_to_write(b"unpack ok", &mut *writer).await?;
                for update in &ref_updates {
                    let line = format!("ng {} {}", update.refname, msg);
                    text_to_write(line.as_bytes(), &mut *writer).await?;
                }
                flush_to_write(&mut *writer).await?;
                return Ok(());
            }
        }
    } else {
        None
    };

    let push_refs: Vec<crate::traits::PushRef<'_>> = ref_updates
        .iter()
        .map(|u| crate::traits::PushRef {
            refname: &u.refname,
            kind: backend.compute_push_kind(&repo_id, u),
        })
        .collect();
    if let Err(msg) = access.authorize_push(&push_refs, pack_meta.as_ref()) {
        if let Some(pack) = written_pack {
            backend.rollback_ingest(pack);
        }
        text_to_write(b"unpack ok", &mut *writer).await?;
        for update in &ref_updates {
            let line = format!("ng {} {}", update.refname, msg);
            text_to_write(line.as_bytes(), &mut *writer).await?;
        }
        flush_to_write(&mut *writer).await?;
        return Ok(());
    }

    receive::update_refs_and_report(backend, &repo_id, &ref_updates, writer).await?;

    let owned_kinds: Vec<(String, crate::traits::PushKind)> = push_refs
        .iter()
        .map(|pr| (pr.refname.to_string(), pr.kind.clone()))
        .collect();
    let post_refs: Vec<crate::traits::PushRef<'_>> = owned_kinds
        .iter()
        .map(|(name, kind)| crate::traits::PushRef {
            refname: name.as_str(),
            kind: kind.clone(),
        })
        .collect();
    access.post_receive(&post_refs).await;

    Ok(())
}
