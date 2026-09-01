#!/bin/sh
set -eu

local_ip="${VXLAN_LOCAL_IP:-198.19.250.20}"
remote_ip="${VXLAN_REMOTE_IP:-198.19.250.10}"
dst_port="${VXLAN_DST_PORT:-4789}"
underlay_dev="$(ip -o -4 address show | awk -v ip="$local_ip" '$4 ~ ("^" ip "/") { print $2; exit }')"

if [ -z "$underlay_dev" ]; then
  echo "vxlan meter: no underlay interface owns $local_ip" >&2
  exit 1
fi

ip link add vx-client type vxlan id 1001 dev "$underlay_dev" local "$local_ip" remote "$remote_ip" dstport "$dst_port"
ip link add vx-server type vxlan id 1002 dev "$underlay_dev" local "$local_ip" remote "$remote_ip" dstport "$dst_port"
ip link add br-client type bridge
ip link add br-server type bridge
ip link set br-client type bridge stp_state 0 forward_delay 0
ip link set br-server type bridge stp_state 0 forward_delay 0
ip link add up-client type veth peer name meter-client
ip link add up-server type veth peer name meter-server
ip link set vx-client master br-client
ip link set up-client master br-client
ip link set vx-server master br-server
ip link set up-server master br-server

for interface in vx-client vx-server br-client br-server up-client up-server meter-client meter-server; do
  ip link set "$interface" up
done

echo "vxlan meter: meter-client <-> VNI 1001, meter-server <-> VNI 1002" >&2

/proxy-control &
control_pid=$!
agent_pid=""

terminate() {
  if [ -n "$agent_pid" ]; then
    kill "$agent_pid" 2>/dev/null || true
    wait "$agent_pid" 2>/dev/null || true
  fi
  kill "$control_pid" 2>/dev/null || true
  wait "$control_pid" 2>/dev/null || true
}
trap 'exit 0' INT TERM
trap terminate EXIT

while true; do
  /proxy-agent &
  agent_pid=$!
  if wait "$agent_pid"; then
    status=0
  else
    status=$?
  fi
  agent_pid=""
  echo "vxlan meter: Agent exited with status $status; restarting" >&2
  sleep 2
done
