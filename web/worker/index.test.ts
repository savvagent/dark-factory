import { describe, expect, it } from 'vitest';

import { belongsToOrigin } from './index';

/**
 * `belongsToOrigin` is a second copy of `API_PREFIXES` in
 * `crates/df-server/src/lib.rs`, in a different language, that nothing forces to
 * agree with the first. The Rust side has
 * `api_prefixes_do_not_match_by_string_prefix_alone` asserting exactly these
 * cases; this is its other half.
 *
 * Drift here is silent in the worst direction. If the edge stops recognising a
 * path as the origin's, the asset router answers it from `index.html` with a
 * `200`, and an agent polling `/api/orgs/nope` gets HTML forever instead of the
 * `404` it can parse.
 */
describe('belongsToOrigin', () => {
  it('claims the API surfaces, bare and nested', () => {
    for (const path of [
      '/api',
      '/api/',
      '/api/orgs/acme',
      '/api/orgs/acme/jobs/job-1',
      '/oauth',
      '/oauth/authorize',
      '/oauth/token',
      '/mcp',
      '/.well-known',
      '/.well-known/oauth-protected-resource',
      '/.well-known/oauth-authorization-server',
      '/healthz',
      '/readyz'
    ]) {
      expect(belongsToOrigin(path), path).toBe(true);
    }
  });

  it('leaves the console its own routes, including the look-alikes', () => {
    for (const path of [
      '/',
      '/login',
      '/verify',
      '/o/acme/queue',
      '/settings/billing',
      // `/apiary` is a legal org slug and `/mcp-guide` a legal page. A prefix
      // test that is not segment-aware sends both to the origin, which answers
      // a JSON 404 for a page the SPA was going to render.
      '/apiary',
      '/apiary/queue',
      '/mcp-guide',
      '/oauthentication',
      '/readyzzz',
      '/healthzcheck',
      '/.well-knownish'
    ]) {
      expect(belongsToOrigin(path), path).toBe(false);
    }
  });

  it('matches the prefix list the server keeps, plus the two health routes', () => {
    // The server does not list /healthz and /readyz in API_PREFIXES because it
    // mounts them as real routes ahead of its SPA fallback. The edge has no such
    // precedence, so it must name them or answer them with the console's HTML.
    const serverPrefixes = ['/api', '/oauth', '/mcp', '/.well-known'];
    const edgeOnly = ['/healthz', '/readyz'];

    for (const prefix of [...serverPrefixes, ...edgeOnly]) {
      expect(belongsToOrigin(prefix), prefix).toBe(true);
      expect(belongsToOrigin(`${prefix}/x`), `${prefix}/x`).toBe(true);
      expect(belongsToOrigin(`${prefix}x`), `${prefix}x`).toBe(false);
    }
  });
});
