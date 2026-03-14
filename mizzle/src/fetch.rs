use crate::utils::skip_till_delimiter;
use anyhow::Context;
use core::sync::atomic::AtomicBool;
use gix::{ObjectId, objs::Exists, parallel::InOrderIter};
use gix_packetline::{
    async_io::encode::{band_to_write, delim_to_write, flush_to_write, text_to_write},
    Channel, PacketLineRef,
};

#[derive(Debug)]
pub struct FetchArgs {
    /// Indicates to the server an object which the client wants to
    /// retrieve.  Wants can be anything and are not limited to
    /// advertised objects.
    want: Vec<ObjectId>,
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
                    Some(oid) => args.want.push(ObjectId::from_hex(oid)?),
                    None => {
                        match arg.strip_prefix(b"have ") {
                            Some(oid) => args.have.push(ObjectId::from_hex(oid)?),
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

pub async fn perform_fetch(
    mut handle: gix::OdbHandle,
    args: &FetchArgs,
    mut writer: piper::Writer,
) -> anyhow::Result<()> {
    let ready = false;
    let mut acks: Vec<ObjectId> = Vec::new();

    // output = acknowledgements flush-pkt |
    //   [acknowledgments delim-pkt] [shallow-info delim-pkt]
    //   [wanted-refs delim-pkt] [packfile-uris delim-pkt]
    //   packfile flush-pkt

    if !args.done {
        // TODO: Calculate acks and readiness
        for id in args.have.iter() {
            if handle.clone().into_inner().exists(id) {
                acks.push(id.clone());
            }
        }

        text_to_write(b"acknowledgments", &mut writer).await?;
        if !ready {
            if acks.is_empty() {
                text_to_write(b"NAK", &mut writer).await?;
            } else {
                for ack in acks {
                    text_to_write(format!("ACK {}", ack).as_bytes(), &mut writer).await?;
                }
            }
            flush_to_write(&mut writer).await?;
            return Ok(());
        }
        text_to_write(b"ready", &mut writer).await?;
        delim_to_write(&mut writer).await?;
    } else {
        // TODO: Start building packfile
        // See gitoxide-core/pack/create
        // Uses
        // gix_pack::data::output::count::objects
        // gix_pack::data::output::entry::iter_from_counts

        // TODO: Allow for interuption on disconnect
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
    // TODO: Support ref-in-want
    // TODO: Support packfile-uris (probably only useful for other backends)
    // TODO: Support wait-for-done

    Ok(())
}
