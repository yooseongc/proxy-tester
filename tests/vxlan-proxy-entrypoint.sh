#!/bin/sh
set -eu

local_ip="${VXLAN_LOCAL_IP:-198.19.250.10}"
remote_ip="${VXLAN_REMOTE_IP:-198.19.250.20}"
dst_port="${VXLAN_DST_PORT:-4789}"
underlay_dev="$(ip -o -4 address show | awk -v ip="$local_ip" '$4 ~ ("^" ip "/") { print $2; exit }')"

if [ -z "$underlay_dev" ]; then
  echo "vxlan proxy: no underlay interface owns $local_ip" >&2
  exit 1
fi

ip link add vx-client type vxlan id 1001 dev "$underlay_dev" local "$local_ip" remote "$remote_ip" dstport "$dst_port"
ip link add vx-server type vxlan id 1002 dev "$underlay_dev" local "$local_ip" remote "$remote_ip" dstport "$dst_port"
ip link add br-proxy type bridge
ip link set br-proxy type bridge stp_state 0 forward_delay 0
ip link set vx-client master br-proxy
ip link set vx-server master br-proxy
ip link set vx-client up
ip link set vx-server up
ip link set br-proxy up

echo "vxlan proxy: vx-client(VNI 1001) <-> br-proxy <-> vx-server(VNI 1002)" >&2
exec sleep infinity
