use anyhow::Context;
use gix_packetline::PacketLineRef;

pub async fn skip_till_delimiter<T>(
    parser: &mut gix_packetline::async_io::StreamingPeekableIter<T>,
) -> anyhow::Result<()>
where
    T: futures_lite::AsyncRead + Unpin,
{
    loop {
        let line = parser.read_line().await.context("expected delimiter")???;
        match line {
            PacketLineRef::ResponseEnd | PacketLineRef::Flush => {
                anyhow::bail!("found end of response expected delimiter")
            }
            PacketLineRef::Delimiter => return Ok(()),
            PacketLineRef::Data(_) => (),
        }
    }
}

pub fn u16_to_hex(value: u16) -> [u8; 4] {
    let mut buf = [0u8; 4];
    faster_hex::hex_encode(&value.to_be_bytes(), &mut buf)
        .expect("two bytes to 4 hex chars never fails");
    buf
}
