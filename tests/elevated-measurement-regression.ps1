param(
    [Parameter(Mandatory)]
    [string]$ProfileRevisionId,
    [string]$BaseUrl = 'http://localhost:18080',
    [int]$VirtualClients = 8,
    [int]$RampSeconds = 3,
    [int]$HoldSeconds = 9,
    [int]$RampDownSeconds = 3,
    [int]$RequestBytes = 16KB,
    [int]$ResponseBytes = 64KB
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/scenario-v4-helpers.ps1"

function New-ElevatedScenario {
    param(
        [string]$Name,
        [ValidateSet('managed_direct', 'explicit_proxy')]
        [string]$PathKind,
        [ValidateSet('tcp', 'http1', 'http2')]
        [string]$Protocol,
        $Certificate
    )

    $tlsEnabled = $Protocol -eq 'http2'
    $transactions = if ($Protocol -eq 'tcp') { 1 } elseif ($Protocol -eq 'http1') { 10 } else { 100 }
    @{
        version = 4
        id = [guid]::NewGuid().ToString()
        name = $Name
        path = New-ScenarioPath $BaseUrl $PathKind $ProfileRevisionId
        protocol = $Protocol
        virtual_clients = $VirtualClients
        duration_secs = $RampSeconds + $HoldSeconds + $RampDownSeconds
        load_stages = @(
            @{ name = 'ramp-up'; mode = 'ramp'; duration_secs = $RampSeconds; target_virtual_clients = $VirtualClients; include_in_results = $false },
            @{ name = 'hold'; mode = 'hold'; duration_secs = $HoldSeconds; target_virtual_clients = $VirtualClients; include_in_results = $true },
            @{ name = 'ramp-down'; mode = 'ramp'; duration_secs = $RampDownSeconds; target_virtual_clients = 0; include_in_results = $false }
        )
        request = @{
            method = if ($Protocol -eq 'tcp') { 'GET' } else { 'POST' }
            path = '/elevated'
            host = 'proxy-tester.local'
            request_body_bytes = 0
            response_body_bytes = 0
            keep_alive = $true
            transactions_per_connection = $transactions
            think_time_ms = 0
        }
        http2 = @{ max_concurrent_streams = 32 }
        tcp = @{ tx_bytes = 0; rx_bytes = 0 }
        payload_mode = 'manual'
        capture_artifact_id = $null
        request_payload = @{ kind = 'fixed'; size_bytes = $RequestBytes; text = ''; artifact_id = $null; random_format = 'binary' }
        response_payload = @{ kind = 'fixed'; size_bytes = $ResponseBytes; text = ''; artifact_id = $null; random_format = 'binary' }
        tls = @{
            enabled = $tlsEnabled
            verify_peer = $tlsEnabled
            version = 'tls13'
            cipher_suite = $null
            server_name = 'proxy-tester.local'
            ca_pem = if ($tlsEnabled) { $Certificate.ca_pem } else { $null }
            server_cert_pem = if ($tlsEnabled) { $Certificate.server_cert_pem } else { $null }
            server_key_pem = if ($tlsEnabled) { $Certificate.server_key_pem } else { $null }
        }
        timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 10000 }
        observation_interfaces = @('eth0')
    }
}

function Invoke-ElevatedScenario {
    param([hashtable]$Scenario)

    $run = Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($Scenario | ConvertTo-Json -Depth 10 -Compress)
    for ($attempt = 0; $attempt -lt 80; $attempt++) {
        Start-Sleep -Milliseconds 500
        $detail = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)"
        if ($detail.status -notin @('preparing', 'running', 'paused', 'degraded')) { break }
    }
    if ($detail.status -ne 'completed') {
        throw "$($Scenario.name): status=$($detail.status) error=$($detail.error)"
    }

    $client = @($detail.samples | Where-Object { $_.role -eq 1 })
    $server = @($detail.samples | Where-Object { $_.role -eq 2 })
    if ($client.Count -lt 2 -or $server.Count -lt 2) { throw "$($Scenario.name): telemetry samples are missing" }
    $cm = $client[-1].metrics
    $sm = $server[-1].metrics
    if ($cm.connections_failed -ne 0 -or $cm.transaction_errors -ne 0) {
        throw "$($Scenario.name): connections_failed=$($cm.connections_failed) transaction_errors=$($cm.transaction_errors)"
    }
    if ($cm.active_connections -ne 0 -or $sm.active_connections -ne 0) {
        throw "$($Scenario.name): active connections did not return to zero"
    }
    if ($Scenario.path.kind -eq 'managed_direct' -and ($cm.bytes_tx -ne $sm.bytes_rx -or $cm.bytes_rx -ne $sm.bytes_tx)) {
        throw "$($Scenario.name): direct endpoint byte invariant failed"
    }

    $hold = @($client | Where-Object { $_.metrics.load_stage_index -eq 1 -and $_.metrics.included_in_results })
    if ($hold.Count -lt 2) { throw "$($Scenario.name): hold telemetry is missing" }
    $appBps = ($hold.metrics.tx_bps | Measure-Object -Maximum).Maximum
    $wireBps = ($hold.metrics.wire_tx_bps | Measure-Object -Maximum).Maximum
    $wirePps = ($hold.metrics.wire_tx_pps | Measure-Object -Maximum).Maximum
    $cps = ($hold.metrics.cps | Measure-Object -Maximum).Maximum
    $tps = ($hold.metrics.tps | Measure-Object -Maximum).Maximum
    if ($appBps -le 0 -or $wireBps -le 0 -or $wirePps -le 0 -or $cps -le 0) {
        throw "$($Scenario.name): application or wire measurement is zero"
    }
    if ($Scenario.protocol -ne 'tcp' -and $tps -le 0) { throw "$($Scenario.name): TPS is zero" }

    $latency = if ($Scenario.protocol -eq 'tcp') {
        ($hold.metrics.tcp_connect_latency_p99_ms | Measure-Object -Maximum).Maximum
    } else {
        ($hold.metrics.http_latency_p99_ms | Measure-Object -Maximum).Maximum
    }
    if ($latency -le 0) { throw "$($Scenario.name): p99 latency is zero" }

    [pscustomobject]@{
        name = $Scenario.name
        run_id = $run.id
        transactions = $cm.transactions
        peak_cps = [math]::Round($cps, 1)
        peak_tps = [math]::Round($tps, 1)
        app_tx_mbps = [math]::Round($appBps / 1e6, 2)
        wire_tx_mbps = [math]::Round($wireBps / 1e6, 2)
        wire_tx_pps = [math]::Round($wirePps, 1)
        latency_p99_ms = [math]::Round($latency, 3)
        errors = $cm.transaction_errors + $cm.connections_failed
    }
}

if ($VirtualClients -lt 1 -or $RampSeconds -lt 1 -or $HoldSeconds -lt 2 -or $RampDownSeconds -lt 1) {
    throw 'VU and stage durations must be positive, and hold must be at least two seconds'
}

$diagnose = Invoke-RestMethod "$BaseUrl/api/network/diagnose" -Method Post -ContentType 'application/json' -Body (@{ profile_revision_id = $ProfileRevisionId } | ConvertTo-Json -Compress)
if (-not $diagnose.ok) { throw 'managed-direct network diagnostics failed' }
$certificate = Invoke-RestMethod "$BaseUrl/api/tls/certificates" -Method Post -ContentType 'application/json' -Body '{"server_name":"proxy-tester.local"}'

$scenarios = @(
    (New-ElevatedScenario 'elevated-tcp-direct' 'managed_direct' 'tcp' $certificate),
    (New-ElevatedScenario 'elevated-tcp-connect' 'explicit_proxy' 'tcp' $certificate),
    (New-ElevatedScenario 'elevated-http1-direct' 'managed_direct' 'http1' $certificate),
    (New-ElevatedScenario 'elevated-http1-forward' 'explicit_proxy' 'http1' $certificate),
    (New-ElevatedScenario 'elevated-http2-direct' 'managed_direct' 'http2' $certificate),
    (New-ElevatedScenario 'elevated-http2-connect' 'explicit_proxy' 'http2' $certificate)
)

$results = foreach ($scenario in $scenarios) {
    Write-Host "starting $($scenario.name)"
    Invoke-ElevatedScenario $scenario
}
$results | Format-Table -AutoSize
