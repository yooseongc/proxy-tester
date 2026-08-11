# 전체 구현 로드맵

각 마일스톤은 독립적으로 실행 가능한 상태에서 완료하고, 구현·테스트·문서를 함께 커밋한다. `main`에는 테스트를 통과한 커밋만 남긴다.

## M0 — 기준선과 Scenario v2 핵심 (완료)

- Git 저장소와 재현 가능한 Rust/Frontend 빌드
- v1 → v2 자동 변환
- 방향별 empty/fixed/text/random 모델과 64 MiB 제한
- Run 준비 시 불변 payload 생성 및 worker 공유
- TCP/HTTP 실제 payload 송수신과 기본 UI

완료 조건: workspace test, Vitest, typecheck, production build 통과.

## M1 — Artifact 계약과 안전한 전송

- Artifact kind를 `payload`/`pcap`으로 분리하고 각각 64 MiB/512 MiB 제한
- 업로드 streaming, SHA-256, 중복 제거, 원자적 저장
- gRPC metadata + chunk stream, offset/length/SHA-256 무결성 검사
- Agent Run 준비 시 필요한 artifact만 내려받아 공유 buffer 또는 임시 파일로 준비
- file payload 실행 및 결과 JSON redaction

완료 조건: 손상·누락·크기 초과 chunk가 traffic 시작 전에 실패하고 서로 다른 요청/응답 파일의 byte 정확성이 통합시험에서 검증됨.

## M2 — Scapy 기반 복호화 PCAP fixture와 TCP 재조립

- 고정 seed의 Ethernet IPv4/IPv6 PCAP fixture 생성기
- 정상 양방향 흐름, retransmission, overlap, out-of-order, 방향 전환 fixture
- fragmented IP, UDP, truncated flow, TLS ciphertext 제외 fixture
- 5-tuple flow 분리, sequence 기준 재조립, retransmission 중복 제거
- 지원/제외 reason code와 flow/packet 수 분석 결과

완료 조건: fixture를 매번 동일하게 생성하고 Rust parser golden test가 모든 포함/제외 수와 byte stream을 검증함.

## M3 — TCP capture replay 실행기 (완료)

- 재조립 stream을 방향 전환 기준 turn으로 변환
- VU별 flow template round-robin scheduler
- VU가 flow보다 많거나 적은 경계조건
- 원본 timing을 제거하고 turn 순서만 보존하여 최대속도 반복
- 평문 payload를 선택한 새 TLS 세션에서 재암호화

구현 완료: turn 생성, gRPC capture 전송, Agent 임시파일 저장·재분석, flow round-robin, 첫 client turn 기반 responder 매칭, 직접/TLS/CONNECT 실행 경로.

검증: `tests/capture-replay-regression.ps1`가 Scapy 다중 flow fixture를 업로드한 뒤 Docker Compose의 실제 Control/Client/Server/Proxy 프로세스에서 직접 연결, HTTP CONNECT, TLS 재암호화 경로를 차례로 실행한다. 각 Run의 완료 상태, 0건의 client transaction error, 실제 transaction 발생 및 직접 연결의 endpoint byte 대칭을 확인한다.

완료 조건: 다중 flow fixture를 직접/TLS/CONNECT 경로에서 재현하고 client/server transcript가 원본 turn과 일치함.

## M4 — HTTP/1.1 capture replay (완료)

- request/status line, Content-Length, chunked body, keep-alive 메시지 경계 추출
- capture endpoint를 현재 target/Host 정책에 맞게 재작성
- request/response template transaction replay
- 불완전·upgrade·암호화 메시지의 명시적 제외

구현 완료: 재조립된 양방향 stream에서 `Content-Length`와 chunked framing을 포함한 HTTP/1.1 메시지를 추출하고 request/response transaction으로 대응시킨다. 캡처의 method·path·body는 유지하면서 현재 Host를 적용하고, 평문 forward proxy Client에서만 absolute-form target으로 변환한다. Agent는 응답마다 TPS와 HTTP latency를 계측한다.

검증: `tests/http-capture-replay-regression.ps1`가 2개 keep-alive transaction을 담은 Scapy fixture를 업로드하고 직접 연결, 평문 HTTP forward proxy, TLS-over-CONNECT 경로를 실제 Compose 프로세스에서 반복 실행한다. 완료 상태, transaction 발생, HTTP latency, client 오류 0건과 직접 연결 endpoint byte 대칭을 확인한다.

완료 조건: 다중 transaction/connection fixture가 메시지 단위로 재현되고 TPS·오류 계측이 일치함.

## M5 — TLS와 연결 경로 완성

- TLS 1.2/1.3 선택과 rustls protocol version 제한
- cipher suite/version 호환 validation
- 검증 기본 OFF, SNI/CA/responder cert/key 고급 설정
- 직접 연결, HTTP forward proxy, CONNECT 통합시험
- reset/timeout/HTTP 오류 응답 분류

완료 조건: protocol/version/path 조합 matrix와 인증서 검증 on/off 회귀시험 통과.

## M6 — Traffic-first UI와 분석 UX

- `프로토콜 → 보안 → Payload → 부하` 구성 순서
- 현재 선택 한 줄 요약과 구성 이름/Run 이름 분리
- payload artifact upload/선택과 PCAP 분석 진행상태
- 지원 flow 및 제외 reason 요약, 실행 차단 사유 표시
- 고급 설정 접기, 반응형 overflow, 접근성

완료 조건: Playwright에서 조건부 필드, 독립 payload, 분석 상태, 고급 설정, mobile layout을 검증함.

## M7 — 결과 메타데이터와 운영 완성도

- 결과에 방향/kind/size/artifact ID 또는 random SHA-256만 저장
- App/Wire B/W 정책 및 CPS/TPS/pps 회귀 검증
- SQLite migration과 PostgreSQL repository 경계 문서화
- musl release build, Docker/systemd lifecycle, retention/cleanup
- 설치·운영·트러블슈팅·성능 기준 문서 완성

완료 조건: 전체 자동시험, musl build, lifecycle regression과 clean-install smoke test 통과.

## 커밋 규칙

- 한 커밋은 한 계약 변경 또는 수직 기능 단위로 제한한다.
- schema/proto 변경에는 같은 커밋에 migration과 호환성 테스트를 포함한다.
- `feat:`, `fix:`, `test:`, `docs:`, `build:` 접두사를 사용한다.
- 각 마일스톤 마지막 커밋은 문서의 완료 조건과 실제 검증 명령을 갱신한다.
