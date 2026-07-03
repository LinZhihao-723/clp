# syntax=docker/dockerfile:1

# Builds the benchmark binaries from the workspace. The build context is the repository root because
# Cargo requires every workspace member to be present, even to build a single crate.
FROM rust:1.96-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        libssl-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy the workspace manifest, lockfile, and every member crate.
COPY Cargo.toml Cargo.lock ./
COPY components/api-server components/api-server
COPY components/clp-rust-utils components/clp-rust-utils
COPY components/log-ingestor components/log-ingestor
COPY components/log-ingestor-benchmark components/log-ingestor-benchmark

RUN cargo build -p log-ingestor-benchmark --release --bins

# Runtime image with only the two binaries and their config.
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /build/target/release/mock-ingestor /usr/local/bin/mock-ingestor
COPY --from=builder /build/target/release/mock-scheduler /usr/local/bin/mock-scheduler
COPY components/log-ingestor-benchmark/config /app/config
