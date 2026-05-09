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

use anyhow::Result;
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
#[allow(dead_code)] // fields used starting in Phase 2
pub struct SqlBackend {
    pool: SqlitePool,
    /// Directory for cached pack files (see Phase 6).
    pack_cache_dir: PathBuf,
}

/// Per-request handle for an opened repository.
#[allow(dead_code)] // fields used starting in Phase 2
pub struct SqlRepo {
    pool: SqlitePool,
    /// Row id in the `repositories` table.
    repo_db_id: i64,
}

/// Opaque handle for an ingested pack, used for rollback on auth failure.
#[allow(dead_code)] // fields used starting in Phase 3
pub struct SqlIngestedPack {
    metadata: PackMetadata,
    /// OIDs inserted during this ingest (for potential rollback).
    inserted_oids: Vec<ObjectId>,
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
// StorageBackend impl — stubs (filled in Phases 2–6)
// ---------------------------------------------------------------------------

impl StorageBackend for SqlBackend {
    type RepoId = PathBuf;
    type Repo = SqlRepo;
    type IngestedPack = SqlIngestedPack;

    fn open(&self, _id: &PathBuf) -> impl std::future::Future<Output = Result<SqlRepo>> + Send {
        async { todo!("Phase 2") }
    }

    fn list_refs(
        &self,
        _repo: &SqlRepo,
    ) -> impl std::future::Future<Output = Result<RefsSnapshot>> + Send {
        async { todo!("Phase 2") }
    }

    fn resolve_ref(
        &self,
        _repo: &SqlRepo,
        _refname: &str,
    ) -> impl std::future::Future<Output = Result<Option<ObjectId>>> + Send {
        async { todo!("Phase 2") }
    }

    fn update_refs(
        &self,
        _repo: &SqlRepo,
        _updates: &[RefUpdate],
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        async { todo!("Phase 2") }
    }

    fn init_repo(&self, _repo: &PathBuf) -> impl std::future::Future<Output = Result<()>> + Send {
        async { todo!("Phase 2") }
    }

    fn has_object(
        &self,
        _repo: &SqlRepo,
        _oid: &ObjectId,
    ) -> impl std::future::Future<Output = Result<bool>> + Send {
        async { todo!("Phase 2") }
    }

    fn compute_push_kind(
        &self,
        _repo: &SqlRepo,
        _update: &RefUpdate,
    ) -> impl std::future::Future<Output = PushKind> + Send {
        async { todo!("Phase 4") }
    }

    fn build_pack(
        &self,
        _repo: &SqlRepo,
        _want: &[ObjectId],
        _have: &[ObjectId],
        _opts: &PackOptions,
    ) -> impl std::future::Future<Output = Result<PackOutput>> + Send {
        async { todo!("Phase 5") }
    }

    fn ingest_pack(
        &self,
        _repo: &SqlRepo,
        _staged_pack: &Path,
    ) -> impl std::future::Future<Output = Result<Option<SqlIngestedPack>>> + Send {
        async { todo!("Phase 3") }
    }

    fn inspect_ingested(
        &self,
        _pack: &SqlIngestedPack,
    ) -> impl std::future::Future<Output = Result<PackMetadata>> + Send {
        async { todo!("Phase 3") }
    }

    fn rollback_ingest(
        &self,
        _pack: SqlIngestedPack,
    ) -> impl std::future::Future<Output = ()> + Send {
        async { /* no-op — orphan objects are harmless */ }
    }

    fn reachable_excluding(
        &self,
        _repo: &SqlRepo,
        _from: &[ObjectId],
        _excluding: &[ObjectId],
        _cap: usize,
    ) -> impl std::future::Future<Output = std::result::Result<Vec<ObjectId>, ReachableError>> + Send
    {
        async { todo!("Phase 4") }
    }

    fn tree_diff(
        &self,
        _repo: &SqlRepo,
        _parent_tree: Option<ObjectId>,
        _child_tree: ObjectId,
    ) -> impl std::future::Future<Output = Result<RefDiff>> + Send {
        async { todo!("Phase 4") }
    }

    fn read_commit_info(
        &self,
        _repo: &SqlRepo,
        _oid: ObjectId,
    ) -> impl std::future::Future<Output = Result<CommitInfo>> + Send {
        async { todo!("Phase 2") }
    }

    fn read_blob(
        &self,
        _repo: &SqlRepo,
        _oid: ObjectId,
        _cap: u64,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>>> + Send {
        async { todo!("Phase 2") }
    }

    fn read_object_raw(
        &self,
        _repo: &SqlRepo,
        _oid: ObjectId,
        _cap: u64,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>>> + Send {
        async { todo!("Phase 2") }
    }
}
