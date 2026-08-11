# Scenario v2와 방향별 Payload

Scenario v2는 장비 유형 대신 생성할 트래픽을 중심으로 구성한다. `request_payload`는 Client→Server, `response_payload`는 Server→Client 방향이며 TCP payload 또는 HTTP body로 사용된다.

지원 종류는 `empty`, `fixed`, `text`, `file`, `random`이고 random 형식은 `binary`와 `printable_ascii`다. 최대 크기는 방향별 64 MiB다. Agent는 PrepareRun 단계에서 payload를 한 번 생성한 뒤 `Arc<[u8]>`로 worker 사이에 공유한다. 따라서 VU 또는 transaction 반복마다 난수를 만들거나 payload를 복제하지 않는다. 생성 시 원문 대신 크기와 SHA-256을 로그에 남긴다.

기존 version 1 JSON은 control plane과 agent에서 자동으로 version 2로 승격한다. HTTP의 `request_body_bytes`/`response_body_bytes`, TCP의 `tx_bytes`/`rx_bytes`는 각각 v2 fixed payload로 변환된다. 저장된 구성과 기존 Run JSON을 읽을 때에도 같은 변환을 적용한다.

현재 PCAP 업로드는 형식과 packet record 무결성 분석까지만 제공한다. TCP flow 재조립 및 gRPC artifact 전송은 아직 실행 경로에 연결되지 않았으므로 `capture_replay`는 validation에서 명시적으로 차단된다. file payload도 artifact ID validation은 수행하지만 agent 전송이 구현되기 전까지 PrepareRun에서 실패한다. 지원되지 않는 구성을 조용히 zero-fill로 실행하지 않기 위한 동작이다.

빌드 환경에는 시스템 `protoc`가 없어도 되도록 vendored protoc를 사용한다.
