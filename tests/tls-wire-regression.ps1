param(
    [Parameter(Mandatory)] [string]$CertificatePath,
    [Parameter(Mandatory)] [string]$PrivateKeyPath,
    [string]$CaCertificatePath = $CertificatePath,
    [string]$BaseUrl = 'http://localhost:18080',
    [string]$ProfileRevisionId
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/scenario-v4-helpers.ps1"
$certificate = [System.IO.File]::ReadAllText((Resolve-Path -LiteralPath $CertificatePath))
$privateKey = [System.IO.File]::ReadAllText((Resolve-Path -LiteralPath $PrivateKeyPath))
$caRoot = [System.IO.File]::ReadAllText((Resolve-Path -LiteralPath $CaCertificatePath))

function Invoke-TlsScenario($name, $pathKind, $verifyPeer) {
    Write-Host "starting $name"
    $caCertificate = $null
    if ($verifyPeer) { $caCertificate = $caRoot }
    Write-Host "building scenario"
    $scenario = @{
        version = 4; id = [guid]::NewGuid().ToString(); name = $name
        path = New-ScenarioPath $BaseUrl $pathKind $ProfileRevisionId
        protocol = 'http1'; virtual_clients = 2; duration_secs = 2; load_stages = @()
        request = @{ method = 'GET'; path = '/'; host = 'proxy-tester.local'; request_body_bytes = 64; response_body_bytes = 1024; keep_alive = $true; transactions_per_connection = 5; think_time_ms = 0 }
        http2 = @{ max_concurrent_streams = 100 }
        tcp = @{ tx_bytes = 64; rx_bytes = 64 }
        payload_mode = 'manual'; capture_artifact_id = $null
        request_payload = @{ kind = 'fixed'; size_bytes = 64; text = ''; artifact_id = $null; random_format = 'binary' }
        response_payload = @{ kind = 'fixed'; size_bytes = 1024; text = ''; artifact_id = $null; random_format = 'binary' }
        tls = @{ enabled = $true; verify_peer = $verifyPeer; version = 'tls13'; cipher_suite = $null; server_name = 'proxy-tester.local'; ca_pem = $caCertificate; server_cert_pem = $certificate; server_key_pem = $privateKey }
        timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 5000 }
        observation_interfaces = @('eth0')
    }
    Write-Host "scenario built"
    $body = $scenario | ConvertTo-Json -Depth 8 -Compress
    Write-Host "request bytes $($body.Length)"
    $run = Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body $body -TimeoutSec 10
    Write-Host "run $($run.id) created"
    for ($i = 0; $i -lt 20; $i++) {
        Start-Sleep -Milliseconds 500
        $detail = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)" -TimeoutSec 10
        if ($detail.status -notin @('preparing', 'running', 'paused')) { break }
    }
    if ($detail.status -ne 'completed') { throw "$name failed: $($detail.status)" }
    $client = @($detail.samples | Where-Object { $_.role -eq 1 })
    $server = @($detail.samples | Where-Object { $_.role -eq 2 })
    $cm = $client[-1].metrics; $sm = $server[-1].metrics
    $httpP99 = ($client.metrics.http_latency_p99_ms | Measure-Object -Maximum).Maximum
    $clientWire = ($client.metrics.wire_tx_bps | Measure-Object -Maximum).Maximum
    $serverWire = ($server.metrics.wire_rx_bps | Measure-Object -Maximum).Maximum
    if ($cm.transactions -le 0 -or $httpP99 -le 0) { throw "$name produced no TLS HTTP transactions/latency" }
    if ($clientWire -le 0 -or $serverWire -le 0) { throw "$name produced no NIC wire metrics" }
    if ($pathKind -eq 'managed_direct' -and ($cm.bytes_tx -ne $sm.bytes_rx -or $cm.bytes_rx -ne $sm.bytes_tx)) { throw "$name application byte invariant failed" }
    [pscustomobject]@{ name = $name; verify = $verifyPeer; transactions = $cm.transactions; http_p99_ms = $httpP99; client_wire_mbps = [math]::Round($clientWire / 1e6, 3); server_wire_mbps = [math]::Round($serverWire / 1e6, 3); retransmissions = $cm.tcp_retransmissions }
}

$results = @()
$results += Invoke-TlsScenario 'https-direct-no-verify' 'managed_direct' $false
$results += Invoke-TlsScenario 'https-direct-verified' 'managed_direct' $true
$results += Invoke-TlsScenario 'https-explicit-connect' 'explicit_proxy' $true
$results | Format-Table -AutoSize
