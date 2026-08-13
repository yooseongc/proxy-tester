# HTTP/2 시험과 Capture 재현

HTTP/2는 Scenario v4의 `protocol=http2`로 선택한다. h2c와 HTTP Upgrade는 지원하지 않으며 TLS가 필수다. 직접 연결은 대상에 TLS+ALPN `h2`로 연결하고, 명시적 proxy에서는 HTTP/1.1 CONNECT tunnel을 만든 뒤 같은 방식으로 협상한다.

`http2.max_concurrent_streams`는 VU별 연결 하나에서 동시에 실행할 stream 상한이며 기본값은 100, 범위는 1–1000이다. 실제 동시성은 peer SETTINGS 제한을 함께 따른다. request/response payload는 DATA frame으로 flow-control capacity에 맞춰 보내며 완료된 response stream을 TPS와 HTTP latency로 집계한다.

복호화 Capture 재현은 TCP payload가 HTTP/2 client preface와 plaintext frame을 포함할 때 동작한다. SETTINGS, HEADERS/CONTINUATION, DATA와 END_STREAM을 stream ID별로 조립하고 방향별 HPACK 상태를 유지한다. 추출된 request/response body는 원래 stream ID와 timing을 버리고 새 TLS+h2 session에서 재생한다. TLS ciphertext, server push, RST_STREAM/조기 GOAWAY와 불완전 HPACK/header block은 제외한다.

`tests/http2-regression.ps1`는 직접 연결과 CONNECT에서 ALPN, 다중 stream, 양방향 payload와 h2c 차단을 검증한다. Capture parser는 interleaved stream과 HPACK fixture를 Rust golden test로 검증한다.
