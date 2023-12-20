use crate::utils::skip_till_delimiter;
use anyhow::Context;
use gix_packetline::{PacketLineRef, encode::{text_to_write, flush_to_write}};


#[derive(Debug)]
pub struct FetchArgs {
    /// Indicates to the server an object which the client wants to
    /// retrieve.  Wants can be anything and are not limited to
    /// advertised objects.
    want: Vec<Box<[u8]>>,
    /// Indicates to the server an object which the client has locally.
    /// This allows the server to make a packfile which only contains
    /// the objects that the client needs. Multiple 'have' lines can be
    /// supplied.
    have: Vec<Box<[u8]>>,
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
}

pub async fn read_fetch_args<T>(parser: &mut gix_packetline::StreamingPeekableIter<T>) -> anyhow::Result<FetchArgs>
where
    T: futures_lite::AsyncRead + Unpin,
{
    // "command=ls-refs"
    // "agent=git/2.40.1"
    // None (delimiter)
    skip_till_delimiter(parser).await?; // TODO: Is this info we're skipping ever useful?
    // "peel"
    // "symrefs"
    // "ref-prefix HEAD"
    // "ref-prefix refs/heads/"
    // "ref-prefix refs/tags/"
    let mut args = FetchArgs {
        want: Vec::new(),
        have: Vec::new(),
        done: false,
        thin_pack: false,
        no_progress: false,
        include_tag: false,
        ofs_delta: false,
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
                match arg.strip_prefix(b"want ") {
                    Some(oid) => args.want.push(oid.into()),
                    None => {
                        match arg.strip_prefix(b"have ") {
                            Some(oid) => args.have.push(oid.into()),
                            None => {
                                match arg {
                                    b"done" => args.done = true,
                                    b"thin-pack" => args.thin_pack = true,
                                    b"no-progress" => args.no_progress = true,
                                    b"include-tag" => args.include_tag = true,
                                    b"ofs-delta" => args.ofs_delta = true,
                                    _ => anyhow::bail!("unrecognised fetch argument"),
                                };
                            }
                        };
                    }
                };
            }
        }
    }
    Ok(args)
}