use crate::utils::skip_till_delimiter;
use anyhow::Context;
use core::sync::atomic::AtomicBool;
use gix::{objs::Exists, parallel::InOrderIter, ObjectId};
use gix_packetline::{
    async_io::encode::{band_to_write, delim_to_write, flush_to_write, text_to_write},
    Channel, PacketLineRef,
};

#[derive(Debug)]
pub struct FetchArgs {
    /// Indicates to the server an object which the client wants to
    /// retrieve.  Wants can be anything and are not limited to
    /// advertised objects.
    pub want: Vec<ObjectId>,
    /// Ref names the client wants resolved and included, as an
    /// alternative to specifying object IDs directly (ref-in-want).
    pub want_refs: Vec<String>,
    /// Indicates to the server an object which the client has locally.
    /// This allows the server to make a packfile which only contains
    /// the objects that the client needs. Multiple 'have' lines can be
    /// supplied.
    have: Vec<ObjectId>,
    /// Indicates to the server that negotiation should terminate (or
    /// not even begin if performing a clone) and that the server should
    /// use the information supplied in the request to construct the
    /// packfile.
    done: bool,
    /// Request that a thin pack be sent, which is a pack with deltas
    /// which reference base objects not contained within the pack (but
    /// are known to exist at the receiving end). This can reduce the
    /// network traffic significantly, but it requires the receiving end
    /// to know how to "thicken" these packs by adding the missing bases
    /// to the pack.
    thin_pack: bool,
    /// Request that progress information that would normally be sent on
    /// side-band channel 2, during the packfile transfer, should not be
    /// sent.  However, the side-band channel 3 is still used for error
    /// responses.
    no_progress: bool,
    /// Request that annotated tags should be sent if the objects they
    /// point to are being sent.
    include_tag: bool,
    /// Indicate that the client understands PACKv2 with delta referring
    /// to its base by position in pack rather than by an oid.  That is,
    /// they can read OBJ_OFS_DELTA (aka type 6) in a packfile.
    ofs_delta: bool,
    /// Client will explicitly send `done`; server must not declare
    /// `ready` on its own (wait-for-done).
    pub wait_for_done: bool,
}

pub async fn read_fetch_args<T>(
    parser: &mut gix_packetline::async_io::StreamingPeekableIter<T>,
) -> anyhow::Result<FetchArgs>
where
    T: futures_lite::AsyncRead + Unpin,
{
    // "command=ls-refs"
    // "agent=git/2.40.1"
    // None (delimiter)
    skip_till_delimiter(parser).await?; // TODO: Is this info we're skipping ever useful?
    let mut args = FetchArgs {
        want: Vec::new(),
        want_refs: Vec::new(),
        have: Vec::new(),
        done: false,
        thin_pack: false,
        no_progress: false,
        include_tag: false,
        ofs_delta: false,
        wait_for_done: false,
    };
    loop {
        let line = parser
            .read_line()
            .await
            .context("unexpected eof (missing flush packet?)")???;
        match line {
            PacketLineRef::ResponseEnd | PacketLineRef::Flush => break,
            PacketLineRef::Delimiter => anyhow::bail!("unexpected delimiter"),
            PacketLineRef::Data(d) => {
                let arg = d.strip_suffix(b"\n").unwrap_or(d);
                if let Some(oid) = arg.strip_prefix(b"want ") {
                    args.want.push(ObjectId::from_hex(oid)?);
                } else if let Some(oid) = arg.strip_prefix(b"have ") {
                    args.have.push(ObjectId::from_hex(oid)?);
                } else if let Some(refname) = arg.strip_prefix(b"want-ref ") {
                    args.want_refs.push(String::from_utf8(refname.to_vec())?);
                } else {
                    match arg {
                        b"done" => args.done = true,
                        b"thin-pack" => args.thin_pack = true,
                        b"no-progress" => args.no_progress = true,
                        b"include-tag" => args.include_tag = true,
                        b"ofs-delta" => args.ofs_delta = true,
                        b"wait-for-done" => args.wait_for_done = true,
                        _ => anyhow::bail!("unrecognised fetch argument"),
                    }
                }
            }
        }
    }
    Ok(args)
}

pub async fn perform_fetch(
    mut handle: gix::OdbHandle,
    args: &FetchArgs,
    mut writer: piper::Writer,
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

        text_to_write(b"acknowledgments", &mut writer).await?;
        if acks.is_empty() {
            text_to_write(b"NAK", &mut writer).await?;
        } else {
            for ack in &acks {
                text_to_write(format!("ACK {}", ack).as_bytes(), &mut writer).await?;
            }
        }
        if !ready {
            flush_to_write(&mut writer).await?;
            return Ok(());
        }
        text_to_write(b"ready", &mut writer).await?;
        delim_to_write(&mut writer).await?;
        // Fall through to build packfile below.
    }

    {
        let should_interrupt = AtomicBool::new(false);

        let progress = gix_features::progress::Discard {};

        handle.prevent_pack_unload();
        handle.ignore_replacements = true;

        let pack_objects =
            crate::pack::objects_for_fetch(handle.clone().into_inner(), &args.want, &args.have)?;

        let (counts, _) = gix_pack::data::output::count::objects(
            handle.clone().into_inner(),
            Box::new(pack_objects.objects.into_iter().map(|id| Ok(id))),
            &progress,
            &should_interrupt,
            gix_pack::data::output::count::objects::Options {
                thread_limit: None,
                chunk_size: 16,
                input_object_expansion:
                    gix_pack::data::output::count::objects::ObjectExpansion::AsIs,
            },
        )?;
        let counts: Vec<_> = counts.into_iter().collect();

        let num_objects = counts.len();

        let mut in_order_entries = InOrderIter::from(gix_pack::data::output::entry::iter_from_counts(
            counts,
            handle.into_inner(),
            Box::new(progress),
            gix_pack::data::output::entry::iter_from_counts::Options {
                thread_limit: None, // Use all cores
                mode: gix_pack::data::output::entry::iter_from_counts::Mode::PackCopyAndBaseObjects,
                allow_thin_pack: args.thin_pack,
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

        text_to_write(b"packfile", &mut writer).await?;

        for chunk in buf.chunks(65516 - 16) {
            band_to_write(Channel::Data, chunk, &mut writer).await?;
        }

        flush_to_write(&mut writer).await?;
    }

    // TODO: Support shallow clones
    // TODO: Support packfile-uris (probably only useful for other backends)

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix_packetline::async_io::StreamingPeekableIter;
    use std::{fs, path::Path, process::Command};
    use tempfile::tempdir;

    fn pkt_line(data: &[u8]) -> Vec<u8> {
        let len = data.len() + 4;
        let mut out = format!("{:04x}", len).into_bytes();
        out.extend_from_slice(data);
        out
    }

    const PKT_DELIMITER: &[u8] = b"0001";
    const PKT_FLUSH: &[u8] = b"0000";

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

    #[test]
    fn test_want_ref_parsed() {
        futures_lite::future::block_on(async {
            let mut input = Vec::new();
            input.extend(pkt_line(b"agent=test/1.0\n"));
            input.extend(PKT_DELIMITER);
            input.extend(pkt_line(b"want-ref refs/heads/main\n"));
            input.extend(PKT_FLUSH);

            let mut parser = StreamingPeekableIter::new(input.as_slice(), &[], false);
            let args = read_fetch_args(&mut parser).await.unwrap();

            assert_eq!(args.want_refs, vec!["refs/heads/main".to_string()]);
            assert!(args.want.is_empty());
            assert!(!args.wait_for_done);
        });
    }

    #[test]
    fn test_wait_for_done_parsed() {
        futures_lite::future::block_on(async {
            let mut input = Vec::new();
            input.extend(pkt_line(b"agent=test/1.0\n"));
            input.extend(PKT_DELIMITER);
            input.extend(pkt_line(b"wait-for-done\n"));
            input.extend(PKT_FLUSH);

            let mut parser = StreamingPeekableIter::new(input.as_slice(), &[], false);
            let args = read_fetch_args(&mut parser).await.unwrap();

            assert!(args.wait_for_done);
        });
    }

    #[test]
    fn test_want_ref_and_wait_for_done_together() {
        futures_lite::future::block_on(async {
            let mut input = Vec::new();
            input.extend(pkt_line(b"agent=test/1.0\n"));
            input.extend(PKT_DELIMITER);
            input.extend(pkt_line(b"want-ref refs/heads/main\n"));
            input.extend(pkt_line(b"want-ref refs/heads/dev\n"));
            input.extend(pkt_line(b"wait-for-done\n"));
            input.extend(PKT_FLUSH);

            let mut parser = StreamingPeekableIter::new(input.as_slice(), &[], false);
            let args = read_fetch_args(&mut parser).await.unwrap();

            assert_eq!(
                args.want_refs,
                vec!["refs/heads/main".to_string(), "refs/heads/dev".to_string()]
            );
            assert!(args.wait_for_done);
        });
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
        };

        let (reader, writer) = piper::pipe(65536);
        futures_lite::future::block_on(async {
            perform_fetch(handle, &args, writer).await.unwrap();
        });

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
        };

        let (reader, writer) = piper::pipe(65536);
        futures_lite::future::block_on(async {
            perform_fetch(handle, &args, writer).await.unwrap();
        });

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
}
