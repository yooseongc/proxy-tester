# 분산 제어 안정성

Control과 Agent는 양방향 gRPC stream을 사용한다. Agent 프로세스는 재시작마다 새로운 `instance_id`를 만들고 Control은 연결마다 session generation을 부여한다. 동일 Agent ID가 다시 연결되면 새 session이 이전 session을 대체하며, 이전 stream의 종료 처리는 generation이 일치할 때만 registry를 제거한다.

Prepare, Start, Pause, Resume, Stop에는 고유 command ID가 있다. Agent는 payload 준비나 상태 변경이 끝난 후 ACK하고 최근 256개 command ID를 기억해 재전송을 멱등 처리한다. Control은 `PROXY_TESTER_COMMAND_TIMEOUT_SECS` 안에 양쪽 ACK가 없으면 Run을 시작하거나 상태 전이하지 않는다. participant별 instance, phase, command와 오류는 `run_participants`에 저장한다.

실행 중 participant 연결이 끊기면 Run은 `degraded`가 된다. 기본 10초의 `PROXY_TESTER_AGENT_GRACE_SECS` 동안 재접속을 기다리지만 측정 데이터의 연속성을 보장할 수 없으므로 진행 중 Run은 재개하지 않는다. 재접속 여부에 따라 `agent_reconnected_no_resume` 또는 `agent_disconnected`로 실패하고 새 Run부터 허용한다.

Control 재시작 시 남아 있는 preparing/running/paused/degraded Run은 `control_restarted`로 실패 처리한다. Run 완료는 Client와 Server가 모두 `run_completed`를 보고한 뒤 확정한다.
