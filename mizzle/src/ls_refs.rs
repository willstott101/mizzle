use futures_lite::AsyncWrite;
use gix_packetline::async_io::encode::{flush_to_write, text_to_write};

pub use mizzle_proto::ls_refs::{read_lsrefs_args, ListRefsArgs};

use crate::backend::RefsSnapshot;

pub async fn perform_listrefs(
    snapshot: &RefsSnapshot,
    args: &ListRefsArgs,
    writer: &mut (impl AsyncWrite + Unpin),
) -> anyhow::Result<()> {
    // HEAD
    match &snapshot.head {
        Some(head) => {
            if args.symrefs {
                if let Some(target) = &head.symref_target {
                    text_to_write(
                        format!("{} HEAD symref-target:{}", head.oid, target).as_bytes(),
                        &mut *writer,
                    )
                    .await?;
                } else {
                    text_to_write(format!("{} HEAD", head.oid).as_bytes(), &mut *writer).await?;
                }
            } else {
                text_to_write(format!("{} HEAD", head.oid).as_bytes(), &mut *writer).await?;
            }
        }
        None => {
            if args.unborn {
                text_to_write(b"unborn HEAD", &mut *writer).await?;
            }
        }
    }

    for r in &snapshot.refs {
        // Filter by requested prefixes
        if !args.prefixes.is_empty()
            && !args
                .prefixes
                .iter()
                .any(|prefix| r.name.as_bytes().starts_with(prefix))
        {
            continue;
        }

        match (&r.symref_target, &r.peeled) {
            // Symbolic ref
            (Some(target), _) => {
                if args.symrefs {
                    text_to_write(
                        format!("{} {} symref-target:{}", r.oid, r.name, target).as_bytes(),
                        &mut *writer,
                    )
                    .await?;
                } else {
                    text_to_write(format!("{} {}", r.oid, r.name).as_bytes(), &mut *writer).await?;
                }
            }
            // Annotated tag (peeled differs from oid)
            (None, Some(peeled)) if args.peel => {
                text_to_write(
                    format!("{} {} peeled:{}", r.oid, r.name, peeled).as_bytes(),
                    &mut *writer,
                )
                .await?;
            }
            // Regular ref or tag without peel requested
            _ => {
                text_to_write(format!("{} {}", r.oid, r.name).as_bytes(), &mut *writer).await?;
            }
        }
    }

    flush_to_write(&mut *writer).await?;
    Ok(())
}
