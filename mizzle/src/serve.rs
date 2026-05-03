use crate::auth::{run_comparison, ComparisonOptions};
use crate::auth_types::PushRef;
use crate::backend::{PackMetadata, StorageBackend};
use crate::command::{read_command, Command};
use crate::traits::RepoAccess;
use crate::{fetch, ls_refs, receive};
use futures_lite::{AsyncRead, AsyncWrite};
use gix::ObjectId;
use gix_packetline::async_io::encode::{flush_to_write, text_to_write};
use gix_packetline::async_io::StreamingPeekableIter;
pub use mizzle_proto::limits::ProtocolLimits;
use piper::{Reader, Writer};
use std::future::Future;
use std::pin::Pin;
use tracing::{error, info};

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
    let repo = res_try!(backend.open(repo_id));
    let snapshot = res_try!(backend.list_refs(&repo));
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

/// Build a `Vec<PushRef<'_>>` from `RefUpdate`s using the supplied kind classifier.
fn build_push_refs<'a>(
    updates: &'a [crate::backend::RefUpdate],
    mut kind: impl FnMut(&crate::backend::RefUpdate) -> crate::auth_types::PushKind,
) -> Vec<PushRef<'a>> {
    updates
        .iter()
        .map(|u| PushRef {
            refname: &u.refname,
            kind: kind(u),
            old_oid: u.old_oid,
            new_oid: u.new_oid,
        })
        .collect()
}

/// Existing ref tips, used as exclusion set when computing `new_commits`.
fn existing_ref_tips<B: StorageBackend>(
    backend: &B,
    repo: &B::Repo,
    push_refs: &[PushRef<'_>],
) -> Vec<ObjectId> {
    let snapshot = match backend.list_refs(repo) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let pushed: std::collections::HashSet<&str> = push_refs.iter().map(|r| r.refname).collect();
    let mut out = Vec::new();
    for r in &snapshot.refs {
        if pushed.contains(r.name.as_str()) {
            // The push is updating this ref — we must NOT include the
            // existing tip in the exclusion set, otherwise the new commits
            // walk would always be empty for fast-forwards.
            continue;
        }
        out.push(r.oid);
        if let Some(p) = r.peeled {
            out.push(p);
        }
    }
    out
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

    // Preliminary auth check: refnames + old/new oid + cheap PushKind.
    let preliminary_refs = build_push_refs(&ref_updates, receive::preliminary_push_kind);
    let push_ctx = match access.authorize_preliminary(&preliminary_refs) {
        Ok(c) => c,
        Err(msg) => return reject_response(spawn, ref_updates, msg),
    };

    // Open the repo handle once for the remainder of this request.
    let repo = res_try!(backend.open(&repo_id));

    // Stage pack data to a temp file (streamed, not buffered in memory).
    let staged = res_try!(receive::stage_pack(body, None).await);
    let written_pack = if let Some(ref staged) = staged {
        res_try!(backend.ingest_pack(&repo, staged.path()))
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
                return reject_response(
                    spawn,
                    ref_updates,
                    format!("pack inspection failed: {e:#}"),
                );
            }
        }
    } else {
        None
    };

    let push_refs = build_push_refs(&ref_updates, |u| backend.compute_push_kind(&repo, u));
    let existing = existing_ref_tips(&backend, &repo, &push_refs);
    let empty_meta = PackMetadata {
        objects: Vec::new(),
    };
    let pack_ref = pack_meta.as_ref().unwrap_or(&empty_meta);

    let auth_result = run_comparison(
        &access,
        &backend,
        &repo,
        pack_ref,
        push_refs.clone(),
        existing.clone(),
        ComparisonOptions::default(),
        |comp| access.authorize_push(&push_ctx, comp),
    );

    let rejected: Vec<(String, String)> = match auth_result {
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
        let owned_kinds: Vec<(String, crate::auth_types::PushKind)> = push_refs
            .iter()
            .map(|pr| (pr.refname.to_string(), pr.kind.clone()))
            .collect();
        let owned_old_new: Vec<(ObjectId, ObjectId)> = push_refs
            .iter()
            .map(|pr| (pr.old_oid, pr.new_oid))
            .collect();
        let pack_meta_owned = pack_meta;
        let existing_owned = existing;
        let b = backend.clone();
        spawn(Box::pin(async move {
            let mut w = writer;
            match receive::update_refs_and_report(&b, &repo, &ref_updates, &mut w).await {
                Ok(()) => {
                    let post_refs: Vec<PushRef<'_>> = owned_kinds
                        .iter()
                        .zip(owned_old_new.iter())
                        .map(|((name, kind), (old, new))| PushRef {
                            refname: name.as_str(),
                            kind: kind.clone(),
                            old_oid: *old,
                            new_oid: *new,
                        })
                        .collect();
                    let empty = PackMetadata {
                        objects: Vec::new(),
                    };
                    let pack_ref = pack_meta_owned.as_ref().unwrap_or(&empty);
                    run_comparison(
                        &access,
                        &b,
                        &repo,
                        pack_ref,
                        post_refs,
                        existing_owned,
                        ComparisonOptions::default(),
                        |comp| access.post_receive(comp),
                    )
                    .await;
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

fn reject_response(
    spawn: impl Fn(SpawnFut),
    ref_updates: Vec<crate::backend::RefUpdate>,
    msg: String,
) -> GitResponse {
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
            error!("rejection write error: {:#}", e);
        }
    }));
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
        let repo = res_try!(backend.open(&repo_id));
        let snapshot = res_try!(backend.list_refs(&repo));
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
        let repo = res_try!(backend.open(&repo_id));
        let mut args = res_try!(fetch::read_fetch_args_v1(body, limits).await);
        for refname in &args.want_refs {
            if let Ok(Some(oid)) = backend.resolve_ref(&repo, refname.as_str()) {
                args.want.push(oid);
            }
        }
        let (reader, writer) = piper::pipe(4096);
        spawn(Box::pin(async move {
            let mut w = writer;
            if let Err(e) = fetch::perform_fetch_v1(&backend, &repo, &args, &mut w).await {
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
                let repo = res_try!(backend.open(&repo_id));
                let snapshot = res_try!(backend.list_refs(&repo));
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
                let repo = res_try!(backend.open(&repo_id));
                let mut args = res_try!(fetch::read_fetch_args(&mut parser, limits).await);
                // Resolve any want-ref names to OIDs and add to wants.
                for refname in &args.want_refs {
                    if let Ok(Some(oid)) = backend.resolve_ref(&repo, refname.as_str()) {
                        args.want.push(oid);
                    }
                }
                info!("FETCH: {:?}", args);
                let (reader, writer) = piper::pipe(4096);
                spawn(Box::pin(async move {
                    let mut w = writer;
                    if let Err(e) = fetch::perform_fetch(&backend, &repo, &args, &mut w).await {
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

    let repo = backend.open(&repo_id)?;

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
                    let snapshot = backend.list_refs(&repo)?;
                    ls_refs::perform_listrefs(&snapshot, &args, writer).await?;
                }
                Command::Fetch => {
                    let mut args = fetch::read_fetch_args(&mut parser, limits).await?;
                    for refname in &args.want_refs {
                        if let Ok(Some(oid)) = backend.resolve_ref(&repo, refname.as_str()) {
                            args.want.push(oid);
                        }
                    }
                    info!("FETCH: {:?}", args);
                    fetch::perform_fetch(backend, &repo, &args, writer).await?;
                }
                Command::Empty => break,
            }
        }
    } else {
        info!("upload-pack v1 {:?}", repo_id);
        let snapshot = backend.list_refs(&repo)?;
        let refs = snapshot.as_upload_pack_v1();
        upload_pack_v1_refs(&refs, writer).await?;

        let mut args = fetch::read_fetch_args_v1(reader, limits).await?;
        for refname in &args.want_refs {
            if let Ok(Some(oid)) = backend.resolve_ref(&repo, refname.as_str()) {
                args.want.push(oid);
            }
        }
        fetch::perform_fetch_v1(backend, &repo, &args, writer).await?;
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

    let repo = backend.open(&repo_id)?;

    // Advertise refs (no HTTP preamble).
    let snapshot = backend.list_refs(&repo)?;
    let refs = snapshot.as_receive_pack();
    receive::info_refs_receive_pack_task(refs, writer).await?;

    // Read the ref update commands (the reader is left positioned at the pack).
    let (ref_updates, reader) = receive::read_receive_request(reader, limits).await?;

    // Preliminary auth check.
    let preliminary_refs = build_push_refs(&ref_updates, receive::preliminary_push_kind);
    let push_ctx = match access.authorize_preliminary(&preliminary_refs) {
        Ok(c) => c,
        Err(msg) => {
            text_to_write(b"unpack ok", &mut *writer).await?;
            for update in &ref_updates {
                let line = format!("ng {} {}", update.refname, msg);
                text_to_write(line.as_bytes(), &mut *writer).await?;
            }
            flush_to_write(&mut *writer).await?;
            return Ok(());
        }
    };

    // Stage pack data to a temp file (streamed, not buffered in memory).
    let staged = receive::stage_pack(reader, None).await?;
    let written_pack = if let Some(ref staged) = staged {
        backend.ingest_pack(&repo, staged.path())?
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

    let push_refs = build_push_refs(&ref_updates, |u| backend.compute_push_kind(&repo, u));
    let existing = existing_ref_tips(backend, &repo, &push_refs);
    let empty_meta = PackMetadata {
        objects: Vec::new(),
    };
    let pack_ref = pack_meta.as_ref().unwrap_or(&empty_meta);

    let auth_result = run_comparison(
        &access,
        backend,
        &repo,
        pack_ref,
        push_refs.clone(),
        existing.clone(),
        ComparisonOptions::default(),
        |comp| access.authorize_push(&push_ctx, comp),
    );

    if let Err(msg) = auth_result {
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

    receive::update_refs_and_report(backend, &repo, &ref_updates, writer).await?;

    // post_receive uses a fresh Comparison (the moved push_refs were
    // consumed by run_comparison above; rebuild from the same data).
    let post_refs = build_push_refs(&ref_updates, |u| backend.compute_push_kind(&repo, u));
    let pack_ref2 = pack_meta.as_ref().unwrap_or(&empty_meta);
    run_comparison(
        &access,
        backend,
        &repo,
        pack_ref2,
        post_refs,
        existing,
        ComparisonOptions::default(),
        |comp| access.post_receive(comp),
    )
    .await;

    Ok(())
}
