use std::time::Duration;

use anyhow::anyhow;
use tokio::time::Instant;

use crate::{
    app::SyncDnsClient,
    proxy::AnyOutboundHandler,
    session::{Session, SocksAddr},
};

pub async fn tcp(
    dns_client: SyncDnsClient,
    handler: AnyOutboundHandler,
) -> anyhow::Result<Duration> {
    let sess = Session {
        destination: SocksAddr::Domain("healthcheck.leaf".to_string(), 80),
        new_conn_once: true,
        ..Default::default()
    };
    let start = Instant::now();
    let stream = crate::proxy::connect_stream_outbound(&sess, dns_client, &handler).await?;
    let mut stream = handler.stream()?.handle(&sess, None, stream).await?;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream.write_all(b"PING").await?;
    // Read exactly 4 bytes. A single read_buf call is not sufficient because TCP
    // may deliver the response in fragments; loop until the full PONG is received
    // or the connection closes early.
    let mut buf = [0u8; 4];
    let mut filled = 0usize;
    while filled < 4 {
        let n = stream.read(&mut buf[filled..]).await?;
        if n == 0 {
            return Err(anyhow!(
                "EOF during TCP health check for [{}] after {} byte(s)",
                handler.tag(),
                filled
            ));
        }
        filled += n;
    }
    if &buf == b"PONG" {
        Ok(Instant::now().duration_since(start))
    } else {
        Err(anyhow!(
            "Unexpected TCP health check response from [{}]: {}",
            handler.tag(),
            String::from_utf8_lossy(&buf)
        ))
    }
}

pub async fn udp(
    dns_client: SyncDnsClient,
    handler: AnyOutboundHandler,
) -> anyhow::Result<Duration> {
    let addr = SocksAddr::Domain("healthcheck.leaf".to_string(), 80);
    let sess = Session {
        destination: addr.clone(),
        new_conn_once: true,
        ..Default::default()
    };
    let start = Instant::now();
    let dgram = crate::proxy::connect_datagram_outbound(&sess, dns_client, &handler).await?;
    let dgram = handler.datagram()?.handle(&sess, dgram).await?;
    let (mut recv, mut send) = dgram.split();
    send.send_to(b"PING", &addr).await?;
    let mut buf = [0u8; 2 * 1024];
    let (n, _src_addr) = recv.recv_from(&mut buf).await?;
    if &buf[..n] == b"PONG" {
        Ok(Instant::now().duration_since(start))
    } else {
        Err(anyhow!(
            "Unexpected UDP health check response from [{}]",
            handler.tag()
        ))
    }
}
