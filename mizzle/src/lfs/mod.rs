//! Git LFS support for mizzle.
//!
//! `LfsStore` is orthogonal to `StorageBackend`; they share only the
//! `RepoId` associated type enforced at the `serve_*` entry points.
//!
//! # Module layout
//! - [`mod@fs`] — `FsLfs`, the filesystem reference store.
//! - [`mod@batch`] — batch API handler.
//! - [`mod@transfer`] — proxy GET/PUT/verify endpoints.

pub mod batch;
pub mod fs;
pub mod transfer;

pub use fs::FsLfs;

use std::future::Future;

use anyhow::Result;
use futures_lite::AsyncRead;

pub use mizzle_proto::lfs::{LfsOid, Operation as LfsOperation};

// ---------------------------------------------------------------------------
// TransferAction
// ---------------------------------------------------------------------------

/// How a client should transfer an LFS object.
pub enum TransferAction {
    /// mizzle streams the bytes via its own proxy transfer endpoint.
    Proxy,
    /// Client transfers directly against the given URL (e.g. a presigned S3 URL).
    Redirect {
        href: String,
        header: Vec<(String, String)>,
        expires_at: Option<std::time::SystemTime>,
    },
}

// ---------------------------------------------------------------------------
// LfsStore trait
// ---------------------------------------------------------------------------

/// Thin storage trait for Git LFS objects.
///
/// Mirrors [`StorageBackend`](crate::backend::StorageBackend)'s async RPITIT
/// convention and `RepoId`/`Repo`/`open` shape so a coupled backend can reuse
/// its existing repo handle.
pub trait LfsStore: Send + Sync + 'static {
    /// Identifier for a repository (shared with `StorageBackend::RepoId`).
    type RepoId: Send + Sync + Clone + 'static;

    /// Per-request handle for an opened LFS repository.
    type Repo: Send + Sync;

    /// Open an LFS repo, returning a reusable handle.
    fn open(&self, id: &Self::RepoId) -> impl Future<Output = Result<Self::Repo>> + Send;

    /// Check existence and return the stored size.  `None` = object absent.
    fn stat(
        &self,
        repo: &Self::Repo,
        oid: &LfsOid,
    ) -> impl Future<Output = Result<Option<u64>>> + Send;

    /// Decide how the client should download a present object.
    fn download_action(
        &self,
        repo: &Self::Repo,
        oid: &LfsOid,
        size: u64,
    ) -> impl Future<Output = Result<TransferAction>> + Send;

    /// Decide how the client should upload a missing object.
    fn upload_action(
        &self,
        repo: &Self::Repo,
        oid: &LfsOid,
        size: u64,
    ) -> impl Future<Output = Result<TransferAction>> + Send;

    /// Stream a stored object to the caller (proxy transfer only).
    ///
    /// Only called when `download_action` returned `TransferAction::Proxy`.
    fn read(
        &self,
        repo: &Self::Repo,
        oid: &LfsOid,
    ) -> impl Future<Output = Result<Box<dyn AsyncRead + Send + Unpin>>> + Send;

    /// Receive and store an object from the caller (proxy transfer only).
    ///
    /// Only called when `upload_action` returned `TransferAction::Proxy`.
    /// Implementations **must** verify the SHA-256 of the received bytes and
    /// reject a mismatch against `oid`.
    fn write(
        &self,
        repo: &Self::Repo,
        oid: &LfsOid,
        size: u64,
        src: impl AsyncRead + Send + Unpin,
    ) -> impl Future<Output = Result<()>> + Send;
}
