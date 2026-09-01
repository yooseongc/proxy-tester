FROM node:20-bookworm-slim AS frontend
WORKDIR /src/frontend
COPY frontend/package*.json ./
RUN npm ci
COPY frontend ./
RUN npm run build

FROM rust:1.93-bookworm AS backend
ARG PROXY_TESTER_BUILD_COMMIT=unknown
ENV PROXY_TESTER_BUILD_COMMIT=${PROXY_TESTER_BUILD_COMMIT}
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler musl-tools git && rustup target add x86_64-unknown-linux-musl && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release --target x86_64-unknown-linux-musl --locked

FROM scratch AS release-files
COPY --from=backend /src/target/x86_64-unknown-linux-musl/release/proxy-control /usr/bin/proxy-control
COPY --from=backend /src/target/x86_64-unknown-linux-musl/release/proxy-agent /usr/bin/proxy-agent
COPY --from=frontend /src/frontend/dist /usr/share/proxy-tester/frontend/dist

FROM scratch AS control
COPY --from=release-files /usr/bin/proxy-control /proxy-control
COPY --from=release-files /usr/share/proxy-tester/frontend/dist /frontend/dist
WORKDIR /
ENTRYPOINT ["/proxy-control"]

FROM alpine:3.23 AS agent
RUN apk add --no-cache iproute2 ethtool iputils
COPY --from=release-files /usr/bin/proxy-agent /proxy-agent
COPY tests/managed-direct-entrypoint.sh /managed-direct-entrypoint.sh
ENTRYPOINT ["/proxy-agent"]

FROM agent AS client
COPY --from=release-files /usr/bin/proxy-agent /proxy-client
ENTRYPOINT ["/proxy-client"]

FROM agent AS server
COPY --from=release-files /usr/bin/proxy-agent /proxy-server
ENTRYPOINT ["/proxy-server"]

FROM alpine:3.23 AS vxlan-lab-base
RUN apk add --no-cache iproute2 ethtool iputils procps
COPY --from=release-files /usr/bin/proxy-control /proxy-control
COPY --from=release-files /usr/bin/proxy-agent /proxy-agent
COPY --from=release-files /usr/share/proxy-tester/frontend/dist /frontend/dist
COPY tests/vxlan-proxy-entrypoint.sh /vxlan-proxy-entrypoint.sh
COPY tests/vxlan-meter-entrypoint.sh /vxlan-meter-entrypoint.sh

FROM vxlan-lab-base AS vxlan-proxy-lab
ENTRYPOINT ["/bin/sh", "/vxlan-proxy-entrypoint.sh"]

FROM vxlan-lab-base AS vxlan-meter-lab
ENTRYPOINT ["/bin/sh", "/vxlan-meter-entrypoint.sh"]
