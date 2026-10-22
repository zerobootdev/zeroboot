# syntax=docker/dockerfile:1
# Zeroboot server image
# Multi-stage build: compile Rust binary, then assemble minimal runtime image.
#
# Usage:
#   docker build -t zeroboot:latest .
#
# The image does NOT bundle vmlinux or rootfs — mount them via PersistentVolume.
# See deploy/k8s/ for Kubernetes manifests.

# ─── Stage 1: Build zeroboot binary ──────────────────────────────────────────
FROM rust:1.86-bookworm AS builder

WORKDIR /build

# Cache dependencies separately from source
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs && \
    cargo build --release && \
    rm -f target/release/zeroboot target/release/deps/zeroboot*

# Build actual source
COPY src/ src/
COPY guest/ guest/
RUN cargo build --release

# ─── Stage 2: Runtime image ───────────────────────────────────────────────────
FROM ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive

# Runtime dependencies only
RUN apt-get update -qq && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/*

# Install Firecracker
ARG FC_VERSION=v1.15.0
RUN curl -fsSL -o /tmp/fc.tgz \
        "https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}/firecracker-${FC_VERSION}-x86_64.tgz" && \
    tar -xzf /tmp/fc.tgz -C /tmp && \
    mv "/tmp/release-${FC_VERSION}-x86_64/firecracker-${FC_VERSION}-x86_64" /usr/local/bin/firecracker && \
    chmod +x /usr/local/bin/firecracker && \
    rm -rf /tmp/fc.tgz /tmp/release-*

# Copy zeroboot binary
COPY --from=builder /build/target/release/zeroboot /usr/local/bin/zeroboot

# Data directory — mount a PersistentVolume here to persist snapshots
VOLUME ["/var/lib/zeroboot"]

# Copy entrypoint
COPY docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 8080

ENTRYPOINT ["/entrypoint.sh"]
