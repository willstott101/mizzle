//! Types exposed through the auth surface: structured commit/tag metadata,
//! `Comparison` accessor results, and verification primitives.
//!
//! See `design/auth.md` and `design/auth-implementation-plan.md`.

use std::collections::HashMap;

use bstr::{BStr, BString, ByteSlice};
use gix::ObjectId;

pub use mizzle_proto::types::PushKind;

/// `serialize_with` helpers used across this module.
///
/// `gix-hash` and `bstr` each have their own optional `serde` support, but
/// neither produces what a forge actually wants in a log: `ObjectId`'s
/// derive emits `{"Sha1": [<20 raw bytes>]}` and `BString`'s `Serialize`
/// calls `serialize_bytes`, which most human-readable formats (including
/// `serde_json`) render as an array of integers, not a string. Routing every
/// OID and every git-identity field through these helpers instead means a
/// forge gets a plain hex string / UTF-8 string with zero decoding of its
/// own — matching the "as few decisions as possible" bar for the audit
/// surface (`Comparison::receipt`, see `design/auth.md`).
#[cfg(feature = "serde")]
pub(crate) mod serde_support {
    use bstr::{BString, ByteSlice};
    use gix::ObjectId;
    use serde::Serializer;

    pub(crate) fn oid_hex<S: Serializer>(oid: &ObjectId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&oid.to_hex().to_string())
    }

    // A `Vec<ObjectId>` variant (`serialize_seq` of `oid.to_hex()`) will be
    // needed once `CommitInfo::parents` or `PushReceipt`'s dropped-commit
    // lists grow a `Serialize` impl — add it there rather than speculatively
    // here.

    /// Lossy: git allows non-UTF-8 bytes in names/emails; invalid sequences
    /// become U+FFFD rather than failing serialization.
    pub(crate) fn bstring_lossy<S: Serializer>(b: &BString, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&b.to_str_lossy())
    }

    pub(crate) fn opt_bstring_lossy<S: Serializer>(
        b: &Option<BString>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match b {
            Some(b) => s.serialize_some(b.to_str_lossy().as_ref()),
            None => s.serialize_none(),
        }
    }
}

/// A single ref update within a push.
///
/// Carries identifying information mizzle has computed without opening the
/// repository: the refname, the [`PushKind`] classification, and the
/// before / after OIDs from the receive-pack commands.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct PushRef<'a> {
    pub refname: &'a str,
    pub kind: PushKind,
    #[cfg_attr(feature = "serde", serde(serialize_with = "serde_support::oid_hex"))]
    pub old_oid: ObjectId,
    #[cfg_attr(feature = "serde", serde(serialize_with = "serde_support::oid_hex"))]
    pub new_oid: ObjectId,
}

/// Identity from a commit/tag header (author, committer, tagger).
///
/// Fields are stored as raw bytes because git allows non-UTF-8 in name and
/// email fields.  Forges that only deal with ASCII identities can use
/// [`bstr::ByteSlice::to_str`] to lift them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Identity {
    #[cfg_attr(
        feature = "serde",
        serde(serialize_with = "serde_support::bstring_lossy")
    )]
    pub name: BString,
    #[cfg_attr(
        feature = "serde",
        serde(serialize_with = "serde_support::bstring_lossy")
    )]
    pub email: BString,
    /// Raw time field as it appears in the header, e.g. `1700000000 +0000`.
    #[cfg_attr(
        feature = "serde",
        serde(serialize_with = "serde_support::bstring_lossy")
    )]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub enum SignedIdentity {
    Pgp {
        key_id: String,
        #[cfg_attr(
            feature = "serde",
            serde(serialize_with = "serde_support::bstring_lossy")
        )]
        email: BString,
    },
    Ssh {
        fingerprint: String,
        #[cfg_attr(
            feature = "serde",
            serde(serialize_with = "serde_support::opt_bstring_lossy")
        )]
        principal: Option<BString>,
    },
    X509 {
        subject: String,
        san: Option<String>,
    },
    /// Identity material outside the natively-supported formats.
    Other { description: String },
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

#[cfg(all(test, feature = "serde"))]
mod serde_tests {
    use super::*;

    /// Locks in the actual point of `serde_support`: OIDs and git-identity
    /// bytes must come out as plain strings, not the raw-bytes shape
    /// `gix-hash`'s / `bstr`'s own `Serialize` impls would produce.
    #[test]
    fn push_ref_serializes_oids_as_hex_strings() {
        let oid = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
        let push_ref = PushRef {
            refname: "refs/heads/main",
            kind: PushKind::FastForward,
            old_oid: gix::hash::Kind::Sha1.null(),
            new_oid: oid,
        };
        let json = serde_json::to_value(&push_ref).unwrap();
        assert_eq!(json["new_oid"], "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(json["old_oid"], "0000000000000000000000000000000000000000");
        assert_eq!(json["kind"], "FastForward");
    }

    #[test]
    fn identity_serializes_bstring_as_utf8_string() {
        let identity = Identity {
            name: BString::from("Alice Example"),
            email: BString::from("alice@example.com"),
            time: BString::from("1700000000 +0000"),
        };
        let json = serde_json::to_value(&identity).unwrap();
        assert_eq!(json["name"], "Alice Example");
        assert_eq!(json["email"], "alice@example.com");
    }

    #[test]
    fn verification_status_verified_serializes_readably() {
        let status = VerificationStatus::Verified {
            identity: SignedIdentity::Pgp {
                key_id: "ABCDEF1234567890".into(),
                email: BString::from("alice@example.com"),
            },
            format: SignatureFormat::OpenPgp,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(
            json["Verified"]["identity"]["Pgp"]["email"],
            "alice@example.com"
        );
        assert_eq!(json["Verified"]["format"], "OpenPgp");
    }
}
