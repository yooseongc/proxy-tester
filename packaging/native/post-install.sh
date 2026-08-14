#!/bin/sh
set -e

getent group proxy-tester >/dev/null 2>&1 || groupadd --system proxy-tester
id proxy-tester >/dev/null 2>&1 || useradd --system --gid proxy-tester --home-dir /var/lib/proxy-tester --shell /usr/sbin/nologin proxy-tester
install -d -m 0750 -o proxy-tester -g proxy-tester /var/lib/proxy-tester /var/lib/proxy-tester/artifacts
if [ -d /run/systemd/system ]; then systemctl daemon-reload; fi
echo "proxy-tester installed; configure /etc/proxy-tester and start only the required service"
