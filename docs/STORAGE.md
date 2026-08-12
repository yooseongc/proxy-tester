# 저장소 경계와 보존 정책

Control plane의 영속 데이터는 시나리오, Run, metric sample, event, artifact metadata로 나뉜다. 현재 구현은 단일 Control 인스턴스에 적합한 SQLite를 사용하며 DB 파일과 artifact 디렉터리를 같은 백업 단위로 취급한다.

## PostgreSQL 확장 경계

HTTP/gRPC handler가 요구하는 저장소 연산은 다음 경계로 분리한다.

- Scenario repository: upsert, list, ID 조회
- Run repository: 생성, 상태 전이, 최근 목록, 상세 조회
- Telemetry repository: sample/event append와 Run별 조회
- Artifact repository: metadata upsert/조회/삭제와 content store 경로 연결

PostgreSQL 지원 시 domain/API JSON은 바꾸지 않고 이 연산을 trait 구현으로 옮긴다. `TEXT` JSON 컬럼은 JSONB로, timestamp 문자열은 `timestamptz`로 치환하고 artifact bytes는 DB가 아닌 content store에 유지한다. 상태 전이는 transaction과 조건부 update로 보호하고 telemetry append는 batch insert를 사용한다. 마이그레이션은 번호가 있는 SQL 파일과 `schema_migrations` 테이블로 전환한 뒤 SQLite와 PostgreSQL에 같은 계약 시험을 실행한다.

## 보존 및 정리

`PROXY_TESTER_RETENTION_DAYS`의 기본값은 90일이다. Control 시작 시 종료 시각이 기준보다 오래된 Run과 그 metric/event를 제거한다. 같은 기간보다 오래된 artifact는 저장된 Scenario나 남아 있는 Run snapshot이 ID를 참조하지 않을 때만 파일과 metadata를 제거한다. 0 이하로 설정하면 자동 정리를 끈다.

백업은 Control을 정지하거나 SQLite online backup을 사용한 상태에서 DB와 artifact 디렉터리를 함께 수행한다. 복구 후에는 두 경로의 소유권과 Control 로그의 migration 오류를 확인한다.
