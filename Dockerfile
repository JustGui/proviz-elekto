# ============================================================================
# proviz-elekto — multi-stage Rust build
# Stage 1: build the server binary
# Stage 2: minimal runtime image
# ============================================================================

FROM rust:1.87-slim AS builder

# System deps for libpq (postgres crate uses native client)
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    libpq-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy workspace manifests first for layer caching
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml crates/core/Cargo.toml
COPY crates/storage-common/Cargo.toml crates/storage-common/Cargo.toml
COPY crates/storage-sqlite/Cargo.toml crates/storage-sqlite/Cargo.toml
COPY crates/storage-pg/Cargo.toml crates/storage-pg/Cargo.toml
COPY server/Cargo.toml server/Cargo.toml
COPY cli/Cargo.toml cli/Cargo.toml

# Dummy sources so cargo can resolve dependencies without full source
RUN mkdir -p crates/core/src crates/storage-common/src crates/storage-sqlite/src \
             crates/storage-pg/src server/src cli/src && \
    echo 'pub fn _dummy() {}' > crates/core/src/lib.rs && \
    echo 'pub fn _dummy() {}' > crates/storage-common/src/lib.rs && \
    echo 'pub fn _dummy() {}' > crates/storage-sqlite/src/lib.rs && \
    echo 'pub fn _dummy() {}' > crates/storage-pg/src/lib.rs && \
    echo 'fn main() {}' > server/src/main.rs && \
    echo 'fn main() {}' > cli/src/main.rs

RUN cargo build --release --bin proviz-server 2>&1 | tail -5

# Now copy real sources and rebuild (only server and its deps change)
COPY providers/ providers/
COPY crates/ crates/
COPY server/ server/
COPY cli/ cli/

# Touch to force rebuild
RUN touch crates/core/src/lib.rs crates/storage-common/src/lib.rs \
          crates/storage-sqlite/src/lib.rs crates/storage-pg/src/lib.rs \
          server/src/main.rs

RUN cargo build --release --bin proviz-server

# ============================================================================
# Runtime image
# ============================================================================
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    libpq5 \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/proviz-server /usr/local/bin/proviz-server
COPY --from=builder /app/providers /app/providers

EXPOSE 63130

ENV PROVIZ_PORT=63130 \
    PROVIZ_STORAGE=postgres \
    PROVIZ_PROVIDERS_DIR=/app/providers \
    RUST_LOG=proviz_server=info,proviz_elekto_core=info

ENTRYPOINT ["proviz-server"]
