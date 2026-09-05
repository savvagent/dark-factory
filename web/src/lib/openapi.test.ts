import { describe, expect, it } from 'vitest';
import { groupByTag, schemaSummary, type OpenApiDocument } from './openapi';

export const fixtureDoc: OpenApiDocument = {
  paths: {
    '/api/orgs/{org}/repos': {
      get: {
        operationId: 'listRepos',
        summary: 'List repos',
        description: 'Every repo in the org.',
        tags: ['repos'],
        parameters: [{ name: 'org', in: 'path', description: 'Org slug.' }],
        responses: {
          '200': {
            content: { 'application/json': { schema: { $ref: '#/components/schemas/RepoList' } } }
          }
        }
      },
      post: {
        operationId: 'createRepo',
        summary: 'Create a repo',
        description: 'Register one.',
        tags: ['repos'],
        requestBody: {
          content: { 'application/json': { schema: { $ref: '#/components/schemas/CreateRepo' } } }
        },
        responses: {
          '201': {
            content: { 'application/json': { schema: { $ref: '#/components/schemas/Repo' } } }
          }
        },
        'x-dark-factory-auth': 'org admin'
      }
    },
    '/api/orgs/{org}/webhooks': {
      post: {
        operationId: 'ingestWebhook',
        summary: 'Ingest',
        description: 'From a tracker.',
        tags: ['a-brand-new-tag-nobody-has-seen'],
        responses: { '200': {} },
        'x-dark-factory-auth': 'public'
      }
    }
  },
  components: {
    schemas: {
      Repo: {
        type: 'object',
        properties: { id: { type: 'string' }, slug: { type: 'string', description: 'Handle.' } },
        required: ['id', 'slug']
      }
    }
  }
};

describe('groupByTag', () => {
  it('keeps exactly one entry per path+verb pair, with nothing dropped or duplicated', () => {
    const groups = groupByTag(fixtureDoc);
    const flat = groups.flatMap((g) => g.endpoints);
    expect(flat).toHaveLength(3);
    const pairs = flat.map((e) => `${e.method} ${e.path}`).sort();
    expect(pairs).toEqual([
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
    expect(groups.map((g) => g.tag)).toEqual(['a-brand-new-tag-nobody-has-seen', 'repos']);

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
