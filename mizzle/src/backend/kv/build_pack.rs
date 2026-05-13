//! Pack-build and tree-diff stubs.
//!
//! Real impls land in the fetch-path commit.  These stubs let the spine
//! compile and ingest-only flows (push without fetch) work.

use std::path::Path;

use anyhow::{bail, Result};
use gix::ObjectId;
use tikv_client::TransactionClient;

use crate::auth_types::RefDiff;
use crate::backend::{PackOptions, PackOutput};

pub(super) async fn build_pack(
    _db: &TransactionClient,
    _repo_id: u64,
    _want: &[ObjectId],
    _have: &[ObjectId],
    _opts: &PackOptions,
    _cache_dir: &Path,
) -> Result<PackOutput> {
    bail!("build_pack not yet implemented for KV backend")
}

pub(super) async fn tree_diff(
    _db: &TransactionClient,
    _repo_id: u64,
    _parent_tree: Option<ObjectId>,
    _child_tree: ObjectId,
) -> Result<RefDiff> {
    bail!("tree_diff not yet implemented for KV backend")
}
