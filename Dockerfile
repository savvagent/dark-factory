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

# Deliberately not `COPY rust-toolchain.toml`: this image's rustc is 1.85, but
# it ships via rustup, and rustup honors a `rust-toolchain.toml` it finds in
# the working directory — including its `channel = "stable"`. Copying the
# file in would make cargo fetch and install whatever "stable" is that day
# (plus rustfmt/clippy, neither of which this build needs) instead of using
# the 1.85 already on the image, silently defeating the `FROM` tag's pin and
# making the build depend on rustup's network availability.
#
# Manifests are copied ahead of `crates/` so a lockfile-only change does not
# require re-copying source, but this is not full dependency-layer caching:
# there is one `cargo build` below, over the whole workspace, so any source
# edit under `crates/` still invalidates it. A `cargo-chef`-style recipe stage
# would get real separate caching; not worth the added moving parts yet at
# this build's size.
COPY Cargo.toml Cargo.lock ./
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
