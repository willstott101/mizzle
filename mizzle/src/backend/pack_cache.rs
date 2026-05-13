//! On-disk pack cache shared by SQL- and KV-style backends.
//!
//! Git objects are content-addressed: the set of objects reachable from a
//! fixed set of OIDs is immutable (objects are never mutated, only created or
//! GC'd).  A backend that builds a pack from a `(repo_id, wants, haves, opts)`
//! tuple can therefore cache the resulting bytes on local disk keyed by a
//! hash of that tuple and serve identical requests from disk on subsequent
//! calls.
//!
//! The cache is per-backend-instance — each backend owns a directory and
//! decides what `repo_id` namespace to use.  Files land at
//! `{dir}/{repo_id}/{key}.pack`.

use std::io::BufReader;
use std::path::{Path, PathBuf};

use gix::ObjectId;
use sha2::{Digest, Sha256};

use crate::backend::{Filter, PackOptions, PackOutput};

/// Stable key for a `(wants, haves, opts)` triple within a repository.
///
/// Construct via [`key`].  Two `CacheKey`s compare equal iff they would
/// produce identical pack bytes from the same repository state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey(String);

impl CacheKey {
    fn as_filename(&self) -> String {
        format!("{}.pack", self.0)
    }
}

/// Compute the cache key for a pack request.
///
/// The key is `SHA-256(repo_id ‖ sorted_wants ‖ 0x00 ‖ sorted_haves ‖ 0x01 ‖ opts)`
/// rendered as lowercase hex.  Sorting the OID lists makes the key invariant
/// to client-side ordering; mixing the options in ensures different
/// `filter` / `deepen` / `thin_pack` settings never share a cached file.
pub fn key(repo_id: u64, want: &[ObjectId], have: &[ObjectId], opts: &PackOptions) -> CacheKey {
    let mut hasher = Sha256::new();
    hasher.update(repo_id.to_le_bytes());

    let mut sorted_wants: Vec<ObjectId> = want.to_vec();
    sorted_wants.sort();
    for oid in &sorted_wants {
        hasher.update(oid.as_bytes());
    }

    hasher.update([0x00]);

    let mut sorted_haves: Vec<ObjectId> = have.to_vec();
    sorted_haves.sort();
    for oid in &sorted_haves {
        hasher.update(oid.as_bytes());
    }

    hasher.update([0x01]);
    hasher.update(opts.deepen.unwrap_or(0).to_le_bytes());
    let filter_tag: u8 = match &opts.filter {
        None => 0,
        Some(Filter::BlobNone) => 1,
        Some(Filter::TreeNone) => 2,
    };
    hasher.update([filter_tag]);
    hasher.update([opts.thin_pack as u8]);

    CacheKey(format!("{:x}", hasher.finalize()))
}

/// Absolute filesystem path for the given cached pack.
pub fn cache_path(dir: &Path, repo_id: u64, key: &CacheKey) -> PathBuf {
    dir.join(repo_id.to_string()).join(key.as_filename())
}

/// Try to serve a previously cached pack.  Returns `Some(PackOutput)` on hit.
///
/// I/O errors are treated as misses — a corrupt cache file must never block
/// a fresh build.
pub fn try_hit(dir: &Path, repo_id: u64, key: &CacheKey) -> Option<PackOutput> {
    let path = cache_path(dir, repo_id, key);
    if !path.exists() {
        return None;
    }
    let file = std::fs::File::open(&path).ok()?;
    Some(PackOutput {
        reader: Box::new(BufReader::new(file)),
        shallow: Vec::new(),
        progress: None,
    })
}

/// Persist freshly built pack bytes.  Best-effort: I/O errors are swallowed
/// so a full or read-only cache directory cannot fail a push or fetch.
pub fn write(dir: &Path, repo_id: u64, key: &CacheKey, data: &[u8]) {
    let repo_dir = dir.join(repo_id.to_string());
    let _ = std::fs::create_dir_all(&repo_dir);
    let _ = std::fs::write(repo_dir.join(key.as_filename()), data);
}
