import type { OpenApiDocument } from './openapi';

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
    },
    '/api/orgs/{org}/teams/{team}': {
      delete: {
        operationId: 'deleteTeam',
        summary: 'Delete a team',
        description: 'Removes it.',
        tags: ['teams'],
        responses: {
          '204': {},
          '400': {
            content: { 'application/json': { schema: { $ref: '#/components/schemas/Error' } } }
          },
          '401': {
            content: { 'application/json': { schema: { $ref: '#/components/schemas/Error' } } }
          },
          '404': {
            content: { 'application/json': { schema: { $ref: '#/components/schemas/Error' } } }
          },
          '500': {
            content: { 'application/json': { schema: { $ref: '#/components/schemas/Error' } } }
          }
        },
        'x-dark-factory-auth': 'org admin'
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
