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

$payloadPath = (Resolve-Path "$PSScriptRoot/payload/dlp-sentinel.txt").Path
$artifact = (& curl.exe -sS -f -F "file=@$payloadPath" "$BaseUrl/api/artifacts?kind=payload") | ConvertFrom-Json
$scenario = @{
    version = 4; id = [guid]::NewGuid().ToString(); name = 'result-metadata-redaction'
    path = New-ScenarioPath $BaseUrl 'managed_direct' $ProfileRevisionId
    protocol = 'tcp'; virtual_clients = 2; duration_secs = 2; load_stages = @()
    payload_mode = 'manual'; capture_artifact_id = $null
    request_payload = @{ kind = 'file'; size_bytes = $artifact.size_bytes; text = ''; artifact_id = $artifact.id; random_format = 'binary' }
    response_payload = @{ kind = 'random'; size_bytes = 1024; text = ''; artifact_id = $null; random_format = 'printable_ascii' }
    request = @{ method = 'GET'; path = '/'; host = 'proxy-tester.local'; request_body_bytes = 0; response_body_bytes = 0; keep_alive = $true; transactions_per_connection = 1; think_time_ms = 0 }
    http2 = @{ max_concurrent_streams = 100 }
    tcp = @{ tx_bytes = 0; rx_bytes = 0 }; tls = @{ enabled = $false; verify_peer = $false; version = 'tls13'; cipher_suite = $null; server_name = 'proxy-tester.local'; ca_pem = $null; server_cert_pem = $null; server_key_pem = $null }
    timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 5000 }; observation_interfaces = @()
}
$run = Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($scenario | ConvertTo-Json -Depth 10 -Compress)
for ($attempt = 0; $attempt -lt 40; $attempt++) {
    Start-Sleep -Milliseconds 500
    $detail = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)"
    if ($detail.status -notin @('preparing', 'running', 'paused')) { break }
}
if ($detail.status -ne 'completed') { throw "metadata run failed: $($detail.status)" }
$metadata = $detail.payload_metadata
if ($metadata.request.direction -ne 'client_to_server' -or $metadata.request.artifact_id -ne $artifact.id) { throw 'request artifact metadata mismatch' }
if ($metadata.response.direction -ne 'server_to_client' -or $metadata.response.random_format -ne 'printable_ascii' -or $metadata.response.sha256 -notmatch '^[0-9a-f]{64}$') { throw 'response random metadata mismatch' }
$json = $detail | ConvertTo-Json -Depth 20 -Compress
if ($json.Contains('M7-DLP-SENTINEL-DO-NOT-LEAK')) { throw 'payload content leaked into result JSON' }
[pscustomobject]@{ status=$detail.status; request_artifact=$metadata.request.artifact_id; response_sha256=$metadata.response.sha256 }
