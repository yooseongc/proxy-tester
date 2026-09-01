param(
    [string]$BaseUrl = 'http://localhost:18090',
    [string]$ProjectName = 'proxy-tester-vxlan-inline'
)

$ErrorActionPreference = 'Stop'
$composeFile = 'docker/compose.vxlan-inline.yaml'
$revisionId = $null

function Invoke-LabExec {
    param([string]$Service, [string[]]$Arguments)
    $output = & docker compose -p $ProjectName -f $composeFile exec -T $Service @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "docker compose exec $Service failed: $($Arguments -join ' ')"
    }
    return $output
}

function Get-LabLink {
    param([string]$Service, [string]$Interface)
    $link = Invoke-LabExec $Service @('ip', '-d', '-j', 'link', 'show', $Interface) |
        ConvertFrom-Json
    return @($link)[0]
}

function New-LabScenario {
    param([string]$Name, [string]$Protocol, [bool]$KeepAlive = $true)
    return @{
        version = 4
        id = [guid]::NewGuid().ToString()
        name = $Name
        path = @{
            kind = 'managed_direct'
            profile_revision_id = $revisionId
            server_port = 8080
        }
        protocol = $Protocol
        virtual_clients = 4
        duration_secs = 2
        load_stages = @()
        request = @{
            method = 'GET'; path = '/'; host = 'vxlan-lab.local'
            request_body_bytes = 128; response_body_bytes = 1024
            keep_alive = $KeepAlive; transactions_per_connection = 4; think_time_ms = 0
        }
        http2 = @{ max_concurrent_streams = 100 }
        tcp = @{ tx_bytes = 256; rx_bytes = 512 }
        payload_mode = 'manual'
        capture_artifact_id = $null
        request_payload = @{ kind = 'fixed'; size_bytes = 128; text = ''; artifact_id = $null; random_format = 'binary' }
        response_payload = @{ kind = 'fixed'; size_bytes = 1024; text = ''; artifact_id = $null; random_format = 'binary' }
        tls = @{
            enabled = $false; verify_peer = $false; version = 'tls13'; cipher_suite = $null
            server_name = 'vxlan-lab.local'; ca_pem = $null; server_cert_pem = $null; server_key_pem = $null
        }
        timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 5000 }
        observation_interfaces = @('meter-client', 'meter-server')
    }
}

function Invoke-LabScenario {
    param([hashtable]$Scenario)
    $run = Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($Scenario | ConvertTo-Json -Depth 10 -Compress)
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        Start-Sleep -Milliseconds 500
        $detail = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)"
        if ($detail.status -notin @('preparing', 'running', 'paused')) { break }
    }
    if ($detail.status -ne 'completed') {
        throw "$($Scenario.name): status=$($detail.status), error=$($detail.error)"
    }
    $client = @($detail.samples | Where-Object { $_.role -eq 1 })[-1].metrics
    $server = @($detail.samples | Where-Object { $_.role -eq 2 })[-1].metrics
    if ($client.transactions -le 0 -or $server.transactions -le 0) {
        throw "$($Scenario.name): no end-to-end transactions"
    }
    if ($client.bytes_tx -ne $server.bytes_rx -or $client.bytes_rx -ne $server.bytes_tx) {
        throw "$($Scenario.name): endpoint byte invariant failed"
    }
    return [pscustomobject]@{
        name = $Scenario.name
        run_id = $run.id
        transactions = $client.transactions
        connections = $client.connections_established
        client_tx = $client.bytes_tx
        client_rx = $client.bytes_rx
        wire_tx = $client.wire_tx_bytes
        wire_rx = $client.wire_rx_bytes
    }
}

for ($attempt = 0; $attempt -lt 30; $attempt++) {
    Start-Sleep -Milliseconds 500
    try {
        $agents = @(Invoke-RestMethod "$BaseUrl/api/agents")
        if (@($agents | Where-Object { $_.id -eq 'meter-b' -and $_.online }).Count -eq 1) { break }
    } catch {}
}
if (@($agents | Where-Object { $_.id -eq 'meter-b' -and $_.online }).Count -ne 1) {
    throw 'meter-b Agent did not connect'
}

$proxyClient = Get-LabLink 'proxy-a' 'vx-client'
$proxyServer = Get-LabLink 'proxy-a' 'vx-server'
if ($proxyClient.master -ne 'br-proxy' -or $proxyServer.master -ne 'br-proxy') {
    throw 'proxy-a VXLAN ports are not attached to br-proxy'
}

$profileId = [guid]::NewGuid().ToString()
$profile = @{
    id = $profileId
    name = 'Two-container VXLAN inline lab'
    provisioning = 'managed_namespace'
    allow_virtual_interfaces = $true
    client_endpoint = @{ node_id = 'meter-b'; interface_name = 'meter-client'; start_cidr = '10.77.0.10/24'; count = 4 }
    server_endpoint = @{ node_id = 'meter-b'; interface_name = 'meter-server'; start_cidr = '10.77.0.20/24'; count = 4 }
    mtu = 1370
    diagnostic_port = 39000
    path_probe_enabled = $true
}

try {
    Invoke-RestMethod "$BaseUrl/api/network/profiles" -Method Post -ContentType 'application/json' -Body ($profile | ConvertTo-Json -Depth 8 -Compress) | Out-Null
    $plan = Invoke-RestMethod "$BaseUrl/api/network/profiles/$profileId/plan" -Method Post
    $revisionId = $plan.profile_revision_id
    $nodePlan = $plan.detail.plans.'meter-b'
    if (-not $nodePlan -or @($nodePlan.endpoints).Count -ne 2) {
        throw 'same-Agent client/server plan did not contain two endpoints'
    }
    $applyBody = @{ plan_token = $plan.plan_token } | ConvertTo-Json -Compress
    $apply = Invoke-RestMethod "$BaseUrl/api/network/operations/$($plan.operation_id)/apply" -Method Post -ContentType 'application/json' -Body $applyBody
    if ($apply.status -ne 'prepared') { throw "network apply status=$($apply.status)" }

    Invoke-LabExec 'meter-b' @('pkill', '-x', 'proxy-agent') | Out-Null
    Start-Sleep -Seconds 4
    $namespaces = @(Invoke-LabExec 'meter-b' @('ip', 'netns', 'list'))
    if (@($namespaces | Where-Object { $_ -match "pt-$($revisionId.Substring(0, 8))-(client|server)" }).Count -ne 2) {
        throw 'prepared endpoint namespaces did not survive the Agent restart'
    }
    $reconnected = @(Invoke-RestMethod "$BaseUrl/api/agents") |
        Where-Object { $_.id -eq 'meter-b' -and $_.online } |
        Select-Object -First 1
    if (-not $reconnected) { throw 'meter-b Agent did not reconnect after restart' }

    $diagnose = Invoke-RestMethod "$BaseUrl/api/network/diagnose" -Method Post -ContentType 'application/json' -Body (@{ profile_revision_id = $revisionId } | ConvertTo-Json -Compress)
    if (-not $diagnose.ok) { throw 'VXLAN managed-direct diagnostics failed' }

    $results = @(
        Invoke-LabScenario (New-LabScenario 'vxlan-tcp-inline' 'tcp')
        Invoke-LabScenario (New-LabScenario 'vxlan-http-inline' 'http1')
    )
    $results | Format-Table -AutoSize
} finally {
    if ($revisionId) {
        Invoke-RestMethod "$BaseUrl/api/network/revisions/$revisionId/teardown" -Method Post | Out-Null
    }
}

$meterClient = Get-LabLink 'meter-b' 'meter-client'
$meterServer = Get-LabLink 'meter-b' 'meter-server'
$meterVxClient = Get-LabLink 'meter-b' 'vx-client'
$meterVxServer = Get-LabLink 'meter-b' 'vx-server'
if ($meterClient.linkinfo.info_kind -ne 'veth' -or $meterServer.linkinfo.info_kind -ne 'veth') {
    throw 'meter veth endpoints were not preserved after teardown'
}
if ($meterVxClient.master -ne 'br-client' -or $meterVxServer.master -ne 'br-server') {
    throw 'meter VXLAN/bridge topology changed after teardown'
}

[pscustomobject]@{
    profile_revision_id = $revisionId
    agent_restart = 'passed'
    teardown = 'passed'
    meter_veths = 'preserved'
    meter_vxlans = 'preserved'
    proxy_bridge = 'preserved'
} | Format-List
