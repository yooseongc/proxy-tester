use std::{net::SocketAddr, sync::atomic::Ordering, time::Duration};

use anyhow::{Context, bail};
use proxy_tester_domain::{Protocol, Scenario};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpSocket, TcpStream},
};

use crate::{telemetry::Counters, tls};

pub(crate) trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}

pub(crate) async fn connect_tcp(
    scenario: &Scenario,
    connection_index: u64,
    counters: &Counters,
) -> anyhow::Result<TcpStream> {
    let target_addr = scenario.target_addr();
    let connect_addr = scenario.proxy_addr().unwrap_or(&target_addr);
    let connect_started = std::time::Instant::now();
    let connect_future = async {
        let remote = tokio::net::lookup_host(connect_addr)
            .await?
            .next()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "target address did not resolve",
                )
            })?;
        let socket = if remote.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        if let Some(ip) = scenario
            .runtime_source_ips
            .get(connection_index as usize % scenario.runtime_source_ips.len().max(1))
        {
            socket.bind(SocketAddr::new(*ip, 0))?;
        }
        socket.connect(remote).await
    };
    let stream = tokio::time::timeout(
        Duration::from_millis(scenario.timeouts.connect_ms),
        connect_future,
    )
    .await?
    .context("TCP connect failed")?;
    counters
        .tcp_connect_latencies_us
        .lock()
        .await
        .push(connect_started.elapsed().as_micros() as u64);
    counters.connection_established();
    Ok(stream)
}

pub(crate) async fn upgrade(
    mut stream: TcpStream,
    scenario: &Scenario,
    counters: &Counters,
) -> anyhow::Result<Box<dyn IoStream>> {
    let needs_tunnel = scenario.is_explicit_proxy()
        && (scenario.tls.enabled || scenario.protocol != Protocol::Http1);
    if needs_tunnel {
        connect_tunnel(&mut stream, scenario, counters)
            .await
            .context("HTTP CONNECT failed")?;
    }
    if scenario.tls.enabled {
        Ok(Box::new(
            tls::connect(stream, scenario)
                .await
                .context("TLS handshake failed")?,
        ))
    } else {
        Ok(Box::new(stream))
    }
}

async fn connect_tunnel(
    stream: &mut TcpStream,
    scenario: &Scenario,
    counters: &Counters,
) -> anyhow::Result<()> {
    let request = format!(
        "CONNECT {} HTTP/1.1\r\nHost: {}\r\nProxy-Connection: keep-alive\r\n\r\n",
        scenario.target_addr(),
        scenario.target_addr()
    );
    stream.write_all(request.as_bytes()).await?;
    counters
        .tx
        .fetch_add(request.len() as u64, Ordering::Relaxed);
    let response = tokio::time::timeout(
        Duration::from_millis(scenario.timeouts.proxy_connect_ms),
        read_connect_headers(stream),
    )
    .await??;
    counters
        .rx
        .fetch_add(response.len() as u64, Ordering::Relaxed);
    if !response.starts_with(b"HTTP/1.1 200") && !response.starts_with(b"HTTP/1.0 200") {
        bail!(
            "CONNECT rejected with response {}",
            String::from_utf8_lossy(&response[..response.len().min(128)])
        );
    }
    Ok(())
}

async fn read_connect_headers(stream: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
    const MAX_HEADER_BYTES: usize = 16 * 1024;
    let mut response = Vec::with_capacity(512);
    let mut byte = [0_u8; 1];
    while response.len() < MAX_HEADER_BYTES {
        stream.read_exact(&mut byte).await?;
        response.push(byte[0]);
        if response.ends_with(b"\r\n\r\n") {
            return Ok(response);
        }
    }
    bail!("HTTP CONNECT response headers exceed {MAX_HEADER_BYTES} bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_tunnel_reports_rejection_and_accounts_bytes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let responder = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_connect_headers(&mut stream).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            request
        });
        let mut stream = TcpStream::connect(address).await.unwrap();
        let scenario = Scenario::default();
        let counters = Counters::default();

        let error = connect_tunnel(&mut stream, &scenario, &counters)
            .await
            .unwrap_err();
        let request = responder.await.unwrap();

        assert!(error.to_string().contains("403 Forbidden"));
        assert!(request.starts_with(b"CONNECT "));
        assert_eq!(counters.tx.load(Ordering::Relaxed), request.len() as u64);
        assert_eq!(counters.rx.load(Ordering::Relaxed), 45);
    }
}
