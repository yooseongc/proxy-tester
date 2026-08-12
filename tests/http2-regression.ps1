param([string]$BaseUrl = 'http://localhost:18080')
$ErrorActionPreference = 'Stop'
$certificate = Invoke-RestMethod "$BaseUrl/api/tls/certificates" -Method Post -ContentType 'application/json' -Body '{"server_name":"proxy-tester.local"}'

function Invoke-H2([string]$name, [string]$topology, [int]$streams, [int]$transactions) {
    $scenario = @{
        version = 3; id = [guid]::NewGuid().ToString(); name = $name
        topology = $topology; protocol = 'http2'; client_agent_id = 'client-1'; server_agent_id = 'server-1'
        proxy_addr = if ($topology -eq 'explicit_proxy') { 'proxy:3128' } else { $null }
        target_addr = 'server:8080'; source_ips = @(); virtual_clients = 2; duration_secs = 3; warmup_secs = 0; load_stages = @()
        request = @{ method = 'POST'; path = '/h2'; host = 'proxy-tester.local'; request_body_bytes = 0; response_body_bytes = 0; keep_alive = $true; transactions_per_connection = $transactions; think_time_ms = 0 }
        http2 = @{ max_concurrent_streams = $streams }
        tcp = @{ tx_bytes = 0; rx_bytes = 0 }
        payload_mode = 'manual'; capture_artifact_id = $null
        request_payload = @{ kind = 'fixed'; size_bytes = 4096; text = ''; artifact_id = $null; random_format = 'binary' }
        response_payload = @{ kind = 'fixed'; size_bytes = 16384; text = ''; artifact_id = $null; random_format = 'binary' }
        tls = @{ enabled = $true; verify_peer = $true; version = 'tls13'; cipher_suite = $null; server_name = 'proxy-tester.local'; ca_pem = $certificate.ca_pem; server_cert_pem = $certificate.server_cert_pem; server_key_pem = $certificate.server_key_pem }
        timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 5000 }; observation_interfaces = @()
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
    if ($topology -eq 'transparent_proxy' -and ($client.bytes_tx -ne $server.bytes_rx -or $client.bytes_rx -ne $server.bytes_tx)) { throw "$name endpoint byte mismatch" }
    [pscustomobject]@{ name = $name; streams = $streams; transactions = $client.transactions; tx = $client.bytes_tx; rx = $client.bytes_rx }
}

$plain = @{
    version = 3; id = [guid]::NewGuid().ToString(); name = 'h2c-rejected'; topology = 'transparent_proxy'; protocol = 'http2'
    client_agent_id = 'client-1'; server_agent_id = 'server-1'; target_addr = 'server:8080'; virtual_clients = 1; duration_secs = 1
}
try {
    Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($plain | ConvertTo-Json -Depth 5 -Compress) | Out-Null
    throw 'h2c scenario was accepted'
} catch {
    if ($_.Exception.Message -eq 'h2c scenario was accepted') { throw }
}

@(
    (Invoke-H2 'h2-direct' 'transparent_proxy' 8 20),
    (Invoke-H2 'h2-connect' 'explicit_proxy' 32 40)
) | Format-Table -AutoSize
