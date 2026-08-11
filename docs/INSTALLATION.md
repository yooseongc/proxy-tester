# 설치와 설정

## 구성 원칙

Control은 UI, REST/WebSocket, Agent gRPC, SQLite를 함께 제공한다. Client와 Server는 같은 `proxy-agent` 실행 파일이며 `PROXY_TESTER_ROLE=client|server`로 역할을 정한다. Agent ID는 전체 Control 범위에서 고유해야 한다. 운영 환경에서는 실행 파일 이름에 따른 역할 추론에 의존하지 않는다.

`proxy` 서비스는 제품 구성요소가 아니라 로컬 explicit proxy 시험을 위한 fixture다. 실제 배포에서는 측정 대상 proxy를 별도로 준비하고 시나리오의 `proxy_addr` 또는 네트워크의 transparent 경로로 연결한다.

## 빠른 로컬 평가

```powershell
docker compose build
docker compose up -d
```

이 구성은 control, client, server와 테스트용 proxy를 함께 기동한다. 개발용 `dev` 서비스는 `compose.dev.yaml`을 명시했을 때만 사용한다. UI는 기본적으로 `http://localhost:18080`이다.

## 단일 호스트 운영

`.env.example`을 `.env`로 복사하고 이미지 이름과 포트를 지정한 뒤 실행한다.

```powershell
docker compose -f compose.production.yaml --env-file .env up -d
```

운영 Compose에는 fixture proxy와 개발 컨테이너가 없다. SQLite DB, 업로드 artifact, Run 결과는 `proxy-data` 볼륨에 보존한다. 백업 시에는 시험을 중지한 뒤 이 볼륨의 `/data`를 백업한다.

## 분산 Linux 장비

Control 호스트는 TCP 8080(UI/API)과 50051(Agent gRPC)을 수신한다. 외부 Agent를 사용할 때 방화벽에서 필요한 출발지에만 50051을 허용한다. 현재 Agent gRPC는 평문이므로 신뢰할 수 있는 측정망 또는 VPN 내부에서만 사용한다.

각 부하 장비에 musl `proxy-agent`를 `/usr/local/bin/proxy-agent`로 설치하고 `packaging/systemd` 템플릿을 배치한다.

```text
/etc/systemd/system/proxy-tester-agent.service
/etc/proxy-tester/agent.env
/usr/local/bin/proxy-agent
```

Client 예시는 `PROXY_TESTER_ROLE=client`, Server 예시는 `server`로 설정한다. `PROXY_TESTER_CONTROL`에는 Control 장비에서 접근 가능한 주소를 사용한다. 여러 Agent를 설치할 때 `PROXY_TESTER_AGENT_ID`를 중복시키면 마지막 연결이 기존 세션을 대체하므로 반드시 고유하게 정한다.

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now proxy-tester-agent
journalctl -u proxy-tester-agent -f
```

NIC wire 계측을 사용하려면 Agent가 해당 Linux interface의 `/sys/class/net` 통계를 읽을 수 있어야 한다. 고부하 시험 전에는 파일 descriptor 한도, ephemeral port 범위, NIC offload, MTU와 라우팅을 별도로 점검한다.

## 설정 항목

| 구성요소 | 환경변수 | 기본값 | 설명 |
|---|---|---|---|
| Control | `PROXY_TESTER_HTTP_ADDR` | `0.0.0.0:8080` | UI/API listen 주소 |
| Control | `PROXY_TESTER_GRPC_ADDR` | `0.0.0.0:50051` | Agent gRPC listen 주소 |
| Control | `DATABASE_URL` | `sqlite://data/proxy-tester.db?mode=rwc` | SQLite 연결 문자열 |
| Control | `PROXY_TESTER_STATIC_DIR` | `frontend/dist` | CSR 정적 자산 경로 |
| Control | `PROXY_TESTER_ARTIFACT_DIR` | `data/artifacts` | 업로드 artifact 경로 |
| Agent | `PROXY_TESTER_CONTROL` | `http://control:50051` | Control gRPC endpoint |
| Agent | `PROXY_TESTER_AGENT_ID` | 역할 기반 `*-1` | UI와 시나리오에서 사용할 고유 ID |
| Agent | `PROXY_TESTER_ROLE` | 실행 파일명에서 추론 | `client` 또는 `server` |

