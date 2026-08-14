param(
    [string]$BaseUrl = 'http://localhost:18080',
    [string]$ProfileRevisionId
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/scenario-v4-helpers.ps1"
$certificate = Invoke-RestMethod "$BaseUrl/api/tls/certificates" -Method Post -ContentType 'application/json' -Body '{"server_name":"proxy-tester.local"}'

function Invoke-H2([string]$name, [string]$pathKind, [string]$tlsVersion, [int]$streams, [int]$transactions) {
    $scenario = @{
        version = 4; id = [guid]::NewGuid().ToString(); name = $name
        path = New-ScenarioPath $BaseUrl $pathKind $ProfileRevisionId
        protocol = 'http2'; virtual_clients = 2; duration_secs = 3; load_stages = @()
        request = @{ method = 'POST'; path = '/h2'; host = 'proxy-tester.local'; request_body_bytes = 0; response_body_bytes = 0; keep_alive = $true; transactions_per_connection = $transactions; think_time_ms = 0 }
        http2 = @{ max_concurrent_streams = $streams }
        tcp = @{ tx_bytes = 0; rx_bytes = 0 }
        payload_mode = 'manual'; capture_artifact_id = $null
        request_payload = @{ kind = 'fixed'; size_bytes = 4096; text = ''; artifact_id = $null; random_format = 'binary' }
        response_payload = @{ kind = 'fixed'; size_bytes = 16384; text = ''; artifact_id = $null; random_format = 'binary' }
        tls = @{ enabled = $true; verify_peer = $true; version = $tlsVersion; cipher_suite = $null; server_name = 'proxy-tester.local'; ca_pem = $certificate.ca_pem; server_cert_pem = $certificate.server_cert_pem; server_key_pem = $certificate.server_key_pem }
        timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 5000 }; observation_interfaces = @('eth0')
    }
    $run = Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($scenario | ConvertTo-Json -Depth 8 -Compress)
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        Start-Sleep -Milliseconds 500
        $detail = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)"
        if ($detail.status -notin @('preparing','running','paused','degraded')) { break }
    }
    if ($detail.status -ne 'completed') { throw "$name status=$($detail.status) error=$($detail.error)" }
    $client = @($detail.samples | Where-Object role -eq 1)[-1].metrics
    $server = @($detail.samples | Where-Object role -eq 2)[-1].metrics
    if ($client.transactions -lt ($transactions * 2) -or $client.transaction_errors -ne 0) { throw "$name missing HTTP/2 transactions" }
    if ($pathKind -eq 'managed_direct' -and ($client.bytes_tx -ne $server.bytes_rx -or $client.bytes_rx -ne $server.bytes_tx)) { throw "$name endpoint byte mismatch" }
    [pscustomobject]@{ name = $name; streams = $streams; transactions = $client.transactions; tx = $client.bytes_tx; rx = $client.bytes_rx }
}

$plain = @{
    version = 4; id = [guid]::NewGuid().ToString(); name = 'h2c-rejected'
    path = New-ScenarioPath $BaseUrl 'managed_direct' $ProfileRevisionId
    protocol = 'http2'; virtual_clients = 1; duration_secs = 1; load_stages = @()
    request = @{ method = 'GET'; path = '/'; host = 'proxy-tester.local'; request_body_bytes = 0; response_body_bytes = 0; keep_alive = $true; transactions_per_connection = 1; think_time_ms = 0 }
    http2 = @{ max_concurrent_streams = 8 }; tcp = @{ tx_bytes = 0; rx_bytes = 0 }
    payload_mode = 'manual'; capture_artifact_id = $null
    request_payload = @{ kind = 'empty'; size_bytes = 0; text = ''; artifact_id = $null; random_format = 'binary' }
    response_payload = @{ kind = 'empty'; size_bytes = 0; text = ''; artifact_id = $null; random_format = 'binary' }
    tls = @{ enabled = $false; verify_peer = $false; version = 'tls13'; cipher_suite = $null; server_name = 'proxy-tester.local'; ca_pem = $null; server_cert_pem = $null; server_key_pem = $null }
    timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 5000 }; observation_interfaces = @('eth0')
}
try {
    Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($plain | ConvertTo-Json -Depth 5 -Compress) | Out-Null
    throw 'h2c scenario was accepted'
} catch {
    if ($_.Exception.Message -eq 'h2c scenario was accepted') { throw }
}

@(
    (Invoke-H2 'h2-tls12-direct' 'managed_direct' 'tls12' 8 20),
    (Invoke-H2 'h2-tls13-direct' 'managed_direct' 'tls13' 8 20),
    (Invoke-H2 'h2-tls12-connect' 'explicit_proxy' 'tls12' 32 40),
    (Invoke-H2 'h2-tls13-connect' 'explicit_proxy' 'tls13' 32 40)
) | Format-Table -AutoSize
