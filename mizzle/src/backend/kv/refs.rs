//! Ref operations for the KV backend.

use anyhow::{Context, Result};
use gix::ObjectId;
use tikv_client::TransactionClient;

use super::keys;
use crate::auth_types::CommitInfo;
use crate::backend::{HeadInfo, RefInfo, RefsSnapshot};

const SCAN_LIMIT: u32 = u32::MAX;

/// Read all refs of a repository plus its HEAD symref target.
pub(super) async fn list_refs(db: &TransactionClient, repo_id: u64) -> Result<RefsSnapshot> {
    let mut txn = db
        .begin_optimistic()
        .await
        .context("begin txn for list_refs")?;

    let start = keys::refs_prefix_start(repo_id);
    let end = keys::refs_prefix_end(repo_id);

    let pairs = txn
        .scan(start..end, SCAN_LIMIT)
        .await
        .context("scan refs")?;

    let mut refs: Vec<RefInfo> = Vec::new();
    for pair in pairs {
        let key_bytes: Vec<u8> = pair.key().clone().into();
        let value: Vec<u8> = pair.value().clone();
        let name =
            keys::refname_from_key(repo_id, &key_bytes).context("decoding ref name from key")?;
        let oid = ObjectId::try_from(value.as_slice())
            .map_err(|e| anyhow::anyhow!("corrupt ref oid for {name}: {e}"))?;
        refs.push(RefInfo {
            name,
            oid,
            peeled: None,
            symref_target: None,
        });
    }

    let head_target = txn
        .get(keys::repo_meta(repo_id, keys::REPO_META_HEAD))
        .await
        .context("read HEAD symref")?
        .and_then(|v| String::from_utf8(v).ok())
        .unwrap_or_else(|| "refs/heads/main".to_string());

    let head = refs
        .iter()
        .find(|r| r.name == head_target)
        .map(|r| HeadInfo {
            oid: r.oid,
            symref_target: Some(head_target.clone()),
        });

    txn.commit().await.context("commit list_refs txn")?;

    Ok(RefsSnapshot { head, refs })
}

pub(super) async fn resolve_ref(
    db: &TransactionClient,
    repo_id: u64,
    refname: &str,
) -> Result<Option<ObjectId>> {
    let mut txn = db
        .begin_optimistic()
        .await
        .context("begin txn for resolve_ref")?;
    let value = txn
        .get(keys::refkey(repo_id, refname))
        .await
        .context("read ref")?;
    txn.commit().await.ok();

    match value {
        Some(bytes) => {
            Ok(Some(ObjectId::try_from(bytes.as_slice()).map_err(|e| {
                anyhow::anyhow!("corrupt ref value for {refname}: {e}")
            })?))
        }
        None => Ok(None),
    }
}

/// Apply a batch of `RefUpdate`s atomically with per-ref CAS.
///
/// Either every edit lands or none do.  CAS rules per the trait contract:
///
/// - `old_oid` null → ref must not exist (create).
/// - `new_oid` null → ref must exist and equal `old_oid` (delete).
/// - both non-null → ref must exist and currently equal `old_oid` (update).
pub(super) async fn update_refs(
    db: &TransactionClient,
    repo_id: u64,
    updates: &[(ObjectId, ObjectId, String)],
) -> Result<()> {
    let null = ObjectId::null(gix_hash::Kind::Sha1);

    let mut txn = db
        .begin_pessimistic()
        .await
        .context("begin txn for update_refs")?;

    for (old_oid, new_oid, refname) in updates {
        let key = keys::refkey(repo_id, refname);
        let current = txn
            .get_for_update(key.clone())
            .await
            .context("CAS read")?
            .map(|v| ObjectId::try_from(v.as_slice()))
            .transpose()
            .map_err(|e| anyhow::anyhow!("corrupt ref value for {refname}: {e}"))?;

        let create = *old_oid == null;
        let delete = *new_oid == null;

        match (create, delete, current) {
            (true, _, Some(actual)) => {
                anyhow::bail!("stale info: {refname} should not exist but is at {actual}");
            }
            (false, _, None) => {
                anyhow::bail!("stale info: {refname} expected {old_oid} but is absent");
            }
            (false, _, Some(actual)) if actual != *old_oid => {
                anyhow::bail!("stale info: {refname} expected {old_oid} but is {actual}");
            }
            _ => {}
        }

        if delete {
            txn.delete(key).await.context("delete ref")?;
        } else {
            txn.put(key, new_oid.as_bytes().to_vec())
                .await
                .context("write ref")?;
        }
    }

    txn.commit().await.context("commit update_refs txn")?;
    Ok(())
}

/// Read commit metadata for a commit already in the store.
pub(super) async fn read_commit_info(
    db: &TransactionClient,
    repo_id: u64,
    oid: ObjectId,
) -> Result<CommitInfo> {
    let raw = super::objects::read_object_raw(db, repo_id, oid, u64::MAX)
        .await?
        .with_context(|| format!("commit {oid} not in object store"))?;
    crate::inspect::parse_commit_info(&raw, oid)
}
