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
    request = b"POST /scan HTTP/1.1\r\nHost: dlp.test\r\nContent-Length: 9\r\n\r\nDLP-SECRET"
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


if __name__ == "__main__":
    main()
