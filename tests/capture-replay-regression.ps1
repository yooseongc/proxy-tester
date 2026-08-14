param(
    [string]$BaseUrl = 'http://localhost:18080',
    [string]$ProfileRevisionId
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/scenario-v4-helpers.ps1"

for ($attempt = 0; $attempt -lt 30; $attempt++) {
    try { if ((Invoke-RestMethod "$BaseUrl/api/agents").Count -ge 2) { break } } catch {}
    Start-Sleep -Milliseconds 500
}
if ((Invoke-RestMethod "$BaseUrl/api/agents").Count -lt 2) { throw 'client/server agents did not register' }

$capturePath = (Resolve-Path "$PSScriptRoot/pcap/fixtures/plaintext_flows.pcap").Path
$artifact = (& curl.exe -sS -f -F "file=@$capturePath" "$BaseUrl/api/artifacts?kind=pcap") | ConvertFrom-Json
if ($artifact.analysis.supported_flow_count -ne 2) { throw "expected 2 supported flows, got $($artifact.analysis.supported_flow_count)" }
if ($artifact.analysis.exclusions.non_tcp_packets -ne 1) { throw 'UDP exclusion was not reported' }

function Invoke-Replay($scenario) {
    $run = Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($scenario | ConvertTo-Json -Depth 10 -Compress)
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        Start-Sleep -Milliseconds 500
        $detail = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)"
        if ($detail.status -notin @('preparing', 'running', 'paused')) { break }
    }
    if ($detail.status -ne 'completed') { throw "$($scenario.name) failed: status=$($detail.status) error=$($detail.error)" }
    $client = @($detail.samples | Where-Object { $_.role -eq 1 })[-1].metrics
    $server = @($detail.samples | Where-Object { $_.role -eq 2 })[-1].metrics
    if ($client.transactions -le 0) { throw "$($scenario.name) completed no transactions" }
    if ($client.transaction_errors -ne 0) { throw "$($scenario.name) recorded $($client.transaction_errors) client errors" }
    if ($scenario.path.kind -eq 'managed_direct' -and ($client.bytes_tx -ne $server.bytes_rx -or $client.bytes_rx -ne $server.bytes_tx)) { throw "$($scenario.name) endpoint byte invariant failed" }
    [pscustomobject]@{ name=$scenario.name; status=$detail.status; transactions=$client.transactions; client_tx=$client.bytes_tx; client_rx=$client.bytes_rx }
}

$scenario = @{
    version = 4; id = [guid]::NewGuid().ToString(); name = 'tcp-capture-replay'
    path = New-ScenarioPath $BaseUrl 'managed_direct' $ProfileRevisionId
    protocol = 'tcp'; virtual_clients = 4; duration_secs = 3; load_stages = @()
    payload_mode = 'capture_replay'; capture_artifact_id = $artifact.id
    request_payload = @{ kind = 'empty'; size_bytes = 0; text = ''; artifact_id = $null; random_format = 'binary' }
    response_payload = @{ kind = 'empty'; size_bytes = 0; text = ''; artifact_id = $null; random_format = 'binary' }
    request = @{ method = 'GET'; path = '/'; host = 'proxy-tester.local'; request_body_bytes = 0; response_body_bytes = 0; keep_alive = $true; transactions_per_connection = 1; think_time_ms = 0 }
    http2 = @{ max_concurrent_streams = 100 }
    tcp = @{ tx_bytes = 0; rx_bytes = 0 }
    tls = @{ enabled = $false; verify_peer = $false; server_name = 'proxy-tester.local'; ca_pem = $null; server_cert_pem = $null; server_key_pem = $null }
    timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 5000 }
    observation_interfaces = @()
}

$results = @()
$results += Invoke-Replay $scenario

$scenario.id = [guid]::NewGuid().ToString(); $scenario.name = 'tcp-capture-replay-connect'
$scenario.path = New-ScenarioPath $BaseUrl 'explicit_proxy' $ProfileRevisionId
$results += Invoke-Replay $scenario

$certificate = Invoke-RestMethod "$BaseUrl/api/tls/certificates" -Method Post -ContentType 'application/json' -Body '{"server_name":"proxy-tester.local"}'
$scenario.id = [guid]::NewGuid().ToString(); $scenario.name = 'tcp-capture-replay-tls'
$scenario.path = New-ScenarioPath $BaseUrl 'managed_direct' $ProfileRevisionId
$scenario.tls.enabled = $true; $scenario.tls.verify_peer = $true
$scenario.tls.ca_pem = $certificate.ca_pem; $scenario.tls.server_cert_pem = $certificate.server_cert_pem; $scenario.tls.server_key_pem = $certificate.server_key_pem
$results += Invoke-Replay $scenario

$scenario.id = [guid]::NewGuid().ToString(); $scenario.name = 'tcp-capture-replay-tls-connect'
$scenario.path = New-ScenarioPath $BaseUrl 'explicit_proxy' $ProfileRevisionId
$results += Invoke-Replay $scenario

$results | Format-Table -AutoSize
