use anyhow::{Context, Result};
use futures_lite::AsyncWrite;
use gix::ObjectId;
use gix_packetline::async_io::encode::{flush_to_write, text_to_write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use crate::traits::PushKind;

pub use mizzle_proto::receive::{
    pack_object_count, preliminary_push_kind, read_receive_request, RefUpdate,
};

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

/// The pack and index files written by [`write_pack`].
///
/// If auth is denied after the pack has been written, call [`WrittenPack::delete`]
/// to remove them.  Undeleted packs leave orphan objects that are cleaned up
/// by the next `git gc`.
pub struct WrittenPack {
    pub pack: PathBuf,
    pub index: PathBuf,
}

impl WrittenPack {
    pub fn delete(self) {
        let _ = std::fs::remove_file(&self.index);
        let _ = std::fs::remove_file(&self.pack);
    }
}

/// Writes the received pack to the repository's object store and returns the
/// paths of the written files, or `None` if the pack contained no objects
/// (all referenced commits were already in the odb).
///
/// The pack is first written to a temporary directory inside `objects/` (same
/// filesystem), then atomically renamed into `objects/pack/`.  This lets the
/// caller call [`compute_push_kind`] with the full odb and then delete the pack
/// on auth failure rather than leaving orphaned objects behind forever.
pub fn write_pack(repo_path: &str, pack_data: &[u8]) -> Result<Option<WrittenPack>> {
    // Nothing to write if the pack has no objects — they're already in the odb.
    if pack_object_count(pack_data).unwrap_or(0) == 0 {
        return Ok(None);
    }

    let repo = gix::open(repo_path)?;
    let pack_dir = repo.path().join("objects").join("pack");
    std::fs::create_dir_all(&pack_dir)?;

    // Write into a temp dir in /tmp; use a cross-filesystem move when renaming.
    let temp_dir = tempfile::Builder::new()
        .prefix("mizzle_")
        .tempdir()
        .context("creating temp dir for pack")?;

    let mut progress = gix_features::progress::Discard;
    let interrupt = AtomicBool::new(false);

    gix_pack::Bundle::write_to_directory(
        &mut std::io::BufReader::new(pack_data),
        Some(temp_dir.path()),
        &mut progress,
        &interrupt,
        None::<gix::objs::find::Never>,
        Default::default(),
    )
    .context("indexing received pack")?;

    // Locate the .pack and .idx files written into the temp dir.
    let mut pack_src = None;
    let mut idx_src = None;
    for entry in std::fs::read_dir(temp_dir.path()).context("reading temp dir")? {
        let path = entry?.path();
        match path.extension().and_then(|e| e.to_str()) {
            Some("pack") => pack_src = Some(path),
            Some("idx") => idx_src = Some(path),
            _ => {}
        }
    }
    let pack_src = pack_src.context("no .pack file written")?;
    let idx_src = idx_src.context("no .idx file written")?;

    // Move into the real pack directory (copy+delete fallback for cross-fs moves).
    let pack_dst = pack_dir.join(pack_src.file_name().unwrap());
    let idx_dst = pack_dir.join(idx_src.file_name().unwrap());
    move_file(&pack_src, &pack_dst).context("moving pack file")?;
    move_file(&idx_src, &idx_dst).context("moving index file")?;

    Ok(Some(WrittenPack {
        pack: pack_dst,
        index: idx_dst,
    }))
}

/// Moves `src` to `dst`, falling back to copy+delete when they are on different
/// filesystems (which would cause `rename` to fail with EXDEV).
fn move_file(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dst).context("copying file cross-filesystem")?;
    std::fs::remove_file(src).context("removing source after copy")?;
    Ok(())
}

/// Initialises a bare repository at `repo_path` if none exists yet.
/// Called before the first push when [`crate::traits::RepoAccess::auto_init`]
/// returns `true`.
pub fn init_bare_if_missing(repo_path: &str) -> Result<()> {
    let path = std::path::Path::new(repo_path);
    if !path.exists() {
        gix::init_bare(path).with_context(|| format!("initialising bare repo at {repo_path}"))?;
    }
    Ok(())
}

/// Updates refs and sends the receive-pack result (unpack ok + per-ref ok lines)
/// to `writer`.  The pack must already have been written via [`write_pack`].
pub async fn update_refs_and_report(
    repo_path: &str,
    ref_updates: &[RefUpdate],
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    update_refs(repo_path, ref_updates).context("updating refs")?;

    text_to_write(b"unpack ok", &mut *writer).await?;
    for update in ref_updates {
        let msg = format!("ok {}", update.refname);
        text_to_write(msg.as_bytes(), &mut *writer).await?;
    }
    flush_to_write(&mut *writer).await?;

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

/// Gathers the refs to advertise for a receive-pack discovery request.
/// Called before spawning the response task so errors can be returned as a
/// proper HTTP 500 instead of a truncated stream.
pub fn gather_receive_pack_refs(repo_path: &str) -> Result<Vec<(ObjectId, String)>> {
    let repo = gix::open(repo_path)?;
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
}

/// Writes the receive-pack ref advertisement to `writer`.  Expects pre-gathered
/// refs from [`gather_receive_pack_refs`].
pub async fn info_refs_receive_pack_task(
    refs: Vec<(ObjectId, String)>,
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_init_bare_if_missing_creates_empty_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("test.git");
        assert!(!repo_path.exists());
        init_bare_if_missing(repo_path.to_str().unwrap()).unwrap();
        assert!(repo_path.exists());
        let repo = gix::open(&repo_path).unwrap();
        let refs: Vec<_> = repo.references().unwrap().all().unwrap().collect();
        assert!(refs.is_empty(), "freshly init'd repo should have no refs");
        // Calling again is a no-op (already exists)
        init_bare_if_missing(repo_path.to_str().unwrap()).unwrap();
    }
}
