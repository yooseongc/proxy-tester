use std::collections::{BTreeMap, HashMap};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub address: [u8; 16],
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FlowKey {
    pub client: Endpoint,
    pub server: Endpoint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReassembledFlow {
    pub key: FlowKey,
    pub client_to_server: Vec<u8>,
    pub server_to_client: Vec<u8>,
    pub retransmitted_bytes: u64,
    pub turns: Vec<ReplayTurn>,
    pub http_transactions: Vec<HttpTransaction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpTransaction {
    pub request: Vec<u8>,
    pub response: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToServer,
    ServerToClient,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayTurn {
    pub direction: Direction,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exclusions {
    pub non_tcp_packets: u64,
    pub fragmented_packets: u64,
    pub truncated_packets: u64,
    pub unsupported_link_packets: u64,
    pub incomplete_flows: u64,
    pub encrypted_tls_flows: u64,
    pub non_http_flows: u64,
    pub unsupported_http_flows: u64,
    pub http_upgrade_flows: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFormat {
    Pcap,
    PcapNg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureAnalysis {
    pub packet_count: u64,
    pub captured_bytes: u64,
    pub flows: Vec<ReassembledFlow>,
    pub exclusions: Exclusions,
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("capture is truncated")]
    Truncated,
    #[error("unsupported capture format")]
    UnsupportedFormat,
    #[error("unsupported PCAP link type {0}")]
    UnsupportedLinkType(u32),
}

pub fn analyze_capture(data: &[u8]) -> Result<(CaptureFormat, CaptureAnalysis), CaptureError> {
    if data.starts_with(&[0x0a, 0x0d, 0x0d, 0x0a]) {
        analyze_pcapng(data).map(|analysis| (CaptureFormat::PcapNg, analysis))
    } else {
        analyze_pcap(data).map(|analysis| (CaptureFormat::Pcap, analysis))
    }
}

#[derive(Default)]
struct Segments {
    client: BTreeMap<u32, Vec<u8>>,
    server: BTreeMap<u32, Vec<u8>>,
    duplicate_bytes: u64,
    events: Vec<(Direction, u32, Vec<u8>)>,
}

pub fn analyze_pcap(data: &[u8]) -> Result<CaptureAnalysis, CaptureError> {
    if data.len() < 24 {
        return Err(CaptureError::Truncated);
    }
    let magic = &data[..4];
    let little = magic == [0xd4, 0xc3, 0xb2, 0xa1] || magic == [0x4d, 0x3c, 0xb2, 0xa1];
    let big = magic == [0xa1, 0xb2, 0xc3, 0xd4] || magic == [0xa1, 0xb2, 0x3c, 0x4d];
    if !little && !big {
        return Err(CaptureError::UnsupportedFormat);
    }
    let u32_at = |bytes: &[u8]| {
        if little {
            u32::from_le_bytes(bytes.try_into().unwrap())
        } else {
            u32::from_be_bytes(bytes.try_into().unwrap())
        }
    };
    let link_type = u32_at(&data[20..24]);
    if link_type != 1 {
        return Err(CaptureError::UnsupportedLinkType(link_type));
    }
    let mut analysis = CaptureAnalysis {
        packet_count: 0,
        captured_bytes: 0,
        flows: Vec::new(),
        exclusions: Exclusions::default(),
    };
    let mut flow_order = Vec::new();
    let mut segments: HashMap<FlowKey, Segments> = HashMap::new();
    let mut offset = 24;
    while offset < data.len() {
        if offset + 16 > data.len() {
            return Err(CaptureError::Truncated);
        }
        let included = u32_at(&data[offset + 8..offset + 12]) as usize;
        let original = u32_at(&data[offset + 12..offset + 16]) as u64;
        offset += 16;
        if offset + included > data.len() {
            return Err(CaptureError::Truncated);
        }
        analysis.packet_count += 1;
        analysis.captured_bytes += original;
        parse_ethernet(
            &data[offset..offset + included],
            &mut flow_order,
            &mut segments,
            &mut analysis.exclusions,
        );
        offset += included;
    }
    finish_analysis(&mut analysis, flow_order, segments);
    Ok(analysis)
}

fn finish_analysis(
    analysis: &mut CaptureAnalysis,
    flow_order: Vec<FlowKey>,
    mut segments: HashMap<FlowKey, Segments>,
) {
    for key in flow_order {
        let parts = segments.remove(&key).unwrap_or_default();
        let (client_to_server, client_dup, client_gap) = reassemble(parts.client);
        let (server_to_client, server_dup, server_gap) = reassemble(parts.server);
        if client_to_server.is_empty() || server_to_client.is_empty() || client_gap || server_gap {
            analysis.exclusions.incomplete_flows += 1;
            continue;
        }
        if looks_like_tls(&client_to_server) || looks_like_tls(&server_to_client) {
            analysis.exclusions.encrypted_tls_flows += 1;
            continue;
        }
        let http_transactions =
            match extract_http_transactions(&client_to_server, &server_to_client) {
                Some(transactions) => transactions,
                None if !client_to_server.starts_with(b"HTTP/")
                    && !client_to_server.windows(6).any(|part| part == b" HTTP/") =>
                {
                    analysis.exclusions.non_http_flows += 1;
                    Vec::new()
                }
                None if contains_ascii_case_insensitive(&client_to_server, b"upgrade:")
                    || contains_ascii_case_insensitive(
                        &client_to_server,
                        b"connection: upgrade",
                    )
                    || contains_ascii_case_insensitive(&server_to_client, b"upgrade:")
                    || contains_ascii_case_insensitive(
                        &server_to_client,
                        b"connection: upgrade",
                    ) =>
                {
                    analysis.exclusions.http_upgrade_flows += 1;
                    Vec::new()
                }
                None => {
                    analysis.exclusions.unsupported_http_flows += 1;
                    Vec::new()
                }
            };
        analysis.flows.push(ReassembledFlow {
            key,
            client_to_server,
            server_to_client,
            retransmitted_bytes: parts.duplicate_bytes + client_dup + server_dup,
            turns: build_turns(parts.events),
            http_transactions,
        });
    }
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|part| part.eq_ignore_ascii_case(needle))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpMessageKind {
    Request,
    Response,
}

pub fn extract_http_transactions(
    client_to_server: &[u8],
    server_to_client: &[u8],
) -> Option<Vec<HttpTransaction>> {
    let requests = split_http_messages(client_to_server, HttpMessageKind::Request)?;
    let responses = split_http_messages(server_to_client, HttpMessageKind::Response)?;
    if requests.is_empty() || requests.len() != responses.len() {
        return None;
    }
    Some(
        requests
            .into_iter()
            .zip(responses)
            .map(|(request, response)| HttpTransaction { request, response })
            .collect(),
    )
}

fn split_http_messages(stream: &[u8], kind: HttpMessageKind) -> Option<Vec<Vec<u8>>> {
    let mut messages = Vec::new();
    let mut offset = 0;
    while offset < stream.len() {
        let header_relative = stream[offset..]
            .windows(4)
            .position(|part| part == b"\r\n\r\n")?;
        let header_end = offset + header_relative + 4;
        let headers = &stream[offset..header_end];
        let first_line_end = headers.windows(2).position(|part| part == b"\r\n")?;
        let first_line = &headers[..first_line_end];
        match kind {
            HttpMessageKind::Request if !valid_request_line(first_line) => return None,
            HttpMessageKind::Response if !valid_status_line(first_line) => return None,
            _ => {}
        }
        if header_value(headers, b"upgrade").is_some()
            || header_tokens(headers, b"connection")
                .any(|token| token.eq_ignore_ascii_case(b"upgrade"))
        {
            return None;
        }
        let message_end = if header_tokens(headers, b"transfer-encoding")
            .any(|token| token.eq_ignore_ascii_case(b"chunked"))
        {
            chunked_end(stream, header_end)?
        } else if let Some(value) = header_value(headers, b"content-length") {
            let length = std::str::from_utf8(trim_ascii(value))
                .ok()?
                .parse::<usize>()
                .ok()?;
            header_end
                .checked_add(length)
                .filter(|end| *end <= stream.len())?
        } else if response_has_no_body(first_line) || kind == HttpMessageKind::Request {
            header_end
        } else {
            return None;
        };
        messages.push(stream[offset..message_end].to_vec());
        offset = message_end;
    }
    Some(messages)
}

fn valid_request_line(line: &[u8]) -> bool {
    let mut fields = line.split(|byte| *byte == b' ');
    let method = fields.next().unwrap_or_default();
    let target = fields.next().unwrap_or_default();
    let version = fields.next().unwrap_or_default();
    !method.is_empty()
        && method
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || *byte == b'-')
        && !target.is_empty()
        && version == b"HTTP/1.1"
        && fields.next().is_none()
}

fn valid_status_line(line: &[u8]) -> bool {
    line.starts_with(b"HTTP/1.1 ")
        && line
            .get(9..12)
            .is_some_and(|status| status.iter().all(u8::is_ascii_digit))
}

fn response_has_no_body(line: &[u8]) -> bool {
    let status = line
        .get(9..12)
        .and_then(|status| std::str::from_utf8(status).ok())
        .and_then(|status| status.parse::<u16>().ok());
    matches!(status, Some(100..=199 | 204 | 304))
}

fn header_value<'a>(headers: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    headers
        .split(|byte| *byte == b'\n')
        .skip(1)
        .find_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let colon = line.iter().position(|byte| *byte == b':')?;
            line[..colon]
                .eq_ignore_ascii_case(name)
                .then(|| trim_ascii(&line[colon + 1..]))
        })
}

fn header_tokens<'a>(headers: &'a [u8], name: &'a [u8]) -> impl Iterator<Item = &'a [u8]> {
    header_value(headers, name)
        .into_iter()
        .flat_map(|value| value.split(|byte| *byte == b',').map(trim_ascii))
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn chunked_end(stream: &[u8], mut offset: usize) -> Option<usize> {
    loop {
        let line_relative = stream[offset..]
            .windows(2)
            .position(|part| part == b"\r\n")?;
        let line_end = offset + line_relative;
        let size_field = stream[offset..line_end]
            .split(|byte| *byte == b';')
            .next()?;
        let size =
            usize::from_str_radix(std::str::from_utf8(trim_ascii(size_field)).ok()?, 16).ok()?;
        offset = line_end + 2;
        if size == 0 {
            let trailers_relative = stream[offset..]
                .windows(4)
                .position(|part| part == b"\r\n\r\n");
            return if stream.get(offset..offset + 2) == Some(b"\r\n") {
                Some(offset + 2)
            } else {
                trailers_relative.map(|relative| offset + relative + 4)
            };
        }
        offset = offset.checked_add(size)?;
        if stream.get(offset..offset + 2) != Some(b"\r\n") {
            return None;
        }
        offset += 2;
    }
}

fn looks_like_tls(data: &[u8]) -> bool {
    data.len() >= 5 && matches!(data[0], 0x14..=0x17) && data[1] == 0x03 && data[2] <= 0x04
}

pub fn analyze_pcapng(data: &[u8]) -> Result<CaptureAnalysis, CaptureError> {
    if data.len() < 28 || !data.starts_with(&[0x0a, 0x0d, 0x0d, 0x0a]) {
        return Err(CaptureError::UnsupportedFormat);
    }
    let little = match &data[8..12] {
        [0x4d, 0x3c, 0x2b, 0x1a] => true,
        [0x1a, 0x2b, 0x3c, 0x4d] => false,
        _ => return Err(CaptureError::UnsupportedFormat),
    };
    let read_u32 = |bytes: &[u8]| {
        if little {
            u32::from_le_bytes(bytes.try_into().unwrap())
        } else {
            u32::from_be_bytes(bytes.try_into().unwrap())
        }
    };
    let read_u16 = |bytes: &[u8]| {
        if little {
            u16::from_le_bytes(bytes.try_into().unwrap())
        } else {
            u16::from_be_bytes(bytes.try_into().unwrap())
        }
    };
    let mut analysis = CaptureAnalysis {
        packet_count: 0,
        captured_bytes: 0,
        flows: Vec::new(),
        exclusions: Exclusions::default(),
    };
    let mut flow_order = Vec::new();
    let mut segments = HashMap::new();
    let mut interfaces = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        if offset + 12 > data.len() {
            return Err(CaptureError::Truncated);
        }
        let block_type = read_u32(&data[offset..offset + 4]);
        let length = read_u32(&data[offset + 4..offset + 8]) as usize;
        if length < 12 || !length.is_multiple_of(4) || offset + length > data.len() {
            return Err(CaptureError::Truncated);
        }
        if read_u32(&data[offset + length - 4..offset + length]) as usize != length {
            return Err(CaptureError::Truncated);
        }
        match block_type {
            1 if length >= 20 => interfaces.push(read_u16(&data[offset + 8..offset + 10]) as u32),
            6 if length >= 32 => {
                let interface = read_u32(&data[offset + 8..offset + 12]) as usize;
                let captured = read_u32(&data[offset + 20..offset + 24]) as usize;
                let original = read_u32(&data[offset + 24..offset + 28]) as u64;
                if 28 + captured + 4 > length {
                    return Err(CaptureError::Truncated);
                }
                analysis.packet_count += 1;
                analysis.captured_bytes += original;
                if interfaces.get(interface) == Some(&1) {
                    parse_ethernet(
                        &data[offset + 28..offset + 28 + captured],
                        &mut flow_order,
                        &mut segments,
                        &mut analysis.exclusions,
                    );
                } else {
                    analysis.exclusions.unsupported_link_packets += 1;
                }
            }
            _ => {}
        }
        offset += length;
    }
    finish_analysis(&mut analysis, flow_order, segments);
    Ok(analysis)
}

fn parse_ethernet(
    frame: &[u8],
    order: &mut Vec<FlowKey>,
    flows: &mut HashMap<FlowKey, Segments>,
    excluded: &mut Exclusions,
) {
    if frame.len() < 14 {
        excluded.truncated_packets += 1;
        return;
    }
    let ether_type = u16::from_be_bytes([frame[12], frame[13]]);
    let packet = match ether_type {
        0x0800 => parse_ipv4(&frame[14..], excluded),
        0x86dd => parse_ipv6(&frame[14..], excluded),
        _ => {
            excluded.unsupported_link_packets += 1;
            return;
        }
    };
    let Some((source, destination, tcp)) = packet else {
        return;
    };
    let source = Endpoint {
        address: source,
        port: tcp.0,
    };
    let destination = Endpoint {
        address: destination,
        port: tcp.1,
    };
    let direct = FlowKey {
        client: source.clone(),
        server: destination.clone(),
    };
    let reverse = FlowKey {
        client: destination,
        server: source,
    };
    let (key, from_client) = if flows.contains_key(&direct) {
        (direct, true)
    } else if flows.contains_key(&reverse) {
        (reverse, false)
    } else {
        order.push(direct.clone());
        flows.insert(direct.clone(), Segments::default());
        (direct, true)
    };
    if tcp.3.is_empty() {
        return;
    }
    let direction = flows.get_mut(&key).unwrap();
    let event_direction = if from_client {
        Direction::ClientToServer
    } else {
        Direction::ServerToClient
    };
    direction
        .events
        .push((event_direction, tcp.2, tcp.3.to_vec()));
    let target = if from_client {
        &mut direction.client
    } else {
        &mut direction.server
    };
    if let Some(existing) = target.get(&tcp.2) {
        direction.duplicate_bytes += existing.len().min(tcp.3.len()) as u64;
    } else {
        target.insert(tcp.2, tcp.3.to_vec());
    }
}

fn build_turns(events: Vec<(Direction, u32, Vec<u8>)>) -> Vec<ReplayTurn> {
    let mut groups: Vec<(Direction, BTreeMap<u32, Vec<u8>>)> = Vec::new();
    for (direction, sequence, payload) in events {
        if payload.is_empty() {
            continue;
        }
        if groups.last().is_none_or(|group| group.0 != direction) {
            groups.push((direction, BTreeMap::new()));
        }
        groups
            .last_mut()
            .unwrap()
            .1
            .entry(sequence)
            .or_insert(payload);
    }
    groups
        .into_iter()
        .filter_map(|(direction, segments)| {
            let (payload, _, gap) = reassemble(segments);
            (!payload.is_empty() && !gap).then_some(ReplayTurn { direction, payload })
        })
        .collect()
}

type ParsedPacket<'a> = ([u8; 16], [u8; 16], (u16, u16, u32, &'a [u8]));

fn parse_ipv4<'a>(packet: &'a [u8], excluded: &mut Exclusions) -> Option<ParsedPacket<'a>> {
    if packet.len() < 20 {
        excluded.truncated_packets += 1;
        return None;
    }
    let header = ((packet[0] & 0x0f) as usize) * 4;
    if header < 20 || packet.len() < header {
        excluded.truncated_packets += 1;
        return None;
    }
    let fragment = u16::from_be_bytes([packet[6], packet[7]]);
    if fragment & 0x3fff != 0 {
        excluded.fragmented_packets += 1;
        return None;
    }
    if packet[9] != 6 {
        excluded.non_tcp_packets += 1;
        return None;
    }
    let mut source = [0; 16];
    source[12..].copy_from_slice(&packet[12..16]);
    let mut destination = [0; 16];
    destination[12..].copy_from_slice(&packet[16..20]);
    parse_tcp(&packet[header..], excluded).map(|tcp| (source, destination, tcp))
}

fn parse_ipv6<'a>(packet: &'a [u8], excluded: &mut Exclusions) -> Option<ParsedPacket<'a>> {
    if packet.len() < 40 {
        excluded.truncated_packets += 1;
        return None;
    }
    if packet[6] != 6 {
        excluded.non_tcp_packets += 1;
        return None;
    }
    let mut source = [0; 16];
    source.copy_from_slice(&packet[8..24]);
    let mut destination = [0; 16];
    destination.copy_from_slice(&packet[24..40]);
    parse_tcp(&packet[40..], excluded).map(|tcp| (source, destination, tcp))
}

fn parse_tcp<'a>(
    segment: &'a [u8],
    excluded: &mut Exclusions,
) -> Option<(u16, u16, u32, &'a [u8])> {
    if segment.len() < 20 {
        excluded.truncated_packets += 1;
        return None;
    }
    let header = ((segment[12] >> 4) as usize) * 4;
    if header < 20 || segment.len() < header {
        excluded.truncated_packets += 1;
        return None;
    }
    Some((
        u16::from_be_bytes([segment[0], segment[1]]),
        u16::from_be_bytes([segment[2], segment[3]]),
        u32::from_be_bytes(segment[4..8].try_into().unwrap()),
        &segment[header..],
    ))
}

fn reassemble(segments: BTreeMap<u32, Vec<u8>>) -> (Vec<u8>, u64, bool) {
    let mut output = Vec::new();
    let Some(first) = segments.keys().next().copied() else {
        return (output, 0, false);
    };
    let mut next = first;
    let mut duplicate = 0;
    let mut gap = false;
    for (sequence, payload) in segments {
        if sequence > next {
            gap = true;
            next = sequence;
        }
        let overlap = next.saturating_sub(sequence) as usize;
        duplicate += overlap.min(payload.len()) as u64;
        if overlap < payload.len() {
            output.extend_from_slice(&payload[overlap..]);
            next = sequence.wrapping_add(payload.len() as u32);
        }
    }
    (output, duplicate, gap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scapy_fixture_reassembles_ipv4_and_ipv6_flows() {
        let capture = include_bytes!("../../../tests/pcap/fixtures/plaintext_flows.pcap");
        let analysis = analyze_pcap(capture).unwrap();
        assert_eq!(analysis.packet_count, 7);
        assert_eq!(analysis.flows.len(), 2);
        assert_eq!(analysis.exclusions.non_tcp_packets, 1);
        assert_eq!(analysis.flows[0].retransmitted_bytes, 30);
        assert_eq!(analysis.flows[0].turns.len(), 2);
        assert_eq!(
            analysis.flows[0].turns[0].direction,
            Direction::ClientToServer
        );
        assert_eq!(
            analysis.flows[0].turns[1].direction,
            Direction::ServerToClient
        );
        assert_eq!(
            analysis.flows[0].client_to_server,
            b"POST /scan HTTP/1.1\r\nHost: dlp.test\r\nContent-Length: 10\r\n\r\nDLP-SECRET"
        );
        assert_eq!(analysis.flows[0].http_transactions.len(), 1);
        assert_eq!(
            analysis.flows[0].server_to_client,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK"
        );
        assert_eq!(analysis.flows[1].client_to_server, b"PING");
        assert_eq!(analysis.flows[1].server_to_client, b"PONG");
        assert_eq!(analysis.exclusions.non_http_flows, 1);
    }

    #[test]
    fn pcapng_fixture_matches_classic_pcap_analysis() {
        let classic = analyze_capture(include_bytes!(
            "../../../tests/pcap/fixtures/plaintext_flows.pcap"
        ))
        .unwrap();
        let pcapng = analyze_capture(include_bytes!(
            "../../../tests/pcap/fixtures/plaintext_flows.pcapng"
        ))
        .unwrap();
        assert_eq!(classic.0, CaptureFormat::Pcap);
        assert_eq!(pcapng.0, CaptureFormat::PcapNg);
        assert_eq!(classic.1, pcapng.1);
    }

    #[test]
    fn scapy_http_fixture_extracts_message_transactions() {
        let analysis = analyze_pcap(include_bytes!(
            "../../../tests/pcap/fixtures/http_transactions.pcap"
        ))
        .unwrap();
        assert_eq!(analysis.flows.len(), 1);
        assert_eq!(analysis.flows[0].http_transactions.len(), 2);
        assert!(
            analysis.flows[0].http_transactions[0]
                .request
                .ends_with(b"alpha")
        );
        assert!(
            analysis.flows[0].http_transactions[1]
                .response
                .ends_with(b"0\r\n\r\n")
        );
    }

    #[test]
    fn recognizes_tls_record_payloads() {
        assert!(looks_like_tls(&[0x16, 0x03, 0x03, 0, 4, 1, 2, 3, 4]));
        assert!(!looks_like_tls(b"GET / HTTP/1.1\r\n"));
    }

    #[test]
    fn sequence_gaps_are_marked_incomplete() {
        let segments = BTreeMap::from([(10, b"ab".to_vec()), (20, b"cd".to_vec())]);
        let (payload, duplicate, gap) = reassemble(segments);
        assert_eq!(payload, b"abcd");
        assert_eq!(duplicate, 0);
        assert!(gap);
    }

    #[test]
    fn extracts_content_length_and_chunked_keep_alive_transactions() {
        let requests = b"POST /one HTTP/1.1\r\nHost: old.test\r\nContent-Length: 3\r\n\r\noneGET /two HTTP/1.1\r\nHost: old.test\r\n\r\n";
        let responses = b"HTTP/1.1 201 Created\r\nContent-Length: 3\r\n\r\ntwoHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nend\r\n0\r\n\r\n";
        let transactions = extract_http_transactions(requests, responses).unwrap();
        assert_eq!(transactions.len(), 2);
        assert_eq!(
            transactions[0].request,
            b"POST /one HTTP/1.1\r\nHost: old.test\r\nContent-Length: 3\r\n\r\none"
        );
        assert_eq!(
            transactions[0].response,
            b"HTTP/1.1 201 Created\r\nContent-Length: 3\r\n\r\ntwo"
        );
        assert_eq!(
            transactions[1].request,
            b"GET /two HTTP/1.1\r\nHost: old.test\r\n\r\n"
        );
        assert_eq!(
            transactions[1].response,
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nend\r\n0\r\n\r\n"
        );
    }

    #[test]
    fn rejects_upgrade_and_close_delimited_http_flows() {
        assert!(
            extract_http_transactions(
                b"GET /chat HTTP/1.1\r\nHost: old.test\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
                b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n"
            )
            .is_none()
        );
        assert!(
            extract_http_transactions(
                b"GET / HTTP/1.1\r\nHost: old.test\r\n\r\n",
                b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nbody"
            )
            .is_none()
        );
    }

    #[test]
    fn http_exclusion_reasons_are_counted_during_capture_analysis() {
        let mut analysis = CaptureAnalysis {
            packet_count: 0,
            captured_bytes: 0,
            flows: Vec::new(),
            exclusions: Exclusions::default(),
        };
        let key = FlowKey {
            client: Endpoint {
                address: [0; 16],
                port: 1,
            },
            server: Endpoint {
                address: [1; 16],
                port: 2,
            },
        };
        let flows = HashMap::from([(
            key.clone(),
            Segments {
                client: BTreeMap::from([(
                    1,
                    b"GET / HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\n\r\n".to_vec(),
                )]),
                server: BTreeMap::from([(
                    1,
                    b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\n\r\n".to_vec(),
                )]),
                ..Default::default()
            },
        )]);
        finish_analysis(&mut analysis, vec![key], flows);
        assert_eq!(analysis.exclusions.http_upgrade_flows, 1);
        assert!(analysis.flows[0].http_transactions.is_empty());
    }
}
