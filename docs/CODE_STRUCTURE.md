# 코드 구조와 리팩터링 규칙

## Frontend

`frontend`는 Prettier와 ESLint를 필수 품질 경계로 사용한다.

- `npm run format`: TS/TSX/CSS/설정 파일을 일괄 포맷한다.
- `npm run format:check`: 파일 변경 없이 포맷 일관성을 검사한다.
- `npm run lint`: TypeScript, React Hooks와 JSX 접근성을 검사한다.
- `src/hooks`: API polling과 WebSocket처럼 React lifecycle에 결합된 기능을 둔다.
- `src/model.ts`: API DTO와 telemetry 계산에 공유되는 타입을 둔다.
- 화면 컴포넌트는 네트워크 준비, 트래픽 설정, 실시간 차트와 결과 조회 경계를 넘어서 상태를 직접 공유하지 않는다.

생성물인 `dist`, `test-results`, `tsconfig.tsbuildinfo`는 소스 관리와 포맷 대상에서 제외한다. 새 코드에서 긴 원라인 JSX나 여러 선언을 한 문장에 압축하지 않는다.

## Backend

Rust workspace는 다음 책임 경계를 따른다.

- `domain`: Scenario, network profile과 metrics의 transport 독립 계약
- `proto`: protobuf 생성 타입과 domain↔wire 공통 변환
- `capture`: PCAP/PCAPNG 분석과 TCP/HTTP replay 추출
- `control`: HTTP/gRPC 제어, 실행 orchestration과 저장소
- `agent`: network 준비와 TCP/HTTP workload 실행

Control의 `main`은 bootstrap과 서버 lifecycle을 담당하고, Router/middleware는 `routes`, 공유 상태와 pending command registry는 `state`, SQLite schema lifecycle은 `database`에 둔다. Scenario, Artifact, Run과 metric sample의 영속성 SQL은 `repository` 아래의 리소스별 모듈에 두며 handler는 저장 형식이나 `sqlx::Row`를 알지 못해야 한다. API 오류는 `error::ApiError`를 통해서만 HTTP 응답으로 변환한다.

Agent의 `main`은 control stream과 workload orchestration을 담당한다. artifact chunk 수신과 무결성 검증은 `artifact`, Run별 payload 준비는 `payload`, 누적 카운터·오류 분류·활성 연결 시간가중 집계는 `telemetry`에 둔다. Linux namespace 변경은 `network`, 인증서 검증·cipher·TLS version·ALPN 정책은 `tls` 모듈에 둔다. client와 responder는 같은 TLS helper를 사용해야 한다.

## 변경 규칙

- REST path, protobuf field number, Scenario v4 JSON과 DB schema 변경은 별도 호환성 작업으로 취급한다.
- domain↔protobuf 변환을 control이나 agent에 다시 구현하지 않는다.
- handler에 새 SQL을 추가하기 전에 database/repository 경계에서 재사용 가능한지 확인한다.
- repository는 저장소 레코드를 반환하고, redaction과 API JSON 구성은 handler 경계에서 수행한다.
- 고빈도 계측 경로의 atomic counter는 `Relaxed` ordering을 사용하되, 연결 수의 시간가중 snapshot처럼 여러 값을 일관되게 바꾸는 상태는 전용 lock 안에서 갱신한다.
- 비동기 작업은 오류를 무시하지 않고 API error, command ACK 또는 agent event 중 하나로 전달한다.
- 각 구조 변경은 `cargo fmt`, clippy `-D warnings`, workspace test, frontend format/lint/typecheck/Vitest를 통과한 뒤 커밋한다.
