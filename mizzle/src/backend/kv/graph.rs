//! Commit-parent graph walks for the KV backend.
//!
//! All walks are in-process BFS over `par/<repo>/<commit>/*` keys.  Each step
//! issues one range scan per commit on the frontier; both TiKV and FDB serve
//! these from a single network round-trip.

use std::collections::{HashSet, VecDeque};

use anyhow::Context;
use gix::ObjectId;
use tikv_client::TransactionClient;

use super::{keys, txn};
use crate::backend::ReachableError;
use crate::traits::PushKind;

/// Classify a ref update as Create / Delete / FastForward / ForcePush.
///
/// For updates (both OIDs non-null) we walk ancestors of `new_oid` looking
/// for `old_oid`.  Found → fast-forward; frontier empties → force push.
pub(super) async fn compute_push_kind(
    db: &TransactionClient,
    repo_id: u64,
    old_oid: ObjectId,
    new_oid: ObjectId,
) -> PushKind {
    if old_oid.is_null() {
        return PushKind::Create;
    }
    if new_oid.is_null() {
        return PushKind::Delete;
    }

    match is_ancestor(db, repo_id, &old_oid, &new_oid).await {
        Ok(true) => PushKind::FastForward,
        _ => PushKind::ForcePush,
    }
}

/// True iff `ancestor` is reachable from `descendant` via parent edges.
async fn is_ancestor(
    db: &TransactionClient,
    repo_id: u64,
    ancestor: &ObjectId,
    descendant: &ObjectId,
) -> anyhow::Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut frontier: VecDeque<ObjectId> = VecDeque::new();
    visited.insert(*descendant);
    frontier.push_back(*descendant);

    while let Some(oid) = frontier.pop_front() {
        for parent in fetch_parents(db, repo_id, &oid).await? {
            if parent == *ancestor {
                return Ok(true);
            }
            if visited.insert(parent) {
                frontier.push_back(parent);
            }
        }
    }
    Ok(false)
}

/// Walk commits reachable from `from`, stopping at any commit reachable from
/// `excluding`.  Returns OIDs in BFS order, capped at `cap`.
///
/// Two-pass: expand the exclude-set first, then walk from-side pruning at
/// the exclude-set.  The `cap` argument bounds the returned `result` length
/// per the trait contract.  As a defensive measure `collect_ancestors` also
/// honours `cap` on the shared `exclude_set` size — since the set is shared
/// across calls, this acts as a global ceiling on the exclude-side rather
/// than a per-exclusion budget.
pub(super) async fn reachable_excluding(
    db: &TransactionClient,
    repo_id: u64,
    from: &[ObjectId],
    excluding: &[ObjectId],
    cap: usize,
) -> Result<Vec<ObjectId>, ReachableError> {
    if from.is_empty() {
        return Ok(Vec::new());
    }

    let mut exclude_set: HashSet<ObjectId> = HashSet::new();
    for oid in excluding {
        collect_ancestors(db, repo_id, oid, &mut exclude_set, cap).await?;
    }

    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    let mut result: Vec<ObjectId> = Vec::new();

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

        let parents = fetch_parents(db, repo_id, &oid)
            .await
            .map_err(|e| ReachableError::Other(anyhow::anyhow!("fetching parents: {e}")))?;
        for parent in parents {
            if !exclude_set.contains(&parent) && visited.insert(parent) {
                queue.push_back(parent);
            }
        }
    }

    Ok(result)
}

/// Collect every ancestor of `oid` (inclusive) into `set`, capped at `cap`.
async fn collect_ancestors(
    db: &TransactionClient,
    repo_id: u64,
    oid: &ObjectId,
    set: &mut HashSet<ObjectId>,
    cap: usize,
) -> Result<(), ReachableError> {
    let mut frontier: VecDeque<ObjectId> = VecDeque::new();
    if set.insert(*oid) {
        frontier.push_back(*oid);
    }

    while let Some(current) = frontier.pop_front() {
        if set.len() > cap {
            return Err(ReachableError::CapExceeded { limit: cap });
        }
        let parents = fetch_parents(db, repo_id, &current)
            .await
            .map_err(|e| ReachableError::Other(anyhow::anyhow!("collecting ancestors: {e}")))?;
        for parent in parents {
            if set.insert(parent) {
                frontier.push_back(parent);
            }
        }
    }

    Ok(())
}

/// Read the parents of a single commit by range-scanning the `par/` subspace.
async fn fetch_parents(
    db: &TransactionClient,
    repo_id: u64,
    commit_oid: &ObjectId,
) -> anyhow::Result<Vec<ObjectId>> {
    let mut t = db
        .begin_optimistic()
        .await
        .context("begin txn for fetch_parents")?;
    let result: anyhow::Result<Vec<ObjectId>> = async {
        let start = keys::parents_prefix_start(repo_id, commit_oid);
        let end = keys::parents_prefix_end(repo_id, commit_oid);
        let pairs = t
            .scan(start..end, u32::MAX)
            .await
            .context("scan commit parents")?;
        // Keys are sorted by pos (big-endian u16) so the iterator is already in
        // parent order.  We only need the values.
        let mut parents = Vec::new();
        for pair in pairs {
            let value: Vec<u8> = pair.value().clone();
            let parent = ObjectId::try_from(value.as_slice())
                .map_err(|e| anyhow::anyhow!("corrupt parent oid: {e}"))?;
            parents.push(parent);
        }
        Ok(parents)
    }
    .await;
    txn::finalize_read(&mut t, result).await
}
