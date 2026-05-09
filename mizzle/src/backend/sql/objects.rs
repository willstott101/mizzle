//! Object read/write/has operations for the SQL backend.

use std::path::Path;

use anyhow::{Context, Result};
use gix::ObjectId;
use sqlx::SqlitePool;

use crate::backend::{ObjectKind, PackMetadata, PackObject};

/// Object kind integer constants matching the schema:
/// 0=blob, 1=tree, 2=commit, 3=tag.
const KIND_BLOB: i32 = 0;
const KIND_TREE: i32 = 1;
const KIND_COMMIT: i32 = 2;
const KIND_TAG: i32 = 3;

/// Return the OID's raw bytes as `&[u8]` for sqlx binding.
pub(super) fn oid_bytes(oid: &ObjectId) -> &[u8] {
    &oid.as_bytes()[..]
}

/// Check whether an object exists in the repository.
pub(super) async fn has_object(pool: &SqlitePool, repo_db_id: i64, oid: &ObjectId) -> Result<bool> {
    let row: (bool,) =
        sqlx::query_as("SELECT EXISTS(SELECT 1 FROM objects WHERE repo_id = ? AND oid = ?)")
            .bind(repo_db_id)
            .bind(oid_bytes(oid))
            .fetch_one(pool)
            .await
            .context("checking object existence")?;

    Ok(row.0)
}

/// Check which of the given OIDs exist in the repository.
///
/// Falls back to per-OID queries since SQLite doesn't support array
/// bind parameters. For large batches a future optimisation could use
/// a temp table or chunked `IN (...)` clauses.
pub(super) async fn has_objects(
    pool: &SqlitePool,
    repo_db_id: i64,
    oids: &[ObjectId],
) -> Result<Vec<bool>> {
    let mut results = Vec::with_capacity(oids.len());
    for oid in oids {
        results.push(has_object(pool, repo_db_id, oid).await?);
    }
    Ok(results)
}

/// Read raw blob bytes, capped at `cap`. Returns `Ok(None)` if the object
/// is not found, not a blob, or larger than the cap.
pub(super) async fn read_blob(
    pool: &SqlitePool,
    repo_db_id: i64,
    oid: ObjectId,
    cap: u64,
) -> Result<Option<Vec<u8>>> {
    let row: Option<(i32, Vec<u8>)> =
        sqlx::query_as("SELECT kind, data FROM objects WHERE repo_id = ? AND oid = ?")
            .bind(repo_db_id)
            .bind(oid_bytes(&oid))
            .fetch_optional(pool)
            .await
            .context("reading blob")?;

    match row {
        Some((kind, data)) => {
            if kind != KIND_BLOB {
                return Ok(None);
            }
            if (data.len() as u64) > cap {
                return Ok(None);
            }
            Ok(Some(data))
        }
        None => Ok(None),
    }
}

/// Read raw object bytes regardless of kind, capped at `cap`. Returns
/// `Ok(None)` if the object is not found or larger than the cap.
pub(super) async fn read_object_raw(
    pool: &SqlitePool,
    repo_db_id: i64,
    oid: ObjectId,
    cap: u64,
) -> Result<Option<Vec<u8>>> {
    let row: Option<(Vec<u8>,)> =
        sqlx::query_as("SELECT data FROM objects WHERE repo_id = ? AND oid = ?")
            .bind(repo_db_id)
            .bind(oid_bytes(&oid))
            .fetch_optional(pool)
            .await
            .context("reading object")?;

    match row {
        Some((data,)) => {
            if (data.len() as u64) > cap {
                return Ok(None);
            }
            Ok(Some(data))
        }
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Ingest
// ---------------------------------------------------------------------------

/// A single extracted object ready for SQL insertion.
struct ExtractedObject {
    oid: ObjectId,
    kind: i32,
    data: Vec<u8>,
    /// For commits: (parent_oid, position) pairs.
    parents: Vec<(ObjectId, i32)>,
}

/// Extract all objects from a pack file (CPU-bound, runs in `spawn_blocking`).
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

        // Full decompress — we need the raw data for SQL storage.
        let (data, _location) = bundle
            .get_object_by_index(index, &mut buf, &mut inflate, &mut cache)
            .context("decoding pack object")?;
        let raw = data.data.to_vec();

        let (sql_kind, obj_kind, parents) = match resolved_kind {
            gix_object::Kind::Blob => (KIND_BLOB, ObjectKind::Blob, Vec::new()),
            gix_object::Kind::Tree => (KIND_TREE, ObjectKind::Tree, Vec::new()),
            gix_object::Kind::Commit => {
                let info = crate::inspect::parse_commit_info(&raw, oid)?;
                let parents: Vec<(ObjectId, i32)> = info
                    .parents
                    .iter()
                    .enumerate()
                    .map(|(i, &p)| (p, i as i32))
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
            kind: sql_kind,
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

/// Ingest a staged pack file: extract objects, insert into SQL, return metadata.
///
/// Returns `None` if the pack contained zero objects.
pub(super) async fn ingest_pack(
    pool: &SqlitePool,
    repo_db_id: i64,
    staged_pack: &Path,
) -> Result<Option<(PackMetadata, Vec<ObjectId>)>> {
    // Check pack header for zero objects.
    let header = {
        let mut file = std::fs::File::open(staged_pack).context("opening staged pack")?;
        let mut buf = [0u8; 12];
        std::io::Read::read(&mut file, &mut buf).context("reading pack header")?;
        buf
    };
    if mizzle_proto::receive::pack_object_count(&header).unwrap_or(0) == 0 {
        return Ok(None);
    }

    // CPU-bound: extract all objects from the pack.
    let pack_path = staged_pack.to_path_buf();
    let (extracted, metadata) = tokio::task::spawn_blocking(move || extract_pack(&pack_path))
        .await
        .map_err(|e| anyhow::anyhow!("extract task panicked: {e}"))??;

    if extracted.is_empty() {
        return Ok(None);
    }

    // Batch insert into SQL.
    let mut inserted_oids = Vec::with_capacity(extracted.len());
    let mut tx = pool.begin().await.context("beginning ingest transaction")?;

    for obj in &extracted {
        sqlx::query("INSERT OR IGNORE INTO objects (repo_id, oid, kind, data) VALUES (?, ?, ?, ?)")
            .bind(repo_db_id)
            .bind(oid_bytes(&obj.oid))
            .bind(obj.kind)
            .bind(&obj.data[..])
            .execute(&mut *tx)
            .await
            .context("inserting object")?;

        inserted_oids.push(obj.oid);

        // Insert commit parents.
        for (parent_oid, position) in &obj.parents {
            sqlx::query(
                "INSERT OR IGNORE INTO commit_parents (repo_id, commit_oid, parent_oid, position) VALUES (?, ?, ?, ?)",
            )
            .bind(repo_db_id)
            .bind(oid_bytes(&obj.oid))
            .bind(oid_bytes(parent_oid))
            .bind(position)
            .execute(&mut *tx)
            .await
            .context("inserting commit parent")?;
        }
    }

    tx.commit().await.context("committing ingest transaction")?;

    Ok(Some((metadata, inserted_oids)))
}

// ---------------------------------------------------------------------------
// Tree diff
// ---------------------------------------------------------------------------

/// Compute the path-level diff between two trees.
///
/// Reads tree objects from SQL into an in-memory map, then uses
/// `gix_diff::tree` with a `Find` impl backed by that map.
pub(super) async fn tree_diff(
    pool: &SqlitePool,
    repo_db_id: i64,
    parent_tree: Option<ObjectId>,
    child_tree: ObjectId,
) -> Result<crate::auth_types::RefDiff> {
    use crate::auth_types::{RefDiffChange, RefDiffEntry};

    let empty_tree = ObjectId::empty_tree(gix_hash::Kind::Sha1);
    let lhs = parent_tree.unwrap_or(empty_tree);
    let rhs = child_tree;

    // Pre-fetch all tree objects reachable from both sides into memory.
    let mut store = MemObjectStore::new();
    fetch_trees_recursive(pool, repo_db_id, &lhs, &mut store).await?;
    fetch_trees_recursive(pool, repo_db_id, &rhs, &mut store).await?;

    // Run the diff synchronously (CPU-bound, but tree diffs are small).
    tokio::task::spawn_blocking(move || {
        use gix_diff::tree::{
            recorder::{Change as RecChange, Location},
            Recorder,
        };
        use gix_object::FindExt;

        let mut buf_l = Vec::new();
        let mut buf_r = Vec::new();
        let lhs_iter = store
            .find_tree_iter(&lhs, &mut buf_l)
            .with_context(|| format!("reading parent tree {lhs}"))?;
        let rhs_iter = store
            .find_tree_iter(&rhs, &mut buf_r)
            .with_context(|| format!("reading child tree {rhs}"))?;

        let mut state = gix_diff::tree::State::default();
        let mut recorder = Recorder::default().track_location(Some(Location::Path));

        gix_diff::tree(lhs_iter, rhs_iter, &mut state, &store, &mut recorder)
            .context("running tree diff")?;

        let mut entries = Vec::with_capacity(recorder.records.len());
        for rec in recorder.records {
            entries.push(match rec {
                RecChange::Addition {
                    entry_mode,
                    oid,
                    path,
                    ..
                } => RefDiffEntry {
                    path,
                    change: RefDiffChange::Added,
                    mode: u32::from(entry_mode.value()),
                    oid,
                },
                RecChange::Deletion {
                    entry_mode,
                    oid,
                    path,
                    ..
                } => RefDiffEntry {
                    path,
                    change: RefDiffChange::Removed,
                    mode: u32::from(entry_mode.value()),
                    oid,
                },
                RecChange::Modification {
                    entry_mode,
                    oid,
                    path,
                    ..
                } => RefDiffEntry {
                    path,
                    change: RefDiffChange::Modified,
                    mode: u32::from(entry_mode.value()),
                    oid,
                },
            });
        }

        Ok(crate::auth_types::RefDiff { entries })
    })
    .await
    .map_err(|e| anyhow::anyhow!("tree diff task panicked: {e}"))?
}

/// In-memory object store for tree diff operations.
struct MemObjectStore {
    objects: std::collections::HashMap<ObjectId, (gix_object::Kind, Vec<u8>)>,
}

impl MemObjectStore {
    fn new() -> Self {
        Self {
            objects: std::collections::HashMap::new(),
        }
    }
}

impl gix_object::Find for MemObjectStore {
    fn try_find<'a>(
        &self,
        id: &gix_hash::oid,
        buf: &'a mut Vec<u8>,
    ) -> Result<Option<gix_object::Data<'a>>, gix_object::find::Error> {
        let oid = id.to_owned();
        match self.objects.get(&oid) {
            Some((kind, data)) => {
                buf.clear();
                buf.extend_from_slice(data);
                Ok(Some(gix_object::Data {
                    kind: *kind,
                    data: buf,
                }))
            }
            None => Ok(None),
        }
    }
}

/// Recursively fetch a tree and all its subtrees from SQL into the store.
async fn fetch_trees_recursive(
    pool: &SqlitePool,
    repo_db_id: i64,
    oid: &ObjectId,
    store: &mut MemObjectStore,
) -> Result<()> {
    // Skip the well-known empty tree (no data in DB).
    let empty_tree = ObjectId::empty_tree(gix_hash::Kind::Sha1);
    if *oid == empty_tree {
        // Insert the empty tree so gix_diff::tree can find it.
        store
            .objects
            .insert(empty_tree, (gix_object::Kind::Tree, Vec::new()));
        return Ok(());
    }

    if store.objects.contains_key(oid) {
        return Ok(());
    }

    let row: Option<(i32, Vec<u8>)> =
        sqlx::query_as("SELECT kind, data FROM objects WHERE repo_id = ? AND oid = ?")
            .bind(repo_db_id)
            .bind(oid_bytes(oid))
            .fetch_optional(pool)
            .await
            .with_context(|| format!("fetching tree {oid}"))?;

    let (kind_int, data) = row.with_context(|| format!("tree {oid} not found"))?;
    if kind_int != KIND_TREE {
        anyhow::bail!("object {oid} is not a tree (kind={kind_int})");
    }

    store
        .objects
        .insert(*oid, (gix_object::Kind::Tree, data.clone()));

    // Parse tree entries and recurse into subtrees.
    let tree =
        gix_object::TreeRef::from_bytes(&data).with_context(|| format!("parsing tree {oid}"))?;
    for entry in tree.entries {
        if entry.mode.is_tree() {
            let child_oid = entry.oid.to_owned();
            // Box the recursive future to avoid infinite-size type.
            Box::pin(fetch_trees_recursive(pool, repo_db_id, &child_oid, store)).await?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Read operations
// ---------------------------------------------------------------------------

/// Read commit metadata for a commit already in the repository.
pub(super) async fn read_commit_info(
    pool: &SqlitePool,
    repo_db_id: i64,
    oid: ObjectId,
) -> Result<crate::auth_types::CommitInfo> {
    let row: Option<(i32, Vec<u8>)> =
        sqlx::query_as("SELECT kind, data FROM objects WHERE repo_id = ? AND oid = ?")
            .bind(repo_db_id)
            .bind(oid_bytes(&oid))
            .fetch_optional(pool)
            .await
            .context("reading commit")?;

    match row {
        Some((kind, data)) => {
            if kind != KIND_COMMIT {
                anyhow::bail!("object {oid} is not a commit (kind={kind})");
            }
            crate::inspect::parse_commit_info(&data, oid)
        }
        None => anyhow::bail!("commit {oid} not in object store"),
    }
}
