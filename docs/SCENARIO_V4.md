# Scenario v4

Scenario v4 is the only accepted traffic contract. Older topology-based fields and implicit payload migration are intentionally unsupported during initial development. Unknown fields and missing directional payloads are rejected at deserialization.

The `path` is either `managed_direct`, pinned to an immutable prepared network revision, or `explicit_proxy`, with explicit Node IDs, endpoint IPv4 addresses, server port, and proxy address. Inline and passive-mirror appliances use managed direct traffic; their external bridge or TAP/SPAN configuration is outside this service.

`request_payload` and `response_payload` are required and independently support `empty`, `fixed`, `text`, `file`, and `random`. Random data is generated once per Run and shared across workers. PCAP mode uses `capture_artifact_id` and reconstructed bidirectional sessions instead of manual payload content.

Supported protocols are TCP, HTTP/1.1, and TLS-only HTTP/2. Explicit proxy TCP and TLS use CONNECT; plaintext HTTP/1.1 uses absolute-form requests. h2c is unsupported.

## TCP direct and TCP CONNECT

`managed_direct` TCP와 explicit-proxy TCP는 같은 application payload를 생성하지만 네트워크 경로는 다르다.

- TCP direct는 client가 설정된 server endpoint로 TCP 연결 하나를 직접 연다. 인라인 투명 프록시와 passive mirror는 이 경로 외부에 배치되며 별도의 Scenario mode가 아니다.
- TCP CONNECT는 client가 HTTP proxy에 먼저 연결하고 server endpoint를 대상으로 HTTP/1.1 `CONNECT` 요청을 보낸다. proxy가 성공 응답을 반환한 뒤에만 설정된 TCP, TLS 또는 replay traffic을 tunnel로 전송한다.
- 따라서 CONNECT 시험에는 proxy의 요청 해석, upstream 연결 수립과 양방향 relay 비용이 포함된다. proxy는 tunnel마다 client-facing TCP leg와 server-facing TCP leg를 각각 유지한다.

Direct와 CONNECT 결과는 서로 다른 경로의 계측값이다. CONNECT CPS 또는 bandwidth가 더 낮다는 사실만으로 traffic generator나 target server의 결함을 의미하지 않으며, proxy의 tunnel 수립 및 relay 처리 용량도 결과에 포함된다.
