//! Git LFS protocol types.
//!
//! Contains the batch API request/response structs, `LfsOid`, pointer-blob
//! parsing, and the transfer-action enum.  No storage dependency.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ---------------------------------------------------------------------------
// LfsOid
// ---------------------------------------------------------------------------

/// A Git LFS object identifier — the SHA-256 hash of the object's content.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct LfsOid(pub [u8; 32]);

impl LfsOid {
    /// Return the lowercase hex encoding of the OID (no prefix).
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.0 {
            use fmt::Write;
            write!(s, "{:02x}", b).unwrap();
        }
        s
    }
}

impl fmt::Display for LfsOid {
    /// Formats as `sha256:<hex>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.to_hex())
    }
}

impl fmt::Debug for LfsOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LfsOid({})", self)
    }
}

/// Parse `sha256:<64 hex digits>`.
impl FromStr for LfsOid {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s
            .strip_prefix("sha256:")
            .ok_or_else(|| format!("LfsOid must start with 'sha256:', got: {s:?}"))?;
        if hex.len() != 64 {
            return Err(format!(
                "LfsOid hex part must be 64 chars, got {}",
                hex.len()
            ));
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
            let hi = hex_nibble(chunk[0])?;
            let lo = hex_nibble(chunk[1])?;
            bytes[i] = (hi << 4) | lo;
        }
        Ok(LfsOid(bytes))
    }
}

fn hex_nibble(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("invalid hex digit: {:?}", b as char)),
    }
}

impl Serialize for LfsOid {
    /// Serializes as plain hex (no `sha256:` prefix).
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for LfsOid {
    /// Deserializes from plain hex (no `sha256:` prefix).
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        // Accept both plain hex and sha256: prefixed forms
        let with_prefix = if s.starts_with("sha256:") {
            s.clone()
        } else {
            format!("sha256:{s}")
        };
        with_prefix
            .parse::<LfsOid>()
            .map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Operation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    Download,
    Upload,
}

// ---------------------------------------------------------------------------
// Batch request types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRequest {
    pub operation: Operation,
    #[serde(default)]
    pub transfers: Vec<String>,
    pub objects: Vec<BatchRequestObject>,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<BatchRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRequestObject {
    pub oid: LfsOid,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRef {
    pub name: String,
}

// ---------------------------------------------------------------------------
// Batch response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResponse {
    pub transfer: String,
    pub objects: Vec<BatchResponseObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResponseObject {
    pub oid: LfsOid,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<BatchObjectActions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BatchObjectError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchObjectActions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download: Option<BatchActionDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload: Option<BatchActionDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify: Option<BatchActionDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchActionDetail {
    pub href: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub header: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchObjectError {
    pub code: u16,
    pub message: String,
}

// ---------------------------------------------------------------------------
// LFS pointer
// ---------------------------------------------------------------------------

/// A parsed Git LFS pointer blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsPointer {
    pub oid: LfsOid,
    pub size: u64,
}

/// Parse a Git LFS pointer blob.
///
/// Returns `None` if the blob does not look like a valid LFS pointer.
pub fn parse_pointer(blob: &[u8]) -> Option<LfsPointer> {
    let text = std::str::from_utf8(blob).ok()?;
    let mut lines = text.lines();

    // First line must be the version header.
    let first = lines.next()?;
    if first != "version https://git-lfs.github.com/spec/v1" {
        return None;
    }

    let mut oid: Option<LfsOid> = None;
    let mut size: Option<u64> = None;

    for line in lines {
        if let Some(rest) = line.strip_prefix("oid ") {
            oid = rest.parse::<LfsOid>().ok();
        } else if let Some(rest) = line.strip_prefix("size ") {
            size = rest.parse::<u64>().ok();
        }
    }

    Some(LfsPointer {
        oid: oid?,
        size: size?,
    })
}

/// Serialize a [`LfsPointer`] into its canonical pointer blob text.
pub fn write_pointer(ptr: &LfsPointer) -> String {
    format!(
        "version https://git-lfs.github.com/spec/v1\noid {}\nsize {}\n",
        ptr.oid, ptr.size
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_roundtrip_display_fromstr() {
        let hex = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let s = format!("sha256:{hex}");
        let oid: LfsOid = s.parse().unwrap();
        assert_eq!(oid.to_string(), s);
        assert_eq!(oid.to_hex(), hex);
    }

    #[test]
    fn oid_serde_plain_hex() {
        let hex = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let s = format!("sha256:{hex}");
        let oid: LfsOid = s.parse().unwrap();

        // Serialises as plain hex (no prefix).
        let json = serde_json::to_string(&oid).unwrap();
        assert_eq!(json, format!("\"{hex}\""));

        // Deserialises from plain hex.
        let oid2: LfsOid = serde_json::from_str(&json).unwrap();
        assert_eq!(oid, oid2);

        // Also deserialises from sha256: prefixed form.
        let json_prefixed = format!("\"sha256:{hex}\"");
        let oid3: LfsOid = serde_json::from_str(&json_prefixed).unwrap();
        assert_eq!(oid, oid3);
    }

    #[test]
    fn pointer_parse_canonical() {
        let blob = b"version https://git-lfs.github.com/spec/v1\noid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\nsize 12345\n";
        let ptr = parse_pointer(blob).unwrap();
        assert_eq!(ptr.size, 12345);
        assert_eq!(
            ptr.oid.to_string(),
            "sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393"
        );
    }

    #[test]
    fn pointer_parse_bad_version() {
        let blob = b"version https://example.com/bad\noid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\nsize 12345\n";
        assert!(parse_pointer(blob).is_none());
    }

    #[test]
    fn pointer_write_roundtrip() {
        let blob = b"version https://git-lfs.github.com/spec/v1\noid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\nsize 12345\n";
        let ptr = parse_pointer(blob).unwrap();
        let written = write_pointer(&ptr);
        assert_eq!(written.as_bytes(), blob.as_slice());
    }
}
