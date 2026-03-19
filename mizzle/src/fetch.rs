use core::sync::atomic::AtomicBool;
use futures_lite::AsyncWrite;
use gix::{objs::Exists, parallel::InOrderIter, ObjectId};
use gix_packetline::{
    async_io::encode::{band_to_write, delim_to_write, flush_to_write, text_to_write},
    Channel,
};

pub use mizzle_proto::fetch::{read_fetch_args, read_fetch_args_v1, FetchArgs};

/// Build a packfile from the given want/have sets, returning the raw pack bytes
/// and the list of shallow boundary commits.
fn build_pack_bytes(
    mut handle: gix::OdbHandle,
    want: &[ObjectId],
    have: &[ObjectId],
    deepen: Option<u32>,
    filter: Option<&crate::pack::Filter>,
    thin_pack: bool,
) -> anyhow::Result<(Vec<u8>, Vec<ObjectId>)> {
    handle.prevent_pack_unload();
    handle.ignore_replacements = true;

    let should_interrupt = AtomicBool::new(false);
    let progress = gix_features::progress::Discard {};

    let pack_objects = crate::pack::objects_for_fetch_filtered(
        handle.clone().into_inner(),
        want,
        have,
        deepen,
        filter,
    )?;

    let shallow = pack_objects.shallow.clone();

    let (counts, _) = gix_pack::data::output::count::objects(
        handle.clone().into_inner(),
        Box::new(pack_objects.objects.into_iter().map(Ok)),
        &progress,
        &should_interrupt,
        gix_pack::data::output::count::objects::Options {
            thread_limit: None,
            chunk_size: 16,
            input_object_expansion: gix_pack::data::output::count::objects::ObjectExpansion::AsIs,
        },
    )?;
    let counts: Vec<_> = counts.into_iter().collect();
    let num_objects = counts.len();

    let mut in_order_entries = InOrderIter::from(gix_pack::data::output::entry::iter_from_counts(
        counts,
        handle.into_inner(),
        Box::new(progress),
        gix_pack::data::output::entry::iter_from_counts::Options {
            thread_limit: None,
            mode: gix_pack::data::output::entry::iter_from_counts::Mode::PackCopyAndBaseObjects,
            allow_thin_pack: thin_pack,
            chunk_size: 16,
            version: Default::default(),
        },
    ));

    let mut buf: Vec<u8> = vec![];
    let mut pack_iter = gix_pack::data::output::bytes::FromEntriesIter::new(
        in_order_entries.by_ref(),
        &mut buf,
        num_objects as u32,
        Default::default(),
        gix_hash::Kind::default(),
    );
    pack_iter.try_for_each(|_| Ok::<_, anyhow::Error>(()))?;

    Ok((buf, shallow))
}

pub async fn perform_fetch(
    handle: gix::OdbHandle,
    args: &FetchArgs,
    writer: &mut (impl AsyncWrite + Unpin),
) -> anyhow::Result<()> {
    let mut acks: Vec<ObjectId> = Vec::new();

    // output = acknowledgements flush-pkt |
    //   [acknowledgments delim-pkt] [shallow-info delim-pkt]
    //   [wanted-refs delim-pkt] [packfile-uris delim-pkt]
    //   packfile flush-pkt

    if !args.done {
        for id in args.have.iter() {
            if handle.clone().into_inner().exists(id) {
                acks.push(id.clone());
            }
        }

        // The server is ready to build a pack when it has at least one
        // common object with the client — unless the client asked for
        // wait-for-done, in which case the server must not declare
        // readiness on its own.
        let ready = !acks.is_empty() && !args.wait_for_done;

        text_to_write(b"acknowledgments", &mut *writer).await?;
        if acks.is_empty() {
            text_to_write(b"NAK", &mut *writer).await?;
        } else {
            for ack in &acks {
                text_to_write(format!("ACK {}", ack).as_bytes(), &mut *writer).await?;
            }
        }
        if !ready {
            flush_to_write(&mut *writer).await?;
            return Ok(());
        }
        text_to_write(b"ready", &mut *writer).await?;
        delim_to_write(&mut *writer).await?;
        // Fall through to build packfile below.
    }

    let filter = args
        .filter
        .as_deref()
        .map(crate::pack::Filter::parse)
        .transpose()?;
    let (pack_bytes, shallow) = build_pack_bytes(
        handle,
        &args.want,
        &args.have,
        args.deepen,
        filter.as_ref(),
        args.thin_pack,
    )?;

    // shallow-info section: tell the client which commits are shallow
    // boundaries so it knows not to expect their parents.
    if !shallow.is_empty() {
        text_to_write(b"shallow-info", &mut *writer).await?;
        for id in &shallow {
            text_to_write(format!("shallow {}", id).as_bytes(), &mut *writer).await?;
        }
        delim_to_write(&mut *writer).await?;
    }

    text_to_write(b"packfile", &mut *writer).await?;
    for chunk in pack_bytes.chunks(65516 - 16) {
        band_to_write(Channel::Data, chunk, &mut *writer).await?;
    }
    flush_to_write(&mut *writer).await?;

    Ok(())
}

/// Send a protocol v1 upload-pack response: `NAK` followed by sideband pack data.
///
/// The pack content is still optimised against the client's `have` set even
/// though we don't send per-object `ACK` lines (we don't advertise multi_ack).
pub async fn perform_fetch_v1(
    handle: gix::OdbHandle,
    args: &FetchArgs,
    writer: &mut (impl AsyncWrite + Unpin),
) -> anyhow::Result<()> {
    let filter = args
        .filter
        .as_deref()
        .map(crate::pack::Filter::parse)
        .transpose()?;
    let (pack_bytes, shallow) = build_pack_bytes(
        handle,
        &args.want,
        &args.have,
        args.deepen,
        filter.as_ref(),
        args.thin_pack,
    )?;

    // In v1, shallow boundaries are sent before the NAK.
    for id in &shallow {
        text_to_write(format!("shallow {}", id).as_bytes(), &mut *writer).await?;
    }
    text_to_write(b"NAK", &mut *writer).await?;
    for chunk in pack_bytes.chunks(65516 - 16) {
        band_to_write(Channel::Data, chunk, &mut *writer).await?;
    }
    flush_to_write(&mut *writer).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::ObjectId;
    use std::{fs, path::Path, process::Command};
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

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.name", "T"]);
        git(dir, &["config", "user.email", "t@t.com"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
    }

    /// Read all pkt-line output from a Writer into a Vec of parsed lines.
    /// Returns (lines, raw_bytes) where lines are decoded text strings.
    fn collect_pkt_output(reader: piper::Reader) -> Vec<u8> {
        use futures_lite::AsyncReadExt;
        futures_lite::future::block_on(async {
            let mut buf = Vec::new();
            futures_lite::pin!(reader);
            reader.read_to_end(&mut buf).await.unwrap();
            buf
        })
    }

    /// Parse pkt-line encoded data and return the text lines (without framing).
    fn parse_pkt_lines(data: &[u8]) -> Vec<String> {
        let mut pos = 0;
        let mut lines = Vec::new();
        while pos + 4 <= data.len() {
            let len_hex = std::str::from_utf8(&data[pos..pos + 4]).unwrap();
            let len = usize::from_str_radix(len_hex, 16).unwrap();
            if len == 0 {
                lines.push("<flush>".to_string());
                pos += 4;
                continue;
            }
            if len == 1 {
                lines.push("<delim>".to_string());
                pos += 4;
                continue;
            }
            if pos + len > data.len() {
                break;
            }
            let payload = &data[pos + 4..pos + len];
            // Skip sideband channel bytes
            if !payload.is_empty() && (payload[0] == 1 || payload[0] == 2 || payload[0] == 3) {
                lines.push(format!("<band-{}>", payload[0]));
            } else {
                let s = String::from_utf8_lossy(payload);
                lines.push(s.trim_end_matches('\n').to_string());
            }
            pos += len;
        }
        lines
    }

    /// When the client sends haves that the server knows (without done),
    /// the server should respond with ACKs + "ready" and include a packfile,
    /// rather than requiring an extra round-trip.
    #[test]
    fn negotiation_sends_ready_when_haves_are_known() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        init_repo(p);

        fs::write(p.join("a.txt"), "a\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "C1"]);
        let c1 = rev_parse(p, "HEAD");

        fs::write(p.join("b.txt"), "b\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "C2"]);
        let c2 = rev_parse(p, "HEAD");

        let handle = gix::open(p).unwrap().objects;
        let args = FetchArgs {
            want: vec![c2],
            want_refs: Vec::new(),
            have: vec![c1],
            done: false,
            thin_pack: false,
            no_progress: true,
            include_tag: false,
            ofs_delta: false,
            wait_for_done: false,
            deepen: None,
            filter: None,
        };

        let (reader, mut writer) = piper::pipe(65536);
        futures_lite::future::block_on(async {
            perform_fetch(handle, &args, &mut writer).await.unwrap();
        });
        drop(writer);

        let raw = collect_pkt_output(reader);
        let lines = parse_pkt_lines(&raw);

        assert!(
            lines.contains(&"acknowledgments".to_string()),
            "expected acknowledgments section, got: {:?}",
            lines
        );
        assert!(
            lines.iter().any(|l| l.starts_with("ACK ")),
            "expected ACK for known have, got: {:?}",
            lines
        );
        assert!(
            lines.contains(&"ready".to_string()),
            "expected ready signal when haves are known, got: {:?}",
            lines
        );
        assert!(
            lines.contains(&"packfile".to_string()),
            "expected packfile section after ready, got: {:?}",
            lines
        );
    }

    /// When wait-for-done is set and done is NOT sent, the server must NOT
    /// send ready or a packfile — even if it knows all the haves.
    #[test]
    fn negotiation_no_ready_with_wait_for_done() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        init_repo(p);

        fs::write(p.join("a.txt"), "a\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "C1"]);
        let c1 = rev_parse(p, "HEAD");

        fs::write(p.join("b.txt"), "b\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "C2"]);
        let c2 = rev_parse(p, "HEAD");

        let handle = gix::open(p).unwrap().objects;
        let args = FetchArgs {
            want: vec![c2],
            want_refs: Vec::new(),
            have: vec![c1],
            done: false,
            thin_pack: false,
            no_progress: true,
            include_tag: false,
            ofs_delta: false,
            wait_for_done: true,
            deepen: None,
            filter: None,
        };

        let (reader, mut writer) = piper::pipe(65536);
        futures_lite::future::block_on(async {
            perform_fetch(handle, &args, &mut writer).await.unwrap();
        });
        drop(writer);

        let raw = collect_pkt_output(reader);
        let lines = parse_pkt_lines(&raw);

        assert!(
            lines.iter().any(|l| l.starts_with("ACK ")),
            "expected ACK lines, got: {:?}",
            lines
        );
        assert!(
            !lines.contains(&"ready".to_string()),
            "server must NOT send ready when wait-for-done is set, got: {:?}",
            lines
        );
        assert!(
            !lines.contains(&"packfile".to_string()),
            "server must NOT send packfile without done when wait-for-done is set, got: {:?}",
            lines
        );
    }

    /// A shallow clone with --depth 1 should only include the tip commit
    /// and its trees/blobs, not any parent commits.
    #[test]
    fn shallow_fetch_depth_1_includes_only_tip() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        init_repo(p);

        fs::write(p.join("a.txt"), "a\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "C1"]);

        fs::write(p.join("b.txt"), "b\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "C2"]);

        fs::write(p.join("c.txt"), "c\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "C3"]);
        let c3 = rev_parse(p, "HEAD");

        let handle = gix::open(p).unwrap().objects;
        let args = FetchArgs {
            want: vec![c3],
            want_refs: Vec::new(),
            have: Vec::new(),
            done: true,
            thin_pack: false,
            no_progress: true,
            include_tag: false,
            ofs_delta: false,
            wait_for_done: false,
            deepen: Some(1),
            filter: None,
        };

        let (reader, mut writer) = piper::pipe(65536);
        futures_lite::future::block_on(async {
            perform_fetch(handle, &args, &mut writer).await.unwrap();
        });
        drop(writer);

        let raw = collect_pkt_output(reader);
        let lines = parse_pkt_lines(&raw);

        // Should have shallow-info with the tip commit as the shallow boundary
        assert!(
            lines.contains(&"shallow-info".to_string()),
            "expected shallow-info section, got: {:?}",
            lines
        );
        assert!(
            lines.iter().any(|l| l == &format!("shallow {}", c3)),
            "expected shallow boundary at C3, got: {:?}",
            lines
        );
        assert!(
            lines.contains(&"packfile".to_string()),
            "expected packfile section, got: {:?}",
            lines
        );
    }
}
