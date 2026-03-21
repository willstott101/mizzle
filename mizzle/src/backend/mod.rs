//! Storage backend traits and types.
//!
//! The [`StorageBackend`] trait abstracts over how repository data is stored,
//! allowing alternative backends (filesystem with git CLI, SQL, distributed KV)
//! to be plugged in alongside the default [`FsGitoxide`](fs_gitoxide::FsGitoxide).

pub mod fs_git_cli;
pub mod fs_gitoxide;

use std::path::Path;

use anyhow::Result;
use gix::ObjectId;

use crate::traits::PushKind;

pub use mizzle_proto::pack::Filter;
pub use mizzle_proto::receive::RefUpdate;

/// Snapshot of a repository's refs.
pub struct RefsSnapshot {
    pub head: Option<HeadInfo>,
    pub refs: Vec<RefInfo>,
}

impl RefsSnapshot {
    /// All refs as (oid, name) pairs with HEAD first (for v1 upload-pack advertisement).
    pub fn as_upload_pack_v1(&self) -> Vec<(ObjectId, String)> {
        let mut result = Vec::new();
        if let Some(head) = &self.head {
            result.push((head.oid, "HEAD".to_string()));
        }
        for r in &self.refs {
            if r.name.starts_with("refs/") {
                result.push((r.oid, r.name.clone()));
            }
        }
        result
    }

    /// Only concrete refs (refs/*) as (oid, name) pairs (for receive-pack advertisement).
    pub fn as_receive_pack(&self) -> Vec<(ObjectId, String)> {
        self.refs
            .iter()
            .filter(|r| r.name.starts_with("refs/"))
            .map(|r| (r.oid, r.name.clone()))
            .collect()
    }
}

/// HEAD information.
pub struct HeadInfo {
    pub oid: ObjectId,
    /// The symbolic ref target (e.g. `refs/heads/main`).
    pub symref_target: Option<String>,
}

/// A single ref.
pub struct RefInfo {
    pub name: String,
    pub oid: ObjectId,
    /// Peeled OID for annotated tags (when the peeled ID differs from `oid`).
    pub peeled: Option<ObjectId>,
    /// Symbolic ref target (for non-HEAD symbolic refs).
    pub symref_target: Option<String>,
}

/// Options for pack generation.
pub struct PackOptions {
    pub deepen: Option<u32>,
    pub filter: Option<Filter>,
    pub thin_pack: bool,
}

/// Result of pack generation.
///
/// Pack data is streamed through `reader` — the caller reads chunks and
/// forwards them to the client.  Only the shallow boundary list is buffered.
pub struct PackOutput {
    /// Streaming reader that yields pack bytes incrementally.
    pub reader: Box<dyn std::io::Read + Send>,
    pub shallow: Vec<ObjectId>,
}

/// Thin storage trait abstracting over repository backends.
///
/// Every method is synchronous (CPU-bound for the filesystem backend).
/// Callers should use `spawn_blocking` for heavy operations like
/// [`build_pack`](StorageBackend::build_pack).
pub trait StorageBackend: Send + Sync + 'static {
    /// Identifier for a repository (e.g. [`PathBuf`](std::path::PathBuf) for
    /// filesystem backends).
    type RepoId: Send + Sync + Clone + 'static;

    /// Opaque handle for an ingested pack, used for rollback on auth failure.
    type IngestedPack: Send;

    /// List all refs in the repository.
    fn list_refs(&self, repo: &Self::RepoId) -> Result<RefsSnapshot>;

    /// Resolve a single ref name to its OID. Returns `None` if the ref does
    /// not exist.
    fn resolve_ref(&self, repo: &Self::RepoId, refname: &str) -> Result<Option<ObjectId>>;

    /// Update refs after a successful push.
    fn update_refs(&self, repo: &Self::RepoId, updates: &[RefUpdate]) -> Result<()>;

    /// Initialize a bare repository if it does not already exist.
    fn init_repo(&self, repo: &Self::RepoId) -> Result<()>;

    /// Check whether an object exists in the repository.
    fn has_object(&self, repo: &Self::RepoId, oid: &ObjectId) -> Result<bool>;

    /// Check which of the given OIDs exist in the repository.
    ///
    /// The default implementation calls [`has_object`](StorageBackend::has_object)
    /// in a loop.  Backends should override this to open the repo once.
    fn has_objects(&self, repo: &Self::RepoId, oids: &[ObjectId]) -> Result<Vec<bool>> {
        oids.iter().map(|oid| self.has_object(repo, oid)).collect()
    }

    /// Classify a ref update as create / delete / fast-forward / force-push.
    fn compute_push_kind(&self, repo: &Self::RepoId, update: &RefUpdate) -> PushKind;

    /// Build a pack containing objects reachable from `want` but not from
    /// `have`.
    fn build_pack(
        &self,
        repo: &Self::RepoId,
        want: &[ObjectId],
        have: &[ObjectId],
        opts: &PackOptions,
    ) -> Result<PackOutput>;

    /// Index a staged pack file into the repository's object store.
    /// Returns `None` if the pack contained no objects.
    fn ingest_pack(
        &self,
        repo: &Self::RepoId,
        staged_pack: &Path,
    ) -> Result<Option<Self::IngestedPack>>;

    /// Roll back a previously ingested pack (e.g. on auth failure).
    fn rollback_ingest(&self, pack: Self::IngestedPack);
}

/// Full-bypass trait for backends that handle the entire git protocol.
///
/// Not implemented in this phase — defined as a marker for the architecture.
pub trait BypassBackend: Send + Sync + 'static {
    type RepoId;
}
