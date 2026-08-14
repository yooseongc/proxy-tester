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
$certificate = Invoke-RestMethod "$BaseUrl/api/tls/certificates" -Method Post -ContentType 'application/json' -Body '{"server_name":"proxy-tester.local"}'

function New-Scenario($name, $version, $cipher, $pathKind, $verify, $serverName = 'proxy-tester.local') {
    @{
        version = 4; id = [guid]::NewGuid().ToString(); name = $name
        path = New-ScenarioPath $BaseUrl $pathKind $ProfileRevisionId
        protocol = 'http1'; virtual_clients = 2; duration_secs = 2; load_stages = @()
        payload_mode = 'manual'; capture_artifact_id = $null
        request_payload = @{ kind = 'text'; size_bytes = 0; text = 'request'; artifact_id = $null; random_format = 'binary' }
        response_payload = @{ kind = 'text'; size_bytes = 0; text = 'response'; artifact_id = $null; random_format = 'binary' }
        request = @{ method = 'POST'; path = '/tls'; host = 'proxy-tester.local'; request_body_bytes = 0; response_body_bytes = 0; keep_alive = $true; transactions_per_connection = 5; think_time_ms = 0 }
        http2 = @{ max_concurrent_streams = 100 }
        tcp = @{ tx_bytes = 0; rx_bytes = 0 }
        tls = @{ enabled = $true; verify_peer = $verify; version = $version; cipher_suite = $cipher; server_name = $serverName; ca_pem = if ($verify) { $certificate.ca_pem } else { $null }; server_cert_pem = $certificate.server_cert_pem; server_key_pem = $certificate.server_key_pem }
        timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 3000 }; observation_interfaces = @('eth0')
    }
}

function Invoke-TlsRun($scenario, [bool]$expectSuccess) {
    $run = Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($scenario | ConvertTo-Json -Depth 10 -Compress)
    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        Start-Sleep -Milliseconds 500
        $detail = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)"
        if ($detail.status -notin @('preparing', 'running', 'paused')) { break }
    }
    if ($detail.status -ne 'completed') { throw "$($scenario.name) status=$($detail.status) error=$($detail.error)" }
    $client = @($detail.samples | Where-Object { $_.role -eq 1 })[-1].metrics
    if ($expectSuccess) {
        if ($client.transactions -le 0 -or $client.transaction_errors -ne 0) { throw "$($scenario.name) did not produce clean TLS traffic" }
    } elseif ($client.tls_handshake_errors -le 0 -or $client.transaction_errors -le 0) {
        throw "$($scenario.name) did not classify TLS handshake failures"
    }
    [pscustomobject]@{ name=$scenario.name; transactions=$client.transactions; errors=$client.transaction_errors; tls_handshake_errors=$client.tls_handshake_errors }
}

$results = @()
$results += Invoke-TlsRun (New-Scenario 'tls13-direct-no-verify' 'tls13' $null 'managed_direct' $false) $true
$results += Invoke-TlsRun (New-Scenario 'tls12-direct-verified' 'tls12' 'TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256' 'managed_direct' $true) $true
$results += Invoke-TlsRun (New-Scenario 'tls13-connect-verified' 'tls13' 'TLS13_AES_128_GCM_SHA256' 'explicit_proxy' $true) $true
$tcp13 = New-Scenario 'tcp-tls13-direct' 'tls13' $null 'managed_direct' $true
$tcp13.protocol = 'tcp'
$results += Invoke-TlsRun $tcp13 $true
$tcp12Connect = New-Scenario 'tcp-tls12-connect' 'tls12' 'TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256' 'explicit_proxy' $true
$tcp12Connect.protocol = 'tcp'
$results += Invoke-TlsRun $tcp12Connect $true
$results += Invoke-TlsRun (New-Scenario 'tls13-sni-mismatch' 'tls13' $null 'managed_direct' $true 'wrong.test') $false

function Invoke-PathError($scenario, $counter) {
    $run = Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($scenario | ConvertTo-Json -Depth 10 -Compress)
    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        Start-Sleep -Milliseconds 500
        $detail = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)"
        if ($detail.status -notin @('preparing', 'running', 'paused')) { break }
    }
    $client = @($detail.samples | Where-Object { $_.role -eq 1 })[-1].metrics
    if ($detail.status -ne 'completed' -or $client.transaction_errors -le 0 -or $client.$counter -le 0) { throw "$($scenario.name) did not classify $counter" }
    [pscustomobject]@{ name=$scenario.name; transactions=$client.transactions; errors=$client.transaction_errors; tls_handshake_errors=$client.tls_handshake_errors }
}

$connectRejected = New-Scenario 'connect-rejected' 'tls13' $null 'explicit_proxy' $false
$connectRejected.path.server_port = 18081
$results += Invoke-PathError $connectRejected 'proxy_connect_errors'

$httpError = New-Scenario 'http-error-response' 'tls13' $null 'explicit_proxy' $false
$httpError.tls.enabled = $false; $httpError.tls.server_cert_pem = $null; $httpError.tls.server_key_pem = $null
$httpError.path.server_port = 18082
$results += Invoke-PathError $httpError 'http_error_responses'

$invalid = New-Scenario 'invalid-version-cipher' 'tls12' 'TLS13_AES_128_GCM_SHA256' 'managed_direct' $false
try {
    Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($invalid | ConvertTo-Json -Depth 10 -Compress) | Out-Null
    throw 'version/cipher mismatch was accepted'
} catch {
    if ($_.Exception.Response.StatusCode.value__ -ne 400) { throw }
}

$results | Format-Table -AutoSize
