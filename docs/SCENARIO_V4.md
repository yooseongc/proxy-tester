# Scenario v4

Scenario v4 is the only accepted traffic contract. Older topology-based fields and implicit payload migration are intentionally unsupported during initial development. Unknown fields and missing directional payloads are rejected at deserialization.

The `path` is either `managed_direct`, pinned to an immutable prepared network revision, or `explicit_proxy`, with explicit Node IDs, endpoint IPv4 addresses, server port, and proxy address. Inline and passive-mirror appliances use managed direct traffic; their external bridge or TAP/SPAN configuration is outside this service.

`request_payload` and `response_payload` are required and independently support `empty`, `fixed`, `text`, `file`, and `random`. Random data is generated once per Run and shared across workers. PCAP mode uses `capture_artifact_id` and reconstructed bidirectional sessions instead of manual payload content.

Supported protocols are TCP, HTTP/1.1, and TLS-only HTTP/2. Explicit proxy TCP and TLS use CONNECT; plaintext HTTP/1.1 uses absolute-form requests. h2c is unsupported.
