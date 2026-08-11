param([string]$BaseUrl = 'http://localhost:18080')
$ErrorActionPreference = 'Stop'

function New-Scenario($name, $duration = 4) {
    @{
        version = 1; id = [guid]::NewGuid().ToString(); name = $name
        topology = 'transparent_proxy'; protocol = 'http1'
        client_agent_id = 'client-1'; server_agent_id = 'server-1'; proxy_addr = $null
        target_addr = 'server:8080'; source_ips = @(); virtual_clients = 4
        duration_secs = $duration; warmup_secs = 0
        request = @{ method = 'GET'; path = '/'; host = 'proxy-tester.local'; request_body_bytes = 32; response_body_bytes = 256; keep_alive = $true; transactions_per_connection = 0; think_time_ms = 5 }
        tcp = @{ tx_bytes = 64; rx_bytes = 64 }
        tls = @{ enabled = $false; verify_peer = $true; server_name = 'proxy-tester.local'; ca_pem = $null }
        timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 5000 }
        observation_interfaces = @()
    }
}

function Start-Scenario($scenario) {
    Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($scenario | ConvertTo-Json -Depth 8 -Compress)
}
function Wait-Run($id, $seconds = 20) {
    for ($i = 0; $i -lt ($seconds * 2); $i++) {
        Start-Sleep -Milliseconds 500
        $detail = Invoke-RestMethod "$BaseUrl/api/runs/$id"
        if ($detail.status -notin @('preparing', 'running', 'paused')) { return $detail }
    }
    throw "run $id did not finish"
}
function Client-Transactions($detail) {
    $samples = @($detail.samples | Where-Object { $_.role -eq 1 })
    if ($samples.Count) { return $samples[-1].metrics.transactions }
    return 0
}

# Pause must stop new HTTP transactions after the currently in-flight request settles.
$scenario = New-Scenario 'pause-resume' 4
$startedAt = Get-Date
$run = Start-Scenario $scenario
Start-Sleep -Milliseconds 1500
Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)/pause" -Method Post | Out-Null
Start-Sleep -Milliseconds 500
$paused1 = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)"
$tx1 = Client-Transactions $paused1
Start-Sleep -Milliseconds 1500
$paused2 = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)"
$tx2 = Client-Transactions $paused2
if ($paused2.status -ne 'paused') { throw 'pause state was not persisted' }
if (($tx2 - $tx1) -gt $scenario.virtual_clients) { throw "transactions continued during pause: $tx1 -> $tx2" }
Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)/resume" -Method Post | Out-Null
$completed = Wait-Run $run.id
if ($completed.status -ne 'completed') { throw 'resumed run did not complete' }
if (((Get-Date) - $startedAt).TotalSeconds -lt 5) { throw 'pause time was counted as active duration' }

# Stop must cancel and release the global run lock immediately.
$long = Start-Scenario (New-Scenario 'stop-test' 30)
Start-Sleep -Milliseconds 1500
$stop = Invoke-RestMethod "$BaseUrl/api/runs/$($long.id)/stop" -Method Post
if ($stop.status -ne 'cancelled') { throw 'stop did not cancel run' }
$active = Invoke-RestMethod "$BaseUrl/api/runs/active"
if ($active.run_id) { throw 'active run lock was not released' }

# A subsequent run proves the control plane and both agents recovered.
$recovery = Start-Scenario (New-Scenario 'post-stop-recovery' 2)
$recovered = Wait-Run $recovery.id
if ($recovered.status -ne 'completed' -or (Client-Transactions $recovered) -le 0) { throw 'post-stop recovery failed' }

# Unsupported settings must fail loudly rather than be ignored.
$unsupported = New-Scenario 'unsupported-tls' 2
$unsupported.tls.enabled = $true
try {
    Invoke-RestMethod "$BaseUrl/api/preflight" -Method Post -ContentType 'application/json' -Body ($unsupported | ConvertTo-Json -Depth 8 -Compress) | Out-Null
    throw 'TLS setting was silently accepted'
} catch {
    if ($_.Exception.Message -eq 'TLS setting was silently accepted') { throw }
}

[pscustomobject]@{
    pause_status = $paused2.status; paused_transactions = "$tx1 -> $tx2"
    resume_status = $completed.status; stop_status = $stop.status
    recovery_status = $recovered.status; unsupported_tls = 'rejected'
} | Format-List
