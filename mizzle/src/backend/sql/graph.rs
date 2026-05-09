//! Commit-parent graph, ancestor checks, and reachability queries.

use gix::ObjectId;
use sqlx::SqlitePool;

use crate::backend::ReachableError;
use crate::traits::PushKind;

use super::objects::oid_bytes;

/// Classify a ref update as Create / Delete / FastForward / ForcePush.
///
/// For updates (both old and new non-null), checks whether `old_oid` is an
/// ancestor of `new_oid` using a recursive CTE on `commit_parents`.
pub(super) async fn compute_push_kind(
    pool: &SqlitePool,
    repo_db_id: i64,
    old_oid: ObjectId,
    new_oid: ObjectId,
) -> PushKind {
    if old_oid.is_null() {
        return PushKind::Create;
    }
    if new_oid.is_null() {
        return PushKind::Delete;
    }

    let is_ancestor = is_ancestor(pool, repo_db_id, &old_oid, &new_oid).await;
    if is_ancestor.unwrap_or(false) {
        PushKind::FastForward
    } else {
        PushKind::ForcePush
    }
}

/// Check whether `ancestor` is reachable from `descendant` by walking the
/// commit-parent graph via a recursive CTE.
async fn is_ancestor(
    pool: &SqlitePool,
    repo_db_id: i64,
    ancestor: &ObjectId,
    descendant: &ObjectId,
) -> anyhow::Result<bool> {
    let row: (bool,) = sqlx::query_as(
        r#"
        WITH RECURSIVE ancestors(oid) AS (
            SELECT ?1
            UNION ALL
            SELECT cp.parent_oid
              FROM commit_parents cp
              JOIN ancestors a ON cp.commit_oid = a.oid
             WHERE cp.repo_id = ?2
        )
        SELECT EXISTS(SELECT 1 FROM ancestors WHERE oid = ?3)
        "#,
    )
    .bind(oid_bytes(descendant))
    .bind(repo_db_id)
    .bind(oid_bytes(ancestor))
    .fetch_one(pool)
    .await?;

    Ok(row.0)
}

/// Walk commits reachable from `from` tips, stopping at any commit reachable
/// from `excluding`.  Returns OIDs up to `cap`.
///
/// Uses a recursive CTE seeded with the `from` OIDs.  The `excluding` set is
/// pre-loaded into a temporary table-valued expression for efficient pruning.
pub(super) async fn reachable_excluding(
    pool: &SqlitePool,
    repo_db_id: i64,
    from: &[ObjectId],
    excluding: &[ObjectId],
    cap: usize,
) -> Result<Vec<ObjectId>, ReachableError> {
    if from.is_empty() {
        return Ok(Vec::new());
    }

    // Build the excluding set by expanding to all ancestors.
    // For efficiency we first collect the full set of OIDs reachable from
    // `excluding`, then walk from `from` and prune.
    //
    // A single CTE that does both is possible but complex.  Instead, do a
    // simpler two-pass approach:
    //
    // 1. Expand the `excluding` tips to the full set of reachable commits.
    // 2. Walk from `from` tips, skipping anything in the excluding set.
    //
    // For correctness and simplicity, we do this in Rust with per-OID queries
    // rather than a single massive CTE (which would need dynamic SQL for
    // variable-length tip lists).

    // Collect the excluding set.
    let mut exclude_set = std::collections::HashSet::new();
    for oid in excluding {
        collect_ancestors(pool, repo_db_id, oid, &mut exclude_set).await?;
    }

    // BFS from `from` tips, pruning at exclude_set, respecting cap.
    let mut visited = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    let mut result = Vec::new();

    for oid in from {
        if !exclude_set.contains(oid) && visited.insert(*oid) {
            queue.push_back(*oid);
        }
    }

    while let Some(oid) = queue.pop_front() {
        if result.len() >= cap {
            return Err(ReachableError::CapExceeded { limit: cap });
        }
        result.push(oid);

        // Fetch parents of this commit.
        let parents: Vec<(Vec<u8>,)> = sqlx::query_as(
            "SELECT parent_oid FROM commit_parents WHERE repo_id = ? AND commit_oid = ? ORDER BY position",
        )
        .bind(repo_db_id)
        .bind(oid_bytes(&oid))
        .fetch_all(pool)
        .await
        .map_err(|e| ReachableError::Other(anyhow::anyhow!("fetching parents: {e}")))?;

        for (parent_bytes,) in parents {
            let parent = ObjectId::from_bytes_or_panic(&parent_bytes);
            if !exclude_set.contains(&parent) && visited.insert(parent) {
                queue.push_back(parent);
            }
        }
    }

    Ok(result)
}

/// Recursively collect all ancestors of `oid` (inclusive) into `set`.
async fn collect_ancestors(
    pool: &SqlitePool,
    repo_db_id: i64,
    oid: &ObjectId,
    set: &mut std::collections::HashSet<ObjectId>,
) -> Result<(), ReachableError> {
    // Use the recursive CTE to get all ancestors in one query.
    let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
        r#"
        WITH RECURSIVE ancestors(oid) AS (
            SELECT ?1
            UNION ALL
            SELECT cp.parent_oid
              FROM commit_parents cp
              JOIN ancestors a ON cp.commit_oid = a.oid
             WHERE cp.repo_id = ?2
        )
        SELECT oid FROM ancestors
        "#,
    )
    .bind(oid_bytes(oid))
    .bind(repo_db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| ReachableError::Other(anyhow::anyhow!("collecting ancestors: {e}")))?;

    for (bytes,) in rows {
        set.insert(ObjectId::from_bytes_or_panic(&bytes));
    }
    Ok(())
}
