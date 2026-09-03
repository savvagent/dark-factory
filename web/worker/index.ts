/**
 * The console at the edge, and the one origin a browser is ever allowed to see.
 *
 * Cloudflare serves the built SPA from its own network and forwards everything
 * dynamic — `/api`, `/oauth`, `/.well-known`, `/mcp`, and the health probes — to
 * `df-server`. The browser talks to exactly one hostname, which is not a
 * performance decision: the console's session is an `HttpOnly`, `__Host-`
 * prefixed cookie, and `__Host-` means the browser refuses to store it unless it
 * is `Secure`, has `Path=/`, and carries no `Domain`. Put the SPA on one
 * hostname and the API on another and that cookie cannot be sent at all — no
 * CORS header rescues it, and the fix would be a different auth transport, which
 * `docs/specs/2026-09-01-dark-factory-design.md` refuses on purpose.
 *
 * This Worker therefore proxies rather than redirects, and it is the reason the
 * split-origin option in the issue was never really an option.
 */

/**
 * Path prefixes that belong to `df-server` rather than to the console's own
 * client-side routing. **This list mirrors `API_PREFIXES` in
 * `crates/df-server/src/lib.rs`** and must not drift from it: the server keeps
 * the same list to decide what gets a JSON `404` instead of `index.html`, and a
 * prefix that exists on one side only is a route that behaves differently
 * depending on whether Cloudflare is in front.
 *
 * `/healthz` and `/readyz` are here and not on the server's list because the
 * server mounts them as real routes ahead of its SPA fallback. At the edge there
 * is no such precedence — anything not proxied is answered from the asset
 * bundle — so leaving them out would make `/readyz` return the console's HTML
 * with a `200`, which is the shape that keeps a health check green while the
 * database is gone.
 */
const ORIGIN_PREFIXES = ['/api', '/oauth', '/mcp', '/.well-known', '/healthz', '/readyz'];

/**
 * Prefix matching on segment boundaries, exactly as the server does it.
 *
 * `startsWith` alone is wrong and quietly so: `/apiary` is a legal org slug and
 * belongs to the console, and `/mcp-guide` is a page. Sending either to the
 * origin would answer a JSON `404` for a route the SPA was going to render.
 */
export function belongsToOrigin(path: string): boolean {
  return ORIGIN_PREFIXES.some((prefix) => path === prefix || path.startsWith(`${prefix}/`));
}

export interface Env {
  /** The built `web/build` bundle, uploaded with the Worker. */
  ASSETS: Fetcher;
  /** Where `df-server` actually listens, e.g. `https://dark-factory-mcp.fly.dev`. */
  DF_ORIGIN: string;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    if (!belongsToOrigin(url.pathname)) {
      // `not_found_handling: "single-page-application"` turns an unknown path
      // into `index.html`, which is what makes a hard refresh of
      // `/o/acme/queue` work.
      return env.ASSETS.fetch(request);
    }

    if (!env.DF_ORIGIN) {
      // Loud, and in the one place it can be seen. Without this the failure is
      // a URL constructor throwing inside a proxy hop, which reaches the caller
      // as a bare 500 with nothing naming the cause.
      return new Response(
        JSON.stringify({
          error: 'misconfigured',
          error_description:
            'this Worker has no DF_ORIGIN, so it does not know where df-server is. ' +
            'Deploy with --env production, or pass --var DF_ORIGIN:https://…'
        }),
        { status: 500, headers: { 'content-type': 'application/json' } }
      );
    }

    const target = new URL(url.pathname + url.search, env.DF_ORIGIN);
    const headers = new Headers(request.headers);

    // The origin keys every per-IP throttle on this header
    // (`DF_CLIENT_IP_HEADER=cf-connecting-ip`), so what it contains has to be
    // the platform's value and never the caller's. Cloudflare overwrites
    // `CF-Connecting-IP` before the Worker is invoked, which is what makes the
    // inbound value safe to forward: a request sent here with
    // `cf-connecting-ip: 9.9.9.9` reached the origin as `127.0.0.1` under
    // `wrangler dev`, the caller's value having been discarded on the way in.
    // The `delete` is for the case where there is no value at all — an empty
    // header would become one throttle bucket shared by everyone.
    //
    // None of this holds if the origin can be reached without going through
    // Cloudflare: a direct caller sets the header itself and every throttle then
    // counts something the attacker chose. Locking the origin down is therefore
    // part of the deployment, not an optimisation — see docs/deploy/cloudflare.md.
    const clientIp = request.headers.get('cf-connecting-ip');
    if (clientIp) {
      headers.set('cf-connecting-ip', clientIp);
    } else {
      headers.delete('cf-connecting-ip');
    }

    return fetch(
      new Request(target, {
        method: request.method,
        headers,
        body: request.body,
        // A proxy that follows redirects is not a proxy. `/oauth/authorize`
        // answers `303` to a loopback address the *client* is listening on —
        // following it here would fetch the callback from Cloudflare, burn the
        // single-use authorization code, and leave the agent waiting forever.
        // This was not theoretical: it is how the first scripted run of the
        // task 12 conformance flow failed.
        redirect: 'manual',
        // Nothing on these paths is cacheable and some of it is per-session.
        // A heuristically cached `GET /api/me` is one user's identity served to
        // another. A negative TTL is the documented way to say "never", where
        // `cacheTtl: 0` only means "expired".
        cf: { cacheTtlByStatus: { '200-599': -1 } }
      })
    );
  }
} satisfies ExportedHandler<Env>;
