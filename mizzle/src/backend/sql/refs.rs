//! Ref list/resolve/update operations for the SQL backend.

use anyhow::{Context, Result};
use gix::ObjectId;
use sqlx::SqlitePool;

use crate::backend::{HeadInfo, RefInfo, RefsSnapshot};

use super::objects::{oid_bytes, parse_oid};

/// List all refs for a repository.
///
/// Synthesises `HeadInfo` from `repositories.head` (or defaults to
/// `refs/heads/main` when null).
pub(super) async fn list_refs(pool: &SqlitePool, repo_db_id: i64) -> Result<RefsSnapshot> {
    // Read the HEAD symref target from the repositories table.
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT head FROM repositories WHERE id = ?")
            .bind(repo_db_id)
            .fetch_optional(pool)
            .await
            .context("reading HEAD from repositories")?;

    let head_target = row
        .and_then(|(h,)| h)
        .unwrap_or_else(|| "refs/heads/main".to_string());

    // Read all refs.
    let ref_rows: Vec<(String, Vec<u8>)> =
        sqlx::query_as("SELECT name, oid FROM refs WHERE repo_id = ?")
            .bind(repo_db_id)
            .fetch_all(pool)
            .await
            .context("listing refs")?;

    let mut refs = Vec::with_capacity(ref_rows.len());
    let mut head = None;

    for (name, raw_oid) in &ref_rows {
        let oid = parse_oid(raw_oid)?;
        refs.push(RefInfo {
            name: name.clone(),
            oid,
            peeled: None,        // SQL backend doesn't store peeled info yet
            symref_target: None, // concrete refs only
        });

        // Synthesise HEAD if this ref matches the symref target.
        if name == &head_target {
            head = Some(HeadInfo {
                oid,
                symref_target: Some(head_target.clone()),
            });
        }
    }

    Ok(RefsSnapshot { head, refs })
}

/// Resolve a single ref name to its OID.
pub(super) async fn resolve_ref(
    pool: &SqlitePool,
    repo_db_id: i64,
    refname: &str,
) -> Result<Option<ObjectId>> {
    let row: Option<(Vec<u8>,)> =
        sqlx::query_as("SELECT oid FROM refs WHERE repo_id = ? AND name = ?")
            .bind(repo_db_id)
            .bind(refname)
            .fetch_optional(pool)
            .await
            .context("resolving ref")?;

    row.map(|(raw,)| parse_oid(&raw)).transpose()
}

/// Apply ref updates as a single all-or-nothing transaction with CAS semantics.
///
/// Takes owned tuples `(old_oid, new_oid, refname)` so the future is `Send + 'static`
/// without requiring `Clone` on `RefUpdate`.
pub(super) async fn update_refs_owned(
    pool: &SqlitePool,
    repo_db_id: i64,
    updates: &[(ObjectId, ObjectId, String)],
) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }

    let mut tx = pool.begin().await.context("beginning ref transaction")?;

    for (old_oid, new_oid, refname) in updates {
        if new_oid.is_null() {
            // Delete: ref must exist and match old_oid (CAS).
            let result = sqlx::query("DELETE FROM refs WHERE repo_id = ? AND name = ? AND oid = ?")
                .bind(repo_db_id)
                .bind(refname)
                .bind(oid_bytes(old_oid))
                .execute(&mut *tx)
                .await
                .context("deleting ref")?;

            if result.rows_affected() == 0 {
                anyhow::bail!("stale info for ref {}: expected {}", refname, old_oid);
            }
        } else if old_oid.is_null() {
            // Create: ref must not already exist.
            sqlx::query("INSERT INTO refs (repo_id, name, oid) VALUES (?, ?, ?)")
                .bind(repo_db_id)
                .bind(refname)
                .bind(oid_bytes(new_oid))
                .execute(&mut *tx)
                .await
                .map_err(|e| anyhow::anyhow!("ref {} already exists: {e}", refname))?;
        } else {
            // Update: ref must exist and match old_oid (CAS).
            let result =
                sqlx::query("UPDATE refs SET oid = ? WHERE repo_id = ? AND name = ? AND oid = ?")
                    .bind(oid_bytes(new_oid))
                    .bind(repo_db_id)
                    .bind(refname)
                    .bind(oid_bytes(old_oid))
                    .execute(&mut *tx)
                    .await
                    .context("updating ref")?;

            if result.rows_affected() == 0 {
                anyhow::bail!("stale info for ref {}: expected {}", refname, old_oid);
            }
        }
    }

    tx.commit().await.context("committing ref transaction")?;
    Ok(())
}
