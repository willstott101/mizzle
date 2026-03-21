use anyhow::{Context, Result};
use futures_lite::{AsyncRead, AsyncReadExt, AsyncWrite};
use gix_packetline::async_io::encode::{flush_to_write, text_to_write};

use crate::backend::{RefUpdate, StorageBackend};

pub use mizzle_proto::receive::{preliminary_push_kind, read_receive_request};

/// Streams pack data from an async reader to a temporary file, without
/// buffering the entire pack in memory.  Returns the temp file (auto-deleted
/// on drop), or `None` if no data was received.
pub async fn stage_pack<R: AsyncRead + Unpin>(
    mut reader: R,
) -> Result<Option<tempfile::NamedTempFile>> {
    let temp = tempfile::Builder::new()
        .prefix("mizzle_pack_")
        .tempfile()
        .context("creating temp file for pack staging")?;

    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    // Write synchronously — the temp file is local disk backed by page cache,
    // so individual writes won't block the executor meaningfully.
    let mut file = temp
        .as_file()
        .try_clone()
        .context("cloning temp file handle")?;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        total += n as u64;
        std::io::Write::write_all(&mut file, &buf[..n])
            .context("writing pack data to temp file")?;
    }

    if total == 0 {
        return Ok(None);
    }

    Ok(Some(temp))
}

/// Updates refs via the backend and sends the receive-pack result (unpack ok
/// + per-ref ok lines) to `writer`.
pub async fn update_refs_and_report<B: StorageBackend>(
    backend: &B,
    repo: &B::RepoId,
    ref_updates: &[RefUpdate],
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    backend
        .update_refs(repo, ref_updates)
        .context("updating refs")?;

    text_to_write(b"unpack ok", &mut *writer).await?;
    for update in ref_updates {
        let msg = format!("ok {}", update.refname);
        text_to_write(msg.as_bytes(), &mut *writer).await?;
    }
    flush_to_write(&mut *writer).await?;

    Ok(())
}

/// Writes the receive-pack ref advertisement to `writer`.  Expects pre-gathered
/// refs (e.g. from [`StorageBackend::list_refs`] via
/// [`RefsSnapshot::as_receive_pack`](crate::backend::RefsSnapshot::as_receive_pack)).
pub async fn info_refs_receive_pack_task(
    refs: Vec<(gix::ObjectId, String)>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let caps = b"report-status delete-refs agent=mizzle/dev";
    if refs.is_empty() {
        // Empty repo: advertise capabilities only.
        let null_oid = "0000000000000000000000000000000000000000";
        let mut line = Vec::new();
        line.extend_from_slice(null_oid.as_bytes());
        line.extend_from_slice(b" capabilities^{}");
        line.push(b'\0');
        line.extend_from_slice(caps);
        text_to_write(&line, &mut *writer).await?;
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
            text_to_write(&line, &mut *writer).await?;
        }
    }
    flush_to_write(&mut *writer).await?;
    Ok(())
}
