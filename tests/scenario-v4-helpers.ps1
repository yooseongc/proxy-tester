function Get-AgentTestAddress {
    param(
        [string]$BaseUrl,
        [string]$NodeId,
        [string]$InterfaceName = 'eth0'
    )

    $agents = Invoke-RestMethod "$BaseUrl/api/agents"
    $agent = $agents |
        Where-Object { $_.id -eq $NodeId } |
        Select-Object -First 1
    if (-not $agent) { throw "agent $NodeId is not registered" }
    $interface = @($agent.inventory.interfaces) |
        Where-Object { $_.name -eq $InterfaceName } |
        Select-Object -First 1
    $cidr = @($interface.addresses) |
        Where-Object { $_ -match '^\d+\.\d+\.\d+\.\d+/' } |
        Select-Object -First 1
    if (-not $cidr) { throw "agent $NodeId has no IPv4 address on $InterfaceName" }
    return ($cidr -split '/', 2)[0]
}

function New-ScenarioPath {
    param(
        [string]$BaseUrl,
        [ValidateSet('managed_direct', 'explicit_proxy')]
        [string]$Kind,
        [string]$ProfileRevisionId,
        [int]$ServerPort = 8080,
        [string]$ProxyAddress = 'proxy:3128'
    )

    if ($Kind -eq 'managed_direct') {
        if (-not $ProfileRevisionId) {
            throw 'managed_direct regression requires -ProfileRevisionId from a prepared network profile'
        }
        return @{
            kind = 'managed_direct'
            profile_revision_id = $ProfileRevisionId
            server_port = $ServerPort
        }
    }

    return @{
        kind = 'explicit_proxy'
        client_node_id = 'client-1'
        client_bind_ip = Get-AgentTestAddress $BaseUrl 'client-1'
        server_node_id = 'server-1'
        server_listen_ip = Get-AgentTestAddress $BaseUrl 'server-1'
        server_port = $ServerPort
        proxy_addr = $ProxyAddress
    }
}
