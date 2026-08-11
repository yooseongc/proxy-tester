# Scenario v2와 방향별 Payload

Scenario v2는 장비 유형 대신 생성할 트래픽을 중심으로 구성한다. `request_payload`는 Client→Server, `response_payload`는 Server→Client 방향이며 TCP payload 또는 HTTP body로 사용된다.

지원 종류는 `empty`, `fixed`, `text`, `file`, `random`이고 random 형식은 `binary`와 `printable_ascii`다. 최대 크기는 방향별 64 MiB다. Agent는 PrepareRun 단계에서 payload를 한 번 생성한 뒤 `Arc<[u8]>`로 worker 사이에 공유한다. 따라서 VU 또는 transaction 반복마다 난수를 만들거나 payload를 복제하지 않는다. 생성 시 원문 대신 크기와 SHA-256을 로그에 남긴다.

기존 version 1 JSON은 control plane과 agent에서 자동으로 version 2로 승격한다. HTTP의 `request_body_bytes`/`response_body_bytes`, TCP의 `tx_bytes`/`rx_bytes`는 각각 v2 fixed payload로 변환된다. 저장된 구성과 기존 Run JSON을 읽을 때에도 같은 변환을 적용한다.

PCAP/PCAPNG 업로드 시 Ethernet IPv4/IPv6 TCP flow를 sequence 기준으로 재조립하고 retransmission, 누락 구간, 비 TCP, fragment, TLS ciphertext를 분류한다. TCP `capture_replay`는 분석된 양방향 평문 flow를 turn 단위로 양쪽 Agent에서 재생하며 flow를 round-robin으로 순환한다. TLS를 선택하면 추출한 평문을 새 TLS 세션에서 재암호화한다.

HTTP/1.1 `capture_replay`는 양방향 stream에서 request/status line과 `Content-Length` 또는 chunked body 경계를 추출하고 같은 연결의 transaction 순서를 유지한다. 캡처의 method, path/query, header, body와 response bytes를 재사용하되 `Host`는 Scenario 값으로 바꾼다. 평문 explicit proxy Client 요청만 현재 target을 사용하는 absolute-form으로 바꾸며, Server는 proxy가 전달하는 origin-form을 사용한다. Upgrade, close-delimited/불완전 framing, non-HTTP 흐름은 분석 요약에 별도 제외 사유로 집계된다. 캡처의 `Content-Length`가 실제 body와 다르면 불완전 HTTP 흐름으로 제외되므로 Scapy fixture에서도 body byte 길이와 헤더를 일치시켜야 한다.

file payload와 capture artifact는 256 KiB gRPC chunk로 양쪽 Agent에 전송하며, Agent가 offset·전체 크기·SHA-256을 검증한다. payload는 공유 buffer로, capture는 임시파일로 준비한다.

빌드 환경에는 시스템 `protoc`가 없어도 되도록 vendored protoc를 사용한다.
