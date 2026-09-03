/**
 * How to point a coding agent at this server.
 *
 * A data table rather than a page full of branches, and every entry has the
 * same shape, because **no client is first-class here**. dark-factory is
 * coding-agent agnostic by constraint: Claude Code, Copilot CLI, Cursor, Codex
 * and anything else speaking MCP are equally supported, nothing depends on one
 * client's plugin or hook system, and `agentType` is never validated against a
 * list. A console that gave one of them a bespoke wizard and the rest a
 * footnote would be the first place that promise quietly broke.
 *
 * Every client gets two forms:
 *
 * - **OAuth** — the intended path. The client discovers the authorization
 *   server from `/.well-known/oauth-protected-resource`, registers itself
 *   (RFC 7591), and sends the human here to consent. No secret is ever pasted.
 * - **Token** — the compatibility path, for a client whose OAuth support is
 *   partial. A personal access token lands in the same table as an OAuth access
 *   token with the same audience and the same scopes, so nothing downstream can
 *   tell which it received.
 *
 * `generic` exists so that a client nobody here has heard of is still a
 * first-class citizen: it is the endpoint and the two discovery documents, which
 * is all any conforming MCP client actually needs.
 */

export interface ClientRecipe {
  id: string;
  name: string;
  /** How the snippet should be syntax-labelled, and what the reader is meant to do with it. */
  kind: 'command' | 'json' | 'toml';
  /** Where a config-file snippet belongs, when it is a file rather than a command. */
  location?: string;
  oauth: (mcpUrl: string) => string;
  token: (mcpUrl: string, token: string) => string;
  note?: string;
}

const PLACEHOLDER = 'df_pat_…';

export const CLIENTS: ClientRecipe[] = [
  {
    id: 'claude-code',
    name: 'Claude Code',
    kind: 'command',
    oauth: (url) => `claude mcp add --transport http dark-factory ${url}`,
    token: (url, token) =>
      `claude mcp add --transport http dark-factory ${url} \\\n  --header "Authorization: Bearer ${token || PLACEHOLDER}"`,
    note:
      'Then run `claude mcp login dark-factory` in an interactive terminal to consent — ' +
      'a `-p` session cannot open a browser and will report the server as unauthenticated.'
  },
  {
    id: 'copilot-cli',
    name: 'Copilot CLI',
    kind: 'json',
    location: '~/.copilot/mcp-config.json',
    oauth: (url) =>
      JSON.stringify({ mcpServers: { 'dark-factory': { type: 'http', url } } }, null, 2),
    token: (url, token) =>
      JSON.stringify(
        {
          mcpServers: {
            'dark-factory': {
              type: 'http',
              url,
              headers: { Authorization: `Bearer ${token || PLACEHOLDER}` }
            }
          }
        },
        null,
        2
      ),
    note:
      'Copilot only offers the browser consent from its interactive session; a `-p` run ' +
      'with no credential reports no such tool rather than asking. Script it with a token.'
  },
  {
    id: 'cursor',
    name: 'Cursor',
    kind: 'json',
    location: '~/.cursor/mcp.json, or .cursor/mcp.json in a project',
    oauth: (url) => JSON.stringify({ mcpServers: { 'dark-factory': { url } } }, null, 2),
    token: (url, token) =>
      JSON.stringify(
        {
          mcpServers: {
            'dark-factory': {
              url,
              headers: { Authorization: `Bearer ${token || PLACEHOLDER}` }
            }
          }
        },
        null,
        2
      )
  },
  {
    id: 'codex',
    name: 'Codex CLI',
    kind: 'toml',
    location: '~/.codex/config.toml',
    oauth: (url) => `[mcp_servers.dark_factory]\nurl = "${url}"`,
    token: (url, token) =>
      `[mcp_servers.dark_factory]\nurl = "${url}"\n\n[mcp_servers.dark_factory.http_headers]\nAuthorization = "Bearer ${token || PLACEHOLDER}"`
  },
  {
    id: 'generic',
    name: 'Any other MCP client',
    kind: 'command',
    oauth: (url) => url,
    token: (url, token) => `Authorization: Bearer ${token || PLACEHOLDER}\n\n${url}`,
    note:
      'Streamable HTTP. A conforming client needs nothing but this URL: the 401 it gets back ' +
      'points at the protected-resource document, which points at the authorization server.'
  }
];
