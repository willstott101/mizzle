//! Object read/write/has operations for the SQL backend.

use anyhow::{Context, Result};
use gix::ObjectId;
use sqlx::SqlitePool;

/// Object kind integer constants matching the schema:
/// 0=blob, 1=tree, 2=commit, 3=tag.
const KIND_BLOB: i32 = 0;
const KIND_COMMIT: i32 = 2;

/// Return the OID's raw bytes as `&[u8]` for sqlx binding.
fn oid_bytes(oid: &ObjectId) -> &[u8] {
    &oid.as_bytes()[..]
}

/// Check whether an object exists in the repository.
pub(super) async fn has_object(pool: &SqlitePool, repo_db_id: i64, oid: &ObjectId) -> Result<bool> {
    let row: (bool,) =
        sqlx::query_as("SELECT EXISTS(SELECT 1 FROM objects WHERE repo_id = ? AND oid = ?)")
            .bind(repo_db_id)
            .bind(oid_bytes(oid))
            .fetch_one(pool)
            .await
            .context("checking object existence")?;

    Ok(row.0)
}

/// Check which of the given OIDs exist in the repository.
///
/// Falls back to per-OID queries since SQLite doesn't support array
/// bind parameters. For large batches a future optimisation could use
/// a temp table or chunked `IN (...)` clauses.
pub(super) async fn has_objects(
    pool: &SqlitePool,
    repo_db_id: i64,
    oids: &[ObjectId],
) -> Result<Vec<bool>> {
    let mut results = Vec::with_capacity(oids.len());
    for oid in oids {
        results.push(has_object(pool, repo_db_id, oid).await?);
    }
    Ok(results)
}

/// Read raw blob bytes, capped at `cap`. Returns `Ok(None)` if the object
/// is not found, not a blob, or larger than the cap.
pub(super) async fn read_blob(
    pool: &SqlitePool,
    repo_db_id: i64,
    oid: ObjectId,
    cap: u64,
) -> Result<Option<Vec<u8>>> {
    let row: Option<(i32, Vec<u8>)> =
        sqlx::query_as("SELECT kind, data FROM objects WHERE repo_id = ? AND oid = ?")
            .bind(repo_db_id)
            .bind(oid_bytes(&oid))
            .fetch_optional(pool)
            .await
            .context("reading blob")?;

    match row {
        Some((kind, data)) => {
            if kind != KIND_BLOB {
                return Ok(None);
            }
            if (data.len() as u64) > cap {
                return Ok(None);
            }
            Ok(Some(data))
        }
        None => Ok(None),
    }
}

/// Read raw object bytes regardless of kind, capped at `cap`. Returns
/// `Ok(None)` if the object is not found or larger than the cap.
pub(super) async fn read_object_raw(
    pool: &SqlitePool,
    repo_db_id: i64,
    oid: ObjectId,
    cap: u64,
) -> Result<Option<Vec<u8>>> {
    let row: Option<(Vec<u8>,)> =
        sqlx::query_as("SELECT data FROM objects WHERE repo_id = ? AND oid = ?")
            .bind(repo_db_id)
            .bind(oid_bytes(&oid))
            .fetch_optional(pool)
            .await
            .context("reading object")?;

    match row {
        Some((data,)) => {
            if (data.len() as u64) > cap {
                return Ok(None);
            }
            Ok(Some(data))
        }
        None => Ok(None),
    }
}

/// Read commit metadata for a commit already in the repository.
pub(super) async fn read_commit_info(
    pool: &SqlitePool,
    repo_db_id: i64,
    oid: ObjectId,
) -> Result<crate::auth_types::CommitInfo> {
    let row: Option<(i32, Vec<u8>)> =
        sqlx::query_as("SELECT kind, data FROM objects WHERE repo_id = ? AND oid = ?")
            .bind(repo_db_id)
            .bind(oid_bytes(&oid))
            .fetch_optional(pool)
            .await
            .context("reading commit")?;

    match row {
        Some((kind, data)) => {
            if kind != KIND_COMMIT {
                anyhow::bail!("object {oid} is not a commit (kind={kind})");
            }
            crate::inspect::parse_commit_info(&data, oid)
        }
        None => anyhow::bail!("commit {oid} not in object store"),
    }
}
