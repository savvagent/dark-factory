# syntax=docker/dockerfile:1
#
# Builds the single df-server binary that mounts every HTTP surface on one
# port (see CLAUDE.md). No sqlx offline cache exists yet (no query!/query_as!
# macros are in use), so this build needs no DATABASE_URL or SQLX_OFFLINE.
#
# TODO(task 11): once crates/df-web ships a SvelteKit console, add a node
# stage here to build it and COPY the output into the runtime image next to
# the binary, the same way migrations are copied below.

FROM rust:1.85-slim-bookworm AS builder
WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Cache dependency compilation separately from source changes.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates crates
RUN cargo build --release -p df-server

FROM debian:bookworm-slim AS runtime
WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Migrations run at startup under an advisory lock (task 13), so the binary
# needs the migration files alongside it, not just embedded query metadata.
COPY --from=builder /build/target/release/df-server /app/df-server
COPY crates/df-core/migrations /app/migrations

ENV RUST_LOG=info,df_server=info
EXPOSE 8080

ENTRYPOINT ["/app/df-server"]
