# 패키징 및 릴리스

## 공식 배포 산출물

운영 릴리스는 같은 musl 바이너리와 UI로 만든 통합 네이티브 패키지만 제공합니다.

1. `proxy-tester-<version>-x86_64-linux-musl.tar.gz`
2. `proxy-tester_<version>_amd64.deb`
3. `proxy-tester-<version>-1.x86_64.rpm`
4. `SHA256SUMS`

각 패키지는 `proxy-control`, `proxy-agent`, CSR UI, systemd unit, 환경변수 예제와 설정 helper를 포함합니다. tarball에는 `install.sh`, `uninstall.sh`, 설치 문서와 LICENSE도 포함합니다. Docker Compose, OCI image와 테스트 fixture는 운영 릴리스 자산이 아닙니다.

## 수동 빌드

`packaging/build-release.sh`는 Docker의 고정 Rust 1.93 builder를 사용해 `x86_64-unknown-linux-musl` 바이너리와 frontend를 만들고 고정된 nfpm v2.43.4 container로 deb/rpm을 생성합니다.

```bash
./packaging/build-release.sh 0.1.2
./tests/native-package-smoke.sh dist/release/0.1.2
```

빌드 script는 요청 버전과 Cargo workspace 버전이 다르면 중단합니다. build commit은 바이너리에 주입되며 `proxy-control --version`, `/api/health`와 Agent 등록 정보로 확인할 수 있습니다.

## 릴리스 절차

1. Rust fmt/clippy/workspace test와 frontend typecheck/lint/Vitest/Playwright를 통과시킵니다.
2. native release script로 tar/deb/rpm과 `SHA256SUMS`를 생성합니다.
3. Debian 12, Ubuntu 22.04 이상과 Rocky Linux 9에서 설치·설정·제거 smoke test를 수행합니다.
4. workspace/frontend 버전, Git tag와 package version이 일치하는지 확인합니다.
5. tag를 수동 push하고 GitHub Release에 네 개의 자산만 게시합니다.
6. 게시된 자산을 다시 내려받아 `sha256sum -c SHA256SUMS`로 검증합니다.

GitHub Actions, 자동 tag, 자동 publish는 사용하지 않습니다. v0.1.0은 테스트 중심 prerelease이며 이 네이티브 배포 계약은 v0.1.1부터 적용합니다.

## 업그레이드 계약

패키지는 실행 파일과 UI를 단순 교체하며 서비스를 자동 재시작하지 않습니다. `/etc/proxy-tester`와 `/var/lib/proxy-tester`는 보존합니다. 운영자는 활성 Run 종료와 데이터 백업 후 package를 설치하고, version/build commit을 확인한 다음 Agent와 Control을 명시적으로 재시작합니다.
