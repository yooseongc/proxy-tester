# 설치와 운영 설정

## 구성 원칙

Control은 UI, REST/WebSocket, agent gRPC와 SQLite를 제공합니다. 모든 노드는 동일한 `proxy-agent` 실행 파일을 사용하며 agent 자체에 client/server 역할을 고정하지 않습니다. 실행할 Scenario와 준비된 network profile revision이 각 endpoint 역할을 결정합니다.

Agent ID는 Control 범위에서 고유해야 합니다. 같은 ID로 새 agent가 연결되면 기존 세션을 대체하므로 장비 교체를 제외하고 ID를 재사용하지 마세요.

## Docker 개발 환경

```powershell
docker compose build
docker compose up -d
Invoke-RestMethod http://localhost:18080/api/health
Invoke-RestMethod http://localhost:18080/api/agents
```

기본 compose의 `proxy`는 explicit proxy 기능 시험용 fixture이며 운영 구성 요소가 아닙니다. Docker Desktop은 기능 검증에만 사용하고 실제 처리량은 Linux 장비에서 측정하세요.

### Docker managed-direct 기능 시험

`compose.managed-direct.yaml`을 추가하면 Client와 Server에 내부 L2 test-plane을 연결합니다. bootstrap은 Docker가 시험 NIC에 할당한 주소를 Agent 시작 전에 제거하므로, Agent inventory에는 관리 route를 가진 `eth0`와 주소가 없는 `eth1`이 구분되어 나타납니다.

```powershell
docker compose -f compose.yaml -f compose.managed-direct.yaml build
docker compose -f compose.yaml -f compose.managed-direct.yaml up -d
```

managed-direct 환경을 시작한 뒤 일부 서비스만 빌드하거나 재시작할 때도 두 Compose 파일을 항상 함께 지정합니다. override를 빼면 Client/Server가 test-plane 없는 기본 구성으로 재생성될 수 있습니다. Proxy fixture만 갱신할 때는 의존 Agent를 건드리지 않도록 다음처럼 실행합니다.

```powershell
docker compose -f compose.yaml -f compose.managed-direct.yaml build proxy
docker compose -f compose.yaml -f compose.managed-direct.yaml up -d --no-deps proxy
```

UI의 네트워크 프로파일은 다음 값으로 준비합니다.

- Client: `client-1`, `eth1`, `172.31.0.10/24`, 주소 수 `1`
- Server: `server-1`, `eth1`, `172.31.0.20/24`, 주소 수 `1`

Docker bridge는 IPAM에 등록되지 않은 주소의 forwarding을 제한하므로 이 구성은 단일 주소의 plan/apply/run/teardown 기능 검증용입니다. 여러 source IP, 실제 inline 장비 경유와 처리량 측정에는 별도 물리 NIC 또는 Docker 외부에서 구성한 전용 bridge를 사용합니다. 로컬 Docker 주소 풀과 `172.31.0.0/24`가 충돌하면 override의 subnet, 고정 주소와 `PROXY_TESTER_TEST_IPV4_PREFIX`를 함께 변경해야 합니다.

Docker 회귀 스크립트는 Scenario v4만 전송합니다. managed-direct 경로가 포함된 스크립트에는 UI/API에서 apply한 revision ID를 `-ProfileRevisionId`로 전달합니다. explicit proxy endpoint 주소는 컨테이너 재생성 때 달라질 수 있으므로 스크립트가 Agent inventory에서 node ID별 `eth0` 주소를 조회합니다.

## 단일 호스트 운영

`.env.example`을 `.env`로 복사해 image와 포트를 지정한 뒤 실행합니다.

```powershell
docker compose -f compose.production.yaml --env-file .env up -d
```

SQLite, artifact와 결과는 `proxy-data` volume에 보존됩니다. 백업할 때는 활성 Run을 중지한 뒤 volume의 `/data` 전체를 함께 백업하세요.

## 분산 Linux agent

Rust 1.93 musl release binary를 각 부하 발생 장비의 `/usr/local/bin/proxy-agent`에 설치합니다. systemd template은 `packaging/systemd`에 있습니다.

```text
/etc/systemd/system/proxy-tester-agent.service
/etc/proxy-tester/agent.env
/usr/local/bin/proxy-agent
```

Agent는 network namespace와 주소를 준비하므로 `CAP_NET_ADMIN`이 필요합니다. wire counter와 namespace 준비에 사용할 시험 전용 NIC를 마련하고 관리 NIC는 보호 목록에 유지하세요.

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now proxy-tester-agent
journalctl -u proxy-tester-agent -f
```

Control의 TCP 50051을 필요한 agent 주소에서만 허용하세요. 현재 gRPC transport 자체 인증은 제공하지 않으므로 신뢰된 측정망 또는 VPN 안에서 운영해야 합니다.

## 환경 변수

| 구성 요소 | 환경 변수 | 기본값 | 설명 |
|---|---|---|---|
| Control | `PROXY_TESTER_HTTP_ADDR` | `0.0.0.0:8080` | UI/API listen 주소 |
| Control | `PROXY_TESTER_GRPC_ADDR` | `0.0.0.0:50051` | agent gRPC listen 주소 |
| Control | `DATABASE_URL` | `sqlite://data/proxy-tester.db?mode=rwc` | SQLite 연결 문자열 |
| Control | `PROXY_TESTER_STATIC_DIR` | `frontend/dist` | CSR 정적 자산 경로 |
| Control | `PROXY_TESTER_ARTIFACT_DIR` | `data/artifacts` | 업로드 artifact 경로 |
| Control | `PROXY_TESTER_RETENTION_DAYS` | `90` | 완료 Run 보존 일수, 0 이하는 자동 정리 중지 |
| Control | `PROXY_TESTER_AGENT_GRACE_SECS` | `10` | agent 단절 후 Run 실패까지 유예 시간 |
| Control | `PROXY_TESTER_COMMAND_TIMEOUT_SECS` | `10` | agent 명령 ACK 제한 시간 |
| Agent | `PROXY_TESTER_CONTROL` | `http://control:50051` | Control gRPC endpoint |
| Agent | `PROXY_TESTER_NODE_ID` | hostname 기반 | Control 전체에서 고유한 node ID |
| Agent | `PROXY_TESTER_PROTECTED_INTERFACES` | route 기반 감지 | 변경을 금지할 관리 interface 목록 |

역할을 정하는 `PROXY_TESTER_ROLE`은 사용하지 않습니다. 서비스 파일과 compose에서도 node ID만 지정합니다.

## 설치 확인과 문제 해결

1. `/api/health`에서 `schema_version=4`와 실제 `database_url`을 확인합니다.
2. `/api/agents`에서 모든 node가 online이고 inventory에 시험 NIC가 있는지 확인합니다.
3. UI에서 network profile을 Plan한 뒤 예상 namespace, 주소와 rollback 명령을 검토합니다.
4. Apply 후 Diagnose를 실행하고 실패하면 network audit와 agent journal을 함께 확인합니다.
5. 준비 중 중단된 노드는 Reconcile 또는 Teardown으로 복구한 뒤 다시 Plan합니다.

NIC가 보이지 않으면 권한, `/sys/class/net`, protected interface 설정을 확인하세요. 처리량이 불안정하면 file descriptor, ephemeral port 범위, MTU와 NIC offload를 점검하세요. 네트워크 변경·rollback 상세는 [NETWORK_CONFIGURATION.md](NETWORK_CONFIGURATION.md), 저장소 복구는 [STORAGE.md](STORAGE.md), 계측 해석은 [TELEMETRY.md](TELEMETRY.md)를 참고하세요.
