use anyhow::Result;
use futures_lite::{AsyncRead, AsyncReadExt};
use gix_hash::ObjectId;

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

/// Parses a receive-pack request body: pkt-line ref-update commands (ending
/// with a flush packet) followed by a raw packfile.
pub async fn read_receive_request<T: AsyncRead + Unpin>(
    mut body: T,
) -> Result<(Vec<RefUpdate>, Vec<u8>)> {
    // Buffer everything; the pack is raw binary after the pkt-line section.
    let mut all_bytes = Vec::new();
    body.read_to_end(&mut all_bytes).await?;

    let mut pos = 0;
    let mut ref_updates = Vec::new();
    let mut first_line = true;

    loop {
        if pos + 4 > all_bytes.len() {
            break;
        }
        let len_str = std::str::from_utf8(&all_bytes[pos..pos + 4])?;
        let len = usize::from_str_radix(len_str, 16)?;

        if len == 0 {
            // flush packet — end of ref-update section
            pos += 4;
            break;
        }
        if len < 4 || pos + len > all_bytes.len() {
            break;
        }

        let data = &all_bytes[pos + 4..pos + len];
        let data = data.strip_suffix(b"\n").unwrap_or(data);

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
        }

        pos += len;
    }

    let pack_data = all_bytes[pos..].to_vec();
    Ok((ref_updates, pack_data))
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
