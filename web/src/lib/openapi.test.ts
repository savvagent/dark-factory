import { describe, expect, it } from 'vitest';
import { groupByTag, schemaSummary } from './openapi';
import { fixtureDoc } from './openapi.fixtures';

describe('groupByTag', () => {
  it('keeps exactly one entry per path+verb pair, with nothing dropped or duplicated', () => {
    const groups = groupByTag(fixtureDoc);
    const flat = groups.flatMap((g) => g.endpoints);
    expect(flat).toHaveLength(4);
    const pairs = flat.map((e) => `${e.method} ${e.path}`).sort();
    expect(pairs).toEqual([
      'delete /api/orgs/{org}/teams/{team}',
      'get /api/orgs/{org}/repos',
      'post /api/orgs/{org}/repos',
      'post /api/orgs/{org}/webhooks'
    ]);
  });

  it('preserves each entry auth level', () => {
    const flat = groupByTag(fixtureDoc).flatMap((g) => g.endpoints);
    const createRepo = flat.find((e) => e.operationId === 'createRepo');
    expect(createRepo?.auth).toBe('org admin');

    const listRepos = flat.find((e) => e.operationId === 'listRepos');
    expect(listRepos?.auth).toBe('public');
  });

  it('gives an unrecognized tag its own group rather than dropping or misfiling it', () => {
    const groups = groupByTag(fixtureDoc);
    const novel = groups.find((g) => g.tag === 'a-brand-new-tag-nobody-has-seen');
    expect(novel?.endpoints).toHaveLength(1);
    expect(novel?.endpoints[0]?.operationId).toBe('ingestWebhook');
  });

  it('sorts groups alphabetically by tag and endpoints by path then verb order', () => {
    const groups = groupByTag(fixtureDoc);
    expect(groups.map((g) => g.tag)).toEqual(['a-brand-new-tag-nobody-has-seen', 'repos', 'teams']);

    const repos = groups.find((g) => g.tag === 'repos');
    expect(repos?.endpoints.map((e) => e.method)).toEqual(['get', 'post']);
  });

  it('extracts request and response schema references', () => {
    const flat = groupByTag(fixtureDoc).flatMap((g) => g.endpoints);
    const createRepo = flat.find((e) => e.operationId === 'createRepo');
    expect(createRepo?.requestSchema).toBe('CreateRepo');
    expect(createRepo?.responseSchema).toBe('Repo');

    const listRepos = flat.find((e) => e.operationId === 'listRepos');
    expect(listRepos?.requestSchema).toBeUndefined();
    expect(listRepos?.responseSchema).toBe('RepoList');
  });

  it('does not mistake an error-only response for a success response schema', () => {
    // A 204-no-body success alongside 400/401/404/500 responses that all
    // reference `Error` — every DELETE endpoint in the real catalog is shaped
    // this way. `responseSchema` must stay undefined, never resolve to
    // `Error`, regardless of how a JS engine orders these numeric-looking keys.
    const flat = groupByTag(fixtureDoc).flatMap((g) => g.endpoints);
    const deleteTeam = flat.find((e) => e.operationId === 'deleteTeam');
    expect(deleteTeam?.responseSchema).toBeUndefined();
  });
});

describe('schemaSummary', () => {
  it('flattens top-level properties with required flags', () => {
    const props = schemaSummary(fixtureDoc, 'Repo');
    expect(props).toEqual([
      { name: 'id', type: 'string', description: undefined, required: true },
      { name: 'slug', type: 'string', description: 'Handle.', required: true }
    ]);
  });

  it('returns an empty list for a schema name that does not exist', () => {
    expect(schemaSummary(fixtureDoc, 'NoSuchSchema')).toEqual([]);
  });
});
