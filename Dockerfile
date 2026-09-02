# syntax=docker/dockerfile:1

# ---------------------------------------------------------------- console
# The SPA is built first and separately. It changes on a different cadence
# from the server and shares none of its toolchain, so a Rust edit must not
# reinstall npm packages and a console edit must not recompile a workspace.
FROM node:22-slim AS console

WORKDIR /web
# Manifests alone, so `npm ci` is cached until a dependency actually changes.
COPY web/package.json web/package-lock.json ./
RUN npm ci

COPY web/ ./
RUN npm run build

# ---------------------------------------------------------------- server
FROM rust:1-slim-bookworm AS build

WORKDIR /app
COPY . .

# No DATABASE_URL, and no `.sqlx` offline data, because there is nothing to
# check against at compile time: every statement in df-core is a runtime
# `sqlx::query`, not a `query!` macro. That is what makes this image buildable
# on a machine with no database.
#
# The cache mounts hold the registry and the target directory across builds.
# `target/` lives inside one, so the binary has to be copied out within the
# same RUN — anything left there vanishes with the mount.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    cargo build --release -p df-server \
    && cp target/release/df-server /usr/local/bin/df-server

# ---------------------------------------------------------------- runtime
FROM debian:bookworm-slim AS runtime

# ca-certificates is not optional: Postgres over TLS and every outbound tracker
# call verify against these roots, and without them the failure is a TLS error
# that reads like a network problem.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Nothing here needs to write to the filesystem or bind a privileged port.
# A high uid rather than --system, which warns above SYS_UID_MAX, and no shell.
RUN useradd --create-home --uid 10001 --shell /usr/sbin/nologin factory
USER factory

COPY --from=build   /usr/local/bin/df-server /usr/local/bin/df-server
COPY --from=console /web/build               /srv/console

# The console bundle's location is the one path baked into the image, so it is
# the one that gets a default here rather than in every deployment.
ENV DF_STATIC_DIR=/srv/console \
    DF_BIND=0.0.0.0:8080 \
    DF_LOG_FORMAT=json \
    RUST_LOG=info

EXPOSE 8080

# Exec form, so the process is PID 1 and receives SIGTERM directly. Under the
# shell form it would be a child of /bin/sh, which does not forward signals —
# the graceful shutdown would never run and every deploy would hard-kill
# whatever was in flight.
ENTRYPOINT ["/usr/local/bin/df-server"]
