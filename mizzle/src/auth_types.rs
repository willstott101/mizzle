//! Types exposed through the auth surface: structured commit/tag metadata,
//! `Comparison` accessor results, and verification primitives.
//!
//! See `design/auth.md` and `design/auth-implementation-plan.md`.

use std::collections::HashMap;

use bstr::{BStr, BString, ByteSlice};
use gix::ObjectId;

pub use mizzle_proto::types::PushKind;

/// A single ref update within a push.
///
/// Carries identifying information mizzle has computed without opening the
/// repository: the refname, the [`PushKind`] classification, and the
/// before / after OIDs from the receive-pack commands.
#[derive(Debug, Clone)]
pub struct PushRef<'a> {
    pub refname: &'a str,
    pub kind: PushKind,
    pub old_oid: ObjectId,
    pub new_oid: ObjectId,
}

/// Identity from a commit/tag header (author, committer, tagger).
///
/// Fields are stored as raw bytes because git allows non-UTF-8 in name and
/// email fields.  Forges that only deal with ASCII identities can use
/// [`bstr::ByteSlice::to_str`] to lift them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: BString,
    pub email: BString,
    /// Raw time field as it appears in the header, e.g. `1700000000 +0000`.
    pub time: BString,
}

/// Metadata extracted from a commit object.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub oid: ObjectId,
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Identity,
    pub committer: Identity,
    pub message: BString,
    /// Encoding header, if present.
    pub encoding: Option<BString>,
    /// Raw signature blob recovered from the object headers, if any.
    /// Used by [`Comparison::verify`](crate::auth::Comparison::verify);
    /// not part of the public commit-data surface.
    pub(crate) signature: Option<SignatureBlob>,
}

/// Metadata extracted from an annotated tag object.
#[derive(Debug, Clone)]
pub struct TagInfo {
    pub oid: ObjectId,
    pub target: ObjectId,
    pub name: BString,
    pub tagger: Option<Identity>,
    pub message: BString,
    pub(crate) signature: Option<SignatureBlob>,
}

/// Raw signature recovered from a commit or tag header.
#[derive(Debug, Clone)]
pub(crate) struct SignatureBlob {
    pub format: SignatureFormat,
    pub bytes: Vec<u8>,
}

/// Signature format detected from the raw signature bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SignatureFormat {
    /// OpenPGP, ASCII-armoured.
    OpenPgp,
    /// SSH signature with namespace `git`.
    Ssh,
    /// X.509 / S-MIME CMS (used by gitsign / Sigstore).
    X509Cms,
    /// Format could not be determined from the header bytes.
    Unknown,
}

impl SignatureFormat {
    /// Sniff the format from the leading bytes of a signature blob.
    pub fn detect(bytes: &[u8]) -> Self {
        if bytes.starts_with(b"-----BEGIN PGP SIGNATURE-----") {
            Self::OpenPgp
        } else if bytes.starts_with(b"-----BEGIN SSH SIGNATURE-----") {
            Self::Ssh
        } else if bytes.starts_with(b"-----BEGIN SIGNED MESSAGE-----")
            || bytes.starts_with(b"-----BEGIN CMS-----")
        {
            Self::X509Cms
        } else {
            Self::Unknown
        }
    }
}

/// Errors returned by [`Comparison`](crate::auth::Comparison) accessors.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ComparisonError {
    /// A bounded walk or iteration exceeded its configured cap.
    CapExceeded { what: &'static str, limit: usize },
    /// The storage backend returned an error while computing this view.
    Backend(String),
}

impl std::fmt::Display for ComparisonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapExceeded { what, limit } => {
                write!(f, "{what} exceeded the configured limit of {limit}")
            }
            Self::Backend(msg) => write!(f, "backend error: {msg}"),
        }
    }
}

impl std::error::Error for ComparisonError {}

/// A path-level diff between two trees.
#[derive(Debug, Clone, Default)]
pub struct RefDiff {
    pub entries: Vec<RefDiffEntry>,
}

impl RefDiff {
    /// All paths touched by the diff (added, modified, or removed).
    pub fn touched_paths(&self) -> impl Iterator<Item = &BStr> {
        self.entries.iter().map(|e| e.path.as_bstr())
    }

    /// Entries that were added or modified (i.e. have a new oid).
    pub fn added_or_modified(&self) -> impl Iterator<Item = &RefDiffEntry> {
        self.entries
            .iter()
            .filter(|e| !matches!(e.change, RefDiffChange::Removed))
    }

    /// Entries that were removed.
    pub fn removed(&self) -> impl Iterator<Item = &RefDiffEntry> {
        self.entries
            .iter()
            .filter(|e| matches!(e.change, RefDiffChange::Removed))
    }
}

/// A single path entry from a [`RefDiff`].
#[derive(Debug, Clone)]
pub struct RefDiffEntry {
    pub path: BString,
    pub change: RefDiffChange,
    /// The git tree-entry mode (e.g. `0o100644` for a normal file).
    pub mode: u32,
    /// For `Added` and `Modified`, the new blob oid.
    /// For `Removed`, the previously-recorded oid.
    pub oid: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefDiffChange {
    Added,
    Modified,
    Removed,
}

// ---------------------------------------------------------------------------
// Verification surface (Phase B plumbing)
// ---------------------------------------------------------------------------

/// Status of a signature check on a commit or tag.
///
/// Lazily populated by [`Comparison::verify`](crate::auth::Comparison::verify).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum VerificationStatus {
    /// The signature verified against a key the forge supplied.
    Verified {
        identity: SignedIdentity,
        format: SignatureFormat,
    },
    /// The signature parsed and matched a candidate key but cryptographic
    /// verification failed.
    BadSignature,
    /// No registered key matched the signature's signer.
    UnknownKey,
    /// No native verifier handles this signature format and `verify_external`
    /// declined.
    UnsupportedFormat,
    /// The commit or tag carries no signature.
    Unsigned,
}

/// Identity material recovered from a verified signature.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SignedIdentity {
    Pgp {
        key_id: String,
        email: BString,
    },
    Ssh {
        fingerprint: String,
        principal: Option<BString>,
    },
    X509 {
        subject: String,
        san: Option<String>,
    },
    /// Identity material outside the natively-supported formats.
    Other {
        description: String,
    },
}

impl SignedIdentity {
    /// Cheap helper for forges that key authorisation off the email field.
    pub fn matches_email(&self, email: &str) -> bool {
        match self {
            Self::Pgp { email: e, .. } => e.as_bstr() == email.as_bytes(),
            Self::X509 { san: Some(s), .. } => s == email,
            Self::Ssh {
                principal: Some(p), ..
            } => p.as_bstr() == email.as_bytes(),
            _ => false,
        }
    }
}

/// Identifying material for a commit's signer, batched and passed to
/// [`RepoAccess::verification_keys`](crate::traits::RepoAccess::verification_keys).
#[derive(Debug, Clone)]
pub struct Signer<'a> {
    pub email: &'a BStr,
    pub format: SignatureFormat,
    /// Format-specific identifier where mizzle could extract one cheaply
    /// (PGP key id, SSH fingerprint, X.509 subject).
    pub identifier: Option<&'a BStr>,
}

impl<'a> Signer<'a> {
    /// Owned key suitable for the [`HashMap`] returned by `verification_keys`.
    pub fn key(&self) -> SignerKey {
        SignerKey {
            email: self.email.to_owned(),
            format: self.format,
            identifier: self.identifier.map(|s| s.to_owned()),
        }
    }
}

/// Owned signer key — see [`Signer::key`].
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct SignerKey {
    pub email: BString,
    pub format: SignatureFormat,
    pub identifier: Option<BString>,
}

/// A candidate verification key supplied by the forge.
#[derive(Debug, Clone)]
pub struct VerificationKey {
    pub format: SignatureFormat,
    /// Format-specific key bytes (armoured PGP public key, SSH
    /// `allowed_signers` line, PEM-encoded X.509 cert chain, etc.).
    pub key_data: Vec<u8>,
}

/// Argument passed to
/// [`RepoAccess::verify_external`](crate::traits::RepoAccess::verify_external).
pub struct ExternalSig<'a> {
    pub format: SignatureFormat,
    pub signature: &'a [u8],
    pub signed_payload: &'a [u8],
    pub signer_email: &'a BStr,
}

/// Convenience type for the `verification_keys` return.
pub type VerificationKeys = HashMap<SignerKey, Vec<VerificationKey>>;
