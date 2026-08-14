FROM node:20-bookworm-slim AS frontend
WORKDIR /src/frontend
COPY frontend/package*.json ./
RUN npm install
COPY frontend ./
RUN npm run build

FROM rust:1.93-bookworm AS backend
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler musl-tools && rustup target add x86_64-unknown-linux-musl && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM scratch AS control
COPY --from=backend /src/target/x86_64-unknown-linux-musl/release/proxy-control /proxy-control
COPY --from=frontend /src/frontend/dist /frontend/dist
WORKDIR /
ENTRYPOINT ["/proxy-control"]

FROM alpine:3.23 AS agent
RUN apk add --no-cache iproute2 ethtool iputils
COPY --from=backend /src/target/x86_64-unknown-linux-musl/release/proxy-agent /proxy-agent
COPY tests/managed-direct-entrypoint.sh /managed-direct-entrypoint.sh
ENTRYPOINT ["/proxy-agent"]

FROM agent AS client
COPY --from=backend /src/target/x86_64-unknown-linux-musl/release/proxy-agent /proxy-client
ENTRYPOINT ["/proxy-client"]

FROM agent AS server
COPY --from=backend /src/target/x86_64-unknown-linux-musl/release/proxy-agent /proxy-server
ENTRYPOINT ["/proxy-server"]
