use anyhow::{Context, Result};
use futures_lite::{AsyncRead, AsyncReadExt};
use gix::ObjectId;
use gix_packetline::async_io::encode::{flush_to_write, text_to_write};
use log::error;
use piper::Writer;
use std::sync::atomic::AtomicBool;

use crate::traits::PushKind;

/// Determines the push kind using only the ref OIDs, without opening the odb.
///
/// `Create` and `Delete` can be determined definitively.  For any other update
/// the kind is optimistically reported as `FastForward`; call
/// [`compute_push_kind`] after the pack has been written to get the real answer.
pub fn preliminary_push_kind(update: &RefUpdate) -> PushKind {
    if update.old_oid.is_null() {
        PushKind::Create
    } else if update.new_oid.is_null() {
        PushKind::Delete
    } else {
        PushKind::FastForward
    }
}

/// Determines how a ref update changes the repository.  Takes the object
/// database so the fast-forward check can walk the commit graph — the caller
/// never needs to open the repo separately.
pub fn compute_push_kind(odb: impl gix::objs::Find + Clone, update: &RefUpdate) -> PushKind {
    if update.old_oid.is_null() {
        return PushKind::Create;
    }
    if update.new_oid.is_null() {
        return PushKind::Delete;
    }

    // Fast-forward if old_oid is an ancestor of new_oid,
    // i.e. old_oid appears in the history when walking back from new_oid.
    let is_ff = gix::traverse::commit::Simple::new(std::iter::once(update.new_oid), odb)
        .any(|r| r.map(|info| info.id == update.old_oid).unwrap_or(false));

    if is_ff {
        PushKind::FastForward
    } else {
        PushKind::ForcePush
    }
}

#[derive(Debug)]
pub struct RefUpdate {
    pub old_oid: ObjectId,
    pub new_oid: ObjectId,
    pub refname: String,
}

/// Parses a receive-pack request body: pkt-line ref-update commands (ending
/// with a flush packet) followed by a raw packfile.
pub async fn read_receive_request<T: AsyncRead + Unpin>(
    mut body: T,
) -> Result<(Vec<RefUpdate>, Vec<u8>)> {
    // Buffer everything; the pack is raw binary after the pkt-line section.
    let mut all_bytes = Vec::new();
    body.read_to_end(&mut all_bytes).await?;

    let mut pos = 0;
    let mut ref_updates = Vec::new();
    let mut first_line = true;

    loop {
        if pos + 4 > all_bytes.len() {
            break;
        }
        let len_str = std::str::from_utf8(&all_bytes[pos..pos + 4])?;
        let len = usize::from_str_radix(len_str, 16)?;

        if len == 0 {
            // flush packet — end of ref-update section
            pos += 4;
            break;
        }
        if len < 4 || pos + len > all_bytes.len() {
            break;
        }

        let data = &all_bytes[pos + 4..pos + len];
        let data = data.strip_suffix(b"\n").unwrap_or(data);

        // Strip capabilities (everything after NUL) from the first line.
        let line = if first_line {
            first_line = false;
            data.splitn(2, |&b| b == b'\0').next().unwrap_or(data)
        } else {
            data
        };

        // Each command: "<old-oid> <new-oid> <refname>"
        let mut parts = line.splitn(3, |&b| b == b' ');
        if let (Some(old_hex), Some(new_hex), Some(name)) =
            (parts.next(), parts.next(), parts.next())
        {
            let old_oid = ObjectId::from_hex(old_hex)?;
            let new_oid = ObjectId::from_hex(new_hex)?;
            let refname = String::from_utf8(name.to_vec())?;
            ref_updates.push(RefUpdate {
                old_oid,
                new_oid,
                refname,
            });
        }

        pos += len;
    }

    let pack_data = all_bytes[pos..].to_vec();
    Ok((ref_updates, pack_data))
}

/// Writes the received pack to the repository's object store.
///
/// Call this before [`compute_push_kind`] so that new objects are available
/// for the fast-forward reachability check.  If auth is later denied, the
/// objects will remain but be unreachable until the next GC.
pub fn write_pack(repo_path: &str, pack_data: &[u8]) -> Result<()> {
    let repo = gix::open(repo_path)?;
    let pack_dir = repo.path().join("objects").join("pack");
    std::fs::create_dir_all(&pack_dir)?;

    let mut progress = gix_features::progress::Discard;
    let interrupt = AtomicBool::new(false);

    gix_pack::Bundle::write_to_directory(
        &mut std::io::BufReader::new(pack_data),
        Some(&pack_dir),
        &mut progress,
        &interrupt,
        None::<gix::objs::find::Never>,
        Default::default(),
    )
    .context("indexing received pack")?;

    Ok(())
}

/// Updates refs and sends the receive-pack result (unpack ok + per-ref ok lines)
/// to `writer`.  The pack must already have been written via [`write_pack`].
pub async fn update_refs_and_report(
    repo_path: &str,
    ref_updates: Vec<RefUpdate>,
    mut writer: Writer,
) -> Result<()> {
    update_refs(repo_path, &ref_updates).context("updating refs")?;

    text_to_write(b"unpack ok", &mut writer).await?;
    for update in &ref_updates {
        let msg = format!("ok {}", update.refname);
        text_to_write(msg.as_bytes(), &mut writer).await?;
    }
    flush_to_write(&mut writer).await?;

    Ok(())
}

fn update_refs(repo_path: &str, updates: &[RefUpdate]) -> Result<()> {
    use gix_ref::transaction::PreviousValue;

    let repo = gix::open(repo_path)?;
    for update in updates {
        repo.reference(
            update.refname.as_str(),
            update.new_oid,
            PreviousValue::Any,
            "push",
        )
        .with_context(|| format!("updating ref {}", update.refname))?;
    }
    Ok(())
}

/// Sends the receive-pack ref advertisement in response to
/// `GET /info/refs?service=git-receive-pack`.
pub async fn info_refs_receive_pack_task(repo_path: Box<str>, mut writer: Writer) {
    let caps = b"report-status delete-refs agent=mizzle/dev";

    let refs_result: Result<Vec<(ObjectId, String)>> = (|| {
        let repo = gix::open(repo_path.as_ref())?;
        let mut result = Vec::new();
        for r in repo.references()?.all()? {
            let mut r = r.map_err(|e| anyhow::anyhow!("{e}"))?;
            let name = r.name().as_bstr().to_string();
            // Only advertise concrete refs, not HEAD or other symrefs.
            if !name.starts_with("refs/") {
                continue;
            }
            if let Ok(id) = r.peel_to_id() {
                result.push((id.detach(), name));
            }
        }
        Ok(result)
    })();

    match refs_result {
        Err(e) => error!("receive-pack info/refs: {}", e),
        Ok(refs) => {
            if refs.is_empty() {
                // Empty repo: advertise capabilities only.
                let null_oid = "0000000000000000000000000000000000000000";
                let mut line = Vec::new();
                line.extend_from_slice(null_oid.as_bytes());
                line.extend_from_slice(b" capabilities^{}");
                line.push(b'\0');
                line.extend_from_slice(caps);
                text_to_write(&line, &mut writer)
                    .await
                    .expect("write caps line");
            } else {
                let mut first = true;
                for (oid, name) in &refs {
                    let mut line = Vec::new();
                    line.extend_from_slice(oid.to_hex().to_string().as_bytes());
                    line.push(b' ');
                    line.extend_from_slice(name.as_bytes());
                    if first {
                        line.push(b'\0');
                        line.extend_from_slice(caps);
                        first = false;
                    }
                    text_to_write(&line, &mut writer)
                        .await
                        .expect("write ref line");
                }
            }
            flush_to_write(&mut writer).await.expect("flush");
        }
    }
}
