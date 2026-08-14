#!/bin/sh
set -eu

[ "$#" -eq 1 ] || { echo "Usage: $0 RELEASE_DIRECTORY" >&2; exit 2; }
release_dir=$(CDPATH= cd -- "$1" && pwd)
docker_release_dir="$release_dir"
case "$(uname -s)" in
    MINGW*|MSYS*)
        docker_release_dir=$(cd "$release_dir" && pwd -W)
        export MSYS_NO_PATHCONV=1
        ;;
esac
archive=$(find "$release_dir" -maxdepth 1 -name 'proxy-tester-*-x86_64-linux-musl.tar.gz' -print -quit)
deb=$(find "$release_dir" -maxdepth 1 -name 'proxy-tester_*_amd64.deb' -print -quit)
rpm=$(find "$release_dir" -maxdepth 1 -name 'proxy-tester-*-1.x86_64.rpm' -print -quit)
[ -n "$archive" ] && [ -n "$deb" ] && [ -n "$rpm" ] || { echo "release assets are incomplete" >&2; exit 1; }

docker run --rm -v "$docker_release_dir:/release:ro" debian:12-slim sh -euxc '
    mkdir /tmp/package
    tar -xzf /release/proxy-tester-*-x86_64-linux-musl.tar.gz -C /tmp/package --strip-components=1
    /tmp/package/install.sh --component control
    test -x /usr/bin/proxy-control
    test -f /usr/share/proxy-tester/frontend/dist/index.html
    test -f /etc/proxy-tester/control.env
    /usr/bin/proxy-control --version | grep "proxy-control"
    ! test -e /etc/proxy-tester/agent.env
    /tmp/package/uninstall.sh
    test -d /etc/proxy-tester
    test -d /var/lib/proxy-tester
'

docker run --rm -v "$docker_release_dir:/release:ro" debian:12-slim sh -euxc '
    apt-get update
    apt-get install -y /release/proxy-tester_*_amd64.deb
    test -x /usr/bin/proxy-agent
    test -f /usr/lib/systemd/system/proxy-tester-agent.service
    test ! -e /etc/proxy-tester/agent.env
    proxy-tester-configure agent --control-url http://control.example:50051 --node-id smoke-deb
    grep -q smoke-deb /etc/proxy-tester/agent.env
'

docker run --rm -v "$docker_release_dir:/release:ro" rockylinux:9 sh -euxc '
    dnf install -y /release/proxy-tester-*-1.x86_64.rpm
    test -x /usr/bin/proxy-control
    test -f /usr/lib/systemd/system/proxy-tester-control.service
    proxy-tester-configure control
    test -f /etc/proxy-tester/control.env
'

echo "native tar/deb/rpm smoke tests passed"
