//! Transactional KV storage backend (TiKV-targeted PoC).
//!
//! Companion to [`SqlBackend`](super::sql::SqlBackend); same trait surface,
//! different storage engine.  See `design/kv-backend-plan.md` for the full
//! design rationale and key layout.
//!
//! This PoC implements the "spine" — `init_repo`, `open`, refs, object CRUD,
//! `ingest_pack`, `inspect_ingested` — and the fetch path (`build_pack`,
//! graph traversal, `tree_diff`).  FDB-style chunking and progress reporting
//! are out of scope.

mod build_pack;
mod graph;
mod keys;
mod objects;
mod refs;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use gix::ObjectId;
use tikv_client::{Config, TransactionClient};

use crate::auth_types::{CommitInfo, RefDiff};
use crate::backend::{
    PackMetadata, PackOptions, PackOutput, ReachableError, RefUpdate, RefsSnapshot, StorageBackend,
};
use crate::traits::PushKind;

/// Transactional KV backend.
///
/// `db` is shared by-clone across all per-request handles — `TransactionClient`
/// is itself an Arc-wrapped pool internally.
#[derive(Clone)]
pub struct KvBackend {
    db: TransactionClient,
    pack_cache_dir: PathBuf,
}

/// Per-request handle for an opened repository.
pub struct KvRepo {
    db: TransactionClient,
    repo_id: u64,
}

/// Opaque handle for an ingested pack, used for rollback on auth failure.
pub struct KvIngestedPack {
    /// `Mutex<Option<…>>` so `inspect_ingested(&self)` can move the metadata
    /// out exactly once — same pattern as the SQL backend.
    metadata: Mutex<Option<PackMetadata>>,
    #[allow(dead_code)] // reserved for future rollback / GC
    inserted_oids: Vec<ObjectId>,
    #[allow(dead_code)]
    repo_id: u64,
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl KvBackend {
    /// Connect to a TiKV cluster via its PD endpoints.
    pub async fn connect(pd_endpoints: Vec<String>, pack_cache_dir: PathBuf) -> Result<Self> {
        let db = TransactionClient::new(pd_endpoints)
            .await
            .context("connecting to TiKV PD")?;
        Ok(Self { db, pack_cache_dir })
    }

    /// Connect with an explicit [`Config`] (TLS, timeouts, etc.).
    pub async fn connect_with_config(
        pd_endpoints: Vec<String>,
        config: Config,
        pack_cache_dir: PathBuf,
    ) -> Result<Self> {
        let db = TransactionClient::new_with_config(pd_endpoints, config)
            .await
            .context("connecting to TiKV PD")?;
        Ok(Self { db, pack_cache_dir })
    }

    /// Wrap an already-connected client (test harnesses, advanced setup).
    pub fn from_client(db: TransactionClient, pack_cache_dir: PathBuf) -> Self {
        Self { db, pack_cache_dir }
    }

    /// Returns the pack cache directory.  Intended for test assertions.
    pub fn pack_cache_dir(&self) -> &Path {
        &self.pack_cache_dir
    }
}

// ---------------------------------------------------------------------------
// init / open
// ---------------------------------------------------------------------------

/// Atomically allocate (or look up) a `repo_id` for `path` and mark the
/// repository as existing.  Idempotent.
async fn init_repo_inner(db: &TransactionClient, path: &str) -> Result<u64> {
    let mut txn = db
        .begin_pessimistic()
        .await
        .context("begin txn for init_repo")?;

    // Fast path: already initialised.
    if let Some(bytes) = txn
        .get(keys::repo_index(path))
        .await
        .context("read repo_index")?
    {
        let id = decode_u64(&bytes).context("decoding repo_id")?;
        txn.rollback().await.ok();
        return Ok(id);
    }

    // Slow path: allocate next id under the counter, then write both keys.
    let counter_key = keys::next_repo_id();
    let next = match txn
        .get_for_update(counter_key.clone())
        .await
        .context("read repo_id counter")?
    {
        Some(b) => decode_u64(&b).context("decoding repo_id counter")?,
        None => 1, // 0 is reserved.
    };

    txn.put(counter_key, encode_u64(next + 1))
        .await
        .context("write repo_id counter")?;
    txn.put(keys::repo_index(path), encode_u64(next))
        .await
        .context("write repo_index")?;
    txn.put(keys::repo_meta(next, keys::REPO_META_EXISTS), Vec::new())
        .await
        .context("mark repo as existing")?;

    txn.commit().await.context("commit init_repo txn")?;
    Ok(next)
}

async fn open_inner(db: &TransactionClient, path: &str) -> Result<u64> {
    let mut txn = db.begin_optimistic().await.context("begin txn for open")?;
    let result = txn
        .get(keys::repo_index(path))
        .await
        .context("read repo_index")?;
    txn.commit().await.ok();

    match result {
        Some(bytes) => decode_u64(&bytes).context("decoding repo_id"),
        None => anyhow::bail!("{path:?} does not appear to be a git repository"),
    }
}

fn encode_u64(v: u64) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}

fn decode_u64(bytes: &[u8]) -> Result<u64> {
    let arr: [u8; 8] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected 8-byte u64, got {} bytes", bytes.len()))?;
    Ok(u64::from_be_bytes(arr))
}

// ---------------------------------------------------------------------------
// StorageBackend impl
// ---------------------------------------------------------------------------

impl StorageBackend for KvBackend {
    type RepoId = PathBuf;
    type Repo = KvRepo;
    type IngestedPack = KvIngestedPack;

    fn open(&self, id: &PathBuf) -> impl std::future::Future<Output = Result<KvRepo>> + Send {
        let db = self.db.clone();
        let path = id.to_string_lossy().to_string();
        async move {
            let repo_id = open_inner(&db, &path).await?;
            Ok(KvRepo { db, repo_id })
        }
    }

    fn init_repo(&self, repo: &PathBuf) -> impl std::future::Future<Output = Result<()>> + Send {
        let db = self.db.clone();
        let path = repo.to_string_lossy().to_string();
        async move {
            init_repo_inner(&db, &path).await?;
            Ok(())
        }
    }

    fn list_refs(
        &self,
        repo: &KvRepo,
    ) -> impl std::future::Future<Output = Result<RefsSnapshot>> + Send {
        let db = repo.db.clone();
        let id = repo.repo_id;
        async move { refs::list_refs(&db, id).await }
    }

    fn resolve_ref(
        &self,
        repo: &KvRepo,
        refname: &str,
    ) -> impl std::future::Future<Output = Result<Option<ObjectId>>> + Send {
        let db = repo.db.clone();
        let id = repo.repo_id;
        let refname = refname.to_string();
        async move { refs::resolve_ref(&db, id, &refname).await }
    }

    fn update_refs(
        &self,
        repo: &KvRepo,
        updates: &[RefUpdate],
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        // RefUpdate isn't Clone; copy field-by-field so the future is 'static.
        let owned: Vec<(ObjectId, ObjectId, String)> = updates
            .iter()
            .map(|u| (u.old_oid, u.new_oid, u.refname.clone()))
            .collect();
        let db = repo.db.clone();
        let id = repo.repo_id;
        async move { refs::update_refs(&db, id, &owned).await }
    }

    fn has_object(
        &self,
        repo: &KvRepo,
        oid: &ObjectId,
    ) -> impl std::future::Future<Output = Result<bool>> + Send {
        let db = repo.db.clone();
        let id = repo.repo_id;
        let oid = *oid;
        async move { objects::has_object(&db, id, &oid).await }
    }

    fn has_objects(
        &self,
        repo: &KvRepo,
        oids: &[ObjectId],
    ) -> impl std::future::Future<Output = Result<Vec<bool>>> + Send {
        let db = repo.db.clone();
        let id = repo.repo_id;
        let oids = oids.to_vec();
        async move { objects::has_objects(&db, id, &oids).await }
    }

    fn read_commit_info(
        &self,
        repo: &KvRepo,
        oid: ObjectId,
    ) -> impl std::future::Future<Output = Result<CommitInfo>> + Send {
        let db = repo.db.clone();
        let id = repo.repo_id;
        async move { refs::read_commit_info(&db, id, oid).await }
    }

    fn read_blob(
        &self,
        repo: &KvRepo,
        oid: ObjectId,
        cap: u64,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>>> + Send {
        let db = repo.db.clone();
        let id = repo.repo_id;
        async move { objects::read_blob(&db, id, oid, cap).await }
    }

    fn read_object_raw(
        &self,
        repo: &KvRepo,
        oid: ObjectId,
        cap: u64,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>>> + Send {
        let db = repo.db.clone();
        let id = repo.repo_id;
        async move { objects::read_object_raw(&db, id, oid, cap).await }
    }

    fn compute_push_kind(
        &self,
        repo: &KvRepo,
        update: &RefUpdate,
    ) -> impl std::future::Future<Output = PushKind> + Send {
        let db = repo.db.clone();
        let id = repo.repo_id;
        let old_oid = update.old_oid;
        let new_oid = update.new_oid;
        async move { graph::compute_push_kind(&db, id, old_oid, new_oid).await }
    }

    fn build_pack(
        &self,
        repo: &KvRepo,
        want: &[ObjectId],
        have: &[ObjectId],
        opts: &PackOptions,
    ) -> impl std::future::Future<Output = Result<PackOutput>> + Send {
        let db = repo.db.clone();
        let id = repo.repo_id;
        let want = want.to_vec();
        let have = have.to_vec();
        let opts = PackOptions {
            deepen: opts.deepen,
            filter: opts.filter.clone(),
            thin_pack: opts.thin_pack,
        };
        let cache_dir = self.pack_cache_dir.clone();
        async move { build_pack::build_pack(&db, id, &want, &have, &opts, &cache_dir).await }
    }

    fn ingest_pack(
        &self,
        repo: &KvRepo,
        staged_pack: &Path,
    ) -> impl std::future::Future<Output = Result<Option<KvIngestedPack>>> + Send {
        let db = repo.db.clone();
        let id = repo.repo_id;
        let staged = staged_pack.to_path_buf();
        async move {
            match objects::ingest_pack(&db, id, &staged).await? {
                Some((metadata, inserted_oids)) => Ok(Some(KvIngestedPack {
                    metadata: Mutex::new(Some(metadata)),
                    inserted_oids,
                    repo_id: id,
                })),
                None => Ok(None),
            }
        }
    }

    fn inspect_ingested(
        &self,
        pack: &KvIngestedPack,
    ) -> impl std::future::Future<Output = Result<PackMetadata>> + Send {
        let result = pack
            .metadata
            .lock()
            .map_err(|_| anyhow::anyhow!("metadata mutex poisoned"))
            .and_then(|mut guard| {
                guard
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("inspect_ingested called more than once"))
            });
        async move { result }
    }

    fn rollback_ingest(
        &self,
        _pack: KvIngestedPack,
    ) -> impl std::future::Future<Output = ()> + Send {
        async { /* no-op: orphan objects are harmless */ }
    }

    fn reachable_excluding(
        &self,
        repo: &KvRepo,
        from: &[ObjectId],
        excluding: &[ObjectId],
        cap: usize,
    ) -> impl std::future::Future<Output = std::result::Result<Vec<ObjectId>, ReachableError>> + Send
    {
        let db = repo.db.clone();
        let id = repo.repo_id;
        let from = from.to_vec();
        let excluding = excluding.to_vec();
        async move { graph::reachable_excluding(&db, id, &from, &excluding, cap).await }
    }

    fn tree_diff(
        &self,
        repo: &KvRepo,
        parent_tree: Option<ObjectId>,
        child_tree: ObjectId,
    ) -> impl std::future::Future<Output = Result<RefDiff>> + Send {
        let db = repo.db.clone();
        let id = repo.repo_id;
        async move { build_pack::tree_diff(&db, id, parent_tree, child_tree).await }
    }
}
