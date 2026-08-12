param([string]$BaseUrl = 'http://localhost:18080')
$ErrorActionPreference = 'Stop'

function New-Scenario($name, $topology, $protocol, $transactions, $keepAlive = $true) {
    @{
        version = 1; id = [guid]::NewGuid().ToString(); name = $name
        topology = $topology; protocol = $protocol
        client_agent_id = 'client-1'; server_agent_id = 'server-1'
        proxy_addr = if ($topology -eq 'explicit_proxy') { 'proxy:3128' } else { $null }
        target_addr = 'server:8080'; source_ips = @(); virtual_clients = 4
        duration_secs = 2; warmup_secs = 0
        request = @{ method = 'GET'; path = '/'; host = 'proxy-tester.local'; request_body_bytes = 128; response_body_bytes = 1024; keep_alive = $keepAlive; transactions_per_connection = $transactions; think_time_ms = 0 }
        tcp = @{ tx_bytes = 256; rx_bytes = 512 }
        tls = @{ enabled = $false; verify_peer = $true; server_name = 'proxy-tester.local'; ca_pem = $null }
        timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 5000 }
        observation_interfaces = @('eth0')
    }
}

function Invoke-Scenario($scenario) {
    $json = $scenario | ConvertTo-Json -Depth 8 -Compress
    $run = Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body $json
    for ($attempt = 0; $attempt -lt 15; $attempt++) {
        Start-Sleep -Milliseconds 500
        $detail = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)"
        if ($detail.status -notin @('preparing', 'running', 'paused')) { break }
    }
    if ($detail.status -ne 'completed') { throw "$($scenario.name): status=$($detail.status)" }
    $client = @($detail.samples | Where-Object { $_.role -eq 1 })
    $server = @($detail.samples | Where-Object { $_.role -eq 2 })
    if ($client.Count -lt 1 -or $server.Count -lt 1) { throw "$($scenario.name): missing telemetry" }
    $cm = $client[-1].metrics; $sm = $server[-1].metrics
    if ($cm.transactions -le 0) { throw "$($scenario.name): no transactions" }
    if ($scenario.topology -eq 'transparent_proxy') {
        if ($cm.bytes_tx -ne $sm.bytes_rx -or $cm.bytes_rx -ne $sm.bytes_tx) {
            throw "$($scenario.name): endpoint byte invariant failed"
        }
    }
    $httpP99 = ($client.metrics.http_latency_p99_ms | Measure-Object -Maximum).Maximum
    $appBps = ($client.metrics.tx_bps | Measure-Object -Maximum).Maximum
    $wireBps = ($client.metrics.wire_tx_bps | Measure-Object -Maximum).Maximum
    $wirePps = ($client.metrics.wire_tx_pps | Measure-Object -Maximum).Maximum
    $cps = ($client.metrics.cps | Measure-Object -Maximum).Maximum
    $tps = ($client.metrics.tps | Measure-Object -Maximum).Maximum
    if ($appBps -le 0) { throw "$($scenario.name): no application bandwidth" }
    if ($wireBps -le 0 -or $wirePps -le 0) { throw "$($scenario.name): no wire bandwidth/PPS" }
    if ($cps -le 0) { throw "$($scenario.name): no CPS" }
    if ($scenario.protocol -eq 'http1' -and $httpP99 -le 0) { throw "$($scenario.name): no HTTP latency" }
    if ($scenario.protocol -eq 'http1' -and $tps -le 0) { throw "$($scenario.name): no TPS" }
    if ($scenario.protocol -eq 'tcp' -and $httpP99 -gt 0) { throw "$($scenario.name): HTTP latency leaked into TCP" }
    [pscustomobject]@{
        name = $scenario.name; connections = $cm.connections_established
        transactions = $cm.transactions; http_p99_ms = $httpP99
        client_tx = $cm.bytes_tx; server_rx = $sm.bytes_rx
        client_rx = $cm.bytes_rx; server_tx = $sm.bytes_tx
        app_mbps = [math]::Round($appBps / 1e6, 3); wire_mbps = [math]::Round($wireBps / 1e6, 3)
        wire_pps = [math]::Round($wirePps, 1); cps = [math]::Round($cps, 1); tps = [math]::Round($tps, 1)
    }
}

for ($attempt = 0; $attempt -lt 20; $attempt++) {
    try { if ((Invoke-RestMethod "$BaseUrl/api/agents").Count -ge 2) { break } } catch {}
    Start-Sleep -Milliseconds 500
}

$scenarios = @(
    (New-Scenario 'tcp-direct' 'transparent_proxy' 'tcp' 1),
    (New-Scenario 'http-single-direct' 'transparent_proxy' 'http1' 1 $false),
    (New-Scenario 'http-fixed-direct' 'transparent_proxy' 'http1' 5),
    (New-Scenario 'http-continuous-direct' 'transparent_proxy' 'http1' 0),
    (New-Scenario 'tcp-explicit-connect' 'explicit_proxy' 'tcp' 1),
    (New-Scenario 'http-explicit-forward' 'explicit_proxy' 'http1' 5)
)

$results = foreach ($scenario in $scenarios) { Invoke-Scenario $scenario }
$results | Format-Table -AutoSize
