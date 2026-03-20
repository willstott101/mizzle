use crate::limits::{check_limit, ProtocolLimits};
use crate::utils::skip_till_delimiter;
use anyhow::Context;
use gix_hash::ObjectId;
use gix_packetline::PacketLineRef;

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
    pub have: Vec<ObjectId>,
    /// Indicates to the server that negotiation should terminate (or
    /// not even begin if performing a clone) and that the server should
    /// use the information supplied in the request to construct the
    /// packfile.
    pub done: bool,
    /// Request that a thin pack be sent, which is a pack with deltas
    /// which reference base objects not contained within the pack (but
    /// are known to exist at the receiving end). This can reduce the
    /// network traffic significantly, but it requires the receiving end
    /// to know how to "thicken" these packs by adding the missing bases
    /// to the pack.
    pub thin_pack: bool,
    /// Request that progress information that would normally be sent on
    /// side-band channel 2, during the packfile transfer, should not be
    /// sent.  However, the side-band channel 3 is still used for error
    /// responses.
    pub no_progress: bool,
    /// Request that annotated tags should be sent if the objects they
    /// point to are being sent.
    pub include_tag: bool,
    /// Indicate that the client understands PACKv2 with delta referring
    /// to its base by position in pack rather than by an oid.  That is,
    /// they can read OBJ_OFS_DELTA (aka type 6) in a packfile.
    pub ofs_delta: bool,
    /// Client will explicitly send `done`; server must not declare
    /// `ready` on its own (wait-for-done).
    pub wait_for_done: bool,
    /// Requests that the shallow clone/fetch should be cut at a specific
    /// depth, relative to the requested want tips.
    pub deepen: Option<u32>,
    /// Partial clone filter specification. When set, the server omits
    /// objects matching the filter from the packfile.
    pub filter: Option<String>,
}

pub async fn read_fetch_args<T>(
    parser: &mut gix_packetline::async_io::StreamingPeekableIter<T>,
    limits: &ProtocolLimits,
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
        deepen: None,
        filter: None,
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
                    check_limit(args.want.len(), limits.max_wants, "want lines")?;
                } else if let Some(oid) = arg.strip_prefix(b"have ") {
                    args.have.push(ObjectId::from_hex(oid)?);
                    check_limit(args.have.len(), limits.max_haves, "have lines")?;
                } else if let Some(refname) = arg.strip_prefix(b"want-ref ") {
                    args.want_refs.push(String::from_utf8(refname.to_vec())?);
                    check_limit(args.want_refs.len(), limits.max_want_refs, "want-ref lines")?;
                } else if let Some(depth) = arg.strip_prefix(b"deepen ") {
                    let n = std::str::from_utf8(depth)?
                        .parse::<u32>()
                        .map_err(|e| anyhow::anyhow!("invalid deepen value: {e}"))?;
                    if n == 0 {
                        anyhow::bail!("deepen 0 is not valid");
                    }
                    args.deepen = Some(n);
                } else if let Some(spec) = arg.strip_prefix(b"filter ") {
                    args.filter = Some(String::from_utf8(spec.to_vec())?);
                } else if arg.starts_with(b"deepen-since ")
                    || arg.starts_with(b"deepen-not ")
                    || arg.starts_with(b"deepen-relative")
                {
                    anyhow::bail!(
                        "unsupported fetch argument: {}",
                        String::from_utf8_lossy(arg)
                    );
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

/// Parse a protocol v1 upload-pack POST body.
///
/// Format (stateless HTTP):
/// ```text
/// PKT-LINE(want <oid> [NUL <capabilities>]\n)
/// PKT-LINE(want <oid>\n)*
/// PKT-LINE(have <oid>\n)*
/// flush (0000)
/// PKT-LINE(done\n)
/// ```
pub async fn read_fetch_args_v1<T>(body: T, limits: &ProtocolLimits) -> anyhow::Result<FetchArgs>
where
    T: futures_lite::AsyncRead + Unpin,
{
    let mut parser = gix_packetline::async_io::StreamingPeekableIter::new(body, &[], false);
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
        deepen: None,
        filter: None,
    };

    loop {
        let line = parser
            .read_line()
            .await
            .context("unexpected eof in v1 request body")???;
        match line {
            PacketLineRef::Flush => break,
            PacketLineRef::Data(d) => {
                let data = d.strip_suffix(b"\n").unwrap_or(d);
                if let Some(rest) = data.strip_prefix(b"want ") {
                    // The first want line carries space-separated capabilities
                    // after the OID; subsequent want lines carry just the OID.
                    let (oid_bytes, caps_opt) = match rest.iter().position(|&b| b == b' ') {
                        Some(pos) => (&rest[..pos], Some(&rest[pos + 1..])),
                        None => (rest, None),
                    };
                    args.want.push(ObjectId::from_hex(oid_bytes)?);
                    check_limit(args.want.len(), limits.max_wants, "want lines")?;
                    if let Some(caps) = caps_opt {
                        for cap in caps.split(|&b| b == b' ') {
                            match cap {
                                b"ofs-delta" => args.ofs_delta = true,
                                b"thin-pack" => args.thin_pack = true,
                                b"no-progress" => args.no_progress = true,
                                b"include-tag" => args.include_tag = true,
                                _ => {}
                            }
                        }
                    }
                } else if let Some(rest) = data.strip_prefix(b"have ") {
                    args.have.push(ObjectId::from_hex(rest)?);
                    check_limit(args.have.len(), limits.max_haves, "have lines")?;
                } else if let Some(depth) = data.strip_prefix(b"deepen ") {
                    let n = std::str::from_utf8(depth)?
                        .parse::<u32>()
                        .map_err(|e| anyhow::anyhow!("invalid deepen value: {e}"))?;
                    anyhow::ensure!(n != 0, "deepen 0 is not valid");
                    args.deepen = Some(n);
                } else if let Some(spec) = data.strip_prefix(b"filter ") {
                    args.filter = Some(String::from_utf8(spec.to_vec())?);
                }
                // Unknown lines (shallow, agent, etc.) are silently ignored.
            }
            _ => {}
        }
    }

    // After flush, check for `done`.
    if let Some(Ok(Ok(PacketLineRef::Data(d)))) = parser.read_line().await {
        if d.strip_suffix(b"\n").unwrap_or(d) == b"done" {
            args.done = true;
        }
    }

    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix_packetline::async_io::StreamingPeekableIter;

    fn pkt_line(data: &[u8]) -> Vec<u8> {
        let len = data.len() + 4;
        let mut out = format!("{:04x}", len).into_bytes();
        out.extend_from_slice(data);
        out
    }

    const PKT_DELIMITER: &[u8] = b"0001";
    const PKT_FLUSH: &[u8] = b"0000";

    #[test]
    fn test_want_ref_parsed() {
        futures_lite::future::block_on(async {
            let mut input = Vec::new();
            input.extend(pkt_line(b"agent=test/1.0\n"));
            input.extend(PKT_DELIMITER);
            input.extend(pkt_line(b"want-ref refs/heads/main\n"));
            input.extend(PKT_FLUSH);

            let mut parser = StreamingPeekableIter::new(input.as_slice(), &[], false);
            let limits = crate::limits::ProtocolLimits::default();
            let args = read_fetch_args(&mut parser, &limits).await.unwrap();

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
            let limits = crate::limits::ProtocolLimits::default();
            let args = read_fetch_args(&mut parser, &limits).await.unwrap();

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
            let limits = crate::limits::ProtocolLimits::default();
            let args = read_fetch_args(&mut parser, &limits).await.unwrap();

            assert_eq!(
                args.want_refs,
                vec!["refs/heads/main".to_string(), "refs/heads/dev".to_string()]
            );
            assert!(args.wait_for_done);
        });
    }

    #[test]
    fn test_deepen_parsed() {
        futures_lite::future::block_on(async {
            let mut input = Vec::new();
            input.extend(pkt_line(b"agent=test/1.0\n"));
            input.extend(PKT_DELIMITER);
            input.extend(pkt_line(b"deepen 3\n"));
            input.extend(pkt_line(b"done\n"));
            input.extend(PKT_FLUSH);

            let mut parser = StreamingPeekableIter::new(input.as_slice(), &[], false);
            let limits = crate::limits::ProtocolLimits::default();
            let args = read_fetch_args(&mut parser, &limits).await.unwrap();

            assert_eq!(args.deepen, Some(3));
            assert!(args.done);
        });
    }

    #[test]
    fn test_filter_parsed() {
        futures_lite::future::block_on(async {
            let mut input = Vec::new();
            input.extend(pkt_line(b"agent=test/1.0\n"));
            input.extend(PKT_DELIMITER);
            input.extend(pkt_line(b"filter blob:none\n"));
            input.extend(pkt_line(b"done\n"));
            input.extend(PKT_FLUSH);

            let mut parser = StreamingPeekableIter::new(input.as_slice(), &[], false);
            let limits = crate::limits::ProtocolLimits::default();
            let args = read_fetch_args(&mut parser, &limits).await.unwrap();

            assert_eq!(args.filter, Some("blob:none".to_string()));
        });
    }

    #[test]
    fn rejects_too_many_wants() {
        futures_lite::future::block_on(async {
            let limits = crate::limits::ProtocolLimits {
                max_wants: 2,
                ..Default::default()
            };

            let mut input = Vec::new();
            input.extend(pkt_line(b"agent=test/1.0\n"));
            input.extend(PKT_DELIMITER);
            for i in 0..3u8 {
                let oid = format!("{:0>40x}", i + 1);
                input.extend(pkt_line(format!("want {oid}\n").as_bytes()));
            }
            input.extend(PKT_FLUSH);

            let mut parser = StreamingPeekableIter::new(input.as_slice(), &[], false);
            let err = read_fetch_args(&mut parser, &limits).await.unwrap_err();
            assert!(
                err.to_string().contains("too many want lines"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn rejects_too_many_haves() {
        futures_lite::future::block_on(async {
            let limits = crate::limits::ProtocolLimits {
                max_haves: 1,
                ..Default::default()
            };

            let mut input = Vec::new();
            input.extend(pkt_line(b"agent=test/1.0\n"));
            input.extend(PKT_DELIMITER);
            for i in 0..2u8 {
                let oid = format!("{:0>40x}", i + 1);
                input.extend(pkt_line(format!("have {oid}\n").as_bytes()));
            }
            input.extend(PKT_FLUSH);

            let mut parser = StreamingPeekableIter::new(input.as_slice(), &[], false);
            let err = read_fetch_args(&mut parser, &limits).await.unwrap_err();
            assert!(
                err.to_string().contains("too many have lines"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn rejects_too_many_want_refs() {
        futures_lite::future::block_on(async {
            let limits = crate::limits::ProtocolLimits {
                max_want_refs: 1,
                ..Default::default()
            };

            let mut input = Vec::new();
            input.extend(pkt_line(b"agent=test/1.0\n"));
            input.extend(PKT_DELIMITER);
            input.extend(pkt_line(b"want-ref refs/heads/a\n"));
            input.extend(pkt_line(b"want-ref refs/heads/b\n"));
            input.extend(PKT_FLUSH);

            let mut parser = StreamingPeekableIter::new(input.as_slice(), &[], false);
            let err = read_fetch_args(&mut parser, &limits).await.unwrap_err();
            assert!(
                err.to_string().contains("too many want-ref lines"),
                "unexpected error: {err}"
            );
        });
    }
}
