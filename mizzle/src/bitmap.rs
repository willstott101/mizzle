//! Pack reachability-bitmap reader.
//!
//! Parses git's `.bitmap` (v1) + `.rev` files sitting alongside a pack's
//! `.idx`.  Given a set of `have` commit OIDs, returns the full set of
//! objects reachable from those commits in one pass by OR-ing the per-commit
//! EWAH bitmaps — replacing the commit + tree walk in
//! [`crate::pack::build_have_set`].
//!
//! Gitoxide does not ship a reachability-bitmap reader, only the lower-level
//! EWAH primitive in `gix-bitmap`.  This module fills the gap for the
//! filesystem backend.
//!
//! Scope: v1 bitmaps, sha1 hashes, single-pack.  Multi-pack bitmaps (midx)
//! and OPT_LOOKUP_TABLE (v3) are not supported; a bitmap that uses features
//! we don't handle is treated as "no bitmap" and the caller falls back to
//! the walker.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use gix::ObjectId;

const BITMAP_MAGIC: &[u8; 4] = b"BITM";
const REV_MAGIC: &[u8; 4] = b"RIDX";
const SHA1_LEN: usize = 20;
const BITMAP_HEADER_LEN: usize = 4 + 2 + 2 + 4 + SHA1_LEN;

const FLAG_LOOKUP_TABLE: u16 = 0x10;

/// Loaded reachability bitmap for a single pack.
pub struct PackBitmap {
    /// Bit position (in the bitmap's pack-offset ordering) → idx position
    /// (OID-sorted position within the `.idx`).  Read from the `.rev` file.
    offset_pos_to_idx_pos: Vec<u32>,
    /// Bitmap position → OID.  Populated by [`build_oid_index`].
    offset_pos_to_oid: Vec<Option<ObjectId>>,
    /// OID → entry index in `entries`.
    oid_to_entry: HashMap<ObjectId, usize>,
    /// Entries in on-disk order, with pre-decoded EWAH bitmaps.
    entries: Vec<Entry>,
    /// Number of objects in the pack.  Drives the dense bitmap size.
    pack_object_count: u32,
}

struct Entry {
    /// The commit's position in the `.idx` (OID-sorted) order.
    idx_pos: u32,
    /// Number of entries back in this list whose fully-decoded bitmap must
    /// be XOR'd with ours to recover the real bitmap.  `0` means no XOR.
    xor_offset: u8,
    /// Parsed EWAH bitmap.
    ewah: gix_bitmap::ewah::Vec,
}

impl PackBitmap {
    /// Try to load a bitmap for the pack whose `.idx` is at `idx_path`.
    /// Returns `Ok(None)` if no `.bitmap` or `.rev` file is present, or if
    /// the bitmap uses features this reader doesn't support.
    pub fn load(idx_path: &Path, pack_object_count: u32) -> Result<Option<Self>> {
        let bitmap_path = idx_path.with_extension("bitmap");
        let rev_path = idx_path.with_extension("rev");
        if !bitmap_path.is_file() || !rev_path.is_file() {
            return Ok(None);
        }

        let bitmap_bytes = fs::read(&bitmap_path).context("reading .bitmap")?;
        let rev_bytes = fs::read(&rev_path).context("reading .rev")?;

        let offset_pos_to_idx_pos = parse_rev(&rev_bytes, pack_object_count)?;
        let Some((entries, _flags)) = parse_bitmap(&bitmap_bytes)? else {
            return Ok(None);
        };

        Ok(Some(Self {
            offset_pos_to_oid: vec![None; offset_pos_to_idx_pos.len()],
            offset_pos_to_idx_pos,
            oid_to_entry: HashMap::new(),
            entries,
            pack_object_count,
        }))
    }

    /// Populate the OID lookup tables.  Called after [`load`] with a callback
    /// that maps `.idx` positions to object IDs (via `gix_pack::index::File`
    /// in the caller).
    ///
    /// Note: an entry's first u32 is the commit's `.idx` position (the
    /// position of the commit's OID in the pack index's OID-sorted order),
    /// not its pack-offset-order position.  The `.rev` translation only
    /// applies to bit positions *within* the EWAH payload.
    pub fn build_oid_index(&mut self, oid_at_idx_pos: impl Fn(u32) -> Option<ObjectId>) {
        self.oid_to_entry.reserve(self.entries.len());
        for (entry_idx, entry) in self.entries.iter().enumerate() {
            if let Some(oid) = oid_at_idx_pos(entry.idx_pos) {
                self.oid_to_entry.insert(oid, entry_idx);
            }
        }
        for (bitmap_pos, &idx_pos) in self.offset_pos_to_idx_pos.iter().enumerate() {
            self.offset_pos_to_oid[bitmap_pos] = oid_at_idx_pos(idx_pos);
        }
    }

    /// If every `have` OID is covered by this bitmap, return the complete
    /// set of OIDs reachable from any of them.  Returns `None` if any have
    /// is missing from the bitmap (caller should fall back to the walker).
    ///
    /// Must be called after [`build_oid_index`].
    pub fn have_reachable(&self, haves: &[ObjectId]) -> Option<HashSet<ObjectId>> {
        // Resolve each have to an entry index up front.
        let mut entry_indices = Vec::with_capacity(haves.len());
        for have in haves {
            let idx = *self.oid_to_entry.get(have)?;
            entry_indices.push(idx);
        }

        // Decode the union of the haves' reachability bitmaps, chasing XOR
        // chains as we go.  `decoded` caches fully-decoded bitmaps keyed by
        // entry index; the chain dependencies often overlap.
        let mut decoded: HashMap<usize, DenseBitmap> = HashMap::new();
        let mut union = DenseBitmap::new(self.pack_object_count as usize);
        for entry_idx in entry_indices {
            let bitmap = self.decode_entry(entry_idx, &mut decoded);
            union.or_with(&bitmap);
        }

        // Map set bits back to OIDs using the index built by build_oid_index.
        let mut out: HashSet<ObjectId> = HashSet::with_capacity(union.popcount());
        union.for_each_set_bit(|bitmap_pos| {
            let oid = self.offset_pos_to_oid.get(bitmap_pos)?.as_ref()?;
            out.insert(*oid);
            Some(())
        });
        Some(out)
    }

    fn decode_entry(
        &self,
        entry_idx: usize,
        cache: &mut HashMap<usize, DenseBitmap>,
    ) -> DenseBitmap {
        if let Some(cached) = cache.get(&entry_idx) {
            return cached.clone();
        }
        let entry = &self.entries[entry_idx];
        let mut bitmap = DenseBitmap::from_ewah(&entry.ewah, self.pack_object_count as usize);
        if entry.xor_offset != 0 {
            let base_idx = entry_idx
                .checked_sub(entry.xor_offset as usize)
                .expect("xor_offset points past start of entries");
            let base = self.decode_entry(base_idx, cache);
            bitmap.xor_with(&base);
        }
        cache.insert(entry_idx, bitmap.clone());
        bitmap
    }
}

// ── .rev parsing ────────────────────────────────────────────────────────────

fn parse_rev(bytes: &[u8], pack_object_count: u32) -> Result<Vec<u32>> {
    if bytes.len() < 12 + SHA1_LEN {
        bail!(".rev file too short: {} bytes", bytes.len());
    }
    if &bytes[0..4] != REV_MAGIC {
        bail!(".rev missing RIDX magic");
    }
    let version = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
    if version != 1 {
        bail!(".rev version {} unsupported", version);
    }
    let hash_algo = u32::from_be_bytes(bytes[8..12].try_into().unwrap());
    if hash_algo != 1 {
        bail!(".rev hash algo {} unsupported (sha1 only)", hash_algo);
    }
    let body_len = (pack_object_count as usize) * 4;
    if bytes.len() < 12 + body_len + SHA1_LEN {
        bail!(
            ".rev body too short: {} bytes for {} objects",
            bytes.len(),
            pack_object_count
        );
    }
    let body = &bytes[12..12 + body_len];
    let mut out = Vec::with_capacity(pack_object_count as usize);
    for chunk in body.chunks_exact(4) {
        out.push(u32::from_be_bytes(chunk.try_into().unwrap()));
    }
    Ok(out)
}

// ── .bitmap parsing ─────────────────────────────────────────────────────────

fn parse_bitmap(bytes: &[u8]) -> Result<Option<(Vec<Entry>, u16)>> {
    if bytes.len() < BITMAP_HEADER_LEN + SHA1_LEN {
        bail!(".bitmap too short");
    }
    if &bytes[0..4] != BITMAP_MAGIC {
        bail!(".bitmap missing BITM magic");
    }
    let version = u16::from_be_bytes(bytes[4..6].try_into().unwrap());
    if version != 1 {
        bail!(".bitmap version {} unsupported", version);
    }
    let flags = u16::from_be_bytes(bytes[6..8].try_into().unwrap());
    if flags & FLAG_LOOKUP_TABLE != 0 {
        // v3 lookup table not implemented.
        return Ok(None);
    }
    let entry_count = u32::from_be_bytes(bytes[8..12].try_into().unwrap());
    // Pack-hash at [12..32] — we don't verify here; the caller is
    // responsible for pairing the bitmap with the right pack.

    let mut cursor = &bytes[BITMAP_HEADER_LEN..];

    // Four type bitmaps (commits, trees, blobs, tags) — skip past them.
    for _ in 0..4 {
        let (_vec, rest) =
            gix_bitmap::ewah::decode(cursor).map_err(|e| anyhow!("EWAH type-bitmap: {e}"))?;
        cursor = rest;
    }

    let mut entries = Vec::with_capacity(entry_count as usize);
    for _ in 0..entry_count {
        if cursor.len() < 6 {
            bail!("truncated entry header");
        }
        let idx_pos = u32::from_be_bytes(cursor[0..4].try_into().unwrap());
        let xor_offset = cursor[4];
        let _ = cursor[5]; // per-entry flags, unused
        cursor = &cursor[6..];
        let (ewah, rest) =
            gix_bitmap::ewah::decode(cursor).map_err(|e| anyhow!("EWAH entry bitmap: {e}"))?;
        cursor = rest;
        entries.push(Entry {
            idx_pos,
            xor_offset,
            ewah,
        });
    }

    // If the hash-cache flag (0x4) is set, additional bytes follow the
    // entries before the 20-byte pack hash trailer.  We don't consume them;
    // nothing after this point is parsed.
    Ok(Some((entries, flags)))
}

// ── Dense bitmap helpers ────────────────────────────────────────────────────

#[derive(Clone)]
struct DenseBitmap {
    /// Packed 64-bit words.  Bit `i` lives in `words[i/64]` at position `i%64`.
    words: Vec<u64>,
    num_bits: usize,
}

impl DenseBitmap {
    fn new(num_bits: usize) -> Self {
        Self {
            words: vec![0u64; num_bits.div_ceil(64)],
            num_bits,
        }
    }

    fn from_ewah(ewah: &gix_bitmap::ewah::Vec, num_bits_hint: usize) -> Self {
        let num_bits = ewah.num_bits().max(num_bits_hint);
        let mut out = Self::new(num_bits);
        ewah.for_each_set_bit(|bit| {
            let word = bit / 64;
            let shift = bit % 64;
            if word < out.words.len() {
                out.words[word] |= 1u64 << shift;
            }
            Some(())
        });
        out
    }

    fn or_with(&mut self, other: &Self) {
        self.extend_to(other.num_bits);
        for (a, b) in self.words.iter_mut().zip(other.words.iter()) {
            *a |= *b;
        }
    }

    fn xor_with(&mut self, other: &Self) {
        self.extend_to(other.num_bits);
        for (a, b) in self.words.iter_mut().zip(other.words.iter()) {
            *a ^= *b;
        }
    }

    fn extend_to(&mut self, num_bits: usize) {
        if num_bits > self.num_bits {
            self.num_bits = num_bits;
            self.words.resize(num_bits.div_ceil(64), 0);
        }
    }

    fn popcount(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    fn for_each_set_bit(&self, mut f: impl FnMut(usize) -> Option<()>) {
        for (word_idx, &word) in self.words.iter().enumerate() {
            let mut w = word;
            while w != 0 {
                let bit = w.trailing_zeros() as usize;
                let global = word_idx * 64 + bit;
                if global < self.num_bits {
                    if f(global).is_none() {
                        return;
                    }
                }
                w &= w - 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::objects_for_fetch_filtered;
    use std::collections::HashSet;
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t.com")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t.com")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn rev_parse(cwd: &Path, rev: &str) -> ObjectId {
        ObjectId::from_hex(git(cwd, &["rev-parse", rev]).as_bytes()).unwrap()
    }

    fn find_pack_idx(objects_dir: &Path) -> PathBuf {
        let pack_dir = objects_dir.join("pack");
        for entry in std::fs::read_dir(&pack_dir).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().and_then(|s| s.to_str()) == Some("idx") {
                return p;
            }
        }
        panic!("no .idx file in {:?}", pack_dir);
    }

    /// Build a repo with N linear commits, run `git repack -adb`, and verify
    /// that bitmap-based have-set equals the walker-based have-set.
    #[test]
    fn bitmap_matches_walker_on_linear_history() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "commit.gpgsign", "false"]);

        let mut tips = Vec::new();
        for i in 0..20 {
            std::fs::write(p.join("f.txt"), format!("c{i}\n")).unwrap();
            git(p, &["add", "f.txt"]);
            git(p, &["commit", "-q", "-m", &format!("c{i}")]);
            tips.push(rev_parse(p, "HEAD"));
        }
        git(p, &["repack", "-adb"]);

        let idx_path = find_pack_idx(&p.join(".git/objects"));
        let pack_idx = gix_pack::index::File::at(&idx_path, gix_hash::Kind::Sha1).unwrap();
        let obj_count = pack_idx.num_objects();

        let mut bm = PackBitmap::load(&idx_path, obj_count).unwrap().unwrap();
        bm.build_oid_index(|i| pack_idx.oid_at_index(i).try_into().ok());

        for (i, &have) in tips.iter().enumerate() {
            let from_bitmap = bm.have_reachable(&[have]).expect("have covered by bitmap");

            // Ground truth via the walker.
            let odb = gix::open(p).unwrap().objects;
            let walked = {
                let result =
                    objects_for_fetch_filtered(odb.clone().into_inner(), &[have], &[], None, None)
                        .unwrap();
                let mut s: HashSet<ObjectId> = result.objects.into_iter().collect();
                s.insert(have);
                s
            };

            let mut missing: Vec<_> = walked.difference(&from_bitmap).copied().collect();
            missing.sort();
            let mut extra: Vec<_> = from_bitmap.difference(&walked).copied().collect();
            extra.sort();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "mismatch for tip[{i}]={have}: bitmap={} walker={} missing={missing:?} extra={extra:?}",
                from_bitmap.len(),
                walked.len(),
            );
        }

        // Multi-have: OR of two disjoint tips' closures.
        let haves = vec![tips[5], tips[10]];
        let from_bitmap = bm.have_reachable(&haves).expect("all haves covered");
        let odb = gix::open(p).unwrap().objects;
        let walked: HashSet<ObjectId> = {
            let result =
                objects_for_fetch_filtered(odb.clone().into_inner(), &haves, &[], None, None)
                    .unwrap();
            let mut s: HashSet<ObjectId> = result.objects.into_iter().collect();
            for h in &haves {
                s.insert(*h);
            }
            s
        };
        assert_eq!(from_bitmap, walked, "multi-have mismatch");
    }

    /// A have that isn't a bitmap entry (e.g., an arbitrary tree OID) must
    /// cause `have_reachable` to return `None` so the caller falls back to
    /// the walker.
    #[test]
    fn bitmap_returns_none_for_uncovered_have() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        std::fs::write(p.join("f.txt"), "only\n").unwrap();
        git(p, &["add", "f.txt"]);
        git(p, &["commit", "-q", "-m", "c0"]);
        let tip = rev_parse(p, "HEAD");
        let tree = rev_parse(p, "HEAD^{tree}");
        git(p, &["repack", "-adb"]);

        let idx_path = find_pack_idx(&p.join(".git/objects"));
        let pack_idx = gix_pack::index::File::at(&idx_path, gix_hash::Kind::Sha1).unwrap();
        let mut bm = PackBitmap::load(&idx_path, pack_idx.num_objects())
            .unwrap()
            .unwrap();
        bm.build_oid_index(|pos| pack_idx.oid_at_index(pos).try_into().ok());

        // Tree OID is not a bitmap entry (only commits are).
        assert!(bm.have_reachable(&[tree]).is_none());
        // Commit OID works.
        assert!(bm.have_reachable(&[tip]).is_some());
    }
}
