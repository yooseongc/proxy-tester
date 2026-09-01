# Managed network configuration

Scenario v4 separates a reusable network profile from the traffic scenario. A managed-direct scenario references an immutable, prepared profile revision; an explicit-proxy scenario continues to use operator-managed addresses and does not mutate interfaces.

A managed-direct profile has an explicit provisioning policy:

- `managed_namespace` borrows a dedicated, unaddressed physical interface, moves it into a Proxy Tester namespace, and assigns the configured IPv4 pool. Bridge members and virtual links are rejected by default. Disposable container/test environments may explicitly enable `allow_virtual_interfaces`; never enable it for operator-owned veth/VXLAN topology.
- `operator_managed` uses interfaces and IPv4 addresses already configured by the operator. It never moves links, changes offloads, assigns addresses, or deletes namespaces. Use this policy for veth, bridge, VXLAN, macvlan, or other externally owned topology.

## Safety model

The Agent inventories Linux interfaces, IPv4/IPv6 addresses, link kind, bridge master, link state, MTU, and both IPv4 and IPv6 default-route interfaces. A default-route interface is protected and cannot be selected. In `managed_namespace`, planning rejects an existing IPv4 address, an externally owned virtual/enslaved link, overlapping pools, pools outside one IPv4 subnet, an MTU outside 576–9216, or more than 4096 addresses per endpoint. IPv6-only interfaces are accepted and their IPv6 addresses are preserved. In `operator_managed`, every configured pool address must already exist on the selected interface.

`Plan` is read-only. It returns exact commands, semantic changes, warnings, an inventory fingerprint, and a single-use token valid for five minutes. `Apply` uses a two-phase operation across every participating Node:

1. Each Node checks the saved inventory fingerprint, probes every address with duplicate-address ARP where `arping` is available, and stages its namespace and addresses.
2. If all Nodes staged successfully, Control commits them. Any stage or commit failure requests rollback from every staged Node.
3. An uncommitted lease expires after 180 seconds and the Agent rolls it back locally.

The Agent journal is written atomically to `/var/lib/proxy-tester/network-state.json`. Agent restart rolls back only incomplete `applying` or `staged` work; a committed `prepared` configuration survives restart until explicit Teardown or Reconcile. Teardown retries every rollback command three times. Namespace deletion is skipped if any borrowed-interface restoration fails, preventing a remaining veth and its peer from being destroyed. An unrecoverable teardown is marked `quarantined` for operator reconciliation.

## Linux data plane

Each selected endpoint gets a network namespace named from the immutable revision and its role. The physical test interface is moved into that namespace, assigned the configured pool and MTU, brought up, and has GRO/GSO/TSO/LRO/RX/TX offloads disabled. Traffic workers enter the namespace before creating sockets. Client connections cycle through the prepared source-IP pool, while the responder binds the prepared server address.

The packaged Agent needs `CAP_NET_ADMIN`, `CAP_NET_RAW`, and `CAP_SYS_ADMIN` (for the bind mounts created by `ip netns`), plus `iproute2`, `ethtool`, and `arping`. The supplied Compose definitions include those capabilities and persistent journals. A bare-metal service should use equivalent systemd capabilities and a writable `/var/lib/proxy-tester`.

## Operator workflow

1. Confirm both Nodes are online and their inventories are visible.
2. Select a Node, an unprotected interface, first IPv4 CIDR and address count for Client and Server.
3. Save and inspect the generated plan. Never approve a plan that mentions the management interface.
4. Apply the plan, then run diagnostics. The chosen revision is automatically pinned into the traffic scenario.
5. Finish all runs before teardown. Archive a profile only after its prepared revision is torn down.

`GET /api/network/audit` returns operations with their ordered, per-Node stage events. `POST /api/network/diagnose` verifies revision validity, Node liveness and current inventory availability; run preflight performs the final namespace path check. `POST /api/network/nodes/{id}/reconcile` requests local journal reconciliation after an interrupted operation.

An Agent restart preserves a prepared local journal and does not resume traffic workers automatically. Use Teardown or Reconcile to remove the prepared configuration. Inventory is captured when the Agent connects, so a namespace-changing recovery must finish before reconnect or be followed by an Agent reconnect to refresh the UI snapshot.

The plan review UI shows endpoint, namespace, address assignment, semantic changes and warnings per Node. Actual commands, rollback commands and the inventory fingerprint stay collapsed under advanced details. `GET /api/network/operations/{id}` backs the contextual diagnostics drawer with an ordered event timeline; the drawer refreshes from persisted state whenever a matching WebSocket event arrives.

## Failure recovery

- **Plan reports inventory changed:** inspect the Node and create a new plan; saved commands are deliberately never recomputed during Apply.
- **Node becomes offline during Apply:** wait for the lease rollback or restart the Agent, then reconcile and plan again.
- **Revision is quarantined:** stop traffic, inspect the audit event and namespace/interface state with `ip netns list` and `ip link`, correct the external conflict, then reconcile.
- **Address conflict:** choose a non-conflicting pool. ARP probing is a guard, not a substitute for IP address management.
- **Control restarts during a run:** the run is failed as `control_restarted`; Agent workers are not resumed automatically.

Bridge construction, TAP/SPAN configuration, transparent appliance policy, and DLP verdict collection remain external to the tester. Inline and passive-mirror tests both generate the same direct Client-to-Server traffic.
