param(
    [string]$ProjectName = 'proxy-tester-clean-smoke',
    [int]$HttpPort = 18081,
    [int]$GrpcPort = 50052
)
$ErrorActionPreference = 'Stop'
$env:PROXY_TESTER_PORT = "$HttpPort"
$env:PROXY_TESTER_GRPC_PORT = "$GrpcPort"
$baseUrl = "http://localhost:$HttpPort"

try {
    docker compose -p $ProjectName -f compose.production.yaml up -d
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        try {
            $health = Invoke-RestMethod "$baseUrl/api/health" -TimeoutSec 2
            $agents = Invoke-RestMethod "$baseUrl/api/agents" -TimeoutSec 2
            if ($health.status -eq 'ok' -and $agents.Count -ge 2) { break }
        } catch {}
        Start-Sleep -Seconds 1
    }
    if ($health.status -ne 'ok') { throw 'control health check failed' }
    if ($agents.Count -lt 2) { throw "expected two agents, got $($agents.Count)" }
    [pscustomobject]@{ project = $ProjectName; health = $health.status; agents = $agents.Count }
} finally {
    docker compose -p $ProjectName -f compose.production.yaml down -v
    Remove-Item Env:PROXY_TESTER_PORT -ErrorAction SilentlyContinue
    Remove-Item Env:PROXY_TESTER_GRPC_PORT -ErrorAction SilentlyContinue
}
