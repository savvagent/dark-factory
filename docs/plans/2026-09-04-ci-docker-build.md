# CI: build the Docker image on every PR/push — implementation plan

**Goal:** Add a `docker-build` job to `.github/workflows/ci.yml` that builds the full multi-stage
image (`console` → `build` → `runtime`) on every push to `master` and on any PR touching
`Dockerfile`, `**/Cargo.toml`, `Cargo.lock`, or `web/**`, failing the check the same way
`cargo test`/`clippy` do today if the build fails. No image is pushed or deployed — this closes the
gap that let the Docker image silently fail to build for days after the passkey migration
(`a12fdf9`, fixed in #37) without CI ever noticing, because `cargo test` doesn't need the OpenSSL
dev headers the container's build stage lacked.

## Status — 2026-09-04

⬜ Not started.

## Spec

`docs/specs/2026-09-04-ci-docker-build-design.md` — read it first. This plan implements it exactly.

## Global Constraints

- No AI self-attribution in the commit or PR.
- No SQL, no tenant table, no MCP tool, no config surface, no schema, no auth-spine change —
  vacuously satisfied; this is pure CI infrastructure.
- No change to deploy behavior: `push: false` on the build action, no registry credentials, no
  `fly deploy` invocation anywhere in this change.
- No change to the existing `rust`/`web` jobs' triggers or behavior — the new job's path filter
  must not gate the workflow's shared `on:` block (which would silently suppress `rust`/`web` on
  unrelated PRs), only the `docker-build` job's own steps.
- Path filter must cover `Dockerfile`, `**/Cargo.toml` (root **and** every `crates/*/Cargo.toml`),
  `Cargo.lock`, and `web/**` — per the spec's §1/Scope, generalized beyond the issue's literal
  `Cargo.toml` wording to the workspace's actual multi-crate manifest layout.
- Validate workflow YAML syntax before commit (no `actionlint` available in this environment; use
  a `yaml.safe_load` parse check instead).

## File Structure

| File | Responsibility |
|---|---|
| `.github/workflows/ci.yml` | **Modify.** Add the `docker-build` job alongside the existing `rust` and `web` jobs. |

## Task Order & Rationale

Single task: this is one YAML addition to one existing file. No sequencing decision to make.

## Task 1 — Add the `docker-build` CI job — ⬜

**Files:** `.github/workflows/ci.yml`

**Interfaces:** Produces a new GitHub Actions check named `docker-build` on every PR and push to
`master`. Consumes nothing new — no new secrets, no new repo settings required (`docker/build-push-action@v6`'s GHA cache backend uses the workflow's own ambient
`ACTIONS_CACHE_URL`/`ACTIONS_RUNTIME_TOKEN`, already available to every job).

- [ ] Add the `docker-build` job to `.github/workflows/ci.yml`, placed after the existing `web`
      job, exactly per spec §1:
      - `runs-on: ubuntu-latest`
      - `actions/checkout@v4`
      - a `dorny/paths-filter@v3` step (`id: filter`), gated `if: github.event_name == 'pull_request'`,
        with a `filters:` block naming the `image` output over `Dockerfile`, `**/Cargo.toml`,
        `Cargo.lock`, `web/**`
      - `docker/setup-buildx-action@v3`, gated `if: github.event_name == 'push' ||
        steps.filter.outputs.image == 'true'`
      - a `docker/build-push-action@v6` step ("Build image"), same gating condition, with
        `context: .`, `push: false`, `cache-from: type=gha`, `cache-to: type=gha,mode=max`
      - a leading comment on the job explaining why it exists (mirrors the spec's §1 comment: cargo
        test doesn't need the OpenSSL dev headers the container lacked, so it can't catch this class
        of break; this job proves the image itself builds)
      - the job carries **no job-level `if:`** — only its steps skip, so a PR untouched by the
        filter still reports a real (not disappeared) success, per spec §1/Error Handling
- [ ] Validate the YAML parses: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))"`
      — expect no exception, and manually re-read the rendered job to confirm the `rust` and `web`
      jobs are byte-for-byte unchanged (no accidental reflow/indentation change from an editor).
- [ ] Local equivalence check (no GitHub Actions runner available here): confirm the Dockerfile
      itself still builds with the engine available on this machine — `podman build -t
      dark-factory-ci-check .` from the repo root — so a locally-detectable break isn't shipped
      inside the same PR as the new gate. Expect: build completes successfully (it already did as
      of #37's fix; this is a regression check, not new ground).
- [ ] Format and commit: `git commit -m "ci: build the Docker image on every PR/push touching Dockerfile, Cargo manifests, or web/"`.
      (No `cargo fmt`/`clippy`/`cargo test` gate applies — no Rust source changed. No `web/` gate
      applies — no console source changed.)

## Out-of-band verification (Phase 5, step 14)

- **CI** — `.github/workflows/` changed: confirm the workflow parses (done above) and that it
  actually runs on this PR. Because this PR's own diff is `.github/workflows/ci.yml` only (not
  `Dockerfile`/`Cargo.lock`/`web/**`), the new job will report success via its no-op path on this
  PR itself (per spec §Risks) — check the Actions run for this PR shows `docker-build` present and
  green, with its build step skipped (not run), and separately confirm via local `podman build`
  that the image itself is buildable, since this PR cannot self-exercise the "PR touching
  Dockerfile" path. The `push`-to-`master` path is exercised for real once this PR merges — verify
  post-merge in Phase 5 step 13/14 that the `master` push's `docker-build` job actually ran the
  build (not skipped) and passed.
- **Container image** — not modified by this change (`Dockerfile` untouched), but the new job now
  builds it as part of CI; the local `podman build` check above stands in for the CI-native
  `docker build` this environment can't run directly.
- No other out-of-band surface (`web/`, `web/worker/`, migrations, `DF_*` config) is touched.
