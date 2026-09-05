# CI Docker image build gate design

> **Status:** DRAFT — adds a build-verification job so the deploy artifact is exercised on every PR/push
> **Depends on:** `docs/specs/2026-09-04-dockerfile-openssl-design.md` (the incident this closes the gap for)

## Assumptions

- The job proves the image **builds**; it never pushes, tags for a registry, or deploys. Deploy
  remains manual and out of band per `CLAUDE.md` — this is a build-verification gate only,
  matching the issue's explicit non-goal.
- `docker build` (not `podman build`) is used inside the GitHub Actions runner: `docker` is
  preinstalled on `ubuntu-latest` runners and is what `actions/checkout` + first-party Docker
  actions (`docker/build-push-action`, `docker/setup-buildx-action`) are built around. Local dev
  tooling uses `podman` (per `CLAUDE.md`'s `podman compose up -d` and `docs/deploy/fly.md`)
  because it's what's available on this machine, not because the CI runner requires it — the
  `Dockerfile` itself is engine-agnostic (a syntax-versioned multi-stage `Dockerfile`, no
  Podman-specific instructions), so either engine builds it identically. Using `docker` in CI
  avoids installing a third-party Podman setup action on a runner that already ships Docker.
- `docker/build-push-action@v6` with `push: false` is used rather than a bare `docker build .`
  shell step, because it gives layer caching (`cache-from`/`cache-to: type=gha`) for free via the
  GitHub Actions cache backend — the multi-stage image (console + rust + runtime) is expensive to
  rebuild from scratch on every PR otherwise, and BuildKit's GHA cache backend is the
  first-party-documented way to do this without standing up any external registry or storage.
- Path filtering: the job runs on any push to `master`, and on PRs that touch `Dockerfile`,
  `Cargo.toml`, `Cargo.lock`, or `web/**` — exactly the surfaces the issue names, because those
  are the only inputs to the image build (`web/` for the console stage, `Cargo.toml`/`Cargo.lock`
  for the dependency graph the rust stage compiles, `Dockerfile` for the build recipe itself). A
  change to, say, `docs/**` or a test-only Rust file with no `Cargo.lock` delta gets no image
  build — it can't affect the image.
- The new job is independent of (does not depend on / is not depended on by) the existing `rust`
  and `web` jobs — it runs in parallel, matching this workflow's existing structure where `rust`
  and `web` already run as independent jobs with no `needs:`.
- No new public interface, no schema change, no tenant table, no MCP tool, no `DF_*` config key,
  no auth-spine change. The three constraints in `CLAUDE.md` are not implicated — this is pure CI
  infrastructure, not a product capability, so there is no "could this live in a customer skill"
  question to ask.
- The job needs no database service (unlike `rust`): the image build never runs `cargo test`,
  only `cargo build --release -p df-server`, so nothing inside it touches Postgres.

## Premise corrections

- None. The issue's premise — no CI job builds the Docker image today — was confirmed directly
  by reading `.github/workflows/ci.yml`: only `rust` (fmt/clippy/test) and `web` (check/lint/test)
  jobs exist; neither invokes `docker build`/`podman build`.

## Scope

**In:**
- A new `docker-build` job in `.github/workflows/ci.yml` that builds the full multi-stage image
  (`console` → `build` → `runtime`) via `docker/build-push-action@v6` with `push: false`.
- Path-filtered triggers: always on `push` to `master`; on `pull_request` only when the diff
  touches `Dockerfile`, `Cargo.toml`, `Cargo.lock`, or `web/**`.
- GitHub Actions cache (`cache-from`/`cache-to: type=gha`) so repeated builds of an unchanged
  layer (e.g. the `console` stage's `npm ci` when only Rust source changed within a triggering
  `Cargo.lock` bump) don't pay full cost every run.

**Out:**
- Pushing/tagging the built image to any registry (GHCR, Docker Hub, Fly's registry). The issue
  explicitly rules this out; deploy stays manual (`fly deploy`).
- Running the built image and smoke-testing `/healthz`/`/readyz` in CI. Valuable, but a separate
  concern from "does the image build" and adds runtime complexity (network namespace, env vars,
  a fake `DATABASE_URL`) the issue doesn't ask for. Left as a possible future follow-up, not
  implemented here.
- Changing `Dockerfile` itself. The image already builds correctly as of `docs/specs/2026-09-04-
  dockerfile-openssl-design.md`; this ticket only adds the CI gate that would have caught the
  regression that spec fixed.
- Multi-platform builds (`linux/amd64` + `linux/arm64`). Fly.io deploys `linux/amd64` only
  (`docs/deploy/fly.md`); building extra platforms in CI would slow every run for a target
  nothing deploys to.
- Any change to the `rust` or `web` jobs' existing behavior.

## §1 Job definition

Add to `.github/workflows/ci.yml`, alongside the existing `rust` and `web` jobs:

```yaml
  # Proves the deploy artifact actually builds. cargo test/clippy above never
  # exercise Dockerfile — a new crate dependency (openssl-sys via webauthn-rs,
  # see docs/specs/2026-09-04-dockerfile-openssl-design.md) can break the
  # image build while cargo test stays green, because the host running
  # cargo test already has the dev packages the container doesn't. This job
  # closes that gap: it never pushes anywhere, it only proves `docker build`
  # succeeds, the same way cargo test proves the binary builds and passes.
  docker-build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Check for image-relevant changes
        if: github.event_name == 'pull_request'
        id: filter
        uses: dorny/paths-filter@v3
        with:
          filters: |
            image:
              - 'Dockerfile'
              - 'Cargo.toml'
              - 'Cargo.lock'
              - 'web/**'

      - uses: docker/setup-buildx-action@v3
        if: github.event_name == 'push' || steps.filter.outputs.image == 'true'

      - name: Build image
        if: github.event_name == 'push' || steps.filter.outputs.image == 'true'
        uses: docker/build-push-action@v6
        with:
          context: .
          push: false
          cache-from: type=gha
          cache-to: type=gha,mode=max
```

The mechanism: `dorny/paths-filter@v3` evaluates the PR diff against the listed globs and exposes
`steps.filter.outputs.image` as `'true'`/`'false'`; every subsequent step gates on
`github.event_name == 'push' || steps.filter.outputs.image == 'true'` so a push to `master` always
builds and a PR only builds when it touches a listed path. The job itself carries no `if:` —
letting its steps individually skip (rather than skipping the whole job) is what makes it report
a real, inspectable "no-op success" instead of the job disappearing entirely, which matters for
anyone later making it a required status check.

## §2 Trigger mechanism — why a filter step, not `on: pull_request: paths:`

`.github/workflows/ci.yml` has one `on:` block shared by every job (`rust`, `web`, and the new
`docker-build`). A top-level `on: pull_request: paths: [...]` would suppress the **entire
workflow** — including `rust` and `web` — for a PR that doesn't touch those paths, which is wrong:
`rust` and `web` must keep running on every PR regardless of what changed. Job-level path
filtering isn't a native GitHub Actions primitive (`on.<event>.paths` only exists at the workflow
level), so `dorny/paths-filter@v3` (a widely used, already-battle-tested action for exactly this)
computes the changed-paths decision as a job output and every build step conditions on it. This
keeps `rust`/`web` unconditional and only gates `docker-build`'s steps.

On `push` to `master` there is no "diff against a base" (a push event has no PR base ref in the
same sense), and the issue's acceptance criterion says "on every push to master" unconditionally —
so the push path skips the filter step entirely and always builds.

## §3 Cache scope and cost

`type=gha` cache entries are scoped per-workflow by default and evicted under GitHub's standard
10 GB-per-repo cache eviction policy (least-recently-used). A stale cache on a long-idle branch
just means a slower first build after the eviction, not a correctness problem — BuildKit falls
back to a full rebuild for any layer whose cache entry is missing or invalid. No new secrets or
external storage are introduced; the GHA cache backend authenticates with the workflow's own
`ACTIONS_CACHE_URL`/`ACTIONS_RUNTIME_TOKEN`, already available to every job with no additional
permissions grant.

## §4 Failure semantics

`docker/build-push-action@v6` exits non-zero (surfacing as a failed step, failed job, failed
check) exactly when the underlying `docker buildx build` fails — the same failure mode `cargo
test`/`cargo clippy` already produce for the `rust` job. No `continue-on-error`, no
`if: always()` swallowing — a broken image build blocks the PR the same way a broken test does,
per the issue's second acceptance criterion.

## §5 Testing

- Validate the workflow YAML parses: `actionlint` isn't installed in this environment, but
  `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))"` (or equivalent)
  confirms syntactic validity before commit.
- Push the branch and open the PR (which itself touches `Dockerfile`? — no, this PR touches only
  `.github/workflows/ci.yml`, so the new job's own PR will **not** trigger the image build via the
  path filter; see Risks below). Confirm the job is present in the Actions UI and, on a follow-up
  PR that touches `Dockerfile`/`Cargo.lock`/`web/**` (or by pushing to `master` after merge, or by
  temporarily touching `Dockerfile` with a no-op comment during verification), confirm it runs and
  passes.
- No `cargo test`/`clippy` impact: no Rust source changes.

## Error Handling & Edge Cases

- **A PR that touches none of the filtered paths.** The `docker-build` job still appears as a
  required-looking job in the Actions UI but its steps after the filter check are skipped
  (`if:` false), so it reports success trivially rather than failing or hanging — this is the
  same "vacuously satisfied" shape as any other conditional CI step, and avoids making the job a
  spurious required-check blocker for unrelated PRs.
- **`dorny/paths-filter` on a push event.** The action's path-filtering step is itself gated to
  `if: github.event_name == 'pull_request'`, so it never runs (and never needs a base ref to diff
  against) on `push` — the push path always builds unconditionally per the acceptance criterion.
- **This PR itself.** Since this PR's diff is `.github/workflows/ci.yml` only, the new job will
  report success without actually invoking a build on this PR (filter says no match). This is
  called out explicitly in the PR body and verified out-of-band (Phase 5, step 14) by running
  `docker build .` locally (or `podman build .`, this machine's available engine) instead of
  relying on this PR's own check run to prove the job works end-to-end.

## Risks & Open Questions

- **This PR can't self-verify the "PR touching Dockerfile" trigger path**, since it only touches
  the workflow file. Mitigated by local verification (`podman build -t dark-factory .`, this
  machine's available engine — functionally equivalent to `docker build` for this Dockerfile) and
  by the `push`-to-`master` path being unconditional, which the merge of this very PR will
  exercise for real once it lands. Whoever authors the next PR touching `Dockerfile`/`Cargo.lock`/
  `web/**` gets the first real end-to-end proof of the filtered path; noted for the architect
  reviewer as an accepted gap rather than a defect.
- **`dorny/paths-filter@v3` is a third-party action**, not a GitHub-authored one. It's a widely
  used (thousands of dependents), narrowly-scoped action (path-diffing only, no code execution
  beyond that) and is the standard idiom for job-level path filtering since GitHub Actions has no
  native primitive for it. Pinned to a major version tag (`@v3`), matching this workflow's existing
  convention for third-party actions (`Swatinem/rust-cache@v2`).
