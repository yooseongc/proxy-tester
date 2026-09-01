# Two-container VXLAN inline lab

This lab reproduces a two-appliance layout with two logical data ports on each appliance.

```text
meter-b/client namespace
  meter-client -- veth -- br-client -- vx-client (VNI 1001)
                                           |
                                      VXLAN underlay
                                           |
proxy-a: vx-client -- br-proxy -- vx-server
                                           |
                                      VXLAN underlay
                                           |
meter-b/server namespace
  meter-server -- veth -- br-server -- vx-server (VNI 1002)
```

- `proxy-a` represents appliance A. `br-proxy` is a transparent, pass-through forwarding plane with two VXLAN-facing ports. Replace this bridge behavior with the real proxy process when its container image is available.
- `meter-b` represents appliance B. It runs Control and one Agent. The same Agent runs both client and server endpoint roles on separate veth interfaces.
- The Agent borrows only `meter-client` and `meter-server`. VXLAN devices and bridges remain operator-owned in the root namespace.
- The profile explicitly enables managed use of virtual interfaces. Teardown must return both veth interfaces without deleting their peers, bridges, or VXLAN devices.

Run from the repository root:

```powershell
docker compose -p proxy-tester-vxlan-inline -f docker/compose.vxlan-inline.yaml up -d --build
.\tests\vxlan-inline-regression.ps1
docker compose -p proxy-tester-vxlan-inline -f docker/compose.vxlan-inline.yaml down -v
```

The regression prepares a same-Agent managed profile, forcibly restarts the Agent while the profile is prepared, verifies that both endpoint namespaces survive, runs TCP and HTTP traffic through both VXLAN segments and the A-side bridge, tears the profile down once, and verifies that all external topology objects still exist.

The defaults use the benchmark-only `198.19.250.0/24` underlay. Override `VXLAN_UNDERLAY_SUBNET`, `VXLAN_PROXY_IP`, `VXLAN_METER_IP`, or `VXLAN_LAB_PORT` when they conflict with the host Docker environment.
