# Scenario v2와 방향별 Payload

Scenario v2는 장비 유형 대신 생성할 트래픽을 중심으로 구성한다. `request_payload`는 Client→Server, `response_payload`는 Server→Client 방향이며 TCP payload 또는 HTTP body로 사용된다.

지원 종류는 `empty`, `fixed`, `text`, `file`, `random`이고 random 형식은 `binary`와 `printable_ascii`다. 최대 크기는 방향별 64 MiB다. Agent는 PrepareRun 단계에서 payload를 한 번 생성한 뒤 `Arc<[u8]>`로 worker 사이에 공유한다. 따라서 VU 또는 transaction 반복마다 난수를 만들거나 payload를 복제하지 않는다. 생성 시 원문 대신 크기와 SHA-256을 로그에 남긴다.

기존 version 1 JSON은 control plane과 agent에서 자동으로 version 2로 승격한다. HTTP의 `request_body_bytes`/`response_body_bytes`, TCP의 `tx_bytes`/`rx_bytes`는 각각 v2 fixed payload로 변환된다. 저장된 구성과 기존 Run JSON을 읽을 때에도 같은 변환을 적용한다.

PCAP/PCAPNG 업로드 시 Ethernet IPv4/IPv6 TCP flow를 sequence 기준으로 재조립하고 retransmission, 누락 구간, 비 TCP, fragment, TLS ciphertext를 분류한다. TCP `capture_replay`는 분석된 양방향 평문 flow를 turn 단위로 양쪽 Agent에서 재생하며 flow를 round-robin으로 순환한다. TLS를 선택하면 추출한 평문을 새 TLS 세션에서 재암호화한다. HTTP 메시지 단위 replay는 M4까지 실행을 차단한다. file payload는 payload artifact를 256 KiB gRPC chunk로 양쪽 Agent에 전송하며, Agent가 offset·전체 크기·SHA-256을 검증한 뒤 공유 buffer로 준비한다.

빌드 환경에는 시스템 `protoc`가 없어도 되도록 vendored protoc를 사용한다.
