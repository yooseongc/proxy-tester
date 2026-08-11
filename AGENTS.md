
- proxy 성능 측정기
  - 지원 측정 대상
    - 1) 인라인 투명프록시
    - 2) passive mirror
    - 3) 명시적프록시 (HTTP PROXY/CONNECT)
  - 측정 항목
    - TCP Connection Per Second (CPS)
    - TCP Bandwidth (B/W), Packet per Second (pps)
    - HTTP Transaction per Second (TPS)
  - 설정 가능 항목
    - 가상 TCP 클라이언트/서버 수
    - 사용할 복호화 패킷 - 입력받은 복호화 패킷으로 부터 TCP 세션 재현을 할 수 있게
    - TLS 설정 : 인증서 유효성 검사 on/off
    - 기타 네트워크 측정에 필요한 항목
 

- 아키텍처
  -  총 3가지 요소로 분리됨
     -  TCP client
     -  TCP server
     -  control plane (UI 제공 web 포함)

- 기술 요구사항
  -  rust 1.93 + musl
  -  frontend는 CSR 기반 react.js
  -  DB는 기본적으로 내장 sqlite 사용, 이후 postgres 확장 가능하도록 준비

- 기타 요구사항
  - UI의 사용성과 디자인 일관성
  - 모든 작업 및 트러블슈팅 내용은 문서화
  - 계측 시나리오 프로파일의 설정 용이성 및 유연성

- 참고 프로젝트 : https://github.com/yooseongc/net-meter
  - 어느정도 완성은 시켰지만 원하는 퀄리티가 안나와서 중단

