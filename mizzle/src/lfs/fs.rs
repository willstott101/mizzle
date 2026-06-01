//! Filesystem LFS store — the reference implementation.
//!
//! Objects are stored using the standard git-lfs on-disk layout:
//! `<root>/lfs/objects/<oid[0:2]>/<oid[2:4]>/<oid>`.
//!
//! Both `download_action` and `upload_action` return `TransferAction::Proxy`,
//! so all bytes flow through mizzle's own transfer endpoints.

use std::path::PathBuf;

use anyhow::Result;
use futures_lite::AsyncRead;
use sha2::{Digest, Sha256};
use tokio_util::compat::TokioAsyncReadCompatExt;

use super::{LfsOid, LfsStore, LfsWriteError, TransferAction};

/// Filesystem LFS store.
///
/// Stateless — all state lives on disk.  Clone/Copy freely.
#[derive(Clone, Copy)]
pub struct FsLfs;

/// Per-request handle for an `FsLfs` repo.
pub struct FsLfsRepo {
    /// Root of the LFS object store: `<repo_id>/lfs/objects`.
    root: PathBuf,
}

impl FsLfsRepo {
    /// Canonical path for a stored object.
    fn object_path(&self, oid: &LfsOid) -> PathBuf {
        let h = oid.to_hex();
        self.root.join(&h[..2]).join(&h[2..4]).join(&h)
    }

    /// Directory that contains a stored object.
    fn object_dir(&self, oid: &LfsOid) -> PathBuf {
        let h = oid.to_hex();
        self.root.join(&h[..2]).join(&h[2..4])
    }
}

impl LfsStore for FsLfs {
    type RepoId = PathBuf;
    type Repo = FsLfsRepo;

    async fn open(&self, id: &PathBuf) -> Result<FsLfsRepo> {
        Ok(FsLfsRepo {
            root: id.join("lfs").join("objects"),
        })
    }

    async fn stat(&self, repo: &FsLfsRepo, oid: &LfsOid) -> Result<Option<u64>> {
        match tokio::fs::metadata(repo.object_path(oid)).await {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn download_action(
        &self,
        _repo: &FsLfsRepo,
        _oid: &LfsOid,
        _size: u64,
    ) -> Result<TransferAction> {
        Ok(TransferAction::Proxy)
    }

    async fn upload_action(
        &self,
        _repo: &FsLfsRepo,
        _oid: &LfsOid,
        _size: u64,
    ) -> Result<TransferAction> {
        Ok(TransferAction::Proxy)
    }

    async fn read(
        &self,
        repo: &FsLfsRepo,
        oid: &LfsOid,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        let file = tokio::fs::File::open(repo.object_path(oid)).await?;
        Ok(Box::new(file.compat()))
    }

    async fn write(
        &self,
        repo: &FsLfsRepo,
        oid: &LfsOid,
        size: u64,
        mut src: impl AsyncRead + Send + Unpin,
    ) -> Result<(), LfsWriteError> {
        use futures_lite::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let dest = repo.object_path(oid);
        let dir = repo.object_dir(oid);

        // Idempotent: if the object is already present, skip.
        if tokio::fs::metadata(&dest).await.is_ok() {
            return Ok(());
        }

        // Ensure directory exists.
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| LfsWriteError::Io(e.into()))?;

        // Write to a temp file in the same directory (for atomic rename).
        let tmp_file =
            tempfile::NamedTempFile::new_in(&dir).map_err(|e| LfsWriteError::Io(e.into()))?;
        let (std_file, tmp_path) = tmp_file.keep().map_err(|e| LfsWriteError::Io(e.into()))?;
        let mut tokio_file = tokio::fs::File::from_std(std_file);

        let mut hasher = Sha256::new();
        let mut total_written: u64 = 0;
        let mut buf = vec![0u8; 64 * 1024]; // 64 KiB read buffer

        loop {
            let n = src
                .read(&mut buf)
                .await
                .map_err(|e| LfsWriteError::Io(e.into()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            tokio_file
                .write_all(&buf[..n])
                .await
                .map_err(|e| LfsWriteError::Io(e.into()))?;
            total_written += n as u64;
        }

        tokio_file
            .flush()
            .await
            .map_err(|e| LfsWriteError::Io(e.into()))?;
        drop(tokio_file);

        // Verify size.
        if total_written != size {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(LfsWriteError::SizeMismatch {
                expected: size,
                actual: total_written,
            });
        }

        // Verify sha256.
        let actual_hash: [u8; 32] = hasher.finalize().into();
        if actual_hash != oid.0 {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(LfsWriteError::HashMismatch {
                expected: oid.to_hex(),
                actual: LfsOid(actual_hash).to_hex(),
            });
        }

        // Atomically rename into place.
        tokio::fs::rename(&tmp_path, &dest)
            .await
            .map_err(|e| LfsWriteError::Io(e.into()))?;

        Ok(())
    }
}
