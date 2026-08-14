param(
    [string]$BaseUrl = 'http://localhost:18080',
    [string]$ProfileRevisionId
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/scenario-v4-helpers.ps1"
$scenario = @{
    version = 4; id = [guid]::NewGuid().ToString(); name = 'ramp-hold-down'
    path = New-ScenarioPath $BaseUrl 'managed_direct' $ProfileRevisionId
    protocol = 'http1'; virtual_clients = 6; duration_secs = 6
    load_stages = @(
        @{ name = 'warmup'; mode = 'ramp'; duration_secs = 2; target_virtual_clients = 6; include_in_results = $false },
        @{ name = 'measure'; mode = 'hold'; duration_secs = 2; target_virtual_clients = 6; include_in_results = $true },
        @{ name = 'ramp-down'; mode = 'ramp'; duration_secs = 2; target_virtual_clients = 0; include_in_results = $true }
    )
    request = @{ method = 'GET'; path = '/'; host = 'proxy-tester.local'; request_body_bytes = 0; response_body_bytes = 256; keep_alive = $true; transactions_per_connection = 0; think_time_ms = 5 }
    http2 = @{ max_concurrent_streams = 100 }
    tcp = @{ tx_bytes = 64; rx_bytes = 64 }
    payload_mode = 'manual'; capture_artifact_id = $null
    request_payload = @{ kind = 'empty'; size_bytes = 0; text = ''; artifact_id = $null; random_format = 'binary' }
    response_payload = @{ kind = 'fixed'; size_bytes = 256; text = ''; artifact_id = $null; random_format = 'binary' }
    tls = @{ enabled = $false; verify_peer = $false; version = 'tls13'; cipher_suite = $null; server_name = 'proxy-tester.local'; ca_pem = $null; server_cert_pem = $null; server_key_pem = $null }
    timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 5000 }
    observation_interfaces = @()
}
$run = Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($scenario | ConvertTo-Json -Depth 10 -Compress)
for ($attempt = 0; $attempt -lt 25; $attempt++) {
    Start-Sleep -Milliseconds 500
    $detail = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)"
    if ($detail.status -notin @('preparing', 'running', 'paused')) { break }
}
if ($detail.status -ne 'completed') { throw "stage run failed: $($detail.status)" }
$client = @($detail.samples | Where-Object { $_.role -eq 1 })
if (-not ($client | Where-Object { $_.metrics.load_stage_index -eq 0 -and -not $_.metrics.included_in_results })) { throw 'excluded ramp stage was not reported' }
if (-not ($client | Where-Object { $_.metrics.load_stage_index -eq 1 -and $_.metrics.desired_virtual_clients -eq 6 })) { throw 'hold stage target was not reported' }
if (-not ($client | Where-Object { $_.metrics.load_stage_index -eq 2 -and $_.metrics.desired_virtual_clients -lt 6 })) { throw 'ramp-down was not observed' }
[pscustomobject]@{ status = $detail.status; samples = $client.Count; stages = (@($client.metrics.load_stage_index | Sort-Object -Unique) -join ','); max_target = ($client.metrics.desired_virtual_clients | Measure-Object -Maximum).Maximum } | Format-List
