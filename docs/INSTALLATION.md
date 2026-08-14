# 설치와 운영 설정

## 지원 환경과 구성 원칙

운영 배포는 x86_64 systemd Linux의 네이티브 패키지만 지원합니다. 검증 대상은 Debian 12, Ubuntu 22.04 이상과 RHEL/Rocky Linux 9 계열입니다. Docker 구성은 개발 및 회귀시험 전용이며 운영 배포 계약이 아닙니다.

Control은 UI, REST/WebSocket, agent gRPC와 SQLite를 제공합니다. 모든 노드는 동일한 `proxy-agent`를 사용하며 Scenario와 준비된 network profile revision이 client/server 역할을 결정합니다. Agent ID는 Control 범위에서 고유해야 합니다.

## tar.gz 설치

릴리스의 `SHA256SUMS`를 먼저 검증한 뒤 압축을 해제합니다.

```bash
sha256sum -c SHA256SUMS
tar -xzf proxy-tester-0.1.1-x86_64-linux-musl.tar.gz
cd proxy-tester-0.1.1
```

Control 호스트는 다음처럼 설치합니다.

```bash
sudo ./install.sh --component control
sudo vi /etc/proxy-tester/control.env
sudo systemctl enable --now proxy-tester-control
```

각 부하 발생 호스트의 Agent는 Control endpoint와 고유 node ID를 지정합니다.

```bash
sudo ./install.sh --component agent \
  --control-url http://CONTROL_HOST:50051 \
  --node-id node-site-a
sudo vi /etc/proxy-tester/agent.env
sudo systemctl enable --now proxy-tester-agent
```

Agent 도구가 없으면 installer는 누락 목록을 출력하고 중단합니다. apt/dnf로 자동 설치하려는 경우에만 `--install-deps`를 추가합니다. Installer는 새 설치와 업그레이드 모두 서비스를 자동 시작하거나 재시작하지 않습니다.

## deb/rpm 설치

통합 OS 패키지는 Control과 Agent 바이너리 및 unit을 함께 설치합니다. 필요한 service의 설정만 생성한 뒤 수동으로 활성화합니다.

```bash
# Debian/Ubuntu
sudo apt install ./proxy-tester_0.1.1_amd64.deb

# RHEL/Rocky
sudo dnf install ./proxy-tester-0.1.1-1.x86_64.rpm

# 필요한 구성 요소 설정
sudo proxy-tester-configure control
sudo proxy-tester-configure agent \
  --control-url http://CONTROL_HOST:50051 \
  --node-id node-site-a
```

기존 `/etc/proxy-tester/*.env`는 tar installer, deb conffile 및 rpm `%config(noreplace)` 정책으로 보존합니다.

## 파일과 권한

| 경로 | 용도 |
|---|---|
| `/usr/bin/proxy-control` | Control 실행 파일 |
| `/usr/bin/proxy-agent` | Agent 실행 파일 |
| `/usr/share/proxy-tester/frontend/dist` | CSR UI |
| `/etc/proxy-tester` | 운영 설정 |
| `/var/lib/proxy-tester` | SQLite, artifact, network journal |
| `/usr/lib/systemd/system` | systemd unit |

Control은 `proxy-tester` system user로 실행합니다. Agent는 network namespace와 주소를 준비하기 위해 root로 실행하되 unit의 capability bounding set을 `CAP_NET_ADMIN`, `CAP_NET_RAW`, `CAP_SYS_ADMIN`으로 제한합니다. 시험 전용 NIC를 마련하고 default route가 있는 관리 NIC는 network profile에서 선택하지 마세요.

## 업그레이드와 제거

업그레이드 전에 활성 Run을 종료하고 `/var/lib/proxy-tester` 전체를 백업합니다. 새 package는 바이너리와 UI를 교체하지만 실행 중인 process를 재시작하지 않습니다. 설치 후 버전을 확인하고 필요한 서비스를 명시적으로 재시작합니다.

```bash
proxy-control --version
proxy-agent --version
sudo systemctl restart proxy-tester-agent
sudo systemctl restart proxy-tester-control
```

tar 설치 제거 시 active service를 먼저 중지해야 합니다. 기본 제거는 설정과 데이터를 보존하며 `--purge`만 이를 삭제합니다.

```bash
sudo systemctl disable --now proxy-tester-agent proxy-tester-control
sudo ./uninstall.sh
# 설정과 데이터까지 영구 삭제할 때만:
sudo ./uninstall.sh --purge
```

deb/rpm은 해당 package manager로 제거하며 `/var/lib/proxy-tester`는 운영자가 별도로 관리합니다.

## 환경 변수

| 구성 요소 | 환경 변수 | 기본값 | 설명 |
|---|---|---|---|
| Control | `PROXY_TESTER_HTTP_ADDR` | `0.0.0.0:8080` | UI/API listen 주소 |
| Control | `PROXY_TESTER_GRPC_ADDR` | `0.0.0.0:50051` | agent gRPC listen 주소 |
| Control | `DATABASE_URL` | `/var/lib/proxy-tester/proxy-tester.db` | SQLite 연결 문자열 |
| Control | `PROXY_TESTER_STATIC_DIR` | `/usr/share/proxy-tester/frontend/dist` | CSR 정적 자산 경로 |
| Control | `PROXY_TESTER_ARTIFACT_DIR` | `/var/lib/proxy-tester/artifacts` | 업로드 artifact 경로 |
| Control | `PROXY_TESTER_RETENTION_DAYS` | `90` | 완료 Run 보존 일수 |
| Agent | `PROXY_TESTER_CONTROL` | 없음 | Control gRPC endpoint |
| Agent | `PROXY_TESTER_NODE_ID` | 없음 | Control 전체에서 고유한 node ID |
| Agent | `PROXY_TESTER_NETWORK_JOURNAL` | `/var/lib/proxy-tester/network-state.json` | network rollback journal |

Control의 TCP 50051은 필요한 Agent 주소에서만 허용하세요. 현재 gRPC transport 인증은 제공하지 않으므로 신뢰된 측정망 또는 VPN 안에서 운영해야 합니다.

## 설치 확인과 문제 해결

1. `systemctl status`와 journal에서 시작 오류를 확인합니다.
2. `/api/health`에서 `version`, `build_commit`, `schema_version=4`와 실제 `database_url`을 확인합니다.
3. `/api/agents`에서 node가 online이고 inventory에 시험 NIC가 있는지 확인합니다.
4. network profile을 Plan한 뒤 예상 namespace, 주소와 rollback 명령을 검토합니다.
5. Apply 후 Diagnose를 실행하고 실패하면 network audit와 Agent journal을 함께 확인합니다.

네트워크 변경·rollback은 [NETWORK_CONFIGURATION.md](NETWORK_CONFIGURATION.md), 저장소 백업은 [STORAGE.md](STORAGE.md), 계측 해석은 [TELEMETRY.md](TELEMETRY.md)를 참고하세요.
