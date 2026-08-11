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
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exclusions {
    pub non_tcp_packets: u64,
    pub fragmented_packets: u64,
    pub truncated_packets: u64,
    pub unsupported_link_packets: u64,
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

#[derive(Default)]
struct Segments {
    client: BTreeMap<u32, Vec<u8>>,
    server: BTreeMap<u32, Vec<u8>>,
    duplicate_bytes: u64,
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
    for key in flow_order {
        let parts = segments.remove(&key).unwrap_or_default();
        let (client_to_server, client_dup) = reassemble(parts.client);
        let (server_to_client, server_dup) = reassemble(parts.server);
        analysis.flows.push(ReassembledFlow {
            key,
            client_to_server,
            server_to_client,
            retransmitted_bytes: parts.duplicate_bytes + client_dup + server_dup,
        });
    }
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

fn reassemble(segments: BTreeMap<u32, Vec<u8>>) -> (Vec<u8>, u64) {
    let mut output = Vec::new();
    let Some(first) = segments.keys().next().copied() else {
        return (output, 0);
    };
    let mut next = first;
    let mut duplicate = 0;
    for (sequence, payload) in segments {
        if sequence > next {
            next = sequence;
        }
        let overlap = next.saturating_sub(sequence) as usize;
        duplicate += overlap.min(payload.len()) as u64;
        if overlap < payload.len() {
            output.extend_from_slice(&payload[overlap..]);
            next = sequence.wrapping_add(payload.len() as u32);
        }
    }
    (output, duplicate)
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
        assert_eq!(
            analysis.flows[0].client_to_server,
            b"POST /scan HTTP/1.1\r\nHost: dlp.test\r\nContent-Length: 9\r\n\r\nDLP-SECRET"
        );
        assert_eq!(
            analysis.flows[0].server_to_client,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK"
        );
        assert_eq!(analysis.flows[1].client_to_server, b"PING");
        assert_eq!(analysis.flows[1].server_to_client, b"PONG");
    }
}
