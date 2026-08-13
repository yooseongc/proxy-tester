# Proxy Tester

Managed Linux interface/namespace setup, rollback, diagnostics, and recovery: [Network configuration](docs/NETWORK_CONFIGURATION.md).

분산 TCP client/server agent로 transparent 또는 explicit proxy의 CPS, bandwidth, PPS 및 HTTP TPS를 측정하는 Rust 기반 도구입니다. Control plane은 React UI와 SQLite 결과 저장소를 제공합니다.

## Docker로 실행

Windows 개발환경에서는 Docker Desktop의 Linux container를 사용합니다.

```powershell
docker compose build
docker compose up -d
```

브라우저에서 `http://localhost:18080`을 엽니다. 다른 포트는 `PROXY_TESTER_PORT` 환경변수로 지정할 수 있습니다.

기본 Compose 토폴로지는 다음 서비스를 실행합니다.

- `control`: REST/WebSocket UI와 agent gRPC endpoint
- `client`: 부하 발생 agent
- `server`: TCP/HTTP responder agent
- `proxy`: HTTP forward/CONNECT 통합 테스트 fixture

Transparent 시험의 기본 target은 `server:8080`입니다. Explicit 시험은 proxy 주소를 `proxy:3128`로 설정합니다.

## 개발 및 검증

```powershell
docker compose -f compose.dev.yaml build
docker compose -f compose.dev.yaml run --rm dev cargo test --workspace
docker compose -f compose.dev.yaml run --rm dev cargo clippy --workspace --all-targets -- -D warnings
docker compose -f compose.dev.yaml run --rm dev cargo fmt --all -- --check
docker compose -f compose.dev.yaml run --rm dev bash -lc "cd frontend && npm install && npm run build"
```

추가 회귀검증:

```powershell
.\tests\scenario-matrix.ps1
.\tests\lifecycle-regression.ps1
.\tests\tls-wire-regression.ps1 -CertificatePath <server-cert.pem> -PrivateKeyPath <server-key.pem> -CaCertificatePath <ca-cert.pem>
cd frontend
npm test
npm run test:e2e
```

릴리스 이미지는 Rust 1.93의 `x86_64-unknown-linux-musl` 정적 바이너리를 사용합니다. Docker Desktop은 기능 검증용이며 물리 NIC PPS, offload, 10–25GbE 성능은 실제 Linux 장비에서 검증해야 합니다.

## 부하 모델

- `virtual_clients`: 동시에 유지할 가상 클라이언트(TCP 연결) 수
- CPS/TPS는 목표값이 아니라 1초 구간별 실측 결과
- HTTP `transactions_per_connection = 0`: 시험 종료까지 같은 keep-alive 연결에서 transaction 반복
- HTTP Request/Response body 크기를 각각 설정하여 TX/RX 대역폭 방향을 조절

## API

- `GET /api/health`, `GET /api/agents`
- `POST /api/preflight`
- `GET|POST /api/scenarios`
- `GET|POST /api/runs`, `POST /api/runs/{id}/pause|resume|stop`
- `GET /api/runs/{id}`
- `GET /api/runs/page`, `GET /api/runs/{id}/samples`, `GET /api/runs/{id}/export`
- `GET|POST /api/artifacts?kind=payload|pcap` (`multipart/form-data`의 `file` 필드, 각각 최대 64 MiB/512 MiB)
- `GET /api/events/ws`

PCAP/PCAPNG 업로드는 형식, record 완결성, packet/byte 수와 SHA-256을 검증·저장합니다. 현재 자동 실행 엔진은 합성 TCP/HTTP/CONNECT workload를 사용합니다.

## TLS, wire 계측 및 결과

HTTP/2는 TLS+ALPN `h2` 전용이며 직접 연결과 HTTP CONNECT를 지원한다. 연결당 동시 stream 수를 설정할 수 있고 plaintext HTTP/2 Capture의 HPACK/frame을 의미 단위 transaction으로 재현한다. 자세한 계약은 [HTTP/2 문서](docs/HTTP2.md)를 참고한다.

- TLS server certificate/private key PEM을 사용해 HTTPS responder를 실행합니다.
- 인증서 검증 ON은 CA PEM을 사용하며, OFF는 명시적으로 peer 검증을 생략합니다.
- 명시적 프록시와 TLS 조합에서는 HTTP 요청 전에 CONNECT tunnel을 자동으로 생성합니다.
- observation interface를 지정하면 client/server Linux NIC의 wire B/W, PPS와 TCP retransmission을 별도로 저장합니다.
- 시험 이력에서 최대 2개 run을 비교하고 원본 sample을 CSV 또는 JSON으로 내보낼 수 있습니다.

Scenario 입력은 v4만 지원하며 이전 버전과 legacy topology 필드는 거부합니다. 자세한 계약은 [Scenario v4](docs/SCENARIO_V4.md)를 참고하십시오.

## UI 테마와 다단계 부하

- UI는 `시험 구성`, `실시간 모니터링`, `결과` 탭으로 분리되며 시험 시작 후 실시간 탭으로 이동합니다.
- Light/Dark 테마는 최초 OS 설정을 따르고 이후 선택을 브라우저에 저장합니다.
- Load Stage마다 Ramp 또는 Hold, 지속시간, 목표 가상 클라이언트 수, 결과 집계 포함 여부를 설정할 수 있습니다.
- 새 시험의 기본 Stage는 Ramp-up 10초, Steady state 30초, Ramp-down 10초이며 시험 구성은 UI에서 저장하고 다시 불러올 수 있습니다.
- 구성 이름은 재사용 프로파일을 식별하고, 시험 이름은 개별 Run을 식별한다. 시험 이름을 비워 두면 서버가 `구성 이름 · 실제 시작 시각`으로 자동 생성한다.
- TLS 화면은 PEM 직접 입력과 파일 선택을 지원하며, `테스트 인증서 자동 생성`으로 30일짜리 로컬 CA 및 서버 인증서를 만들 수 있습니다.
- 자동 생성된 개인키는 시나리오 재실행을 위해 DB에 저장되지만 실행 결과 API에서는 마스킹됩니다.

```powershell
.\tests\stage-regression.ps1
.\tests\auto-certificate-regression.ps1
```
- UI의 TX/RX는 TCP/IP wire bytes가 아니라 각 client/server endpoint에서 처리한 application bytes입니다.
- 명시적 프록시는 CONNECT handshake 및 HTTP URI/header를 변환하므로 Client TX와 Server RX가 정확히 같지 않을 수 있습니다.
- 실제 NIC PPS/offload 및 고속 링크 성능은 Docker Desktop이 아닌 Linux 장비에서 검증해야 합니다.
# 실시간·결과 시계열 차트

운영 대시보드와 결과 상세는 동일한 `TelemetryCharts` 보드를 사용한다. Client의 1초 원본 표본을 기준으로 가장 가까운 Server 표본(최대 1.5초 차이)을 결합하며 보간과 이동평균은 적용하지 않는다. 실시간 표본은 Client/Server 합계 7,200개까지만 보관하고, 완료 Run은 상세 API가 반환한 전체 표본을 표시한다.

- `부하·연결`: CPS/TPS와 목표 VU/Active connections의 독립 축
- ACTIVE는 `현재 / 최근 1분 최대`로 표시하며, 정의와 해석은 [계측 지표 문서](docs/TELEMETRY.md)를 참고한다.
- 실시간·결과 차트 상단의 바로가기 도구 모음에서 원하는 지표 차트로 즉시 이동할 수 있다.
- `처리량`: Client/Server App/Wire TX/RX. Wire는 점선이며 값에 따라 Kbps/Mbps/Gbps로 표시
- `Latency`: TCP connect와 HTTP response P50/P95/P99, 소수점 둘째 자리 ms
- `품질`: connection failure, HTTP transaction error, TCP retransmission의 초당 값

누적 오류 counter는 실제 Client 표본 간격으로 나누어 초당 값으로 변환한다. Agent 재시작이나 Run 전환으로 counter가 감소하면 해당 변화량은 0이다. Stage는 설정 순서대로 누적 시간을 계산해 모든 차트에 밴드와 경계선으로 표시하며, 집계 제외 Stage는 흐린 밴드와 `집계 제외` 문구로 구분한다.

결과 요약의 연결·Transaction·최대 CPS/TPS/B/W/Latency는 Stage 포함 여부와 무관하게 Run 전체 표본을 집계한다. App B/W는 애플리케이션 송수신량이며, Wire B/W는 설정한 관찰 인터페이스의 커널 통계다. 관찰 인터페이스를 지정하지 않으면 Wire B/W는 0이고 App B/W만 사용할 수 있다.

네 차트는 Recharts `syncId`로 동일 시각의 cursor/tooltip을 공유한다. 어느 차트를 수평 스크롤해도 나머지 차트가 같은 위치로 이동한다. 실행 중 오른쪽 끝에서는 새 표본을 자동 추적하며, 끝에서 48px 이상 벗어나면 `과거 구간 확인 중`으로 전환된다. `최신으로`를 누르면 자동 추적을 재개한다. Run이 끝나면 현재 위치를 유지한다. 범례 선택은 `sessionStorage`에 저장되어 현재 브라우저 세션 동안 실시간과 결과 화면에 공통 적용된다.

## 차트 검증 및 트러블슈팅

## Technical Console UI

Frontend는 Tailwind CSS v4 하이브리드 구조를 사용한다. 레이아웃, 간격, 반응형과 상태 표현은 Tailwind utility로 작성하고 `frontend/src/styles.css`에는 의미 기반 theme token, 로컬 폰트, ECharts와 브라우저 전역 스타일만 둔다. 색상을 직접 지정하지 않고 `canvas`, `panel`, `raised`, `line`, `ink`, `dim`, `signal`, `info`, `warn`, `critical` token을 사용한다.

Pretendard Variable 한글 dynamic subset과 JetBrains Mono Variable을 빌드 결과에 포함하므로 폐쇄망에서도 동일하게 표시된다. 일반 문장은 Pretendard, 계측값·시간·주소·ID는 JetBrains Mono와 tabular number를 사용한다.

시계열은 Apache ECharts Canvas renderer로 그린다. 네 차트는 같은 ECharts group으로 연결되어 cursor와 data zoom을 공유하며, 하단 Time navigator가 전체 Run과 현재 범위를 표시한다. 실시간 화면은 최신 60초를 자동 추적하고 사용자가 drag, wheel 또는 navigator로 과거를 탐색하면 추적을 중지한다. 외부 React 범례는 키보드 조작과 `sessionStorage` 기반 표시 상태를 담당한다. 새로운 series는 `TelemetryCharts.tsx`의 정의에 key, 이름, 색상 token, 축과 formatter를 추가한다.

`npm run test:e2e`의 기본 `localhost:18080` 대상은 실행 중인 control 바이너리에 빌드 시점의 정적 자산이 내장되어 있다. UI 변경 E2E 전에는 최신 `frontend/dist`를 포함하도록 control을 다시 빌드·기동해야 한다.

`cd frontend` 후 `npm test`, `npm run typecheck`, `npm run build`로 데이터 파생 로직과 프로덕션 번들을 검증한다. 차트가 비어 있으면 먼저 Run 상세 응답의 `samples`에 role 1(Client) 표본이 있는지 확인한다. Server 선만 0이면 Client와 Server의 `unix_ms` 차이가 1.5초를 넘는지 확인한다. 오류율이 순간적으로 0이 되는 것은 counter 초기화 방어 동작일 수 있으며 다음 증가 표본부터 정상 계산된다. 브라우저 세션의 숨김 범례를 초기화하려면 개발자 도구에서 `sessionStorage`의 `telemetry-hidden-series` 항목을 삭제한다.
