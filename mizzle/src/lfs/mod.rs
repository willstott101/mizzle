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
use std::marker::PhantomData;

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

// ---------------------------------------------------------------------------
// NoLfs — placeholder for repos that don't support LFS
// ---------------------------------------------------------------------------

/// A no-op [`LfsStore`] for repositories that do not support Git LFS.
///
/// Pass this to [`serve_with_backends`](crate::servers::axum::serve_with_backends)
/// when you want git-only behaviour without wiring up a real LFS store.
/// All batch objects are reported as missing; proxy transfer endpoints return
/// an error if somehow reached.
#[derive(Clone, Copy)]
pub struct NoLfs<R>(PhantomData<fn() -> R>);

impl<R> NoLfs<R> {
    pub fn new() -> Self {
        NoLfs(PhantomData)
    }
}

impl<R> Default for NoLfs<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: Send + Sync + Clone + 'static> LfsStore for NoLfs<R> {
    type RepoId = R;
    type Repo = ();

    fn open(&self, _id: &R) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    fn stat(&self, _repo: &(), _oid: &LfsOid) -> impl Future<Output = Result<Option<u64>>> + Send {
        async { Ok(None) }
    }

    fn download_action(
        &self,
        _repo: &(),
        _oid: &LfsOid,
        _size: u64,
    ) -> impl Future<Output = Result<TransferAction>> + Send {
        async { anyhow::bail!("LFS not supported") }
    }

    fn upload_action(
        &self,
        _repo: &(),
        _oid: &LfsOid,
        _size: u64,
    ) -> impl Future<Output = Result<TransferAction>> + Send {
        async { anyhow::bail!("LFS not supported") }
    }

    fn read(
        &self,
        _repo: &(),
        _oid: &LfsOid,
    ) -> impl Future<Output = Result<Box<dyn AsyncRead + Send + Unpin>>> + Send {
        async { anyhow::bail!("LFS not supported") }
    }

    fn write(
        &self,
        _repo: &(),
        _oid: &LfsOid,
        _size: u64,
        _src: impl AsyncRead + Send + Unpin,
    ) -> impl Future<Output = Result<()>> + Send {
        async { anyhow::bail!("LFS not supported") }
    }
}
