use anyhow::Result;
use futures_lite::{AsyncRead, AsyncReadExt};
use gix_hash::ObjectId;

use crate::limits::{check_limit, ProtocolLimits};
use crate::types::PushKind;

/// Classifies a ref update without touching the object database.  Used for
/// the preliminary auth check (before the pack is written) so cheap denials
/// never hit the disk.
///
/// Create and Delete are definitive.  FastForward is optimistic — once the
/// pack is in the odb, [`compute_push_kind`] may upgrade it to ForcePush.
pub fn preliminary_push_kind(update: &RefUpdate) -> PushKind {
    if update.old_oid.is_null() {
        PushKind::Create
    } else if update.new_oid.is_null() {
        PushKind::Delete
    } else {
        PushKind::FastForward
    }
}

#[derive(Debug)]
pub struct RefUpdate {
    pub old_oid: ObjectId,
    pub new_oid: ObjectId,
    pub refname: String,
}

/// Parses the pkt-line ref-update commands from a receive-pack request body,
/// reading incrementally without buffering the trailing packfile.  Returns the
/// parsed ref updates and the reader positioned at the start of the pack data.
pub async fn read_receive_request<T: AsyncRead + Unpin>(
    mut body: T,
    limits: &ProtocolLimits,
) -> Result<(Vec<RefUpdate>, T)> {
    let mut ref_updates = Vec::new();
    let mut first_line = true;
    let mut len_buf = [0u8; 4];

    loop {
        body.read_exact(&mut len_buf).await?;
        let len_str = std::str::from_utf8(&len_buf)?;
        let len = usize::from_str_radix(len_str, 16)?;

        if len == 0 {
            // flush packet — end of ref-update section
            break;
        }
        if len < 4 {
            anyhow::bail!("invalid pkt-line length: {len}");
        }

        let payload_len = len - 4;
        let mut data = vec![0u8; payload_len];
        body.read_exact(&mut data).await?;

        let data = data.strip_suffix(b"\n").unwrap_or(&data);

        // Strip capabilities (everything after NUL) from the first line.
        let line = if first_line {
            first_line = false;
            data.splitn(2, |&b| b == b'\0').next().unwrap_or(data)
        } else {
            data
        };

        // Each command: "<old-oid> <new-oid> <refname>"
        let mut parts = line.splitn(3, |&b| b == b' ');
        if let (Some(old_hex), Some(new_hex), Some(name)) =
            (parts.next(), parts.next(), parts.next())
        {
            let old_oid = ObjectId::from_hex(old_hex)?;
            let new_oid = ObjectId::from_hex(new_hex)?;
            let refname = String::from_utf8(name.to_vec())?;
            ref_updates.push(RefUpdate {
                old_oid,
                new_oid,
                refname,
            });
            check_limit(ref_updates.len(), limits.max_ref_updates, "ref updates")?;
        }
    }

    Ok((ref_updates, body))
}

/// Returns the number of objects in the pack, parsed from the pack header.
/// Returns `None` if the data is not a valid pack header.
pub fn pack_object_count(data: &[u8]) -> Option<u32> {
    if data.len() >= 12 && data[0..4] == *b"PACK" {
        Some(u32::from_be_bytes(data[8..12].try_into().unwrap()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt_line(data: &[u8]) -> Vec<u8> {
        let len = data.len() + 4;
        let mut out = format!("{len:04x}").into_bytes();
        out.extend_from_slice(data);
        out
    }

    fn ref_update_line(refname: &str) -> Vec<u8> {
        let null = "0000000000000000000000000000000000000000";
        let oid = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        pkt_line(format!("{null} {oid} {refname}\n").as_bytes())
    }

    #[test]
    fn rejects_too_many_ref_updates() {
        futures_lite::future::block_on(async {
            let limits = ProtocolLimits {
                max_ref_updates: 2,
                ..Default::default()
            };

            let mut input = Vec::new();
            for i in 0..3 {
                input.extend(ref_update_line(&format!("refs/heads/branch-{i}")));
            }
            input.extend(b"0000");

            let result = read_receive_request(input.as_slice(), &limits).await;
            let err = result.unwrap_err();
            assert!(
                err.to_string().contains("too many ref updates"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn accepts_ref_updates_at_limit() {
        futures_lite::future::block_on(async {
            let limits = ProtocolLimits {
                max_ref_updates: 2,
                ..Default::default()
            };

            let mut input = Vec::new();
            for i in 0..2 {
                input.extend(ref_update_line(&format!("refs/heads/branch-{i}")));
            }
            input.extend(b"0000");

            let (updates, _) = read_receive_request(input.as_slice(), &limits)
                .await
                .unwrap();
            assert_eq!(updates.len(), 2);
        });
    }
}
