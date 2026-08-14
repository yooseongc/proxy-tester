#!/bin/sh
set -eu

test_prefix="${PROXY_TESTER_TEST_IPV4_PREFIX:-172.31.}"
test_interface="$({
  ip -o -4 address show
} | awk -v prefix="$test_prefix" '$4 ~ ("^" prefix) { print $2; exit }')"

if [ -z "$test_interface" ]; then
  echo "managed-direct bootstrap: no interface found for IPv4 prefix $test_prefix" >&2
  exit 1
fi

management_interface="$(ip -o route show default | awk '{ print $5; exit }')"
if [ "$test_interface" = "$management_interface" ]; then
  echo "managed-direct bootstrap: refusing to clear management interface $test_interface" >&2
  exit 1
fi

ip address flush dev "$test_interface"
ip link set dev "$test_interface" up
echo "managed-direct bootstrap: prepared unaddressed test interface $test_interface" >&2
exec "$@"
