# Proxy Tester

Proxy Tester는 분산 TCP client/server agent와 웹 기반 control plane으로 프록시 성능을 측정하는 도구입니다. 직접 연결(인라인 투명 프록시·passive mirror)과 명시적 HTTP Proxy/CONNECT 경로에서 TCP CPS·대역폭·PPS 및 HTTP TPS를 계측합니다.

## 구성 요소

- `proxy-agent`: client와 responder 역할을 실행하고 Linux network namespace를 준비합니다.
- `proxy-control`: REST/WebSocket UI, agent gRPC 제어, SQLite 저장소를 제공합니다.
- `frontend`: CSR 기반 React UI입니다.

Control은 역할이 고정된 agent를 가정하지 않습니다. Scenario v4가 선택한 network profile revision을 기준으로 각 노드의 client/server endpoint를 결정합니다. 인라인 장비와 passive mirror는 도구 관점에서 동일한 직접 Client→Server 트래픽이며, 실제 브리지·TAP/SPAN 배치는 외부 환경에서 구성합니다.

## 빠른 시작

운영 환경은 GitHub Release의 통합 `tar.gz`, `deb` 또는 `rpm` 패키지로 설치합니다. Control과 Agent 설치 예시는 [설치 문서](docs/INSTALLATION.md)를 참고하세요.

Docker는 Windows를 포함한 개발 및 회귀시험 환경에서만 사용합니다.

```powershell
docker compose -f docker/compose.yaml -f docker/compose.managed-direct.yaml build
docker compose -f docker/compose.yaml -f docker/compose.managed-direct.yaml up -d
```

이 구성은 Control 통신용 `eth0`와 주소를 제거한 시험용 `eth1`을 분리합니다. 네트워크 프로파일에서 Client는 `172.31.0.10/24`, Server는 `172.31.0.20/24`, 주소 수는 각각 `1`, 인터페이스는 양쪽 모두 `eth1`로 설정합니다. Docker bridge의 IPAM 필터 때문에 이 기능 시험에서는 예약되지 않은 추가 source IP를 사용할 수 없습니다. 실제 다중 IP 풀과 성능 측정은 전용 Linux 시험 NIC에서 수행하세요.

기본 compose에는 다음 서비스가 포함됩니다.

- `control`: UI/API와 agent gRPC endpoint
- `client`, `server`: 분산 agent fixture
- `proxy`: HTTP forward/CONNECT 통합 시험 fixture

실제 관리형 직접 연결 시험을 시작하려면 먼저 UI의 네트워크 준비 화면에서 노드·전용 NIC·주소 풀을 선택하고 Plan→Apply→Diagnose를 완료해야 합니다. 관리 NIC는 시험 인터페이스로 사용할 수 없습니다. 상세 절차와 복구 방법은 [네트워크 구성](docs/NETWORK_CONFIGURATION.md)을 참고하세요.

## 트래픽 시나리오

입력 계약은 Scenario v4만 지원합니다. 이전 버전 필드나 알 수 없는 필드는 오류로 거부합니다.

- 프로토콜: TCP, HTTP/1.1, TLS 기반 HTTP/2(h2c 제외)
- 보안: 평문 또는 TLS 1.2/1.3, 인증서 검증 선택
- 경로: 준비된 직접 연결 또는 명시적 HTTP Proxy/CONNECT
- Payload: 방향별 empty, fixed, UTF-8 text, artifact file, random
- 재현: 평문 PCAP/PCAPNG의 양방향 TCP/HTTP 세션
- 부하: 가상 연결 수, Stage ramp/hold, keep-alive transaction 수

Random payload는 Run 준비 단계에서 방향별 한 번만 생성해 worker가 공유하며 결과에는 크기·형식·SHA-256만 저장합니다. 업로드 payload는 64 MiB, PCAP/PCAPNG는 512 MiB까지 허용합니다. 암호화된 TLS ciphertext는 복호화하지 않으며, 평문으로 추출한 payload에 TLS를 선택하면 새 TLS 세션에서 다시 암호화합니다. 자세한 계약은 [Scenario v4](docs/SCENARIO_V4.md), [HTTP/2](docs/HTTP2.md)를 참고하세요.

## 개발 및 검증

요구 도구는 Rust 1.93, musl target, Node.js/npm, Docker입니다. 빠른 로컬 검증은 다음 명령 하나로 실행합니다.

```powershell
.\tests\verify.ps1 -Mode Fast
```

Fast 모드는 Rust fmt/clippy/unit test와 frontend typecheck/Vitest를 실행합니다. release musl 빌드, compose 구문 검사, frontend production build와 Playwright까지 확인하려면 다음을 사용합니다.

```powershell
.\tests\verify.ps1 -Mode Full
```

개별 명령은 다음과 같습니다.

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd frontend
npm run typecheck
npm test
npm run build
```

Playwright는 `localhost:18080`에서 최신 frontend bundle을 제공하는 control 프로세스가 필요합니다. 기존 실행 인스턴스를 대상으로 오래된 정적 자산을 검사하지 않도록 E2E 전에 frontend를 빌드하고 control을 다시 시작하세요.

## 저장소와 호환성

기본 저장소는 SQLite입니다. schema v4가 아닌 기존 DB는 삭제하거나 수정하지 않고 `<원래이름>.schema-4.db`를 새로 사용합니다. `GET /api/health`의 `database_url`, `database_fallback`, `schema_version`으로 실제 선택된 DB를 확인할 수 있습니다. 보존 정책과 향후 PostgreSQL 확장 경계는 [저장소](docs/STORAGE.md)에 정리되어 있습니다.

API 오류는 언어에 독립적인 영문 `code`와 사용자 표시용 `message`를 포함합니다.

```json
{ "code": "invalid_scenario", "message": "..." }
```

## 운영 범위와 주의사항

- DLP 탐지·차단 성공 여부와 장비 로그는 자동 판정하지 않습니다. reset, timeout, HTTP 오류는 연결/transaction 오류 지표로 기록합니다.
- FTP/SMTP, UDP, fragmented 또는 불완전 흐름, L2 원본 packet replay, TLS key-log 복호화, HTTP/3는 지원 범위 밖입니다.
- Docker Desktop은 기능 시험용입니다. 실제 PPS, offload, 고속 링크 성능은 전용 NIC가 있는 Linux 장비에서 검증하세요.
- Agent의 network namespace 변경에는 `CAP_NET_ADMIN`이 필요합니다. 서비스 설치와 권한은 [설치](docs/INSTALLATION.md), 패키징은 [패키징](docs/PACKAGING.md)을 참고하세요.
- App B/W는 애플리케이션 bytes, Wire B/W/PPS는 지정한 observation interface의 커널 통계입니다. 해석 기준은 [계측](docs/TELEMETRY.md)을 참고하세요.

## 주요 API

- 상태: `GET /api/health`, `GET /api/agents`
- 네트워크: `/api/network/profiles`, `/plan`, `/apply`, `/diagnose`, `/teardown`
- 시나리오: `GET|POST /api/scenarios`, `POST /api/scenarios/validate`
- 실행: `POST /api/preflight`, `GET|POST /api/runs`, pause/resume/stop
- 결과: run detail/summary/samples/export 및 `GET /api/events/ws`
- Artifact: `GET|POST /api/artifacts?kind=payload|pcap`

전체 설계 진행 상황은 [로드맵](docs/ROADMAP.md), 분산 제어 동작은 [분산 제어](docs/DISTRIBUTED_CONTROL.md), 모듈 책임과 리팩터링 규칙은 [코드 구조](docs/CODE_STRUCTURE.md)에 기록되어 있습니다.
