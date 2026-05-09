//! SQL storage backend.
//!
//! Stores objects, refs, and the commit-parent graph in a SQL database
//! (initially SQLite via `sqlx`).  Pack files are cached on the local
//! filesystem.
//!
//! See `design/sql-backend-plan.md` for the phased implementation plan.

mod graph;
mod objects;
mod refs;
pub(crate) mod schema;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use gix::ObjectId;
use sqlx::SqlitePool;

use crate::auth_types::{CommitInfo, RefDiff};
use crate::backend::{
    PackMetadata, PackOptions, PackOutput, ReachableError, RefUpdate, RefsSnapshot, StorageBackend,
};
use crate::traits::PushKind;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// SQL storage backend.
///
/// `RepoId` is [`PathBuf`] for test-harness compatibility — the path has no
/// filesystem meaning; it is a unique key in the `repositories` table.
#[derive(Clone)]
pub struct SqlBackend {
    pool: SqlitePool,
    /// Directory for cached pack files (see Phase 6).
    #[allow(dead_code)] // used in Phase 6
    pack_cache_dir: PathBuf,
}

/// Per-request handle for an opened repository.
pub struct SqlRepo {
    pool: SqlitePool,
    /// Row id in the `repositories` table.
    repo_db_id: i64,
}

/// Opaque handle for an ingested pack, used for rollback on auth failure.
pub struct SqlIngestedPack {
    /// Wrapped in `Mutex<Option<…>>` so `inspect_ingested` (which takes `&self`)
    /// can move the metadata out exactly once.  The protocol never calls
    /// `inspect_ingested` more than once per ingest.
    metadata: Mutex<Option<PackMetadata>>,
    /// OIDs inserted during this ingest (for potential rollback).
    #[allow(dead_code)] // reserved for future GC
    inserted_oids: Vec<ObjectId>,
    #[allow(dead_code)] // reserved for future GC
    repo_db_id: i64,
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl SqlBackend {
    /// Create a new `SqlBackend` from an existing connection pool.
    ///
    /// Runs `ensure_schema` to create tables if they do not exist.
    pub async fn new(pool: SqlitePool, pack_cache_dir: PathBuf) -> Result<Self> {
        schema::ensure_schema(&pool).await?;
        Ok(Self {
            pool,
            pack_cache_dir,
        })
    }
}

// ---------------------------------------------------------------------------
// StorageBackend impl
// ---------------------------------------------------------------------------

impl StorageBackend for SqlBackend {
    type RepoId = PathBuf;
    type Repo = SqlRepo;
    type IngestedPack = SqlIngestedPack;

    fn open(&self, id: &PathBuf) -> impl std::future::Future<Output = Result<SqlRepo>> + Send {
        let pool = self.pool.clone();
        let path = id.to_string_lossy().to_string();
        async move {
            let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM repositories WHERE path = ?")
                .bind(&path)
                .fetch_optional(&pool)
                .await
                .context("looking up repository")?;

            match row {
                Some((db_id,)) => Ok(SqlRepo {
                    pool,
                    repo_db_id: db_id,
                }),
                None => anyhow::bail!("{path:?} does not appear to be a git repository"),
            }
        }
    }

    fn init_repo(&self, repo: &PathBuf) -> impl std::future::Future<Output = Result<()>> + Send {
        let pool = self.pool.clone();
        let path = repo.to_string_lossy().to_string();
        async move {
            sqlx::query("INSERT OR IGNORE INTO repositories (path) VALUES (?)")
                .bind(&path)
                .execute(&pool)
                .await
                .context("initialising repository")?;
            Ok(())
        }
    }

    fn list_refs(
        &self,
        repo: &SqlRepo,
    ) -> impl std::future::Future<Output = Result<RefsSnapshot>> + Send {
        let pool = repo.pool.clone();
        let db_id = repo.repo_db_id;
        async move { refs::list_refs(&pool, db_id).await }
    }

    fn resolve_ref(
        &self,
        repo: &SqlRepo,
        refname: &str,
    ) -> impl std::future::Future<Output = Result<Option<ObjectId>>> + Send {
        let pool = repo.pool.clone();
        let db_id = repo.repo_db_id;
        let refname = refname.to_string();
        async move { refs::resolve_ref(&pool, db_id, &refname).await }
    }

    fn update_refs(
        &self,
        repo: &SqlRepo,
        updates: &[RefUpdate],
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        // Copy the update data into owned tuples so the future is 'static + Send.
        let owned: Vec<(ObjectId, ObjectId, String)> = updates
            .iter()
            .map(|u| (u.old_oid, u.new_oid, u.refname.clone()))
            .collect();
        let pool = repo.pool.clone();
        let db_id = repo.repo_db_id;
        async move { refs::update_refs_owned(&pool, db_id, &owned).await }
    }

    fn has_object(
        &self,
        repo: &SqlRepo,
        oid: &ObjectId,
    ) -> impl std::future::Future<Output = Result<bool>> + Send {
        let pool = repo.pool.clone();
        let db_id = repo.repo_db_id;
        let oid = *oid;
        async move { objects::has_object(&pool, db_id, &oid).await }
    }

    fn has_objects(
        &self,
        repo: &SqlRepo,
        oids: &[ObjectId],
    ) -> impl std::future::Future<Output = Result<Vec<bool>>> + Send {
        let pool = repo.pool.clone();
        let db_id = repo.repo_db_id;
        let oids = oids.to_vec();
        async move { objects::has_objects(&pool, db_id, &oids).await }
    }

    fn read_commit_info(
        &self,
        repo: &SqlRepo,
        oid: ObjectId,
    ) -> impl std::future::Future<Output = Result<CommitInfo>> + Send {
        let pool = repo.pool.clone();
        let db_id = repo.repo_db_id;
        async move { objects::read_commit_info(&pool, db_id, oid).await }
    }

    fn read_blob(
        &self,
        repo: &SqlRepo,
        oid: ObjectId,
        cap: u64,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>>> + Send {
        let pool = repo.pool.clone();
        let db_id = repo.repo_db_id;
        async move { objects::read_blob(&pool, db_id, oid, cap).await }
    }

    fn read_object_raw(
        &self,
        repo: &SqlRepo,
        oid: ObjectId,
        cap: u64,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>>> + Send {
        let pool = repo.pool.clone();
        let db_id = repo.repo_db_id;
        async move { objects::read_object_raw(&pool, db_id, oid, cap).await }
    }

    fn compute_push_kind(
        &self,
        repo: &SqlRepo,
        update: &RefUpdate,
    ) -> impl std::future::Future<Output = PushKind> + Send {
        let pool = repo.pool.clone();
        let db_id = repo.repo_db_id;
        let old_oid = update.old_oid;
        let new_oid = update.new_oid;
        async move { graph::compute_push_kind(&pool, db_id, old_oid, new_oid).await }
    }

    fn build_pack(
        &self,
        repo: &SqlRepo,
        want: &[ObjectId],
        have: &[ObjectId],
        _opts: &PackOptions,
    ) -> impl std::future::Future<Output = Result<PackOutput>> + Send {
        let pool = repo.pool.clone();
        let db_id = repo.repo_db_id;
        let want = want.to_vec();
        let have = have.to_vec();
        async move { objects::build_pack(&pool, db_id, &want, &have).await }
    }

    fn ingest_pack(
        &self,
        repo: &SqlRepo,
        staged_pack: &Path,
    ) -> impl std::future::Future<Output = Result<Option<SqlIngestedPack>>> + Send {
        let pool = repo.pool.clone();
        let db_id = repo.repo_db_id;
        let staged = staged_pack.to_path_buf();
        async move {
            match objects::ingest_pack(&pool, db_id, &staged).await? {
                Some((metadata, inserted_oids)) => Ok(Some(SqlIngestedPack {
                    metadata: Mutex::new(Some(metadata)),
                    inserted_oids,
                    repo_db_id: db_id,
                })),
                None => Ok(None),
            }
        }
    }

    fn inspect_ingested(
        &self,
        pack: &SqlIngestedPack,
    ) -> impl std::future::Future<Output = Result<PackMetadata>> + Send {
        // Take the pre-computed metadata out of the Mutex.  The protocol
        // only calls inspect_ingested once per ingest.
        let metadata = pack
            .metadata
            .lock()
            .expect("metadata mutex poisoned")
            .take()
            .expect("inspect_ingested called more than once");
        async move { Ok(metadata) }
    }

    fn rollback_ingest(
        &self,
        _pack: SqlIngestedPack,
    ) -> impl std::future::Future<Output = ()> + Send {
        async { /* no-op — orphan objects are harmless */ }
    }

    fn reachable_excluding(
        &self,
        repo: &SqlRepo,
        from: &[ObjectId],
        excluding: &[ObjectId],
        cap: usize,
    ) -> impl std::future::Future<Output = std::result::Result<Vec<ObjectId>, ReachableError>> + Send
    {
        let pool = repo.pool.clone();
        let db_id = repo.repo_db_id;
        let from = from.to_vec();
        let excluding = excluding.to_vec();
        async move { graph::reachable_excluding(&pool, db_id, &from, &excluding, cap).await }
    }

    fn tree_diff(
        &self,
        repo: &SqlRepo,
        parent_tree: Option<ObjectId>,
        child_tree: ObjectId,
    ) -> impl std::future::Future<Output = Result<RefDiff>> + Send {
        let pool = repo.pool.clone();
        let db_id = repo.repo_db_id;
        async move { objects::tree_diff(&pool, db_id, parent_tree, child_tree).await }
    }
}
