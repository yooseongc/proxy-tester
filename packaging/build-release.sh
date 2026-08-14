#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
version="${1:-$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)}"
expected=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
[ "$version" = "$expected" ] || { echo "requested version $version does not match workspace $expected" >&2; exit 1; }
command -v docker >/dev/null 2>&1 || { echo "docker is required for the reproducible musl build" >&2; exit 1; }

commit=$(git rev-parse --short=12 HEAD)
docker_root="$root"
docker_workdir=/workspace
msys_docker=0
case "$(uname -s)" in
    MINGW*|MSYS*)
        docker_root=$(pwd -W)
        docker_workdir=/workspace
        msys_docker=1
        ;;
esac
release_dir="$root/dist/release/$version"
build_dir="$root/dist/build"
package_root="$root/dist/package-root"
archive_root="$root/dist/archive/proxy-tester-$version"
rm -rf "$release_dir" "$build_dir" "$package_root" "$root/dist/archive"
mkdir -p "$release_dir" "$package_root/usr/bin" "$package_root/usr/share/proxy-tester/frontend" \
    "$package_root/usr/share/proxy-tester/examples" "$archive_root/payload"

docker build --file docker/Dockerfile --target release-files \
    --build-arg "PROXY_TESTER_BUILD_COMMIT=$commit" \
    --output "type=local,dest=$build_dir" .

cp "$build_dir/usr/bin/proxy-control" "$package_root/usr/bin/proxy-control"
cp "$build_dir/usr/bin/proxy-agent" "$package_root/usr/bin/proxy-agent"
cp packaging/native/proxy-tester-configure "$package_root/usr/bin/proxy-tester-configure"
cp -R "$build_dir/usr/share/proxy-tester/frontend/dist" "$package_root/usr/share/proxy-tester/frontend/dist"
cp packaging/systemd/control.env.example "$package_root/usr/share/proxy-tester/examples/control.env.example"
cp packaging/systemd/agent.env.example "$package_root/usr/share/proxy-tester/examples/agent.env.example"
chmod 0755 "$package_root/usr/bin/"*

mkdir -p "$archive_root/payload/usr/lib/systemd/system"
cp -R "$package_root/usr/." "$archive_root/payload/usr/"
cp packaging/systemd/proxy-tester-control.service "$archive_root/payload/usr/lib/systemd/system/"
cp packaging/systemd/proxy-tester-agent.service "$archive_root/payload/usr/lib/systemd/system/"
cp packaging/native/install.sh packaging/native/uninstall.sh "$archive_root/"
cp docs/INSTALLATION.md LICENSE "$archive_root/"
if [ "$msys_docker" -eq 1 ]; then export MSYS_NO_PATHCONV=1; fi
docker run --rm -v "$docker_root:/workspace" -w "$docker_workdir" \
    -e "PROXY_TESTER_PACKAGE_VERSION=$version" alpine:3.23 sh -eu -c '
        package="/tmp/proxy-tester-$PROXY_TESTER_PACKAGE_VERSION"
        cp -R "dist/archive/proxy-tester-$PROXY_TESTER_PACKAGE_VERSION" "$package"
        chmod 0755 "$package/install.sh" "$package/uninstall.sh" "$package/payload/usr/bin/"*
        tar -czf "dist/release/$PROXY_TESTER_PACKAGE_VERSION/proxy-tester-$PROXY_TESTER_PACKAGE_VERSION-x86_64-linux-musl.tar.gz" \
            -C /tmp "proxy-tester-$PROXY_TESTER_PACKAGE_VERSION"
    '

docker run --rm -v "$docker_root:/workspace" -w "$docker_workdir" \
    -e "PROXY_TESTER_PACKAGE_VERSION=$version" goreleaser/nfpm:v2.43.4 \
    package --config packaging/nfpm.yaml --packager deb \
    --target "dist/release/$version/proxy-tester_${version}_amd64.deb"
docker run --rm -v "$docker_root:/workspace" -w "$docker_workdir" \
    -e "PROXY_TESTER_PACKAGE_VERSION=$version" goreleaser/nfpm:v2.43.4 \
    package --config packaging/nfpm.yaml --packager rpm \
    --target "dist/release/$version/proxy-tester-${version}-1.x86_64.rpm"

(cd "$release_dir" && sha256sum \
    "proxy-tester-$version-x86_64-linux-musl.tar.gz" \
    "proxy-tester_${version}_amd64.deb" \
    "proxy-tester-${version}-1.x86_64.rpm" >SHA256SUMS)
printf 'release assets created in %s\n' "$release_dir"
