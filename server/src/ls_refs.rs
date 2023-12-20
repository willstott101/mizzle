use crate::utils::skip_till_delimiter;
use anyhow::Context;
use gix_packetline::{
    encode::{flush_to_write, text_to_write},
    PacketLineRef,
};
use gix_ref::file::ReferenceExt;

#[derive(Debug)]
pub struct ListRefsArgs {
    /// In addition to the object pointed by it, show the underlying ref
    /// pointed by it when showing a symbolic ref.
    symrefs: bool,
    /// Show peeled tags.
    peel: bool,
    /// When specified, only references having a prefix matching one of
    /// the provided prefixes are displayed. Multiple instances may be
    /// given, in which case references matching any prefix will be
    /// shown. Note that this is purely for optimization; a server MAY
    /// show refs not matching the prefix if it chooses, and clients
    /// should filter the result themselves.
    prefixes: Vec<Box<[u8]>>,
    /// The server will send information about HEAD even if it is a symref
    /// pointing to an unborn branch in the form "unborn HEAD
    /// symref-target:<target>".
    unborn: bool,
}

pub async fn read_lsrefs_args<T>(
    parser: &mut gix_packetline::StreamingPeekableIter<T>,
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

fn get_head_info(
    repo: &gix::ThreadSafeRepository,
    args: &ListRefsArgs,
) -> anyhow::Result<Option<String>> {
    Ok(match repo.to_thread_local().head_ref()? {
        Some(mut head_ref) => {
            head_ref.peel_to_id_in_place()?;
            match head_ref.inner.peeled {
                None => None,
                Some(oid) => {
                    if args.symrefs {
                        Some(format!(
                            "{} HEAD symref-target:{}",
                            oid,
                            head_ref.name().as_bstr()
                        ))
                    } else {
                        Some(format!("{} HEAD", oid))
                    }
                }
            }
        }
        None => None,
    })
}

pub async fn perform_listrefs(
    repo: &gix::ThreadSafeRepository,
    args: &ListRefsArgs,
    mut writer: piper::Writer,
) -> anyhow::Result<()> {
    match get_head_info(repo, args)? {
        Some(packetline) => {
            text_to_write(packetline.as_bytes(), &mut writer)
                .await
                .expect("to write to output");
        }
        None => {
            if args.unborn {
                text_to_write(b"unborn HEAD", &mut writer)
                    .await
                    .expect("to write to output");
            }
        }
    }

    for reference in repo.refs.iter()?.all()? {
        // TODO: packet-line style error handling
        let r = reference?;
        let mut to_peel = r.clone();

        match r.target {
            // This reference is to an annotated tag or a commit.
            gix_ref::Target::Peeled(oid) => {
                if args.peel {
                    // We check if this is an annotated tag by peeling it
                    let peeled =
                        to_peel.peel_to_id_in_place(&repo.refs, &repo.objects.to_handle())?;
                    // The peeled result changes so this must have been an annotated tag
                    if peeled != oid {
                        // Output the anotated tag's oid & name but also the underlying commit's oid
                        text_to_write(
                            format!("{} {} peeled:{}", oid, r.name, peeled).as_bytes(),
                            &mut writer,
                        )
                        .await
                        .expect("to write to output");
                        continue;
                    }
                }
                // Either this isn't an annotated tag, or we weren't asked to peel
                // So we just return the oid directly
                text_to_write(format!("{} {}", oid, r.name).as_bytes(), &mut writer)
                    .await
                    .expect("to write to output");
            }
            // This is a symbolic reference (such as HEAD)
            gix_ref::Target::Symbolic(symref_target) => {
                // We always need to find the underlying commit oid of the symbolic reference so we do that first.
                let peeled = to_peel.peel_to_id_in_place(&repo.refs, &repo.objects.to_handle())?;
                // We only output the name of the intermediate reference if requested
                if args.symrefs {
                    text_to_write(
                        format!("{} {} symref-target:{}", peeled, r.name, symref_target).as_bytes(),
                        &mut writer,
                    )
                    .await
                    .expect("to write to output");
                } else {
                    text_to_write(format!("{} {}", peeled, r.name).as_bytes(), &mut writer)
                        .await
                        .expect("to write to output");
                }
            }
        }
    }

    flush_to_write(&mut writer)
        .await
        .expect("to write to output");
    Ok(())
}
