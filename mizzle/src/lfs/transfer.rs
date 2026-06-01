//! Git LFS proxy transfer endpoints.
//!
//! Handles the actual byte transfer for stores that use `TransferAction::Proxy`.

use futures_lite::AsyncRead;

use mizzle_proto::lfs::Operation;

use super::{LfsOid, LfsStore};
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
    let oid = match format!("sha256:{oid_hex}").parse::<LfsOid>() {
        Ok(o) => o,
        Err(_) => return (400, None),
    };

    let repo = match lfs.open(repo_id).await {
        Ok(r) => r,
        Err(_) => return (500, None),
    };

    // Check existence.
    match lfs.stat(&repo, &oid).await {
        Ok(None) => return (404, None),
        Ok(Some(_)) => {}
        Err(_) => return (500, None),
    }

    // Stream.
    match lfs.read(&repo, &oid).await {
        Ok(reader) => (200, Some(reader)),
        Err(_) => (500, None),
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
    // Auth gate for uploads.
    if let Err(_reason) = access.authorize_lfs(Operation::Upload, None) {
        return 403;
    }

    let oid = match format!("sha256:{oid_hex}").parse::<LfsOid>() {
        Ok(o) => o,
        Err(_) => return 400,
    };

    let repo = match lfs.open(repo_id).await {
        Ok(r) => r,
        Err(_) => return 500,
    };

    match lfs.write(&repo, &oid, size, body).await {
        Ok(()) => 200,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("sha256")
                || msg.contains("hash")
                || msg.contains("size")
                || msg.contains("mismatch")
            {
                422
            } else {
                500
            }
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
    let repo = match lfs.open(repo_id).await {
        Ok(r) => r,
        Err(_) => return 500,
    };

    match lfs.stat(&repo, oid).await {
        Ok(None) => 404,
        Ok(Some(actual_size)) => {
            if actual_size == expected_size {
                200
            } else {
                422
            }
        }
        Err(_) => 500,
    }
}
