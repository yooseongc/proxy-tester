"""Generate deterministic plaintext packet captures for replay tests.

Run: python tests/pcap/generate_fixtures.py
"""
from pathlib import Path

from scapy.all import Ether, IP, IPv6, TCP, UDP, Raw, PcapNgWriter, wrpcap

OUT = Path(__file__).parent / "fixtures"


def tcp_packet(src, dst, sport, dport, seq, ack, flags="PA", payload=b"", ipv6=False):
    network = IPv6(src=src, dst=dst) if ipv6 else IP(src=src, dst=dst)
    return Ether() / network / TCP(sport=sport, dport=dport, seq=seq, ack=ack, flags=flags) / Raw(payload)


def main():
    OUT.mkdir(exist_ok=True)
    request = b"POST /scan HTTP/1.1\r\nHost: dlp.test\r\nContent-Length: 10\r\n\r\nDLP-SECRET"
    response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK"
    packets = [
        # Intentionally out of order; the parser must order by TCP sequence.
        tcp_packet("192.0.2.10", "192.0.2.20", 41000, 8080, 1031, 5001, payload=request[30:]),
        tcp_packet("192.0.2.10", "192.0.2.20", 41000, 8080, 1001, 5001, payload=request[:30]),
        # Exact retransmission of the first request segment.
        tcp_packet("192.0.2.10", "192.0.2.20", 41000, 8080, 1001, 5001, payload=request[:30]),
        tcp_packet("192.0.2.20", "192.0.2.10", 8080, 41000, 5001, 1001 + len(request), payload=response),
        # A second supported IPv6 TCP flow.
        tcp_packet("2001:db8::10", "2001:db8::20", 42000, 9000, 1, 1, payload=b"PING", ipv6=True),
        tcp_packet("2001:db8::20", "2001:db8::10", 9000, 42000, 1, 5, payload=b"PONG", ipv6=True),
        # Unsupported UDP traffic used to verify exclusion accounting.
        Ether() / IP(src="198.51.100.1", dst="198.51.100.2") / UDP(sport=1, dport=2) / Raw(b"ignore"),
    ]
    for index, packet in enumerate(packets):
        packet.time = 1_700_000_000 + index / 1_000
    wrpcap(str(OUT / "plaintext_flows.pcap"), packets)
    writer = PcapNgWriter(str(OUT / "plaintext_flows.pcapng"))
    for packet in packets:
        writer.write(packet)
    writer.close()

    request_one = b"POST /first?source=pcap HTTP/1.1\r\nHost: captured.test\r\nContent-Length: 5\r\nConnection: keep-alive\r\n\r\nalpha"
    response_one = b"HTTP/1.1 201 Created\r\nContent-Length: 4\r\nConnection: keep-alive\r\n\r\nbeta"
    request_two = b"GET /second HTTP/1.1\r\nHost: captured.test\r\nConnection: close\r\n\r\n"
    response_two = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\ngamma\r\n0\r\n\r\n"
    client_seq = 1001
    server_seq = 9001
    http_packets = []
    for request_message, response_message in ((request_one, response_one), (request_two, response_two)):
        http_packets.append(tcp_packet("203.0.113.10", "203.0.113.20", 43000, 8080, client_seq, server_seq, payload=request_message))
        client_seq += len(request_message)
        http_packets.append(tcp_packet("203.0.113.20", "203.0.113.10", 8080, 43000, server_seq, client_seq, payload=response_message))
        server_seq += len(response_message)
    for index, packet in enumerate(http_packets):
        packet.time = 1_700_000_100 + index / 1_000
    wrpcap(str(OUT / "http_transactions.pcap"), http_packets)


if __name__ == "__main__":
    main()
