use std::{sync::atomic::Ordering, time::Duration};

use anyhow::bail;
use proxy_tester_domain::Scenario;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::WorkerGate;
use crate::{connector::IoStream, payload::PreparedPayloads, telemetry::Counters};

pub(crate) async fn transactions(
    stream: &mut (impl IoStream + ?Sized),
    scenario: &Scenario,
    payloads: &PreparedPayloads,
    absolute_form: bool,
    counters: &Counters,
    gate: &WorkerGate<'_>,
) -> anyhow::Result<()> {
    let mut pending = Vec::with_capacity(8192);
    let mut index = 0;
    while gate.enabled() {
        if scenario.request.transactions_per_connection > 0
            && index >= scenario.request.transactions_per_connection
        {
            break;
        }
        while gate.paused.load(Ordering::Relaxed)
            && gate.running.load(Ordering::Relaxed)
            && gate.generating.load(Ordering::Relaxed)
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if !gate.enabled() {
            break;
        }
        let transaction_started = std::time::Instant::now();
        let target = if absolute_form {
            format!("http://{}{}", scenario.target_addr(), scenario.request.path)
        } else {
            scenario.request.path.clone()
        };
        let last = scenario.request.transactions_per_connection > 0
            && index + 1 == scenario.request.transactions_per_connection;
        let connection = if scenario.request.keep_alive && !last {
            "keep-alive"
        } else {
            "close"
        };
        let request = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: {}\r\nContent-Length: {}\r\nX-Response-Bytes: {}\r\n\r\n",
            scenario.request.method,
            target,
            scenario.request.host,
            connection,
            payloads.request.len(),
            payloads.response.len()
        );
        stream.write_all(request.as_bytes()).await?;
        stream.write_all(&payloads.request).await?;
        counters.tx.fetch_add(
            (request.len() + payloads.request.len()) as u64,
            Ordering::Relaxed,
        );
        let (response, body_len) = tokio::time::timeout(
            Duration::from_millis(scenario.timeouts.response_ms),
            read_message(stream, &mut pending, 64 * 1024),
        )
        .await??;
        counters
            .rx
            .fetch_add((response.len() + body_len) as u64, Ordering::Relaxed);
        if !response.starts_with(b"HTTP/1.1 2") {
            let status = response
                .split(|byte| *byte == b' ')
                .nth(1)
                .unwrap_or_default();
            bail!("HTTP error response {}", String::from_utf8_lossy(status));
        }
        counters.transaction_completed();
        counters
            .http_latencies_us
            .lock()
            .await
            .push(transaction_started.elapsed().as_micros() as u64);
        index += 1;
        if !last && scenario.request.think_time_ms > 0 {
            tokio::time::sleep(Duration::from_millis(scenario.request.think_time_ms)).await;
        }
    }
    Ok(())
}

pub(crate) async fn serve(
    stream: &mut (impl IoStream + ?Sized),
    payloads: &PreparedPayloads,
    counters: &Counters,
) -> anyhow::Result<()> {
    let mut pending = Vec::with_capacity(8192);
    loop {
        let (headers, body_len) = read_message(stream, &mut pending, 64 * 1024).await?;
        counters
            .rx
            .fetch_add((headers.len() + body_len) as u64, Ordering::Relaxed);
        let response_len =
            parse_numeric_header(&headers, "x-response-bytes").unwrap_or(payloads.response.len());
        let keep_alive = header_equals(&headers, "connection", "keep-alive");
        let connection = if keep_alive { "keep-alive" } else { "close" };
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {response_len}\r\nConnection: {connection}\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).await?;
        if response_len == payloads.response.len() {
            stream.write_all(&payloads.response).await?;
        } else {
            write_zeroes(stream, response_len).await?;
        }
        counters
            .tx
            .fetch_add((head.len() + response_len) as u64, Ordering::Relaxed);
        counters.transaction_completed();
        if !keep_alive {
            return Ok(());
        }
    }
}

async fn read_message(
    stream: &mut (impl AsyncRead + Unpin + ?Sized),
    pending: &mut Vec<u8>,
    max_headers: usize,
) -> anyhow::Result<(Vec<u8>, usize)> {
    let header_end = loop {
        if let Some(position) = pending.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if pending.len() > max_headers {
            bail!("headers too large");
        }
        let mut chunk = [0_u8; 8192];
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            bail!("connection closed before HTTP message");
        }
        pending.extend_from_slice(&chunk[..count]);
    };
    let remainder = pending.split_off(header_end);
    let headers = std::mem::replace(pending, remainder);
    let body_len = parse_numeric_header(&headers, "content-length").unwrap_or(0);
    while pending.len() < body_len {
        let mut chunk = [0_u8; 8192];
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            bail!("connection closed before HTTP body");
        }
        pending.extend_from_slice(&chunk[..count]);
    }
    pending.drain(..body_len);
    Ok((headers, body_len))
}

fn parse_numeric_header(raw: &[u8], name: &str) -> Option<usize> {
    let text = String::from_utf8_lossy(raw);
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.trim().parse().ok())
}

fn header_equals(raw: &[u8], name: &str, expected: &str) -> bool {
    let text = String::from_utf8_lossy(raw);
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .any(|(header, value)| {
            header.eq_ignore_ascii_case(name) && value.trim().eq_ignore_ascii_case(expected)
        })
}

async fn write_zeroes(
    stream: &mut (impl AsyncWrite + Unpin + ?Sized),
    mut remaining: usize,
) -> anyhow::Result<()> {
    static ZEROES: [u8; 65_536] = [0; 65_536];
    while remaining > 0 {
        let size = remaining.min(ZEROES.len());
        stream.write_all(&ZEROES[..size]).await?;
        remaining -= size;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn framing_preserves_pipelined_message_after_body() {
        let (mut writer, mut reader) = tokio::io::duplex(512);
        tokio::spawn(async move {
            writer
                .write_all(
                    b"POST /one HTTP/1.1\r\nContent-Length: 3\r\n\r\noneGET /two HTTP/1.1\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let mut pending = Vec::new();

        let (first, first_body_len) = read_message(&mut reader, &mut pending, 1024).await.unwrap();
        let (second, second_body_len) =
            read_message(&mut reader, &mut pending, 1024).await.unwrap();

        assert!(first.starts_with(b"POST /one"));
        assert_eq!(first_body_len, 3);
        assert!(second.starts_with(b"GET /two"));
        assert_eq!(second_body_len, 0);
    }

    #[test]
    fn headers_are_matched_case_insensitively() {
        let headers = b"HTTP/1.1 200 OK\r\nCONTENT-LENGTH: 42\r\nConnection: Keep-Alive\r\n\r\n";

        assert_eq!(parse_numeric_header(headers, "content-length"), Some(42));
        assert!(header_equals(headers, "connection", "keep-alive"));
    }
}
