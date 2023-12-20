
use anyhow::Context;
use gix_packetline::PacketLineRef;

pub async fn skip_till_delimiter<T>(parser: &mut gix_packetline::StreamingPeekableIter<T>) -> anyhow::Result<()>
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
