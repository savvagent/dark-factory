# Docker build stage missing OpenSSL toolchain design

> **Status:** DRAFT — fixes the container image build broken since the TOTP→passkey migration

## Assumptions

- The fix is purely a build-dependency addition (`pkg-config` + `libssl-dev` in the discarded
  Rust build stage) with no change to the shipped runtime image's contents, no new public
  interface, and no schema/config/auth-spine/tenant-isolation/metering impact. The
  **content** of the change is trivial; only its **surface** (`Dockerfile`, i.e. deploy shape)
  disqualifies it from `dark-factory-development`'s fast-path, per that skill's explicit
  exclusion of `Dockerfile` changes from the trivial-task carve-out. This spec exists to
  satisfy that rule, not because the change is architecturally complex.
- Verified locally with `podman build -t dark-factory .` (succeeds) and by starting the
  resulting image (`podman run`) — the binary starts and reaches the database-dial step with
  no dynamic-linking error, confirming `debian:bookworm-slim`'s existing `libssl3` (pulled in
  transitively, not by this change) satisfies the binary's runtime OpenSSL dependency.
- No change to `fly.toml`, no change to CI (`.github/workflows/ci.yml`), no change to
  `web/`/`web/worker/`. Those remain out of scope.

## Premise corrections

- None. The premise — that the Dockerfile fails to build since passkeys were introduced — was
  confirmed by direct reproduction (`podman build` failure with the exact `openssl-sys`
  pkg-config error), not assumed.

## Scope

**In:**
- Add `pkg-config` and `libssl-dev` to the `build` stage of `Dockerfile`, installed before
  `COPY . .` so the layer caches independently of source changes.

**Out:**
- Switching `openssl-sys`/`webauthn-attestation-ca` to a vendored/static OpenSSL build. This
  would avoid needing `libssl-dev` in the builder at the cost of a slower first build and a
  larger dependency on the C toolchain already present in `rust:1-slim-bookworm`. Not pursued
  here: it is a bigger change to the build's risk surface for a fix that only needs to restore
  a previously-working image build, and the dev packages already do not reach the runtime
  image (confirmed below).
- Adding a CI job that builds the Docker image on every PR/push. This is a real, structurally
  sound follow-up — `.github/workflows/ci.yml` does not exercise `podman build`/`docker build`
  at all, which is exactly why this defect went undetected across a full merge and multiple
  Fly deploy attempts — but it is CI/deploy-automation scope, not a Dockerfile line fix, and
  belongs in its own ticket rather than expanding this one. Filed as a follow-up recommendation
  in the PR, not implemented here.
- Anything touching `fly.toml`, `DF_*` config, migrations, or the console (`web/`) — none of
  those are implicated by this defect.
- The three architectural constraints in `CLAUDE.md` (repo-anchored coordination,
  substrate-not-workflow, coding-agent agnostic) are not implicated: this is pure build
  infrastructure with zero product-surface change, so there is no "could this live in a
  customer skill instead" question to ask.

## §1 Root cause

`df-auth`'s dependency on `webauthn-rs` (introduced in `a12fdf9`, "Replace TOTP with
passkeys") transitively pulls in `webauthn-attestation-ca`, which links `openssl` (via
`openssl-sys`) for attestation certificate verification — confirmed via
`cargo tree -p df-server -e normal | grep -B5 openssl-sys`, which shows the dependency path
through `webauthn-rs` → `webauthn-rs-core` → `webauthn-attestation-ca` → `openssl`/`openssl-sys`.
This is a **normal**, not dev-only, dependency of `df-server`'s release build — `reqwest` in
this workspace is already configured with `default-features = false, features =
["rustls-tls", "json"]` (`Cargo.toml`), so this is not a reqwest/TLS-backend regression; it is
specifically the WebAuthn attestation stack.

The Docker build stage (`FROM rust:1-slim-bookworm AS build`) had no `pkg-config` or OpenSSL
development headers, so `openssl-sys`'s build script fails at compile time:
`Could not find openssl via pkg-config` / `pkg-config command could not be found`. Since
`.github/workflows/ci.yml` never builds the Docker image (only `cargo test`/`clippy`/`fmt` and
the `web` job), this went undetected by CI. It surfaced only when a fresh `fly deploy` /
`podman build` was attempted, which is why the live Fly.io deployment (`dark-factory-mcp`) has
silently continued running the pre-passkey (TOTP) build since the passkey merge — every image
build attempted since then has failed outright, so nothing new was ever pushed.

## §2 Fix

Add, in the `build` stage, before `COPY . .`:

```dockerfile
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
```

Placed before `COPY . .` so this layer caches independently of source changes (same rationale
as the existing `console` stage's `COPY web/package.json web/package-lock.json ./` /
`RUN npm ci` ordering).

## §3 Runtime image contents — unaffected

The `runtime` stage is built independently, `FROM debian:bookworm-slim`, and receives only the
compiled binary and static console assets via `COPY --from=build` / `COPY --from=console`. The
`build` stage (carrying `pkg-config`/`libssl-dev`) is discarded entirely; neither package nor
any part of a full OpenSSL dev toolchain reaches the shipped image. Confirmed by inspecting the
built runtime image directly: `libssl.so.3` is dynamically linked by the `df-server` binary
(`ldd`), and is already present in `debian:bookworm-slim` as `libssl3` (a transitive base-image
dependency, `dpkg -l | grep libssl` inside the built image) — unrelated to and unchanged by
this fix, since `libssl3` is a runtime shared library, not the `-dev` headers package added to
the builder.

## §4 Testing

- `podman build -t dark-factory .` — reproduced the failure before the fix, confirmed the fix
  resolves it (full image build completes).
- `podman run` against the built image, with a deliberately invalid `DATABASE_URL` — confirmed
  the binary starts and reaches the database-connection attempt (fails only on DNS resolution
  of the fake host), i.e. no missing-shared-library startup failure.
- No `cargo test`/`clippy` impact: no Rust source changed.

## Error Handling & Edge Cases

- None introduced. This is a build-time dependency addition with a deterministic, already-
  verified outcome (image builds, binary starts).

## Risks & Open Questions

- **No CI gate on the Docker image build.** This is the actual root cause of why the defect
  went unnoticed for as long as it did (multiple Fly deploy attempts, silently continuing to
  serve the stale build). Recommended as a follow-up ticket: add a job to
  `.github/workflows/ci.yml` (or a separate workflow) that runs `docker build .` /
  `podman build .` on every PR touching `Dockerfile`, the workspace `Cargo.toml`/`Cargo.lock`,
  or `web/`. Not implemented in this PR — out of scope per above.
- Unpinned `apt-get install` package versions (`pkg-config`, `libssl-dev`) mirror the existing,
  pre-existing pattern in the `runtime` stage's `ca-certificates` install and the mutable
  `rust:1-slim-bookworm` / `debian:bookworm-slim` base image tags already in use; not a new
  reproducibility regression introduced by this change.
