use crate::utils::skip_till_delimiter;
use anyhow::Context;
use log::info;
use core::sync::atomic::AtomicBool;
use futures_lite::AsyncWriteExt;
use gix::ObjectId;
use gix_packetline::{
    Channel, PacketLineRef, async_io::encode::{band_to_write, delim_to_write, flush_to_write, text_to_write}
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
    let acks: Vec<ObjectId> = Vec::new();

    // output = acknowledgements flush-pkt |
    //   [acknowledgments delim-pkt] [shallow-info delim-pkt]
    //   [wanted-refs delim-pkt] [packfile-uris delim-pkt]
    //   packfile flush-pkt

    if !args.done {
        unimplemented!();
        // TODO: Calculate acks and readiness

        text_to_write(b"acknowledgments", &mut writer).await?;
        if !ready {
            if acks.is_empty() {
                text_to_write(b"nak", &mut writer).await?;
            } else {
                for ack in acks {
                    text_to_write(format!("ack {}", ack).as_bytes(), &mut writer).await?;
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

        let options =  gix_pack::data::output::count::objects::Options {
            thread_limit: None,
            chunk_size: 16,
            // TODO: How do we give state to this expansion mode?
            input_object_expansion: gix_pack::data::output::count::objects::ObjectExpansion::TreeAdditionsComparedToAncestor,
        };

        // TODO: Allow for interuption on disconnect
        let should_interrupt = AtomicBool::new(false);

        let progress = gix_features::progress::Discard {};

        // let repo = gix::open(".")?.into_sync();
        // let mut handle = repo.clone().objects.into_shared_arc().to_cache_arc();

        handle.prevent_pack_unload();
        handle.ignore_replacements = true;

        let count = gix_pack::data::output::count::objects(
            handle.clone().into_inner(),
            Box::new(args.want.clone().into_iter().map(|i| Ok(i))),
            &progress,
            &should_interrupt,
            options,
        )?;

        let entries = gix_pack::data::output::entry::iter_from_counts(
            count.0,
            handle.into_inner(),
            Box::new(progress),
            gix_pack::data::output::entry::iter_from_counts::Options {
                thread_limit: None, // Use all cores
                mode: gix_pack::data::output::entry::iter_from_counts::Mode::PackCopyAndBaseObjects,
                allow_thin_pack: false, // args.thin_pack (IDK if the current thin algo will work here),
                chunk_size: 16,
                version: Default::default(),
            },
        );

        text_to_write(b"packfile", &mut writer).await?;

        for i in entries {
            match i {
                Ok((_seq_id, entries)) => {
                    for entry in entries {
                        // Can't see an efficient way to give a 0x01 prefix with gitoxide's public api
                        println!("writing new entry");

                        // let data_len = entry.compressed_data.len() + 1 + 4;
                        // let buf = crate::utils::u16_to_hex(data_len as u16);

                        // writer.write_all(&buf).await?;
                        // writer.write_all(b"\x01").await?;
                        // writer.write_all(&entry.compressed_data).await?;
                        band_to_write(Channel::Data, &entry.compressed_data, &mut writer).await?;
                        // prefixed_data_to_write(b"\x01", &entry.compressed_data, &mut writer).await?;
                        // data_to_write(&entry.compressed_data, &mut writer).await?;
                    }
                }
                // TODO: Handle errors
                Err(_) => todo!(),
            }
        }

        flush_to_write(&mut writer).await?;

        // info!("COUNTED: {:#?}", count);
    }

    // TODO: Support shallow clones
    // TODO: Support ref-in-want
    // TODO: Support packfile-uris (probably only useful for other backends)
    // TODO: Support wait-for-done

    Ok(())
}
