//! Key encoding for the KV backend.
//!
//! Single-keyspace layout described in `design/kv-backend-plan.md`.  Each
//! logical table gets a one-byte prefix so range scans over one subspace
//! never spill into another.
//!
//! ```text
//! 0x01  repo_index   | path                   → repo_id (u64 BE)
//! 0x02  next_repo_id |                        → u64 BE counter
//! 0x03  repo_meta    | repo_id(8) | tag(u8)   → bytes  (tag 0=exists, 1=head)
//! 0x04  ref          | repo_id(8) | name      → oid[20]
//! 0x05  obj          | repo_id(8) | oid(20)   → kind(u8) ‖ raw object bytes
//! 0x06  par          | repo_id(8) | commit(20) | pos(u16 BE) → parent_oid[20]
//! ```
//!
//! Big-endian everywhere so byte-ordered range scans match numeric / OID
//! ordering on `pos` and `oid` components.

use gix::ObjectId;

pub const PREFIX_REPO_INDEX: u8 = 0x01;
pub const PREFIX_NEXT_REPO_ID: u8 = 0x02;
pub const PREFIX_REPO_META: u8 = 0x03;
pub const PREFIX_REF: u8 = 0x04;
pub const PREFIX_OBJ: u8 = 0x05;
pub const PREFIX_PAR: u8 = 0x06;

pub const REPO_META_EXISTS: u8 = 0x00;
pub const REPO_META_HEAD: u8 = 0x01;

pub fn repo_index(path: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + path.len());
    k.push(PREFIX_REPO_INDEX);
    k.extend_from_slice(path.as_bytes());
    k
}

pub fn next_repo_id() -> Vec<u8> {
    vec![PREFIX_NEXT_REPO_ID]
}

pub fn repo_meta(repo_id: u64, tag: u8) -> Vec<u8> {
    let mut k = Vec::with_capacity(10);
    k.push(PREFIX_REPO_META);
    k.extend_from_slice(&repo_id.to_be_bytes());
    k.push(tag);
    k
}

pub fn refkey(repo_id: u64, name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(9 + name.len());
    k.push(PREFIX_REF);
    k.extend_from_slice(&repo_id.to_be_bytes());
    k.extend_from_slice(name.as_bytes());
    k
}

/// Lower bound (inclusive) for a range scan over all refs of a repo.
pub fn refs_prefix_start(repo_id: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(9);
    k.push(PREFIX_REF);
    k.extend_from_slice(&repo_id.to_be_bytes());
    k
}

/// Upper bound (exclusive) for a range scan over all refs of a repo.
pub fn refs_prefix_end(repo_id: u64) -> Vec<u8> {
    let mut k = refs_prefix_start(repo_id);
    increment_in_place(&mut k);
    k
}

/// Extract the ref name from a `refs_prefix_start(repo_id)`-prefixed key.
pub fn refname_from_key(repo_id: u64, key: &[u8]) -> Option<String> {
    let prefix = refs_prefix_start(repo_id);
    if !key.starts_with(&prefix) {
        return None;
    }
    std::str::from_utf8(&key[prefix.len()..])
        .ok()
        .map(str::to_owned)
}

pub fn obj(repo_id: u64, oid: &ObjectId) -> Vec<u8> {
    let mut k = Vec::with_capacity(29);
    k.push(PREFIX_OBJ);
    k.extend_from_slice(&repo_id.to_be_bytes());
    k.extend_from_slice(oid.as_bytes());
    k
}

pub fn par(repo_id: u64, commit_oid: &ObjectId, pos: u16) -> Vec<u8> {
    let mut k = Vec::with_capacity(31);
    k.push(PREFIX_PAR);
    k.extend_from_slice(&repo_id.to_be_bytes());
    k.extend_from_slice(commit_oid.as_bytes());
    k.extend_from_slice(&pos.to_be_bytes());
    k
}

/// Lower bound (inclusive) for a scan over the parents of a single commit.
pub fn parents_prefix_start(repo_id: u64, commit_oid: &ObjectId) -> Vec<u8> {
    let mut k = Vec::with_capacity(29);
    k.push(PREFIX_PAR);
    k.extend_from_slice(&repo_id.to_be_bytes());
    k.extend_from_slice(commit_oid.as_bytes());
    k
}

/// Upper bound (exclusive) for a scan over the parents of a single commit.
pub fn parents_prefix_end(repo_id: u64, commit_oid: &ObjectId) -> Vec<u8> {
    let mut k = parents_prefix_start(repo_id, commit_oid);
    increment_in_place(&mut k);
    k
}

/// Smallest byte string strictly greater than `key` in byte order.
///
/// Used to turn an inclusive prefix `[k, …]` into an exclusive upper bound
/// `[…, k+1)` for half-open range scans.  Handles 0xFF carry by appending a
/// trailing 0x00 if the entire suffix saturates.
fn increment_in_place(key: &mut Vec<u8>) {
    for i in (0..key.len()).rev() {
        if key[i] < 0xFF {
            key[i] += 1;
            key.truncate(i + 1);
            return;
        }
    }
    key.push(0x00);
}
