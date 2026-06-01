//! Git LFS proxy transfer endpoints.
//!
//! Handles the actual byte transfer for stores that use `TransferAction::Proxy`.

use futures_lite::AsyncRead;
use tracing::{debug, error, info, warn};

use mizzle_proto::lfs::Operation;

use super::{LfsOid, LfsStore, LfsWriteError};
use crate::traits::RepoAccess;

/// Handle a proxy download request (`GET objects/<oid>`).
///
/// Returns `(http_status, optional_reader)`.  The reader is `Some` on success
/// (200); `None` on error (404, 500).
///
/// # Note
///
/// Per the v1 design, read auth is out of scope — this endpoint does *not*
/// call `authorize_lfs`.  The batch endpoint already minted the download URL;
/// the forge's normal per-request auth (bearer token, session) covers access.
pub async fn handle_download<A, L>(
    _access: &A,
    lfs: &L,
    repo_id: &A::RepoId,
    oid_hex: &str,
) -> (u16, Option<Box<dyn AsyncRead + Send + Unpin>>)
where
    A: RepoAccess,
    L: LfsStore<RepoId = A::RepoId>,
{
    debug!(oid = oid_hex, "LFS download request");

    let oid = match format!("sha256:{oid_hex}").parse::<LfsOid>() {
        Ok(o) => o,
        Err(e) => {
            warn!(oid = oid_hex, error = %e, "LFS download: invalid OID");
            return (400, None);
        }
    };

    let repo = match lfs.open(repo_id).await {
        Ok(r) => r,
        Err(e) => {
            error!(oid = oid_hex, error = %e, "LFS download: failed to open store");
            return (500, None);
        }
    };

    // Check existence.
    match lfs.stat(&repo, &oid).await {
        Ok(None) => {
            debug!(oid = oid_hex, "LFS download: object not found");
            return (404, None);
        }
        Ok(Some(_)) => {}
        Err(e) => {
            error!(oid = oid_hex, error = %e, "LFS download: stat failed");
            return (500, None);
        }
    }

    // Stream.
    match lfs.read(&repo, &oid).await {
        Ok(reader) => {
            info!(oid = oid_hex, "LFS download: streaming object");
            (200, Some(reader))
        }
        Err(e) => {
            error!(oid = oid_hex, error = %e, "LFS download: read failed");
            (500, None)
        }
    }
}

/// Handle a proxy upload request (`PUT objects/<oid>`).
///
/// Returns an HTTP status code:
/// - 200 on success
/// - 400 if the OID is unparseable
/// - 403 if `authorize_lfs(Upload)` fails
/// - 422 on SHA-256 mismatch or size mismatch
/// - 500 on internal errors
pub async fn handle_upload<A, L>(
    access: &A,
    lfs: &L,
    repo_id: &A::RepoId,
    oid_hex: &str,
    size: u64,
    body: impl AsyncRead + Send + Unpin,
) -> u16
where
    A: RepoAccess,
    L: LfsStore<RepoId = A::RepoId>,
{
    debug!(oid = oid_hex, size, "LFS upload request");

    // Auth gate for uploads.
    if let Err(reason) = access.authorize_lfs(Operation::Upload, None) {
        warn!(oid = oid_hex, reason, "LFS upload: authorization denied");
        return 403;
    }

    let oid = match format!("sha256:{oid_hex}").parse::<LfsOid>() {
        Ok(o) => o,
        Err(e) => {
            warn!(oid = oid_hex, error = %e, "LFS upload: invalid OID");
            return 400;
        }
    };

    let repo = match lfs.open(repo_id).await {
        Ok(r) => r,
        Err(e) => {
            error!(oid = oid_hex, error = %e, "LFS upload: failed to open store");
            return 500;
        }
    };

    info!(oid = oid_hex, size, "LFS upload: receiving object");
    match lfs.write(&repo, &oid, size, body).await {
        Ok(()) => {
            info!(oid = oid_hex, size, "LFS upload: object stored");
            200
        }
        Err(e @ (LfsWriteError::HashMismatch { .. } | LfsWriteError::SizeMismatch { .. })) => {
            warn!(oid = oid_hex, error = %e, "LFS upload: integrity check failed");
            422
        }
        Err(e) => {
            error!(oid = oid_hex, error = %e, "LFS upload: write failed");
            500
        }
    }
}

/// Handle a post-upload verify request (`POST objects/verify`).
///
/// Returns an HTTP status code:
/// - 200 if the object exists with the expected size
/// - 404 if the object doesn't exist
/// - 422 if the size doesn't match
/// - 500 on internal errors
pub async fn handle_verify<A, L>(
    _access: &A,
    lfs: &L,
    repo_id: &A::RepoId,
    oid: &LfsOid,
    expected_size: u64,
) -> u16
where
    A: RepoAccess,
    L: LfsStore<RepoId = A::RepoId>,
{
    let oid_hex = oid.to_hex();
    debug!(oid = oid_hex, expected_size, "LFS verify request");

    let repo = match lfs.open(repo_id).await {
        Ok(r) => r,
        Err(e) => {
            error!(oid = oid_hex, error = %e, "LFS verify: failed to open store");
            return 500;
        }
    };

    match lfs.stat(&repo, oid).await {
        Ok(None) => {
            warn!(oid = oid_hex, "LFS verify: object not found");
            404
        }
        Ok(Some(actual_size)) => {
            if actual_size == expected_size {
                info!(oid = oid_hex, actual_size, "LFS verify: object OK");
                200
            } else {
                warn!(
                    oid = oid_hex,
                    expected_size, actual_size, "LFS verify: size mismatch"
                );
                422
            }
        }
        Err(e) => {
            error!(oid = oid_hex, error = %e, "LFS verify: stat failed");
            500
        }
    }
}
