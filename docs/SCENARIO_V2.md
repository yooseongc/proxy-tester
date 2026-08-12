# Scenario v2와 방향별 Payload

Scenario v2는 장비 유형 대신 생성할 트래픽을 중심으로 구성한다. `request_payload`는 Client→Server, `response_payload`는 Server→Client 방향이며 TCP payload 또는 HTTP body로 사용된다.

지원 종류는 `empty`, `fixed`, `text`, `file`, `random`이고 random 형식은 `binary`와 `printable_ascii`다. 최대 크기는 방향별 64 MiB다. Agent는 PrepareRun 단계에서 payload를 한 번 생성한 뒤 `Arc<[u8]>`로 worker 사이에 공유한다. 따라서 VU 또는 transaction 반복마다 난수를 만들거나 payload를 복제하지 않는다. 생성 시 원문 대신 크기와 SHA-256을 로그에 남긴다.

기존 version 1 JSON은 control plane과 agent에서 자동으로 version 2로 승격한다. HTTP의 `request_body_bytes`/`response_body_bytes`, TCP의 `tx_bytes`/`rx_bytes`는 각각 v2 fixed payload로 변환된다. 저장된 구성과 기존 Run JSON을 읽을 때에도 같은 변환을 적용한다.

PCAP/PCAPNG 업로드 시 Ethernet IPv4/IPv6 TCP flow를 sequence 기준으로 재조립하고 retransmission, 누락 구간, 비 TCP, fragment, TLS ciphertext를 분류한다. TCP `capture_replay`는 분석된 양방향 평문 flow를 turn 단위로 양쪽 Agent에서 재생하며 flow를 round-robin으로 순환한다. TLS를 선택하면 추출한 평문을 새 TLS 세션에서 재암호화한다.

HTTP/1.1 `capture_replay`는 양방향 stream에서 request/status line과 `Content-Length` 또는 chunked body 경계를 추출하고 같은 연결의 transaction 순서를 유지한다. 캡처의 method, path/query, header, body와 response bytes를 재사용하되 `Host`는 Scenario 값으로 바꾼다. 평문 explicit proxy Client 요청만 현재 target을 사용하는 absolute-form으로 바꾸며, Server는 proxy가 전달하는 origin-form을 사용한다. Upgrade, close-delimited/불완전 framing, non-HTTP 흐름은 분석 요약에 별도 제외 사유로 집계된다. 캡처의 `Content-Length`가 실제 body와 다르면 불완전 HTTP 흐름으로 제외되므로 Scapy fixture에서도 body byte 길이와 헤더를 일치시켜야 한다.

file payload와 capture artifact는 256 KiB gRPC chunk로 양쪽 Agent에 전송하며, Agent가 offset·전체 크기·SHA-256을 검증한다. payload는 공유 buffer로, capture는 임시파일로 준비한다.

빌드 환경에는 시스템 `protoc`가 없어도 되도록 vendored protoc를 사용한다.

## TLS 설정과 오류 분류

`tls.version`은 `tls12` 또는 `tls13`이며 기본값은 `tls13`이다. `tls.cipher_suite`는 `null`이면 해당 버전의 rustls 기본 cipher 순서를 사용한다. 값을 지정하면 선택 버전과 호환되는 TLS 1.2 ECDHE 또는 TLS 1.3 AEAD suite만 허용한다. 인증서 검증은 기본 OFF이며, ON일 때 `ca_pem`과 SNI에 사용하는 `server_name`이 필요하다. responder에는 `server_cert_pem`과 `server_key_pem`을 전달한다.

평문 HTTP explicit proxy는 absolute-form request를 전달하고, TLS 및 TCP explicit proxy는 먼저 CONNECT tunnel을 만든다. 결과 metrics는 기존 `connections_failed`와 `transaction_errors` 외에 `timeout_errors`, `reset_errors`, `tls_handshake_errors`, `proxy_connect_errors`, `http_error_responses`를 제공한다. 이 분류는 장비의 차단 성공을 판정하지 않고 관측된 wire/application 실패 원인만 기록한다.

## Traffic-first UI

설정 화면은 프로토콜, 보안, Payload, 부하 순서로 읽는다. 상단 요약은 현재 선택한 프로토콜과 TLS 버전, 요청·응답의 종류 및 실제 byte 크기 또는 capture 이름, 직접/명시적 Proxy 경로를 즉시 반영한다. `구성 이름`은 재사용할 Scenario profile의 이름이고 `개별 시험 이름`은 한 번의 Run을 이력에서 구분하기 위한 이름이다.

PCAP 모드에서는 선택 프로토콜의 지원 흐름 수가 실행 가능 조건이다. TCP는 `supported_flow_count`, HTTP/1.1은 `http_flow_count`를 사용한다. 0개이면 분석 제외 reason과 함께 차단 안내를 표시한다. endpoint, HTTP Proxy, connect/proxy/response timeout, Wire 계측 인터페이스는 연결 고급 설정에 있고 cipher, SNI, 검증, CA 및 responder 인증서/키는 TLS 고급 설정에 있다.
