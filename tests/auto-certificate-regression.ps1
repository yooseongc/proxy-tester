param([string]$BaseUrl = 'http://localhost:18080')
$ErrorActionPreference = 'Stop'
$cert = Invoke-RestMethod "$BaseUrl/api/tls/certificates" -Method Post -ContentType 'application/json' -Body '{"server_name":"proxy-tester.local"}'
if ($cert.ca_pem -notmatch 'BEGIN CERTIFICATE' -or $cert.server_key_pem -notmatch 'BEGIN PRIVATE KEY') { throw 'generated PEM material is invalid' }
$scenario = @{
    version = 1; id = [guid]::NewGuid().ToString(); name = 'auto-generated-certificate'
    topology = 'transparent_proxy'; protocol = 'http1'; client_agent_id = 'client-1'; server_agent_id = 'server-1'
    proxy_addr = $null; target_addr = 'server:8080'; source_ips = @(); virtual_clients = 2; duration_secs = 2; warmup_secs = 0; load_stages = @()
    request = @{ method = 'GET'; path = '/'; host = 'proxy-tester.local'; request_body_bytes = 0; response_body_bytes = 128; keep_alive = $true; transactions_per_connection = 2; think_time_ms = 0 }
    tcp = @{ tx_bytes = 64; rx_bytes = 64 }
    tls = @{ enabled = $true; verify_peer = $true; server_name = 'proxy-tester.local'; ca_pem = $cert.ca_pem; server_cert_pem = $cert.server_cert_pem; server_key_pem = $cert.server_key_pem }
    timeouts = @{ connect_ms = 3000; proxy_connect_ms = 3000; response_ms = 5000 }; observation_interfaces = @()
}
$run = Invoke-RestMethod "$BaseUrl/api/runs" -Method Post -ContentType 'application/json' -Body ($scenario | ConvertTo-Json -Depth 10 -Compress)
for ($attempt = 0; $attempt -lt 20; $attempt++) { Start-Sleep -Milliseconds 500; $detail = Invoke-RestMethod "$BaseUrl/api/runs/$($run.id)"; if ($detail.status -notin @('preparing','running','paused')) { break } }
$transactions = @($detail.samples | Where-Object role -eq 1 | Select-Object -Last 1).metrics.transactions
if ($detail.status -ne 'completed' -or $transactions -le 0) { throw 'generated certificate HTTPS run failed' }
if ($null -ne $detail.scenario.tls.server_key_pem) { throw 'private key leaked through result API' }
[pscustomobject]@{ status = $detail.status; transactions = $transactions; private_key_redacted = $true; validity_days = $cert.validity_days } | Format-List
