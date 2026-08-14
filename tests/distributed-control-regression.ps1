param([string]$BaseUrl = 'http://localhost:18080')
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/scenario-v4-helpers.ps1"

function New-Scenario([string]$name, [int]$duration) {
    @{
        version = 4; id = [guid]::NewGuid().ToString(); name = $name
        path = New-ScenarioPath $BaseUrl 'explicit_proxy' ''
        protocol = 'http1'; virtual_clients = 2; duration_secs = $duration; load_stages = @()
        request = @{ method = 'GET'; path = '/'; host = 'proxy-tester.local'; request_body_bytes = 16; response_body_bytes = 128; keep_alive = $true; transactions_per_connection = 0; think_time_ms = 5 }
        http2 = @{ max_concurrent_streams = 100 }
        tcp = @{ tx_bytes = 16; rx_bytes = 128 }
        payload_mode = 'manual'; capture_artifact_id = $null
        request_payload = @{ kind = 'fixed'; size_bytes = 16; text = ''; artifact_id = $null; random_format = 'binary' }
        response_payload = @{ kind = 'fixed'; size_bytes = 128; text = ''; artifact_id = $null; random_format = 'binary' }
        tls = @{ enabled = $false; verify_peer = $false; version = 'tls13'; cipher_suite = $null; server_name = 'proxy-tester.local'; ca_pem = $null; server_cert_pem = $null; server_key_pem = $null }
        timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 5000 }
        observation_interfaces = @()
    }
}

function Start-Scenario($scenario) {
    Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($scenario | ConvertTo-Json -Depth 8 -Compress)
}

function Wait-Terminal([string]$id, [int]$attempts = 40) {
    for ($attempt = 0; $attempt -lt $attempts; $attempt++) {
        $detail = Invoke-RestMethod "$BaseUrl/api/runs/$id"
        if ($detail.status -notin @('preparing', 'running', 'paused', 'degraded')) { return $detail }
        Start-Sleep -Milliseconds 500
    }
    throw "run $id did not reach a terminal state"
}

$run = Start-Scenario (New-Scenario 'agent-reconnect-no-resume' 30)
Start-Sleep -Seconds 2
docker compose -f compose.yaml -f compose.managed-direct.yaml restart client | Out-Null
$sawDegraded = $false
for ($attempt = 0; $attempt -lt 30; $attempt++) {
    $detail = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)"
    if ($detail.status -eq 'degraded') { $sawDegraded = $true }
    if ($detail.status -eq 'failed') { break }
    Start-Sleep -Milliseconds 500
}
if (-not $sawDegraded) { throw 'disconnect did not expose degraded state' }
if ($detail.status -ne 'failed' -or $detail.error -notlike 'agent_reconnected_no_resume:*') {
    throw "unexpected reconnect outcome: status=$($detail.status) error=$($detail.error)"
}

for ($attempt = 0; $attempt -lt 20; $attempt++) {
    if (@(Invoke-RestMethod "$BaseUrl/api/agents").Count -ge 2) { break }
    Start-Sleep -Milliseconds 500
}
$recovery = Start-Scenario (New-Scenario 'post-disconnect-recovery' 2)
$recoveryDetail = Wait-Terminal $recovery.id
if ($recoveryDetail.status -ne 'completed') { throw "recovery run failed: $($recoveryDetail.status)" }

[pscustomobject]@{
    degraded = $sawDegraded
    disconnect_status = $detail.status
    disconnect_error = $detail.error
    recovery_status = $recoveryDetail.status
}
