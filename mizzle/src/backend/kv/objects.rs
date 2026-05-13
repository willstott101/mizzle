//! Object read / write / has / ingest for the KV backend.

use std::path::Path;

use anyhow::{Context, Result};
use gix::ObjectId;
use tikv_client::TransactionClient;

use super::keys;
use crate::backend::{ObjectKind, PackMetadata, PackObject};

/// Kind constants matching the SQL backend so we can copy / paste integration
/// code as needed.
pub(super) const KIND_BLOB: u8 = 0;
pub(super) const KIND_TREE: u8 = 1;
pub(super) const KIND_COMMIT: u8 = 2;
pub(super) const KIND_TAG: u8 = 3;

/// Encode `(kind, data)` into the value layout used in `("obj", repo, oid)`:
/// one prefix byte followed by the raw object bytes.
fn encode_object(kind: u8, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + data.len());
    v.push(kind);
    v.extend_from_slice(data);
    v
}

fn decode_object(value: &[u8]) -> Result<(u8, &[u8])> {
    if value.is_empty() {
        anyhow::bail!("object value is empty");
    }
    Ok((value[0], &value[1..]))
}

pub(super) async fn has_object(
    db: &TransactionClient,
    repo_id: u64,
    oid: &ObjectId,
) -> Result<bool> {
    let mut txn = db
        .begin_optimistic()
        .await
        .context("begin txn for has_object")?;
    let exists = txn
        .key_exists(keys::obj(repo_id, oid))
        .await
        .context("key_exists")?;
    txn.commit().await.ok();
    Ok(exists)
}

pub(super) async fn has_objects(
    db: &TransactionClient,
    repo_id: u64,
    oids: &[ObjectId],
) -> Result<Vec<bool>> {
    let mut txn = db
        .begin_optimistic()
        .await
        .context("begin txn for has_objects")?;
    let keys: Vec<Vec<u8>> = oids.iter().map(|o| keys::obj(repo_id, o)).collect();
    let found = txn
        .batch_get(keys.clone())
        .await
        .context("batch_get")?
        .map(|pair| pair.key().clone().into())
        .collect::<std::collections::HashSet<Vec<u8>>>();
    txn.commit().await.ok();
    Ok(keys.iter().map(|k| found.contains(k)).collect())
}

pub(super) async fn read_blob(
    db: &TransactionClient,
    repo_id: u64,
    oid: ObjectId,
    cap: u64,
) -> Result<Option<Vec<u8>>> {
    match read_object_with_kind(db, repo_id, oid, cap).await? {
        Some((KIND_BLOB, data)) => Ok(Some(data)),
        _ => Ok(None),
    }
}

pub(super) async fn read_object_raw(
    db: &TransactionClient,
    repo_id: u64,
    oid: ObjectId,
    cap: u64,
) -> Result<Option<Vec<u8>>> {
    match read_object_with_kind(db, repo_id, oid, cap).await? {
        Some((_, data)) => Ok(Some(data)),
        None => Ok(None),
    }
}

async fn read_object_with_kind(
    db: &TransactionClient,
    repo_id: u64,
    oid: ObjectId,
    cap: u64,
) -> Result<Option<(u8, Vec<u8>)>> {
    let mut txn = db
        .begin_optimistic()
        .await
        .context("begin txn for read_object")?;
    let value = txn.get(keys::obj(repo_id, &oid)).await.context("get obj")?;
    txn.commit().await.ok();

    let Some(value) = value else {
        return Ok(None);
    };
    let (kind, data) = decode_object(&value)?;
    if (data.len() as u64) > cap {
        return Ok(None);
    }
    Ok(Some((kind, data.to_vec())))
}

// ---------------------------------------------------------------------------
// Ingest
// ---------------------------------------------------------------------------

/// A single extracted object ready for KV insertion.
struct ExtractedObject {
    oid: ObjectId,
    kind: u8,
    data: Vec<u8>,
    parents: Vec<(ObjectId, u16)>,
}

/// Extract every object from a pack file (CPU-bound; runs in `spawn_blocking`).
///
/// Structurally identical to the SQL backend's extractor — we reuse
/// `crate::inspect::parse_commit_info` / `parse_tag_info` for the commit-graph
/// metadata path.  Worth lifting into `backend/pack_extract.rs` once both
/// backends are stable; out of scope for the PoC.
fn extract_pack(pack_path: &Path) -> Result<(Vec<ExtractedObject>, PackMetadata)> {
    use gix_pack::data::{decode::header::ResolvedBase, entry::Header};

    let bundle =
        gix_pack::Bundle::at(pack_path, gix_hash::Kind::Sha1).context("opening pack bundle")?;

    let num_objects = bundle.index.num_objects();
    if num_objects == 0 {
        return Ok((
            Vec::new(),
            PackMetadata {
                objects: Vec::new(),
            },
        ));
    }

    let mut extracted = Vec::with_capacity(num_objects as usize);
    let mut pack_objects = Vec::with_capacity(num_objects as usize);
    let mut buf = Vec::new();
    let mut inflate = gix_features::zlib::Inflate::default();
    let mut cache = gix_pack::cache::Never;

    let resolve = |oid: &gix_hash::oid| -> Option<ResolvedBase> {
        let idx = bundle.index.lookup(oid)?;
        let offset = bundle.index.pack_offset_at_index(idx);
        let entry = bundle.pack.entry(offset).ok()?;
        Some(ResolvedBase::InPack(entry))
    };

    for index in 0..num_objects {
        let oid = bundle.index.oid_at_index(index).to_owned();
        let offset = bundle.index.pack_offset_at_index(index);
        let entry = bundle
            .pack
            .entry(offset)
            .context("reading pack entry header")?;

        let resolved_kind = match entry.header {
            Header::Blob | Header::Tree | Header::Commit | Header::Tag => entry
                .header
                .as_kind()
                .expect("non-delta header always has a kind"),
            Header::OfsDelta { .. } | Header::RefDelta { .. } => {
                let outcome = bundle
                    .pack
                    .decode_header(entry, &mut inflate, &resolve)
                    .context("resolving delta header")?;
                outcome.kind
            }
        };

        let (data, _location) = bundle
            .get_object_by_index(index, &mut buf, &mut inflate, &mut cache)
            .context("decoding pack object")?;
        let raw = data.data.to_vec();

        let (kind_byte, obj_kind, parents) = match resolved_kind {
            gix_object::Kind::Blob => (KIND_BLOB, ObjectKind::Blob, Vec::new()),
            gix_object::Kind::Tree => (KIND_TREE, ObjectKind::Tree, Vec::new()),
            gix_object::Kind::Commit => {
                let info = crate::inspect::parse_commit_info(&raw, oid)?;
                let parents: Vec<(ObjectId, u16)> = info
                    .parents
                    .iter()
                    .enumerate()
                    .map(|(i, &p)| (p, i as u16))
                    .collect();
                (KIND_COMMIT, ObjectKind::Commit(info), parents)
            }
            gix_object::Kind::Tag => {
                let info = crate::inspect::parse_tag_info(&raw, oid)?;
                (KIND_TAG, ObjectKind::Tag(info), Vec::new())
            }
        };

        pack_objects.push(PackObject {
            oid,
            kind: obj_kind,
            size: raw.len() as u64,
        });
        extracted.push(ExtractedObject {
            oid,
            kind: kind_byte,
            data: raw,
            parents,
        });
    }

    Ok((
        extracted,
        PackMetadata {
            objects: pack_objects,
        },
    ))
}

pub(super) async fn ingest_pack(
    db: &TransactionClient,
    repo_id: u64,
    staged_pack: &Path,
) -> Result<Option<(PackMetadata, Vec<ObjectId>)>> {
    // Header check: skip empty packs without touching the KV.
    let header = {
        let mut f = std::fs::File::open(staged_pack).context("opening staged pack")?;
        let mut buf = [0u8; 12];
        std::io::Read::read_exact(&mut f, &mut buf).context("reading pack header")?;
        buf
    };
    if mizzle_proto::receive::pack_object_count(&header).unwrap_or(0) == 0 {
        return Ok(None);
    }

    let pack_path = staged_pack.to_path_buf();
    let (extracted, metadata) = tokio::task::spawn_blocking(move || extract_pack(&pack_path))
        .await
        .map_err(|e| anyhow::anyhow!("extract task panicked: {e}"))??;

    if extracted.is_empty() {
        return Ok(None);
    }

    // Single txn for the whole pack.  TiKV's per-txn limits are generous;
    // FDB needs to slice into smaller batches (out of scope for the PoC).
    let mut txn = db
        .begin_optimistic()
        .await
        .context("begin txn for ingest")?;

    let mut inserted_oids = Vec::with_capacity(extracted.len());
    for obj in &extracted {
        txn.put(
            keys::obj(repo_id, &obj.oid),
            encode_object(obj.kind, &obj.data),
        )
        .await
        .context("write object")?;
        inserted_oids.push(obj.oid);

        for (parent_oid, pos) in &obj.parents {
            txn.put(
                keys::par(repo_id, &obj.oid, *pos),
                parent_oid.as_bytes().to_vec(),
            )
            .await
            .context("write commit parent")?;
        }
    }

    txn.commit().await.context("commit ingest txn")?;

    Ok(Some((metadata, inserted_oids)))
}
