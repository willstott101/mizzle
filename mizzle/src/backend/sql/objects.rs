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

/// Fallible conversion from a database BLOB to an [`ObjectId`].
pub(super) fn parse_oid(bytes: &[u8]) -> Result<ObjectId> {
    ObjectId::try_from(bytes).map_err(|e| anyhow::anyhow!("corrupt OID in database: {e}"))
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
    // Check kind and length without deserialising the data column.
    let row: Option<(i32, i64)> =
        sqlx::query_as("SELECT kind, LENGTH(data) FROM objects WHERE repo_id = ? AND oid = ?")
            .bind(repo_db_id)
            .bind(oid_bytes(&oid))
            .fetch_optional(pool)
            .await
            .context("reading blob metadata")?;

    match row {
        Some((kind, len)) => {
            if kind != KIND_BLOB || (len as u64) > cap {
                return Ok(None);
            }
        }
        None => return Ok(None),
    }

    // Object exists, is a blob, and fits within cap — fetch the data.
    let (data,): (Vec<u8>,) =
        sqlx::query_as("SELECT data FROM objects WHERE repo_id = ? AND oid = ?")
            .bind(repo_db_id)
            .bind(oid_bytes(&oid))
            .fetch_one(pool)
            .await
            .context("reading blob data")?;

    Ok(Some(data))
}

/// Read raw object bytes regardless of kind, capped at `cap`. Returns
/// `Ok(None)` if the object is not found or larger than the cap.
pub(super) async fn read_object_raw(
    pool: &SqlitePool,
    repo_db_id: i64,
    oid: ObjectId,
    cap: u64,
) -> Result<Option<Vec<u8>>> {
    // Check length without deserialising the data column.
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT LENGTH(data) FROM objects WHERE repo_id = ? AND oid = ?")
            .bind(repo_db_id)
            .bind(oid_bytes(&oid))
            .fetch_optional(pool)
            .await
            .context("reading object length")?;

    match row {
        Some((len,)) if (len as u64) > cap => return Ok(None),
        Some(_) => {}
        None => return Ok(None),
    }

    // Object exists and fits within cap — fetch the data.
    let (data,): (Vec<u8>,) =
        sqlx::query_as("SELECT data FROM objects WHERE repo_id = ? AND oid = ?")
            .bind(repo_db_id)
            .bind(oid_bytes(&oid))
            .fetch_one(pool)
            .await
            .context("reading object data")?;

    Ok(Some(data))
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
        std::io::Read::read_exact(&mut file, &mut buf).context("reading pack header")?;
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
// Build pack
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Pack cache
// ---------------------------------------------------------------------------

/// Compute a deterministic cache key from wants, haves, and pack options.
///
/// The key is `SHA-256(repo_id || sorted_wants || 0x00 || sorted_haves || 0x01 || opts)`
/// as a hex string.
///
/// ## Correctness
///
/// Git objects are content-addressed: the set of objects reachable from a
/// fixed set of OIDs is immutable (objects are never mutated, only created or
/// GC'd).  Including the options in the key ensures that different
/// filter/deepen/thin_pack settings never share a cached pack.
fn pack_cache_key(
    repo_db_id: i64,
    want: &[ObjectId],
    have: &[ObjectId],
    opts: &crate::backend::PackOptions,
) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(repo_db_id.to_le_bytes());

    let mut sorted_wants: Vec<_> = want.to_vec();
    sorted_wants.sort();
    for oid in &sorted_wants {
        hasher.update(oid.as_bytes());
    }

    hasher.update([0x00]);

    let mut sorted_haves: Vec<_> = have.to_vec();
    sorted_haves.sort();
    for oid in &sorted_haves {
        hasher.update(oid.as_bytes());
    }

    // Pack options: deepen depth, filter variant, thin_pack flag.
    hasher.update([0x01]);
    hasher.update(opts.deepen.unwrap_or(0).to_le_bytes());
    let filter_tag: u8 = match &opts.filter {
        None => 0,
        Some(crate::backend::Filter::BlobNone) => 1,
        Some(crate::backend::Filter::TreeNone) => 2,
    };
    hasher.update([filter_tag]);
    hasher.update([opts.thin_pack as u8]);

    format!("{:x}", hasher.finalize())
}

/// Try to serve a pack from cache.  Returns `Some(PackOutput)` on cache hit.
fn try_cache_hit(
    pack_cache_dir: &std::path::Path,
    repo_db_id: i64,
    want: &[ObjectId],
    have: &[ObjectId],
    opts: &crate::backend::PackOptions,
) -> Option<crate::backend::PackOutput> {
    let key = pack_cache_key(repo_db_id, want, have, opts);
    let cache_path = pack_cache_dir
        .join(repo_db_id.to_string())
        .join(format!("{key}.pack"));

    if cache_path.exists() {
        let file = std::fs::File::open(&cache_path).ok()?;
        Some(crate::backend::PackOutput {
            reader: Box::new(std::io::BufReader::new(file)),
            shallow: Vec::new(),
            progress: None,
        })
    } else {
        None
    }
}

/// Write pack bytes to the cache.  Best-effort — errors are silently ignored.
fn write_to_cache(
    pack_cache_dir: &std::path::Path,
    repo_db_id: i64,
    want: &[ObjectId],
    have: &[ObjectId],
    opts: &crate::backend::PackOptions,
    data: &[u8],
) {
    let key = pack_cache_key(repo_db_id, want, have, opts);
    let dir = pack_cache_dir.join(repo_db_id.to_string());
    let _ = std::fs::create_dir_all(&dir);
    let cache_path = dir.join(format!("{key}.pack"));
    let _ = std::fs::write(&cache_path, data);
}

// ---------------------------------------------------------------------------
// Build pack
// ---------------------------------------------------------------------------

/// Build a pack containing objects reachable from `want` but not from `have`.
///
/// Checks the local filesystem pack cache first.  On miss, builds via a
/// temporary gitoxide repo (Phase 5 approach) and caches the result.
pub(super) async fn build_pack(
    pool: &SqlitePool,
    repo_db_id: i64,
    want: &[ObjectId],
    have: &[ObjectId],
    opts: &crate::backend::PackOptions,
    pack_cache_dir: &std::path::Path,
) -> Result<crate::backend::PackOutput> {
    // Phase 6: check cache.
    if let Some(output) = try_cache_hit(pack_cache_dir, repo_db_id, want, have, opts) {
        return Ok(output);
    }

    // 1. Enumerate commit OIDs: want-reachable minus have-reachable.
    let want_commits = super::graph::reachable_excluding(pool, repo_db_id, want, have, usize::MAX)
        .await
        .map_err(|e| anyhow::anyhow!("reachable walk for build_pack: {e}"))?;

    // 2. Walk trees of those commits to collect all unique OIDs.
    let mut needed_oids = std::collections::HashSet::new();
    for &oid in &want_commits {
        needed_oids.insert(oid);
    }
    // Also include the want tips themselves (they might be tags or non-commit objects).
    for oid in want {
        needed_oids.insert(*oid);
    }

    // For each commit, add its tree and recursively all subtrees/blobs.
    for &commit_oid in &want_commits {
        // Read the commit to get its tree OID.
        let row: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT data FROM objects WHERE repo_id = ? AND oid = ?")
                .bind(repo_db_id)
                .bind(oid_bytes(&commit_oid))
                .fetch_optional(pool)
                .await?;
        if let Some((data,)) = row {
            if let Ok(commit) = gix_object::CommitRef::from_bytes(&data) {
                if let Ok(tree_oid) = ObjectId::from_hex(commit.tree.as_ref()) {
                    collect_tree_oids(pool, repo_db_id, &tree_oid, &mut needed_oids).await?;
                }
            }
        }
    }

    // 3. Subtract objects reachable from have-side.
    //    For simplicity, we only subtract at the commit level (the have-side
    //    commits were already excluded by reachable_excluding above).  We also
    //    need to subtract trees/blobs of have-side commits.  Collect the tree
    //    closure of have tips.
    if !have.is_empty() {
        let have_commits =
            super::graph::reachable_excluding(pool, repo_db_id, have, &[], usize::MAX)
                .await
                .map_err(|e| anyhow::anyhow!("have-side walk: {e}"))?;
        let mut have_oids = std::collections::HashSet::new();
        for &oid in &have_commits {
            have_oids.insert(oid);
        }
        for &commit_oid in &have_commits {
            let row: Option<(Vec<u8>,)> =
                sqlx::query_as("SELECT data FROM objects WHERE repo_id = ? AND oid = ?")
                    .bind(repo_db_id)
                    .bind(oid_bytes(&commit_oid))
                    .fetch_optional(pool)
                    .await?;
            if let Some((data,)) = row {
                if let Ok(commit) = gix_object::CommitRef::from_bytes(&data) {
                    if let Ok(tree_oid) = ObjectId::from_hex(commit.tree.as_ref()) {
                        collect_tree_oids(pool, repo_db_id, &tree_oid, &mut have_oids).await?;
                    }
                }
            }
        }
        // Remove have objects from needed set.
        for oid in &have_oids {
            needed_oids.remove(oid);
        }
    }

    if needed_oids.is_empty() {
        let empty_pack = build_empty_pack();
        write_to_cache(pack_cache_dir, repo_db_id, want, have, opts, &empty_pack);
        return Ok(crate::backend::PackOutput {
            reader: Box::new(std::io::Cursor::new(empty_pack)),
            shallow: Vec::new(),
            progress: None,
        });
    }

    // 4. Bulk fetch needed objects from SQL.
    let mut fetched: Vec<(ObjectId, gix_object::Kind, Vec<u8>)> =
        Vec::with_capacity(needed_oids.len());
    for oid in &needed_oids {
        let row: Option<(i32, Vec<u8>)> =
            sqlx::query_as("SELECT kind, data FROM objects WHERE repo_id = ? AND oid = ?")
                .bind(repo_db_id)
                .bind(oid_bytes(oid))
                .fetch_optional(pool)
                .await?;
        if let Some((kind_int, data)) = row {
            let kind = match kind_int {
                KIND_BLOB => gix_object::Kind::Blob,
                KIND_TREE => gix_object::Kind::Tree,
                KIND_COMMIT => gix_object::Kind::Commit,
                KIND_TAG => gix_object::Kind::Tag,
                _ => continue,
            };
            fetched.push((*oid, kind, data));
        }
    }

    // 5-6. Write objects to temp repo and generate pack (CPU-bound).
    let cache_dir = pack_cache_dir.to_path_buf();
    let cache_key = pack_cache_key(repo_db_id, want, have, opts);

    let pack_bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        // Create a temp bare repo.
        let temp_dir = tempfile::tempdir().context("creating temp dir for pack build")?;
        let temp_repo_path = temp_dir.path().join("pack.git");
        let _repo = gix::init_bare(&temp_repo_path).context("init temp repo")?;

        // Write all objects as loose files.
        let objects_dir = temp_repo_path.join("objects");
        for (oid, kind, data) in &fetched {
            let hex = oid.to_hex().to_string();
            let (dir_part, file_part) = hex.split_at(2);
            let obj_dir = objects_dir.join(dir_part);
            std::fs::create_dir_all(&obj_dir)?;
            let obj_path = obj_dir.join(file_part);
            if obj_path.exists() {
                continue;
            }

            let kind_str = match kind {
                gix_object::Kind::Blob => "blob",
                gix_object::Kind::Tree => "tree",
                gix_object::Kind::Commit => "commit",
                gix_object::Kind::Tag => "tag",
            };
            let header = format!("{kind_str} {}\0", data.len());

            use std::io::Write;
            let file = std::fs::File::create(&obj_path)?;
            let mut encoder = flate2::write::ZlibEncoder::new(file, flate2::Compression::fast());
            encoder.write_all(header.as_bytes())?;
            encoder.write_all(data)?;
            encoder.finish()?;
        }

        // Open the temp repo with gitoxide and generate the pack.
        let repo = gix::open(&temp_repo_path).context("opening temp repo")?;
        let mut handle = repo.objects;
        handle.prevent_pack_unload();
        handle.ignore_replacements = true;

        let oids: Vec<ObjectId> = fetched.iter().map(|(oid, _, _)| *oid).collect();

        let should_interrupt = std::sync::atomic::AtomicBool::new(false);

        let (counts, _) = gix_pack::data::output::count::objects(
            handle.clone().into_inner(),
            Box::new(oids.into_iter().map(Ok)),
            &gix_features::progress::Discard,
            &should_interrupt,
            gix_pack::data::output::count::objects::Options {
                thread_limit: None,
                chunk_size: 16,
                input_object_expansion:
                    gix_pack::data::output::count::objects::ObjectExpansion::AsIs,
            },
        )?;
        let counts: Vec<_> = counts.into_iter().collect();
        let num_objects = counts.len();

        let mut in_order_entries =
            gix::parallel::InOrderIter::from(gix_pack::data::output::entry::iter_from_counts(
                counts,
                handle.into_inner(),
                Box::new(gix_features::progress::Discard),
                gix_pack::data::output::entry::iter_from_counts::Options {
                    thread_limit: None,
                    mode:
                        gix_pack::data::output::entry::iter_from_counts::Mode::PackCopyAndBaseObjects,
                    allow_thin_pack: false,
                    chunk_size: 16,
                    version: Default::default(),
                },
            ));

        let buf = ChunkBuffer::new();
        let mut pack_iter = gix_pack::data::output::bytes::FromEntriesIter::new(
            in_order_entries.by_ref(),
            &buf,
            num_objects as u32,
            Default::default(),
            gix_hash::Kind::default(),
        );

        let mut pack_data = Vec::new();
        for chunk_result in &mut pack_iter {
            chunk_result?;
            let chunk = buf.drain();
            pack_data.extend_from_slice(&chunk);
        }

        // Write to cache (best-effort).
        let dir = cache_dir.join(repo_db_id.to_string());
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(format!("{cache_key}.pack")), &pack_data);

        Ok(pack_data)
    })
    .await
    .map_err(|e| anyhow::anyhow!("pack build task panicked: {e}"))??;

    Ok(crate::backend::PackOutput {
        reader: Box::new(std::io::Cursor::new(pack_bytes)),
        shallow: Vec::new(),
        progress: None,
    })
}

/// Recursively collect all OIDs in a tree (the tree itself + all subtrees + blobs).
async fn collect_tree_oids(
    pool: &SqlitePool,
    repo_db_id: i64,
    tree_oid: &ObjectId,
    set: &mut std::collections::HashSet<ObjectId>,
) -> Result<()> {
    if !set.insert(*tree_oid) {
        return Ok(()); // already visited
    }

    let row: Option<(Vec<u8>,)> =
        sqlx::query_as("SELECT data FROM objects WHERE repo_id = ? AND oid = ? AND kind = ?")
            .bind(repo_db_id)
            .bind(oid_bytes(tree_oid))
            .bind(KIND_TREE)
            .fetch_optional(pool)
            .await?;

    let data = match row {
        Some((d,)) => d,
        None => return Ok(()), // tree not found (shouldn't happen, but defensive)
    };

    let tree = gix_object::TreeRef::from_bytes(&data)
        .with_context(|| format!("parsing tree {tree_oid}"))?;
    for entry in tree.entries {
        let child_oid = entry.oid.to_owned();
        if entry.mode.is_tree() {
            Box::pin(collect_tree_oids(pool, repo_db_id, &child_oid, set)).await?;
        } else {
            set.insert(child_oid);
        }
    }

    Ok(())
}

/// Build a minimal valid empty pack (header + SHA-1 checksum).
fn build_empty_pack() -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"PACK");
    buf.extend_from_slice(&2u32.to_be_bytes()); // version 2
    buf.extend_from_slice(&0u32.to_be_bytes()); // 0 objects
                                                // Trailing SHA-1 checksum of the 12-byte header.
    let mut hasher = gix_hash::hasher(gix_hash::Kind::Sha1);
    hasher.update(&buf);
    let hash = hasher.try_finalize().expect("SHA-1 finalize");
    buf.extend_from_slice(hash.as_bytes());
    buf
}

/// A write target that accumulates bytes between pack iterator steps.
struct ChunkBuffer {
    data: std::sync::Mutex<Vec<u8>>,
}

impl ChunkBuffer {
    fn new() -> Self {
        Self {
            data: std::sync::Mutex::new(Vec::new()),
        }
    }
    fn drain(&self) -> Vec<u8> {
        std::mem::take(&mut *self.data.lock().unwrap())
    }
}

impl std::io::Write for &ChunkBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.data.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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
