use crate::utils::skip_till_delimiter;
use anyhow::Context;
use gix_packetline::PacketLineRef;

#[derive(Debug)]
pub struct ListRefsArgs {
    /// In addition to the object pointed by it, show the underlying ref
    /// pointed by it when showing a symbolic ref.
    pub symrefs: bool,
    /// Show peeled tags.
    pub peel: bool,
    /// When specified, only references having a prefix matching one of
    /// the provided prefixes are displayed. Multiple instances may be
    /// given, in which case references matching any prefix will be
    /// shown. Note that this is purely for optimization; a server MAY
    /// show refs not matching the prefix if it chooses, and clients
    /// should filter the result themselves.
    pub prefixes: Vec<Box<[u8]>>,
    /// The server will send information about HEAD even if it is a symref
    /// pointing to an unborn branch in the form "unborn HEAD
    /// symref-target:<target>".
    pub unborn: bool,
}

pub async fn read_lsrefs_args<T>(
    parser: &mut gix_packetline::async_io::StreamingPeekableIter<T>,
) -> anyhow::Result<ListRefsArgs>
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
    let mut args = ListRefsArgs {
        symrefs: false,
        peel: false,
        unborn: false,
        prefixes: Vec::new(),
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
                match arg.strip_prefix(b"ref-prefix ") {
                    Some(prefix) => args.prefixes.push(prefix.into()),
                    None => {
                        match arg {
                            b"peel" => args.peel = true,
                            b"symrefs" => args.symrefs = true,
                            b"unborn" => args.unborn = true,
                            _ => anyhow::bail!("unrecognised lsrefs argument"),
                        };
                    }
                };
            }
        }
    }
    Ok(args)
}
