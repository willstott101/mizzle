//! Storage backend traits and types.
//!
//! The [`StorageBackend`] trait abstracts over how repository data is stored,
//! allowing alternative backends (filesystem with git CLI, SQL, distributed KV)
//! to be plugged in alongside the default [`FsGitoxide`](fs_gitoxide::FsGitoxide).

pub mod fs_git_cli;
pub mod fs_gitoxide;

use std::path::Path;
use std::sync::mpsc;

use anyhow::Result;
use gix::ObjectId;

use crate::auth_types::{CommitInfo, RefDiff};
use crate::traits::PushKind;

pub use crate::auth_types::TagInfo;
pub use mizzle_proto::pack::Filter;
pub use mizzle_proto::receive::RefUpdate;

// ---------------------------------------------------------------------------
// Pack inspection types
// ---------------------------------------------------------------------------

/// Metadata extracted from an ingested pack for auth inspection.
pub struct PackMetadata {
    pub objects: Vec<PackObject>,
}

impl PackMetadata {
    /// Iterate parsed commits in the order they appear in the pack.
    pub fn commits(&self) -> impl Iterator<Item = &CommitInfo> {
        self.objects.iter().filter_map(|o| match &o.kind {
            ObjectKind::Commit(c) => Some(c),
            _ => None,
        })
    }

    /// Iterate parsed tags in the order they appear in the pack.
    pub fn tags(&self) -> impl Iterator<Item = &TagInfo> {
        self.objects.iter().filter_map(|o| match &o.kind {
            ObjectKind::Tag(t) => Some(t),
            _ => None,
        })
    }

    /// O(n) lookup of a commit by oid.
    pub fn find_commit(&self, oid: &ObjectId) -> Option<&CommitInfo> {
        self.commits().find(|c| &c.oid == oid)
    }
}

/// A single object from an ingested pack.
pub struct PackObject {
    pub oid: ObjectId,
    pub kind: ObjectKind,
    pub size: u64,
}

/// Object kind with extracted metadata for commits and tags.
pub enum ObjectKind {
    Blob,
    Tree,
    Commit(CommitInfo),
    Tag(TagInfo),
}

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
    /// Optional progress messages (e.g. "Counting objects: 42\n").
    /// Backends that don't support progress return `None`.
    pub progress: Option<mpsc::Receiver<String>>,
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

    /// Per-request handle for an opened repository.
    ///
    /// Callers open once via [`open`](StorageBackend::open) and pass the handle
    /// to all subsequent methods within the same request.
    type Repo: Send + Sync;

    /// Opaque handle for an ingested pack, used for rollback on auth failure.
    type IngestedPack: Send;

    /// Open a repository, returning a reusable handle.
    fn open(&self, id: &Self::RepoId) -> Result<Self::Repo>;

    /// List all refs in the repository.
    fn list_refs(&self, repo: &Self::Repo) -> Result<RefsSnapshot>;

    /// Resolve a single ref name to its OID. Returns `None` if the ref does
    /// not exist.
    fn resolve_ref(&self, repo: &Self::Repo, refname: &str) -> Result<Option<ObjectId>>;

    /// Update refs after a successful push.
    fn update_refs(&self, repo: &Self::Repo, updates: &[RefUpdate]) -> Result<()>;

    /// Initialize a bare repository if it does not already exist.
    fn init_repo(&self, repo: &Self::RepoId) -> Result<()>;

    /// Check whether an object exists in the repository.
    fn has_object(&self, repo: &Self::Repo, oid: &ObjectId) -> Result<bool>;

    /// Check which of the given OIDs exist in the repository.
    ///
    /// The default implementation calls [`has_object`](StorageBackend::has_object)
    /// in a loop.
    fn has_objects(&self, repo: &Self::Repo, oids: &[ObjectId]) -> Result<Vec<bool>> {
        oids.iter().map(|oid| self.has_object(repo, oid)).collect()
    }

    /// Classify a ref update as create / delete / fast-forward / force-push.
    fn compute_push_kind(&self, repo: &Self::Repo, update: &RefUpdate) -> PushKind;

    /// Build a pack containing objects reachable from `want` but not from
    /// `have`.
    fn build_pack(
        &self,
        repo: &Self::Repo,
        want: &[ObjectId],
        have: &[ObjectId],
        opts: &PackOptions,
    ) -> Result<PackOutput>;

    /// Index a staged pack file into the repository's object store.
    /// Returns `None` if the pack contained no objects.
    fn ingest_pack(
        &self,
        repo: &Self::Repo,
        staged_pack: &Path,
    ) -> Result<Option<Self::IngestedPack>>;

    /// Inspect an ingested pack and extract metadata (object types, commit
    /// signatures, etc.) for auth decisions.
    fn inspect_ingested(&self, pack: &Self::IngestedPack) -> Result<PackMetadata>;

    /// Roll back a previously ingested pack (e.g. on auth failure).
    fn rollback_ingest(&self, pack: Self::IngestedPack);

    /// Walk commits reachable from `from`, stopping at any commit reachable
    /// from `excluding`.  Returns oids in parent-first topological order.
    ///
    /// Used by both `Comparison::new_commits` (walk from new tip excluding
    /// pre-existing refs and earlier same-push refs) and
    /// `Comparison::dropped_commits` (walk from old tip excluding new tip).
    ///
    /// Returns [`ReachableLimitExceeded`] if the walk hits `cap` before
    /// terminating.  Implementations must observe the cap as a hard ceiling.
    fn reachable_excluding(
        &self,
        repo: &Self::Repo,
        from: &[ObjectId],
        excluding: &[ObjectId],
        cap: usize,
    ) -> Result<Vec<ObjectId>, ReachableError>;

    /// Compute the path-level diff between two trees.
    ///
    /// `parent_tree` may be `None` to mean "the empty tree" (e.g. the diff
    /// for a root commit).
    fn tree_diff(
        &self,
        repo: &Self::Repo,
        parent_tree: Option<ObjectId>,
        child_tree: ObjectId,
    ) -> Result<RefDiff>;

    /// Read commit metadata for a commit already in the repository (used by
    /// `Comparison::dropped_commits`, since dropped commits are not in the
    /// staged pack).
    fn read_commit_info(&self, repo: &Self::Repo, oid: ObjectId) -> Result<CommitInfo>;

    /// Read raw blob bytes, capped at `cap`.  Returns `Ok(None)` if the blob
    /// is larger than the cap or not found.
    fn read_blob(&self, repo: &Self::Repo, oid: ObjectId, cap: u64) -> Result<Option<Vec<u8>>>;
}

/// Error type for [`StorageBackend::reachable_excluding`].
#[derive(Debug)]
pub enum ReachableError {
    /// The walk hit its configured cap before terminating.
    CapExceeded { limit: usize },
    /// The backend itself returned an error.
    Other(anyhow::Error),
}

impl std::fmt::Display for ReachableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapExceeded { limit } => {
                write!(f, "reachability walk exceeded cap of {limit} commits")
            }
            Self::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for ReachableError {}

impl From<anyhow::Error> for ReachableError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

/// Full-bypass trait for backends that handle the entire git protocol.
///
/// Not implemented in this phase — defined as a marker for the architecture.
pub trait BypassBackend: Send + Sync + 'static {
    type RepoId;
}
