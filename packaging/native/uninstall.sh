#!/bin/sh
set -eu

purge=0
[ "$(id -u)" -eq 0 ] || { echo "uninstall.sh must run as root" >&2; exit 1; }
if [ "${1:-}" = "--purge" ]; then purge=1; shift; fi
[ "$#" -eq 0 ] || { echo "Usage: ./uninstall.sh [--purge]" >&2; exit 2; }

for service in proxy-tester-control.service proxy-tester-agent.service; do
    if command -v systemctl >/dev/null 2>&1 && systemctl is-active --quiet "$service"; then
        echo "$service is active; stop it before uninstalling" >&2
        exit 1
    fi
done

rm -f /usr/bin/proxy-control /usr/bin/proxy-agent /usr/bin/proxy-tester-configure
rm -f /usr/lib/systemd/system/proxy-tester-control.service /usr/lib/systemd/system/proxy-tester-agent.service
rm -rf /usr/share/proxy-tester
if [ -d /run/systemd/system ]; then systemctl daemon-reload; fi
if [ "$purge" -eq 1 ]; then
    rm -rf /etc/proxy-tester /var/lib/proxy-tester
    userdel proxy-tester >/dev/null 2>&1 || true
    groupdel proxy-tester >/dev/null 2>&1 || true
    echo "proxy-tester files, configuration and data removed"
else
    echo "proxy-tester removed; /etc/proxy-tester and /var/lib/proxy-tester were preserved"
fi
