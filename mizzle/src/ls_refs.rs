use futures_lite::AsyncWrite;
use gix_packetline::async_io::encode::{flush_to_write, text_to_write};
use gix_ref::file::ReferenceExt;

pub use mizzle_proto::ls_refs::{read_lsrefs_args, ListRefsArgs};

fn get_head_info(
    repo: &gix::ThreadSafeRepository,
    args: &ListRefsArgs,
) -> anyhow::Result<Option<String>> {
    Ok(match repo.to_thread_local().head_ref()? {
        Some(mut head_ref) => {
            head_ref.peel_to_id()?;
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
    writer: &mut (impl AsyncWrite + Unpin),
) -> anyhow::Result<()> {
    match get_head_info(repo, args)? {
        Some(packetline) => {
            text_to_write(packetline.as_bytes(), &mut *writer).await?;
        }
        None => {
            if args.unborn {
                text_to_write(b"unborn HEAD", &mut *writer).await?;
            }
        }
    }

    for reference in repo.refs.iter()?.all()? {
        let r = reference?;

        // Filter requested refs and avoid peeling them
        if !args.prefixes.is_empty() {
            if !args
                .prefixes
                .iter()
                .any(|prefix| r.name.as_bstr().starts_with(prefix))
            {
                continue;
            }
        }

        let mut to_peel = r.clone();

        match r.target {
            // This reference is to an annotated tag or a commit.
            gix_ref::Target::Object(oid) => {
                if args.peel {
                    // We check if this is an annotated tag by peeling it
                    let peeled = to_peel.peel_to_id(&repo.refs, &repo.objects.to_handle())?;
                    // The peeled result changes so this must have been an annotated tag
                    if peeled != oid {
                        // Output the anotated tag's oid & name but also the underlying commit's oid
                        text_to_write(
                            format!("{} {} peeled:{}", oid, r.name, peeled).as_bytes(),
                            &mut *writer,
                        )
                        .await?;
                        continue;
                    }
                }
                // Either this isn't an annotated tag, or we weren't asked to peel
                // So we just return the oid directly
                text_to_write(format!("{} {}", oid, r.name).as_bytes(), &mut *writer).await?;
            }
            // This is a symbolic reference (such as HEAD)
            gix_ref::Target::Symbolic(symref_target) => {
                // We always need to find the underlying commit oid of the symbolic reference so we do that first.
                let peeled = to_peel.peel_to_id(&repo.refs, &repo.objects.to_handle())?;
                // We only output the name of the intermediate reference if requested
                if args.symrefs {
                    text_to_write(
                        format!("{} {} symref-target:{}", peeled, r.name, symref_target).as_bytes(),
                        &mut *writer,
                    )
                    .await?;
                } else {
                    text_to_write(format!("{} {}", peeled, r.name).as_bytes(), &mut *writer)
                        .await?;
                }
            }
        }
    }

    flush_to_write(&mut *writer).await?;
    Ok(())
}
