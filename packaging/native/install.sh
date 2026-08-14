#!/bin/sh
set -eu

usage() {
    cat <<'EOF'
Usage: ./install.sh --component control|agent|all [options]

Options:
  --control-url URL   Required when creating an Agent configuration
  --node-id ID        Required when creating an Agent configuration
  --install-deps      Install missing Agent OS tools with apt-get or dnf
  -h, --help          Show this help

Installation never enables, starts, or restarts services.
EOF
}

[ "$(id -u)" -eq 0 ] || { echo "install.sh must run as root" >&2; exit 1; }
[ "$(uname -s)" = Linux ] || { echo "only Linux is supported" >&2; exit 1; }
case "$(uname -m)" in x86_64|amd64) ;; *) echo "only x86_64 is supported" >&2; exit 1 ;; esac

component=""
control_url=""
node_id=""
install_deps=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --component) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; component="$2"; shift 2 ;;
        --control-url) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; control_url="$2"; shift 2 ;;
        --node-id) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; node_id="$2"; shift 2 ;;
        --install-deps) install_deps=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done
case "$component" in control|agent|all) ;; *) usage >&2; exit 2 ;; esac

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
payload="$script_dir/payload"
[ -x "$payload/usr/bin/proxy-control" ] || { echo "package payload is incomplete" >&2; exit 1; }
[ -x "$payload/usr/bin/proxy-agent" ] || { echo "package payload is incomplete" >&2; exit 1; }

missing=""
if [ "$component" = agent ] || [ "$component" = all ]; then
    for command in ip tc ethtool arping; do
        command -v "$command" >/dev/null 2>&1 || missing="$missing $command"
    done
fi
if [ -n "$missing" ]; then
    if [ "$install_deps" -ne 1 ]; then
        echo "missing Agent tools:$missing" >&2
        echo "re-run with --install-deps or install iproute2/iproute, ethtool and arping" >&2
        exit 1
    elif command -v apt-get >/dev/null 2>&1; then
        apt-get update
        DEBIAN_FRONTEND=noninteractive apt-get install -y iproute2 ethtool iputils-arping
    elif command -v dnf >/dev/null 2>&1; then
        dnf install -y iproute ethtool iputils
    else
        echo "cannot install dependencies: apt-get or dnf is required" >&2
        exit 1
    fi
fi

active=""
for service in proxy-tester-control.service proxy-tester-agent.service; do
    if command -v systemctl >/dev/null 2>&1 && systemctl is-active --quiet "$service"; then
        active="$active $service"
    fi
done

getent group proxy-tester >/dev/null 2>&1 || groupadd --system proxy-tester
id proxy-tester >/dev/null 2>&1 || useradd --system --gid proxy-tester --home-dir /var/lib/proxy-tester --shell /usr/sbin/nologin proxy-tester
install -d -m 0750 -o proxy-tester -g proxy-tester /var/lib/proxy-tester /var/lib/proxy-tester/artifacts
install -d -m 0755 /usr/bin /usr/share/proxy-tester/frontend /usr/share/proxy-tester/examples /usr/lib/systemd/system
install -m 0755 "$payload/usr/bin/proxy-control" /usr/bin/proxy-control
install -m 0755 "$payload/usr/bin/proxy-agent" /usr/bin/proxy-agent
install -m 0755 "$payload/usr/bin/proxy-tester-configure" /usr/bin/proxy-tester-configure
rm -rf /usr/share/proxy-tester/frontend/dist
cp -R "$payload/usr/share/proxy-tester/frontend/dist" /usr/share/proxy-tester/frontend/dist
chmod -R a=rX /usr/share/proxy-tester/frontend/dist
install -m 0644 "$payload/usr/share/proxy-tester/examples/control.env.example" /usr/share/proxy-tester/examples/control.env.example
install -m 0644 "$payload/usr/share/proxy-tester/examples/agent.env.example" /usr/share/proxy-tester/examples/agent.env.example
install -m 0644 "$payload/usr/lib/systemd/system/proxy-tester-control.service" /usr/lib/systemd/system/proxy-tester-control.service
install -m 0644 "$payload/usr/lib/systemd/system/proxy-tester-agent.service" /usr/lib/systemd/system/proxy-tester-agent.service

case "$component" in
    control) /usr/bin/proxy-tester-configure control ;;
    agent) /usr/bin/proxy-tester-configure agent --control-url "$control_url" --node-id "$node_id" ;;
    all) /usr/bin/proxy-tester-configure all --control-url "$control_url" --node-id "$node_id" ;;
esac
if [ -d /run/systemd/system ]; then systemctl daemon-reload; fi

echo "proxy-tester installed; no service was enabled or restarted"
if [ -n "$active" ]; then
    echo "running services still use the previous executable; restart manually:$active" >&2
fi
