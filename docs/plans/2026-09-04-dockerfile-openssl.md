# Dockerfile: install pkg-config/libssl-dev for the passkey build — implementation plan

**Goal:** Make `podman build -t dark-factory .` (and therefore `fly deploy`) succeed again from
current `master`. Since `a12fdf9` ("Replace TOTP with passkeys"), `df-auth`'s dependency on
`webauthn-rs` transitively pulls in `webauthn-attestation-ca`, which links `openssl` (via
`openssl-sys`) for attestation certificate verification — a real, non-dev dependency of
`df-server`'s release build. The Rust build stage in `Dockerfile` (`rust:1-slim-bookworm`) has
no `pkg-config` or OpenSSL development headers, so `openssl-sys`'s build script fails and the
image cannot be built at all. `.github/workflows/ci.yml` never builds the Docker image, so this
went undetected by CI; it was only caught by attempting a fresh Fly deploy.

## Status — 2026-09-04

✅ Done — fix committed, verified locally with `podman build`.

## Spec

`docs/specs/2026-09-04-dockerfile-openssl-design.md` — read it first. This plan implements it
exactly.

**Correction:** an earlier draft of this plan claimed fast-path eligibility. That was wrong —
`dark-factory-development`'s fast-path criteria explicitly exclude changes to deploy/distribution
shape, and `Dockerfile` is named explicitly. The change's *content* is trivial (one dependency
install, no interface change), but its *surface* disqualifies the carve-out, so the design spec
above was written and critiqued rather than skipped.

## Global Constraints

- No AI self-attribution in the commit or PR.
- `cargo fmt --all` — not applicable; no Rust source changed.
- No SQL, no tenant table, no MCP tool, no config surface, no schema change — vacuously satisfied.

## File Structure

| File | Responsibility |
|---|---|
| `Dockerfile` | **Modify.** Add `pkg-config` + `libssl-dev` to the Rust build stage before `cargo build`. |

## Task 1 — Fix the Docker build stage — ✅

**Files:** `Dockerfile`

- [x] Reproduce the failure: `podman build -t dark-factory .` fails during
      `cargo build --release -p df-server` with "Could not find openssl via pkg-config" /
      "pkg-config command could not be found."
- [x] Add `RUN apt-get update && apt-get install -y --no-install-recommends pkg-config
      libssl-dev && rm -rf /var/lib/apt/lists/*` to the `build` stage, before `COPY . .` (so it
      cache-layers ahead of source changes) and before the `cargo build` step, with a comment
      explaining why passkeys made this necessary.
- [x] Re-run `podman build -t dark-factory .` — confirm it completes and produces a runtime
      image that starts (`/healthz` / `/readyz` reachable), not just that `cargo build` alone
      succeeds outside the container.
- [x] Format and commit: `git commit -m "df-server: install pkg-config/libssl-dev in the Docker
      build stage"`.

## Out-of-band verification (Phase 5, step 14)

Container image touched → build it: `podman build -t dark-factory .` — done above, succeeded.
No other out-of-band surface (`web/`, `web/worker/`, migrations, CI workflow, `DF_*` config)
touched by this change.
