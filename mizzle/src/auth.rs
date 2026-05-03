//! The `Comparison` handle exposed to forges during `authorize_push`.
//!
//! See `design/auth.md` for the design — `Comparison` is the read-only,
//! lazily-computed view of the staged push and the existing repo state. Forges
//! that touch only `refs()` pay nothing beyond what the pipeline already did;
//! every other accessor pays its cost on first use, then caches the result.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;

use bstr::BStr;
use gix::ObjectId;

use crate::auth_types::{
    CommitInfo, ComparisonError, ExternalSig, PushRef, RefDiff, SignatureFormat, Signer, SignerKey,
    TagInfo, VerificationKey, VerificationStatus,
};
use crate::backend::{PackMetadata, ReachableError, StorageBackend};
use crate::sigverify;
use crate::traits::RepoAccess;

/// Configurable caps for [`Comparison`] accessors.
///
/// Forges that need different caps construct their own `ComparisonOptions`
/// and pass it through [`run_comparison`].  Defaults err on the "permissive
/// enough for normal pushes, hard ceiling on adversarial ones" side.
#[derive(Debug, Clone, Copy)]
pub struct ComparisonOptions {
    /// Cap on [`Comparison::new_commits`] / [`Comparison::dropped_commits`].
    pub max_reachable_commits: usize,
}

impl Default for ComparisonOptions {
    fn default() -> Self {
        Self {
            max_reachable_commits: 50_000,
        }
    }
}

/// Lazy, read-only view of a push.  See [`design/auth.md`](../../../design/auth.md).
pub trait Comparison<'a> {
    /// Refs being updated in this push, in the order the client sent them.
    fn refs(&self) -> &[PushRef<'a>];

    /// Commits introduced on this ref by this push, parent-first, deduped
    /// against pre-existing refs and earlier refs in the same push.
    ///
    /// Walks are bounded; over-cap returns
    /// [`ComparisonError::CapExceeded`].
    fn new_commits<'b>(&'b self, r: &PushRef<'_>) -> Result<Vec<&'b CommitInfo>, ComparisonError>;

    /// Commits reachable from `old_oid` but not `new_oid` — what a force-push
    /// or delete would lose.
    fn dropped_commits<'b>(
        &'b self,
        r: &PushRef<'_>,
    ) -> Result<Vec<&'b CommitInfo>, ComparisonError>;

    /// Path-level summary of what changed between `old_oid` and `new_oid`.
    fn ref_diff<'b>(&'b self, r: &PushRef<'_>) -> Result<&'b RefDiff, ComparisonError>;

    /// Verify a commit's signature.  Lazy: runs crypto on first call,
    /// returns the cached result on later calls.  Forges that do not call
    /// `verify` pay no signature-verification cost.
    fn verify(&self, c: &CommitInfo) -> VerificationStatus;

    /// Verify an annotated tag's signature.
    fn verify_tag(&self, t: &TagInfo) -> VerificationStatus;

    /// Read raw blob bytes for content-inspection rules (`.gitmodules`,
    /// secret-scanning, etc.).  Returns `None` if the blob is not present in
    /// the staged pack/repo or exceeds `cap`.
    fn read_blob(&self, oid: ObjectId, cap: u64) -> Option<Vec<u8>>;

    /// Pack inspection results from step 5 of the push pipeline.
    fn pack_metadata(&self) -> &PackMetadata;

    /// Annotated tags introduced by this push.
    fn tags(&self) -> &[TagInfo];
}

/// Run `f` with a freshly-constructed [`Comparison`] for this push.
///
/// The Comparison borrows the access value, the backend handle and the staged
/// pack metadata; everything else (reachability walks, ref diffs, signature
/// verification) is computed lazily from inside `f`.
///
/// The closure receives `&dyn Comparison<'_>` so it stays
/// monomorphisation-light at the auth callsite.
pub fn run_comparison<'a, A, B, F, R>(
    access: &'a A,
    backend: &'a B,
    repo: &'a B::Repo,
    pack: &'a PackMetadata,
    refs: Vec<PushRef<'a>>,
    existing_ref_tips: Vec<ObjectId>,
    opts: ComparisonOptions,
    f: F,
) -> R
where
    A: RepoAccess + ?Sized,
    B: StorageBackend,
    F: FnOnce(&dyn Comparison<'a>) -> R,
{
    let tags: Vec<TagInfo> = pack.tags().cloned().collect();

    let n = refs.len();
    let comparison = ConcreteComparison {
        access,
        backend,
        repo,
        pack,
        refs,
        existing_ref_tips,
        opts,
        new_commits: (0..n).map(|_| OnceCell::new()).collect(),
        dropped_commits: (0..n).map(|_| OnceCell::new()).collect(),
        ref_diffs: (0..n).map(|_| OnceCell::new()).collect(),
        verify_cache: RefCell::new(HashMap::new()),
        keys_cache: OnceCell::new(),
        tags,
    };

    f(&comparison)
}

struct ConcreteComparison<'a, A: RepoAccess + ?Sized, B: StorageBackend> {
    access: &'a A,
    backend: &'a B,
    repo: &'a B::Repo,
    pack: &'a PackMetadata,
    refs: Vec<PushRef<'a>>,
    existing_ref_tips: Vec<ObjectId>,
    opts: ComparisonOptions,
    new_commits: Vec<OnceCell<Result<Vec<CommitInfo>, ComparisonError>>>,
    dropped_commits: Vec<OnceCell<Result<Vec<CommitInfo>, ComparisonError>>>,
    ref_diffs: Vec<OnceCell<Result<RefDiff, ComparisonError>>>,
    verify_cache: RefCell<HashMap<ObjectId, VerificationStatus>>,
    keys_cache: OnceCell<crate::auth_types::VerificationKeys>,
    tags: Vec<TagInfo>,
}

impl<'a, A: RepoAccess + ?Sized, B: StorageBackend> ConcreteComparison<'a, A, B> {
    fn ref_index(&self, r: &PushRef<'_>) -> usize {
        self.refs
            .iter()
            .position(|p| p.refname == r.refname)
            .unwrap_or_else(|| {
                panic!("PushRef {:?} not part of this Comparison", r.refname);
            })
    }

    fn keys(&self) -> &crate::auth_types::VerificationKeys {
        self.keys_cache.get_or_init(|| {
            // Collect every signed commit/tag in the pack, build the
            // (email, format) signer batch, and ask the access for keys.
            let mut signers: Vec<Signer<'_>> = Vec::new();
            let mut seen: std::collections::HashSet<(Vec<u8>, SignatureFormat)> =
                std::collections::HashSet::new();
            for c in self.pack.commits() {
                if let Some(sig) = &c.signature {
                    let key = (c.committer.email.to_vec(), sig.format);
                    if seen.insert(key) {
                        signers.push(Signer {
                            email: c.committer.email.as_ref(),
                            format: sig.format,
                            identifier: None,
                        });
                    }
                }
            }
            for t in self.pack.tags() {
                if let (Some(sig), Some(tagger)) = (&t.signature, &t.tagger) {
                    let key = (tagger.email.to_vec(), sig.format);
                    if seen.insert(key) {
                        signers.push(Signer {
                            email: tagger.email.as_ref(),
                            format: sig.format,
                            identifier: None,
                        });
                    }
                }
            }
            self.access.verification_keys(&signers)
        })
    }

    fn run_verify(
        &self,
        signer_email: &BStr,
        format: SignatureFormat,
        signature: &[u8],
        signed_payload: &[u8],
    ) -> VerificationStatus {
        let signer_bytes: &[u8] = signer_email.as_ref();
        let keys: Vec<&VerificationKey> = self
            .keys()
            .iter()
            .filter(|(k, _)| k.email.as_slice() == signer_bytes && k.format == format)
            .flat_map(|(_, v)| v.iter())
            .collect();

        let native = sigverify::verify_native(format, signature, signed_payload, &keys);

        let external = self.access.verify_external(&ExternalSig {
            format,
            signature,
            signed_payload,
            signer_email,
        });

        external.unwrap_or(native)
    }
}

impl<'a, A: RepoAccess + ?Sized, B: StorageBackend> Comparison<'a>
    for ConcreteComparison<'a, A, B>
{
    fn refs(&self) -> &[PushRef<'a>] {
        &self.refs
    }

    fn new_commits<'b>(&'b self, r: &PushRef<'_>) -> Result<Vec<&'b CommitInfo>, ComparisonError> {
        let idx = self.ref_index(r);

        let cached = self.new_commits[idx].get_or_init(|| {
            // Tips: the new oid of this ref (skip if delete).
            if r.new_oid.is_null() {
                return Ok(Vec::new());
            }
            let from = vec![r.new_oid];

            // Excluding: pre-existing tips + the new tips of earlier refs in
            // the push (so multi-ref pushes dedup commits to the earliest
            // ref that introduces them) + this ref's old_oid (if not a
            // create, to skip the parent line).
            let mut excluding: Vec<ObjectId> = self.existing_ref_tips.iter().copied().collect();
            for earlier in &self.refs[..idx] {
                if !earlier.new_oid.is_null() {
                    excluding.push(earlier.new_oid);
                }
            }

            let oids = self
                .backend
                .reachable_excluding(
                    self.repo,
                    &from,
                    &excluding,
                    self.opts.max_reachable_commits,
                )
                .map_err(|e| match e {
                    ReachableError::CapExceeded { limit } => ComparisonError::CapExceeded {
                        what: "new_commits walk",
                        limit,
                    },
                    ReachableError::Other(e) => ComparisonError::Backend(format!("{e:#}")),
                })?;

            // Prefer the staged-pack metadata (already parsed) and fall back
            // to a backend read for OIDs that exist in the ODB but were not
            // shipped in this push (thin-pack omissions, or pushes that point
            // a ref at a commit already present on the server).  Without the
            // fallback, those commits would be silently dropped here, letting
            // any commit-content rule (committer email, DCO, merges-only, …)
            // be bypassed.
            let mut out = Vec::with_capacity(oids.len());
            for oid in oids {
                let info = match self.pack.find_commit(&oid) {
                    Some(c) => c.clone(),
                    None => self
                        .backend
                        .read_commit_info(self.repo, oid)
                        .map_err(|e| ComparisonError::Backend(format!("{e:#}")))?,
                };
                out.push(info);
            }
            Ok(out)
        });

        match cached {
            Ok(commits) => Ok(commits.iter().collect()),
            Err(e) => Err(e.clone()),
        }
    }

    fn dropped_commits<'b>(
        &'b self,
        r: &PushRef<'_>,
    ) -> Result<Vec<&'b CommitInfo>, ComparisonError> {
        let idx = self.ref_index(r);

        let cached = self.dropped_commits[idx].get_or_init(|| {
            if r.old_oid.is_null() {
                return Ok(Vec::new());
            }
            let from = vec![r.old_oid];
            let excluding: Vec<ObjectId> = if r.new_oid.is_null() {
                Vec::new()
            } else {
                vec![r.new_oid]
            };

            let oids = self
                .backend
                .reachable_excluding(
                    self.repo,
                    &from,
                    &excluding,
                    self.opts.max_reachable_commits,
                )
                .map_err(|e| match e {
                    ReachableError::CapExceeded { limit } => ComparisonError::CapExceeded {
                        what: "dropped_commits walk",
                        limit,
                    },
                    ReachableError::Other(e) => ComparisonError::Backend(format!("{e:#}")),
                })?;

            let mut out = Vec::with_capacity(oids.len());
            for oid in oids {
                let info = self
                    .backend
                    .read_commit_info(self.repo, oid)
                    .map_err(|e| ComparisonError::Backend(format!("{e:#}")))?;
                out.push(info);
            }
            Ok(out)
        });

        match cached {
            Ok(commits) => Ok(commits.iter().collect()),
            Err(e) => Err(e.clone()),
        }
    }

    fn ref_diff<'b>(&'b self, r: &PushRef<'_>) -> Result<&'b RefDiff, ComparisonError> {
        let idx = self.ref_index(r);

        let cached = self.ref_diffs[idx].get_or_init(|| {
            // The "child" tree is the tree of the new tip.  If the push
            // creates a ref, parent tree is None.  If it deletes, the diff
            // is from old tree to None — modelled as flipped lhs/rhs.
            if r.new_oid.is_null() {
                // Delete: child is the empty tree, lhs is the old commit's tree.
                let old = self
                    .backend
                    .read_commit_info(self.repo, r.old_oid)
                    .map_err(|e| ComparisonError::Backend(format!("{e:#}")))?;
                return self
                    .backend
                    .tree_diff(
                        self.repo,
                        Some(old.tree),
                        ObjectId::empty_tree(gix_hash::Kind::Sha1),
                    )
                    .map_err(|e| ComparisonError::Backend(format!("{e:#}")));
            }

            let new_tree = self
                .pack
                .find_commit(&r.new_oid)
                .map(|c| c.tree)
                .or_else(|| {
                    // Maybe new tip is already in the repo and not in the pack.
                    self.backend
                        .read_commit_info(self.repo, r.new_oid)
                        .ok()
                        .map(|c| c.tree)
                })
                .ok_or_else(|| {
                    ComparisonError::Backend(format!("new tip {} not found", r.new_oid))
                })?;

            let parent_tree = if r.old_oid.is_null() {
                None
            } else {
                let old = self
                    .backend
                    .read_commit_info(self.repo, r.old_oid)
                    .map_err(|e| ComparisonError::Backend(format!("{e:#}")))?;
                Some(old.tree)
            };

            self.backend
                .tree_diff(self.repo, parent_tree, new_tree)
                .map_err(|e| ComparisonError::Backend(format!("{e:#}")))
        });

        match cached {
            Ok(d) => Ok(d),
            Err(e) => Err(e.clone()),
        }
    }

    fn verify(&self, c: &CommitInfo) -> VerificationStatus {
        if let Some(cached) = self.verify_cache.borrow().get(&c.oid) {
            return cached.clone();
        }

        let status = match &c.signature {
            None => VerificationStatus::Unsigned,
            Some(blob) => {
                // Re-read the raw commit object to reconstruct the canonical
                // signed payload (object bytes minus the gpgsig header).
                let raw = match self
                    .backend
                    .read_object_raw(self.repo, c.oid, 16 * 1024 * 1024)
                {
                    Ok(Some(b)) => b,
                    _ => {
                        return VerificationStatus::BadSignature;
                    }
                };
                let payload = sigverify::strip_gpgsig(&raw);
                self.run_verify(
                    c.committer.email.as_ref(),
                    blob.format,
                    &blob.bytes,
                    &payload,
                )
            }
        };

        self.verify_cache.borrow_mut().insert(c.oid, status.clone());
        status
    }

    fn verify_tag(&self, t: &TagInfo) -> VerificationStatus {
        if let Some(cached) = self.verify_cache.borrow().get(&t.oid) {
            return cached.clone();
        }
        let status = match (&t.signature, &t.tagger) {
            (Some(blob), Some(tagger)) => {
                let raw = match self
                    .backend
                    .read_object_raw(self.repo, t.oid, 16 * 1024 * 1024)
                {
                    Ok(Some(b)) => b,
                    _ => return VerificationStatus::BadSignature,
                };
                let payload = sigverify::strip_tag_signature(&raw);
                self.run_verify(tagger.email.as_ref(), blob.format, &blob.bytes, &payload)
            }
            _ => VerificationStatus::Unsigned,
        };
        self.verify_cache.borrow_mut().insert(t.oid, status.clone());
        status
    }

    fn read_blob(&self, oid: ObjectId, cap: u64) -> Option<Vec<u8>> {
        self.backend.read_blob(self.repo, oid, cap).ok().flatten()
    }

    fn pack_metadata(&self) -> &PackMetadata {
        self.pack
    }

    fn tags(&self) -> &[TagInfo] {
        &self.tags
    }
}

// Re-export the SignerKey path for the trait surface.
#[allow(dead_code)]
fn _signer_key_used(_k: SignerKey) {}
