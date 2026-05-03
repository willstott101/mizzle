//! Pack inspection: extract metadata from ingested pack files for auth.

use std::path::Path;

use anyhow::{Context, Result};
use bstr::BString;
use gix::ObjectId;

use crate::auth_types::{CommitInfo, Identity, SignatureBlob, SignatureFormat, TagInfo};
use crate::backend::{ObjectKind, PackMetadata, PackObject};

/// Inspect an ingested pack file and extract metadata for each object.
///
/// `pack_path` must point to a `.pack` file with a corresponding `.idx` file
/// alongside it (as produced by `ingest_pack`).
pub fn inspect_pack(pack_path: &Path) -> Result<PackMetadata> {
    use gix_pack::data::{decode::header::ResolvedBase, entry::Header};

    let bundle =
        gix_pack::Bundle::at(pack_path, gix_hash::Kind::Sha1).context("opening pack bundle")?;

    let num_objects = bundle.index.num_objects();
    let mut objects = Vec::with_capacity(num_objects as usize);
    let mut buf = Vec::new();
    let mut inflate = gix_features::zlib::Inflate::default();
    let mut cache = gix_pack::cache::Never;

    // Resolve function for RefDelta bases: look up OID in our bundle index.
    let resolve = |oid: &gix_hash::oid| -> Option<ResolvedBase> {
        let idx = bundle.index.lookup(oid)?;
        let offset = bundle.index.pack_offset_at_index(idx);
        let entry = bundle.pack.entry(offset).ok()?;
        Some(ResolvedBase::InPack(entry))
    };

    for index in 0..num_objects {
        let oid = bundle.index.oid_at_index(index).to_owned();
        let offset = bundle.index.pack_offset_at_index(index);
        let entry = bundle
            .pack
            .entry(offset)
            .context("reading pack entry header")?;

        // Determine the resolved object kind and size. For non-delta entries
        // this is immediate from the header; for deltas we chase the base
        // chain via decode_header (partial inflate of ~32 bytes per hop).
        let (resolved_kind, object_size) = match entry.header {
            Header::Blob | Header::Tree | Header::Commit | Header::Tag => (
                entry
                    .header
                    .as_kind()
                    .expect("non-delta header always has a kind"),
                entry.decompressed_size,
            ),
            Header::OfsDelta { .. } | Header::RefDelta { .. } => {
                let outcome = bundle
                    .pack
                    .decode_header(entry, &mut inflate, &resolve)
                    .context("resolving delta header")?;
                (outcome.kind, outcome.object_size)
            }
        };

        match resolved_kind {
            // Blobs and trees: we already have type + size, no need to
            // inflate the object data.
            gix_object::Kind::Blob => {
                objects.push(PackObject {
                    oid,
                    kind: ObjectKind::Blob,
                    size: object_size,
                });
            }
            gix_object::Kind::Tree => {
                objects.push(PackObject {
                    oid,
                    kind: ObjectKind::Tree,
                    size: object_size,
                });
            }
            // Commits and tags: full decompress to parse metadata for auth.
            gix_object::Kind::Commit | gix_object::Kind::Tag => {
                let (data, _location) = bundle
                    .get_object_by_index(index, &mut buf, &mut inflate, &mut cache)
                    .context("decoding pack object")?;

                let kind = if resolved_kind == gix_object::Kind::Commit {
                    ObjectKind::Commit(parse_commit_info(data.data, oid)?)
                } else {
                    ObjectKind::Tag(parse_tag_info(data.data, oid)?)
                };

                objects.push(PackObject {
                    oid,
                    kind,
                    size: data.data.len() as u64,
                });
            }
        }
    }

    Ok(PackMetadata { objects })
}

pub(crate) fn parse_commit_info(data: &[u8], oid: ObjectId) -> Result<CommitInfo> {
    let commit = gix_object::CommitRef::from_bytes(data).context("parsing commit object")?;

    let tree = ObjectId::from_hex(commit.tree.as_ref()).context("parsing commit tree oid")?;
    let parents = commit
        .parents
        .iter()
        .map(|p| ObjectId::from_hex(p.as_ref()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parsing commit parent oids")?;

    let signature = commit
        .extra_headers()
        .pgp_signature()
        .map(|sig| sig.to_vec());
    let signature = signature.map(|bytes| {
        let format = SignatureFormat::detect(&bytes);
        SignatureBlob { format, bytes }
    });

    let author = commit.author().context("parsing commit author")?;
    let committer = commit.committer().context("parsing commit committer")?;

    Ok(CommitInfo {
        oid,
        tree,
        parents,
        author: identity_from(&author),
        committer: identity_from(&committer),
        message: commit.message.to_owned(),
        encoding: commit.encoding.map(|e| e.to_owned()),
        signature,
    })
}

pub(crate) fn parse_tag_info(data: &[u8], oid: ObjectId) -> Result<TagInfo> {
    let tag = gix_object::TagRef::from_bytes(data).context("parsing tag object")?;

    let signature = tag.pgp_signature.map(|s| s.to_vec());
    let signature = signature.map(|bytes| {
        let format = SignatureFormat::detect(&bytes);
        SignatureBlob { format, bytes }
    });

    let tagger = match tag.tagger {
        Some(raw) => {
            let s = gix::actor::SignatureRef::from_bytes::<()>(raw)
                .map_err(|e| anyhow::anyhow!("parsing tag tagger: {e:?}"))?;
            Some(identity_from(&s))
        }
        None => None,
    };

    Ok(TagInfo {
        oid,
        target: ObjectId::from_hex(tag.target.as_ref()).context("parsing tag target")?,
        name: tag.name.to_owned(),
        tagger,
        message: tag.message.to_owned(),
        signature,
    })
}

fn identity_from(s: &gix::actor::SignatureRef<'_>) -> Identity {
    Identity {
        name: s.name.to_owned(),
        email: s.email.to_owned(),
        time: BString::from(s.time),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_types::SignatureFormat;
    use crate::backend::fs_gitoxide::FsGitoxide;
    use crate::backend::{PackOptions, StorageBackend};
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test Author")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_NAME", "Test Committer")
            .env("GIT_COMMITTER_EMAIL", "committer@example.com")
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

    /// Build a pack, ingest it into a fresh repo, and inspect it.
    #[test]
    fn inspect_pack_extracts_commit_metadata() {
        let dir = tempdir().unwrap();
        let p = dir.path();

        // Create a repo with a commit.
        git(p, &["init", "-b", "main"]);
        git(p, &["config", "user.name", "T"]);
        git(p, &["config", "user.email", "t@t.com"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        std::fs::write(p.join("a.txt"), "hello\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "Initial commit"]);

        let main_oid = ObjectId::from_hex(git(p, &["rev-parse", "HEAD"]).as_bytes()).unwrap();

        // Build a pack from the repo.
        let backend = FsGitoxide;
        let repo = backend.open(&p.to_path_buf()).unwrap();
        let mut output = backend
            .build_pack(
                &repo,
                &[main_oid],
                &[],
                &PackOptions {
                    deepen: None,
                    filter: None,
                    thin_pack: false,
                },
            )
            .unwrap();
        let mut pack_data = Vec::new();
        std::io::Read::read_to_end(&mut output.reader, &mut pack_data).unwrap();

        // Create a fresh bare repo and ingest the pack.
        let target_dir = tempdir().unwrap();
        let target = target_dir.path().join("target.git");
        backend.init_repo(&target).unwrap();
        let target_repo = backend.open(&target).unwrap();

        let staged = target_dir.path().join("staged.pack");
        std::fs::write(&staged, &pack_data).unwrap();

        let written = backend.ingest_pack(&target_repo, &staged).unwrap().unwrap();

        // Inspect the pack.
        let meta = backend.inspect_ingested(&written).unwrap();

        // Should contain at least the commit, a tree, and a blob.
        assert!(
            meta.objects.len() >= 3,
            "expected at least 3 objects (commit + tree + blob), got {}",
            meta.objects.len()
        );

        // Find the commit and check the new structured metadata.
        let commit = meta.commits().next().expect("should contain a commit");
        assert_eq!(commit.oid, main_oid);
        assert_eq!(commit.parents.len(), 0, "initial commit has no parents");
        assert_eq!(commit.author.name, "Test Author");
        assert_eq!(commit.author.email, "author@example.com");
        assert_eq!(commit.committer.name, "Test Committer");
        assert_eq!(commit.committer.email, "committer@example.com");
        assert_eq!(commit.message, "Initial commit\n");
        assert!(
            commit.signature.is_none(),
            "unsigned commit should have no signature"
        );

        backend.rollback_ingest(written);
    }

    #[test]
    fn signature_format_detection() {
        assert_eq!(
            SignatureFormat::detect(b"-----BEGIN PGP SIGNATURE-----\nfoo"),
            SignatureFormat::OpenPgp
        );
        assert_eq!(
            SignatureFormat::detect(b"-----BEGIN SSH SIGNATURE-----\nfoo"),
            SignatureFormat::Ssh
        );
        assert_eq!(
            SignatureFormat::detect(b"-----BEGIN SIGNED MESSAGE-----\nfoo"),
            SignatureFormat::X509Cms
        );
        assert_eq!(SignatureFormat::detect(b"junk"), SignatureFormat::Unknown);
    }

    #[test]
    fn signed_payload_strips_gpgsig_header() {
        // A minimal commit object with a fake gpgsig header.
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
                    author A <a@x> 1700000000 +0000\n\
                    committer A <a@x> 1700000000 +0000\n\
                    gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQE...\n \n -----END PGP SIGNATURE-----\n\
                    \n\
                    Hello\n";

        let stripped = crate::sigverify::strip_gpgsig(raw);
        assert!(
            !stripped.windows(b"gpgsig".len()).any(|w| w == b"gpgsig"),
            "gpgsig header should be stripped"
        );
        assert!(
            stripped.windows(b"Hello".len()).any(|w| w == b"Hello"),
            "message body must remain"
        );
        assert!(
            stripped
                .windows(b"author A".len())
                .any(|w| w == b"author A"),
            "author header must remain"
        );
    }

    #[test]
    fn stage_pack_respects_temp_dir() {
        use crate::receive::stage_pack;
        use futures_lite::io::Cursor;

        let custom_dir = tempdir().unwrap();

        // Create a minimal pack with 0 objects (stage_pack returns None, but
        // the temp file should still be created in the custom dir).
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes()); // 1 object (fake, but enough for staging)
                                                     // Add some fake data so total > 0
        pack.extend_from_slice(&[0u8; 100]);

        let result =
            futures_lite::future::block_on(stage_pack(Cursor::new(pack), Some(custom_dir.path())))
                .unwrap();

        let staged = result.expect("should return Some for non-empty data");
        let staged_path = staged.path().to_path_buf();
        assert!(
            staged_path.starts_with(custom_dir.path()),
            "temp file {:?} should be under custom dir {:?}",
            staged_path,
            custom_dir.path()
        );
    }
}
