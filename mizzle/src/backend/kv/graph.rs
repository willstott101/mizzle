//! Graph traversal stubs.
//!
//! Real BFS over `par/` keys lives in the fetch-path commit.  Until then
//! these stubs let the spine compile and pass non-fetch parity tests.

use anyhow::Result;
use gix::ObjectId;
use tikv_client::TransactionClient;

use crate::backend::ReachableError;
use crate::traits::PushKind;

pub(super) async fn compute_push_kind(
    _db: &TransactionClient,
    _repo_id: u64,
    old_oid: ObjectId,
    new_oid: ObjectId,
) -> PushKind {
    let null = ObjectId::null(gix_hash::Kind::Sha1);
    match (old_oid == null, new_oid == null) {
        (true, true) => PushKind::Create, // null→null is nonsense, classify as create
        (true, false) => PushKind::Create,
        (false, true) => PushKind::Delete,
        // Without ancestor information we cannot distinguish FF from force;
        // the graph commit replaces this with a real walk.
        (false, false) => PushKind::ForcePush,
    }
}

pub(super) async fn reachable_excluding(
    _db: &TransactionClient,
    _repo_id: u64,
    _from: &[ObjectId],
    _excluding: &[ObjectId],
    _cap: usize,
) -> std::result::Result<Vec<ObjectId>, ReachableError> {
    Err(ReachableError::Other(anyhow::anyhow!(
        "reachable_excluding not yet implemented for KV backend"
    )))
}

#[allow(dead_code)]
pub(super) async fn _unused() -> Result<()> {
    Ok(())
}
