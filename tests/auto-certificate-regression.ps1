param(
    [string]$BaseUrl = 'http://localhost:18080',
    [string]$ProfileRevisionId
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/scenario-v4-helpers.ps1"
$cert = Invoke-RestMethod "$BaseUrl/api/tls/certificates" -Method Post -ContentType 'application/json' -Body '{"server_name":"proxy-tester.local"}'
if ($cert.ca_pem -notmatch 'BEGIN CERTIFICATE' -or $cert.server_key_pem -notmatch 'BEGIN PRIVATE KEY') { throw 'generated PEM material is invalid' }
$scenario = @{
    version = 4; id = [guid]::NewGuid().ToString(); name = 'auto-generated-certificate'
    path = New-ScenarioPath $BaseUrl 'managed_direct' $ProfileRevisionId
    protocol = 'http1'; virtual_clients = 2; duration_secs = 2; load_stages = @()
    request = @{ method = 'GET'; path = '/'; host = 'proxy-tester.local'; request_body_bytes = 0; response_body_bytes = 128; keep_alive = $true; transactions_per_connection = 2; think_time_ms = 0 }
    http2 = @{ max_concurrent_streams = 100 }
    tcp = @{ tx_bytes = 64; rx_bytes = 64 }
    payload_mode = 'manual'; capture_artifact_id = $null
    request_payload = @{ kind = 'empty'; size_bytes = 0; text = ''; artifact_id = $null; random_format = 'binary' }
    response_payload = @{ kind = 'fixed'; size_bytes = 128; text = ''; artifact_id = $null; random_format = 'binary' }
    tls = @{ enabled = $true; verify_peer = $true; version = 'tls13'; cipher_suite = $null; server_name = 'proxy-tester.local'; ca_pem = $cert.ca_pem; server_cert_pem = $cert.server_cert_pem; server_key_pem = $cert.server_key_pem }
    timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 5000 }; observation_interfaces = @()
}
$run = Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($scenario | ConvertTo-Json -Depth 10 -Compress)
for ($attempt = 0; $attempt -lt 20; $attempt++) { Start-Sleep -Milliseconds 500; $detail = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)"; if ($detail.status -notin @('preparing','running','paused')) { break } }
$transactions = @($detail.samples | Where-Object role -eq 1 | Select-Object -Last 1).metrics.transactions
if ($detail.status -ne 'completed' -or $transactions -le 0) { throw 'generated certificate HTTPS run failed' }
if ($null -ne $detail.scenario.tls.server_key_pem) { throw 'private key leaked through result API' }
[pscustomobject]@{ status = $detail.status; transactions = $transactions; private_key_redacted = $true; validity_days = $cert.validity_days } | Format-List
