//! Pack assembly and tree diff for the KV backend.
//!
//! Same temp-gitoxide-repo trick as the SQL backend (`backend/sql/objects.rs`):
//!
//! 1. Walk the commit graph to find reachable commits.
//! 2. Walk each commit's tree to collect every reachable object.
//! 3. Subtract the have-side closure.
//! 4. Materialise the surviving objects as loose files in a temp bare repo.
//! 5. Hand the temp repo to `gix_pack::data::output` to write the pack.
//!
//! Worth lifting into a shared `backend/temp_pack_repo.rs` once both backends
//! are stable — out of scope for the PoC.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use gix::ObjectId;
use tikv_client::TransactionClient;

use super::{graph, keys, objects, txn};
use crate::auth_types::{RefDiff, RefDiffChange, RefDiffEntry};
use crate::backend::{pack_cache, PackOptions, PackOutput};

pub(super) async fn build_pack(
    db: &TransactionClient,
    repo_id: u64,
    want: &[ObjectId],
    have: &[ObjectId],
    opts: &PackOptions,
    cache_dir: &Path,
) -> Result<PackOutput> {
    let cache_key = pack_cache::key(repo_id, want, have, opts);
    if let Some(output) = pack_cache::try_hit(cache_dir, repo_id, &cache_key) {
        return Ok(output);
    }

    // 1. Want-reachable commits, minus have-reachable.
    let want_commits = graph::reachable_excluding(db, repo_id, want, have, usize::MAX)
        .await
        .map_err(|e| anyhow::anyhow!("reachable walk for build_pack: {e}"))?;

    // 2. Collect every reachable object: commits, tags, trees, blobs.
    let mut needed: HashSet<ObjectId> = HashSet::new();
    for &oid in &want_commits {
        needed.insert(oid);
    }
    // Tips may be tags or non-commit objects too.
    for oid in want {
        needed.insert(*oid);
    }

    for &commit_oid in &want_commits {
        if let Some(tree_oid) = read_commit_tree(db, repo_id, &commit_oid).await? {
            collect_tree_oids(db, repo_id, &tree_oid, &mut needed).await?;
        }
    }

    // 3. Subtract the have-side closure (commits, trees, blobs).
    if !have.is_empty() {
        let have_commits = graph::reachable_excluding(db, repo_id, have, &[], usize::MAX)
            .await
            .map_err(|e| anyhow::anyhow!("have-side walk: {e}"))?;
        let mut have_set: HashSet<ObjectId> = have_commits.iter().copied().collect();
        for &commit_oid in &have_commits {
            if let Some(tree_oid) = read_commit_tree(db, repo_id, &commit_oid).await? {
                collect_tree_oids(db, repo_id, &tree_oid, &mut have_set).await?;
            }
        }
        for oid in &have_set {
            needed.remove(oid);
        }
    }

    if needed.is_empty() {
        let empty_pack = build_empty_pack();
        pack_cache::write(cache_dir, repo_id, &cache_key, &empty_pack);
        return Ok(PackOutput {
            reader: Box::new(std::io::Cursor::new(empty_pack)),
            shallow: Vec::new(),
            progress: None,
        });
    }

    // 4. Bulk-fetch the surviving objects from KV.
    let mut fetched: Vec<(ObjectId, gix_object::Kind, Vec<u8>)> = Vec::with_capacity(needed.len());
    for oid in &needed {
        if let Some((kind, data)) = read_object_with_gix_kind(db, repo_id, oid).await? {
            fetched.push((*oid, kind, data));
        }
    }

    // 5-6. Materialise + pack on the blocking pool.
    let cache_dir_owned = cache_dir.to_path_buf();
    let cache_key_owned = cache_key.clone();
    let pack_bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let pack = assemble_pack(&fetched)?;
        pack_cache::write(&cache_dir_owned, repo_id, &cache_key_owned, &pack);
        Ok(pack)
    })
    .await
    .map_err(|e| anyhow::anyhow!("pack build task panicked: {e}"))??;

    Ok(PackOutput {
        reader: Box::new(std::io::Cursor::new(pack_bytes)),
        shallow: Vec::new(),
        progress: None,
    })
}

/// Read a commit and return its tree OID, if the OID is actually a commit.
///
/// A commit-shaped OID that fails to parse or whose tree field is malformed
/// is a data-corruption signal — we still surface the missing tree as `None`
/// so build_pack can skip it, but log loudly so the corruption gets noticed.
async fn read_commit_tree(
    db: &TransactionClient,
    repo_id: u64,
    commit_oid: &ObjectId,
) -> Result<Option<ObjectId>> {
    let Some(data) = objects::read_object_raw(db, repo_id, *commit_oid, u64::MAX).await? else {
        return Ok(None);
    };
    let commit = match gix_object::CommitRef::from_bytes(&data) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(%commit_oid, error = %e, "failed to parse commit object during build_pack");
            return Ok(None);
        }
    };
    match ObjectId::from_hex(commit.tree.as_ref()) {
        Ok(oid) => Ok(Some(oid)),
        Err(e) => {
            tracing::warn!(%commit_oid, error = %e, "commit has malformed tree oid during build_pack");
            Ok(None)
        }
    }
}

/// Recursively collect every OID reachable from `tree_oid` (inclusive).
async fn collect_tree_oids(
    db: &TransactionClient,
    repo_id: u64,
    tree_oid: &ObjectId,
    set: &mut HashSet<ObjectId>,
) -> Result<()> {
    let mut stack: Vec<ObjectId> = Vec::new();
    if set.insert(*tree_oid) {
        stack.push(*tree_oid);
    }

    while let Some(current) = stack.pop() {
        let data = match read_object_of_kind(db, repo_id, &current, objects::KIND_TREE).await? {
            Some(d) => d,
            None => continue,
        };
        let tree = gix_object::TreeRef::from_bytes(&data)
            .with_context(|| format!("parsing tree {current}"))?;
        for entry in tree.entries {
            let child = entry.oid.to_owned();
            if entry.mode.is_tree() {
                if set.insert(child) {
                    stack.push(child);
                }
            } else {
                set.insert(child);
            }
        }
    }
    Ok(())
}

/// Read an object only if it has the expected `kind` byte.
async fn read_object_of_kind(
    db: &TransactionClient,
    repo_id: u64,
    oid: &ObjectId,
    expected_kind: u8,
) -> Result<Option<Vec<u8>>> {
    let mut t = db
        .begin_optimistic()
        .await
        .context("begin txn for read_object_of_kind")?;
    let result: Result<Option<Vec<u8>>> = async {
        let value = t.get(keys::obj(repo_id, oid)).await.context("get obj")?;
        let Some(value) = value else {
            return Ok(None);
        };
        if value.is_empty() || value[0] != expected_kind {
            return Ok(None);
        }
        Ok(Some(value[1..].to_vec()))
    }
    .await;
    txn::finalize_read(&mut t, result).await
}

async fn read_object_with_gix_kind(
    db: &TransactionClient,
    repo_id: u64,
    oid: &ObjectId,
) -> Result<Option<(gix_object::Kind, Vec<u8>)>> {
    let mut t = db
        .begin_optimistic()
        .await
        .context("begin txn for read_object_with_gix_kind")?;
    let result: Result<Option<(gix_object::Kind, Vec<u8>)>> = async {
        let value = t.get(keys::obj(repo_id, oid)).await.context("get obj")?;
        let Some(value) = value else {
            return Ok(None);
        };
        if value.is_empty() {
            return Ok(None);
        }
        let kind = match value[0] {
            objects::KIND_BLOB => gix_object::Kind::Blob,
            objects::KIND_TREE => gix_object::Kind::Tree,
            objects::KIND_COMMIT => gix_object::Kind::Commit,
            objects::KIND_TAG => gix_object::Kind::Tag,
            _ => return Ok(None),
        };
        Ok(Some((kind, value[1..].to_vec())))
    }
    .await;
    txn::finalize_read(&mut t, result).await
}

/// Stage the fetched objects as loose files in a temp bare repo, then drive
/// `gix_pack::data::output` over them to produce pack bytes.
fn assemble_pack(fetched: &[(ObjectId, gix_object::Kind, Vec<u8>)]) -> Result<Vec<u8>> {
    let temp_dir = tempfile::tempdir().context("creating temp dir for pack build")?;
    let temp_repo_path = temp_dir.path().join("pack.git");
    let _repo = gix::init_bare(&temp_repo_path).context("init temp repo")?;
    let objects_dir = temp_repo_path.join("objects");

    for (oid, kind, data) in fetched {
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

    let repo = gix::open(&temp_repo_path).context("opening temp repo")?;
    let mut handle = repo.objects;
    handle.prevent_pack_unload();
    handle.ignore_replacements = true;

    let oids: Vec<ObjectId> = fetched.iter().map(|(oid, _, _)| *oid).collect();
    let should_interrupt = AtomicBool::new(false);

    let (counts, _) = gix_pack::data::output::count::objects(
        handle.clone().into_inner(),
        Box::new(oids.into_iter().map(Ok)),
        &gix_features::progress::Discard,
        &should_interrupt,
        gix_pack::data::output::count::objects::Options {
            thread_limit: None,
            chunk_size: 16,
            input_object_expansion: gix_pack::data::output::count::objects::ObjectExpansion::AsIs,
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
                mode: gix_pack::data::output::entry::iter_from_counts::Mode::PackCopyAndBaseObjects,
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

    Ok(pack_data)
}

/// Minimal valid empty pack (PACK header + 0 object count + SHA-1 of header).
fn build_empty_pack() -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"PACK");
    buf.extend_from_slice(&2u32.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());
    let mut hasher = gix_hash::hasher(gix_hash::Kind::Sha1);
    hasher.update(&buf);
    let hash = hasher.try_finalize().expect("SHA-1 finalize");
    buf.extend_from_slice(hash.as_bytes());
    buf
}

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

pub(super) async fn tree_diff(
    db: &TransactionClient,
    repo_id: u64,
    parent_tree: Option<ObjectId>,
    child_tree: ObjectId,
) -> Result<RefDiff> {
    let empty_tree = ObjectId::empty_tree(gix_hash::Kind::Sha1);
    let lhs = parent_tree.unwrap_or(empty_tree);
    let rhs = child_tree;

    let mut store = MemObjectStore::new();
    fetch_trees_recursive(db, repo_id, &lhs, &mut store).await?;
    fetch_trees_recursive(db, repo_id, &rhs, &mut store).await?;

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

        Ok(RefDiff { entries })
    })
    .await
    .map_err(|e| anyhow::anyhow!("tree diff task panicked: {e}"))?
}

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

async fn fetch_trees_recursive(
    db: &TransactionClient,
    repo_id: u64,
    oid: &ObjectId,
    store: &mut MemObjectStore,
) -> Result<()> {
    let empty_tree = ObjectId::empty_tree(gix_hash::Kind::Sha1);
    if *oid == empty_tree {
        store
            .objects
            .insert(empty_tree, (gix_object::Kind::Tree, Vec::new()));
        return Ok(());
    }

    if store.objects.contains_key(oid) {
        return Ok(());
    }

    let data = read_object_of_kind(db, repo_id, oid, objects::KIND_TREE)
        .await?
        .with_context(|| format!("tree {oid} not found"))?;
    store
        .objects
        .insert(*oid, (gix_object::Kind::Tree, data.clone()));

    let tree =
        gix_object::TreeRef::from_bytes(&data).with_context(|| format!("parsing tree {oid}"))?;
    for entry in tree.entries {
        if entry.mode.is_tree() {
            let child = entry.oid.to_owned();
            Box::pin(fetch_trees_recursive(db, repo_id, &child, store)).await?;
        }
    }

    Ok(())
}
