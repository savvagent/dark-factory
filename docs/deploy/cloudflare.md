# The console on Cloudflare

Closes the question in [issue #2](https://github.com/savvagent/dark-factory/issues/2):
how to put `web/` on Cloudflare without giving up the single origin the session cookie
depends on.

**One Worker serves the built SPA and proxies everything dynamic to `df-server`.** The
browser sees one hostname. `web/wrangler.jsonc` and `web/worker/index.ts` are the whole of
it; this file is the account-side setup neither of them can express, and the two traps that
a first deploy hits.

Self-hosted deployments are unaffected. `df-server` still serves `web/build` itself, the
`Dockerfile` still bakes it in at `/srv/console`, and nothing in this document is required
to run the product — Cloudflare is a deployment choice, not an architecture.

## Why a Worker rather than Pages, and why not two origins

The console's session is an `HttpOnly`, `__Host-`-prefixed cookie. `__Host-` means the
browser refuses to store it unless it is `Secure`, has `Path=/`, and carries **no `Domain`**
— so it is bound to exactly one origin and cannot be sent to a second one. Hosting the SPA
on `console.example.com` and the API on `api.example.com` does not need a CORS header; it
needs a different authentication transport, which the design rejects.

So whatever fronts the console must *proxy* the API rather than point at it. Given that,
Pages plus a Pages Function and a Worker plus static assets do the same job, and the Worker
is one product, one config file, and one `wrangler deploy`.

A plain CDN passthrough — proxied DNS in front of Fly, cache rules, `df-server` still
serving the bundle — was the other real candidate and remains a reasonable fallback. It
needs no new pipeline and cannot suffer version skew. It also does not put the console on
Cloudflare in any meaningful sense, which is what the issue asked for.

## Deploying

```bash
cd web
npm run build                          # adapter-static → web/build
npx wrangler deploy --env production   # or: npm run deploy, which does both
```

**Naming the environment is load-bearing.** The default configuration is the development
one: a different Worker (`dark-factory-console-dev`) whose `DF_ORIGIN` is
`http://127.0.0.1:8080`. A bare `wrangler deploy` therefore cannot overwrite the Worker the
console runs on, and cannot silently point a staging build at production — it deploys
something that proxies to an origin nobody is running, which fails immediately and visibly.
`--env production` is the only path to the real hostname.

Another environment is a block in `wrangler.jsonc` beside `production`, or a one-off
override:

```bash
npx wrangler deploy --var DF_ORIGIN:https://dark-factory-staging.fly.dev
```

Then attach the hostname the humans will use (`console.example.com`) to the Worker as a
custom domain. That hostname — not the origin's — is the public origin, and every value
below follows from that.

## What must change on the origin

Three environment variables on `df-server`, and none of them is optional.

| Variable | Value | Why |
|---|---|---|
| `DF_PUBLIC_URL` | `https://console.example.com` | The Worker's hostname, never the origin's. The OAuth issuer and both discovery documents are built from it. Point it at the origin and the console tells agents to connect to a host the browser never uses. |
| `DF_ALLOWED_HOSTS` | the origin's hostname, e.g. `dark-factory-mcp.fly.dev` | **See the trap below.** Without it every authenticated MCP call fails. |
| `DF_CLIENT_IP_HEADER` | `cf-connecting-ip` | Cloudflare overwrites this header inbound. `fly-client-ip` would now hold a Cloudflare edge address, and every per-IP throttle would count all of Cloudflare as one caller. |

`DF_RESOURCE_URI` defaults to `$DF_PUBLIC_URL/mcp` and needs nothing.

### Trap 1 — `DF_ALLOWED_HOSTS`, or the MCP endpoint refuses every call

A Worker cannot set the `Host` header on a subrequest; it is derived from the URL being
fetched, so the origin sees `Host: dark-factory-mcp.fly.dev` while `DF_PUBLIC_URL` says
`console.example.com`. `rmcp` validates that header — the check exists because a hosted MCP
server that answers to any `Host` is DNS-rebindable — and rejects the mismatch:

```
$ curl -X POST https://console.example.com/mcp -H "authorization: Bearer df_pat_…" …
Forbidden: Host header is not allowed
```

The failure is worth recognising on sight, because it arrives *only* on authenticated
calls. An unauthenticated `POST /mcp` still answers `401` with a correct
`WWW-Authenticate` challenge, so discovery, registration and the whole OAuth flow look
healthy right up until the first tool call.

Adding the origin's hostname to `DF_ALLOWED_HOSTS` fixes it. An entry with no port matches
any port, which is what the local verification below relies on.

### Trap 2 — the origin must refuse traffic that did not come through Cloudflare

`DF_CLIENT_IP_HEADER=cf-connecting-ip` is safe only because Cloudflare overwrites that
header. Anyone who can reach the Fly app directly sets it themselves, and then every
per-IP throttle — on login, on passkey ceremonies, on client registration — counts a value
the attacker chose, which is worse than having no throttle because it looks like it is
working.

Lock the origin to Cloudflare. In rough order of strength:

1. **Cloudflare Tunnel** (`cloudflared` alongside `df-server`), so the origin has no public
   address at all.
2. **Authenticated Origin Pulls**, so the origin accepts only TLS clients presenting
   Cloudflare's certificate.
3. **Firewall rules** limiting ingress to Cloudflare's published address ranges.

None of these is wired up here: they need the account, the deployed origin, and a domain,
all of which arrive with the first live deploy (milestone 1, task 13). Until then the
choice is written down rather than made — but the deploy is not finished without it.

## Verified locally

`wrangler dev` in front of `cargo run -p df-server`, which is the same shape as the real
thing minus Cloudflare's own edge:

```bash
# origin
DF_PUBLIC_URL=http://localhost:8788 DF_BIND=127.0.0.1:8080 \
DF_ALLOWED_HOSTS=127.0.0.1 DF_CLIENT_IP_HEADER=cf-connecting-ip \
  cargo run -p df-server

# edge
cd web && npm run build
npx wrangler dev --port 8788 --var DF_ORIGIN:http://127.0.0.1:8080
```

What that run established:

- **Routing matches the server's own rule.** `/`, `/o/acme/queue`, `/apiary` and
  `/mcp-guide` all render the SPA; `/api` and `/api/no/such/thing` answer the origin's JSON
  `404`; `/healthz` and `/readyz` answer JSON, not HTML. `/apiary` is the case that matters:
  it is a legal org slug, and a prefix test that is not segment-aware sends it to the origin.
- **The session cookie survives the edge intact** — `__Host-df_session; Path=/; HttpOnly;
  Secure; SameSite=Lax; Max-Age=1209600`, which is the whole of issue #2's third checkbox.
  Sign-up, emailed verification link, TOTP enrolment, org creation and PAT minting were all
  driven through the Worker. (This run predates the removal of email and TOTP — see the
  milestone plan's Task 6 for what replaced them; a re-run today would drive a passkey
  ceremony instead.)
- **The OAuth issuer names the edge**, because it is built from `DF_PUBLIC_URL`. The minted
  PAT's audience came back as `http://localhost:8788/mcp`.
- **`303`s pass through.** `POST /oauth/authorize` returned `303` with
  `Location: http://localhost:3118/callback?code=…` rather than a followed body — the Worker
  sets `redirect: 'manual'`, without which Cloudflare would fetch the client's callback
  itself, burn the single-use code, and leave the agent waiting forever.
- **A forged client address does not reach the origin.** A request carrying
  `cf-connecting-ip: 9.9.9.9` was recorded by the origin under the throttle bucket
  `link:127.0.0.1`.
- **Trap 1 reproduces and the fix works.** Without `DF_ALLOWED_HOSTS`, `whoami` over MCP
  returned `Forbidden: Host header is not allowed`; with it, `org: edge | kind: pat`.

## Caching

The Worker forwards origin paths with `cf: { cacheTtlByStatus: { '200-599': -1 } }` — a
negative TTL, which is the documented way to say "never", where `cacheTtl: 0` only means
"already expired". Nothing under `/api`, `/oauth`, `/.well-known` or `/mcp` is cacheable and
some of it is per-session: a heuristically cached `GET /api/me` is one user's identity
served to another.

Static assets are a different matter and need no configuration. `adapter-static` emits
content-hashed filenames under `_app/immutable/`, and Workers static assets serve and cache
them on Cloudflare's network without the Worker being involved in the response body.

## What is still open

- The origin lock (trap 2) is documented, not implemented.
- There is no CI step that builds or deploys the Worker, because there is no CI
  (milestone 1, task 1).
- The Worker's copy of the bundle and the copy inside the `df-server` image are deployed
  separately, so they can drift. Whoever wires up CI should deploy both from one commit.
- `worker/index.test.ts` asserts the Worker's prefix rule against the same cases the Rust
  side asserts, but nothing mechanically ties the two lists together. A third copy would
  need the same treatment.
