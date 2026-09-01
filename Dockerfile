# syntax=docker/dockerfile:1.7
# Binario estático (musl) para VMs Linux chicas. Sin keyring, sin dbus, sin openssl.
FROM rust:1.96-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
    musl-tools ca-certificates git && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl
WORKDIR /build
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release -p goose-cli --bin ghosty --target x86_64-unknown-linux-musl \
    && cp target/x86_64-unknown-linux-musl/release/ghosty /ghosty

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 1000 -s /bin/bash ghosty && mkdir -p /data /workspace \
    && chown ghosty:ghosty /data /workspace
COPY --from=builder /ghosty /usr/local/bin/ghosty
USER ghosty
WORKDIR /workspace
ENV HOME=/home/ghosty \
    GHOSTY_PATH_ROOT=/data \
    GHOSTY_DISABLE_KEYRING=1 \
    GHOSTY_DISABLE_SESSION_NAMING=true \
    GHOSTY_MODE=auto \
    RUST_LOG=info
VOLUME ["/data", "/workspace"]
EXPOSE 3284
HEALTHCHECK --interval=10s --timeout=3s CMD curl -fsS http://127.0.0.1:3284/status || exit 1
ENTRYPOINT ["ghosty"]
CMD ["serve", "--host", "0.0.0.0", "--port", "3284", "--enable-scheduler"]
