# 패키징 및 릴리스

## 배포 산출물

릴리스 버전마다 다음 산출물을 동일한 버전으로 배포한다.

1. `proxy-tester-control-<version>-x86_64-linux-musl.tar.gz`
   - `proxy-control`
   - `frontend/dist`
   - 설치 문서와 checksum
2. `proxy-tester-agent-<version>-x86_64-linux-musl.tar.gz`
   - `proxy-agent`
   - systemd unit와 환경변수 예제
   - checksum
3. OCI image
   - `proxy-tester-control:<version>`: control 바이너리와 같은 commit의 UI 포함
   - `proxy-tester-agent:<version>`: 하나의 Agent 이미지, 역할은 환경변수로 결정
4. `compose.production.yaml`, `.env.example`, SHA-256 checksum 목록

테스트용 fixture proxy와 개발 이미지는 릴리스 산출물에 포함하지 않는다. 이미지와 tarball은 Cargo workspace 버전과 Git tag가 일치할 때만 발행한다.

## 빌드 순서

1. `npm ci && npm test && npm run build`
2. `cargo fmt --check`, `cargo clippy`, `cargo test --workspace`
3. Rust 1.93으로 `x86_64-unknown-linux-musl` release 바이너리 생성
4. UI와 control을 동일한 묶음 및 OCI image에 복사
5. Agent binary/systemd 템플릿 묶음 생성
6. 산출물 SHA-256 생성, 컨테이너 취약점 검사, 서명
7. 버전 태그와 immutable digest로 registry/GitHub Release에 게시

현재 Dockerfile은 control/client/server target을 만들지만 Client와 Server의 바이너리는 동일하다. 정식 registry 게시 단계에서는 중복 Agent 이미지를 하나로 합치고 `PROXY_TESTER_ROLE`을 필수로 지정하는 형태가 기준이다. 기존 target은 로컬 Compose 호환성을 위해 유지할 수 있다.

## 업그레이드와 롤백

Control 업그레이드 전에 `/data`를 백업한다. 새 이미지는 기존 SQLite schema에 대해 시작 시 migration을 수행한다. Agent와 Control은 같은 minor 버전을 권장하며, 배포는 Server Agent, Client Agent, Control 순으로 사전 검증한 뒤 진행한다. 롤백은 이전 immutable image digest와 데이터 백업을 사용한다. DB migration에 비가역 변경이 추가될 때는 릴리스 노트에 별도 복원 절차가 필요하다.
