//! Signature verification dispatch and signed-payload reconstruction.
//!
//! Phase B plumbing: this module owns the bytes-level work of recovering the
//! canonical signed payload from a commit/tag object and dispatching to a
//! per-format verifier.  Phase C adds real verifiers; until then every format
//! returns `UnsupportedFormat` (or `UnknownKey` if the forge supplied keys
//! but no verifier is wired up).

use crate::auth_types::{SignatureFormat, VerificationKey, VerificationStatus};

/// Strip the `gpgsig` header (and any continuation lines) from a raw commit
/// object, leaving the canonical bytes that were signed.
///
/// Per `git-commit(1)`: signed commits embed the armoured signature inside a
/// `gpgsig` (or `gpgsig-sha256`) extra-header.  The canonical signed payload
/// is the commit object with that header removed entirely.  Continuation
/// lines (those starting with a single space) are part of the same header
/// and must be stripped too.
pub fn strip_gpgsig(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut lines = SplitLinesKeepTerm::new(raw);
    let mut in_body = false;
    let mut skipping = false;

    while let Some(line) = lines.next() {
        if !in_body {
            // Empty line marks the header / body boundary.
            if line == b"\n" || line == b"\r\n" || line.is_empty() {
                in_body = true;
                skipping = false;
                out.extend_from_slice(line);
                continue;
            }

            if skipping {
                // Continuation lines start with a single space.
                if line.starts_with(b" ") {
                    continue;
                }
                skipping = false;
            }

            if line.starts_with(b"gpgsig ") || line.starts_with(b"gpgsig-sha256 ") {
                skipping = true;
                continue;
            }
        }

        out.extend_from_slice(line);
    }
    out
}

/// Strip the trailing PGP signature block from an annotated tag object.
///
/// Tags don't use the `gpgsig` header — instead the signature block is
/// appended to the message body.  The canonical signed payload is the tag
/// object up to (but not including) the line beginning the signature block.
pub fn strip_tag_signature(raw: &[u8]) -> Vec<u8> {
    const MARKERS: &[&[u8]] = &[
        b"-----BEGIN PGP SIGNATURE-----",
        b"-----BEGIN SSH SIGNATURE-----",
        b"-----BEGIN SIGNED MESSAGE-----",
    ];
    for marker in MARKERS {
        if let Some(pos) = find_line_start(raw, marker) {
            return raw[..pos].to_vec();
        }
    }
    raw.to_vec()
}

/// Find the byte position of `needle` only when it starts a line.
fn find_line_start(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let mut search_from = 0;
    while let Some(rel) = haystack[search_from..]
        .windows(needle.len())
        .position(|w| w == needle)
    {
        let pos = search_from + rel;
        if pos == 0 || haystack[pos - 1] == b'\n' {
            return Some(pos);
        }
        search_from = pos + 1;
    }
    None
}

/// Iterator yielding lines including their trailing `\n`.
struct SplitLinesKeepTerm<'a> {
    bytes: &'a [u8],
}

impl<'a> SplitLinesKeepTerm<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.bytes.is_empty() {
            return None;
        }
        let end = match self.bytes.iter().position(|&b| b == b'\n') {
            Some(p) => p + 1,
            None => self.bytes.len(),
        };
        let (line, rest) = self.bytes.split_at(end);
        self.bytes = rest;
        Some(line)
    }
}

/// Dispatch native verification for `format`.  Returns `UnknownKey` if the
/// forge supplied no candidate keys; otherwise `UnsupportedFormat` until
/// Phase C wires real verifiers in.
pub fn verify_native(
    format: SignatureFormat,
    _signature: &[u8],
    _signed_payload: &[u8],
    keys: &[&VerificationKey],
) -> VerificationStatus {
    match format {
        SignatureFormat::OpenPgp | SignatureFormat::Ssh | SignatureFormat::X509Cms => {
            if keys.is_empty() {
                VerificationStatus::UnknownKey
            } else {
                // Phase C will replace this branch with real verification.
                VerificationStatus::UnsupportedFormat
            }
        }
        SignatureFormat::Unknown => VerificationStatus::UnsupportedFormat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_gpgsig_removes_header_block_and_continuations() {
        let raw = b"tree abc\n\
                    parent def\n\
                    author A <a@x> 1 +0000\n\
                    committer A <a@x> 1 +0000\n\
                    gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQE...\n \n -----END PGP SIGNATURE-----\n\
                    \n\
                    Hello\n";
        let out = strip_gpgsig(raw);
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("gpgsig"), "header gone");
        assert!(!s.contains("PGP SIGNATURE"), "continuation lines gone");
        assert!(s.contains("author A"));
        assert!(s.contains("Hello"));
        // Header block separator should be preserved.
        assert!(s.contains("\n\nHello"));
    }

    #[test]
    fn strip_gpgsig_no_signature_is_identity() {
        let raw = b"tree abc\nauthor A <a@x> 1 +0000\n\nHello\n";
        assert_eq!(strip_gpgsig(raw), raw.to_vec());
    }

    #[test]
    fn strip_tag_signature_finds_pgp_block() {
        let raw = b"object abc\ntype commit\ntag v1\ntagger T <t@x> 1 +0000\n\n\
                    Release notes\n\
                    -----BEGIN PGP SIGNATURE-----\n\
                    iQE...\n\
                    -----END PGP SIGNATURE-----\n";
        let out = strip_tag_signature(raw);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("Release notes"));
        assert!(!s.contains("BEGIN PGP SIGNATURE"));
    }
}
