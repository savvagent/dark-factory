/**
 * The one place the console talks to `df-web`.
 *
 * Three things hold here, and each of them is a rule the server also keeps.
 *
 * **No credential is ever spent on a GET.** An invitation link points at a
 * *page* in this app; the page renders a button and
 * the button calls one of the `post` helpers below. A mail scanner that follows
 * the link loads a page and burns nothing. `df-web`'s
 * `every_single_use_redemption_is_a_post` asserts the other half.
 *
 * **The session is never touched by script.** It is an `HttpOnly`,
 * `__Host-`-prefixed cookie, so there is no token to attach and nothing to
 * store. Requests are same-origin and the browser sends it; `credentials` is
 * left at its default for exactly that reason, and a change to `'include'`
 * would be a sign someone has moved the API to another origin, where the
 * `__Host-` prefix means the cookie could not follow anyway.
 *
 * **An error has a code before it has a message.** `ApiError.code` is the
 * stable branch point — `not_found`, `invalid_credentials`, `rate_limited` —
 * and `message` is written by the server to be shown to a person. Pages branch
 * on the code and render the message; they never parse the message.
 */

import type {
  AuditEvent,
  BrowserSession,
  Enrollment,
  Invite,
  CreatedInvite,
  Job,
  JobDetail,
  JobStatus,
  Joined,
  Lease,
  Me,
  Membership,
  MintedToken,
  Org,
  OrgMember,
  ProtectedResourceMetadata,
  QueueStats,
  Repo,
  Role,
  SessionOpened,
  Team,
  TeamMember,
  TokenSummary,
  UsageStatus
} from './types';

/**
 * A failure the server described. Carries the HTTP status too, because a few
 * callers need to tell "no such thing" (`404`) from "not allowed" (`403`) even
 * though the console's own rule is that an org you are not in answers `404`.
 */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }

  /** No session, or one the server no longer honours. */
  get isUnauthenticated(): boolean {
    return this.status === 401;
  }

  get isNotFound(): boolean {
    return this.status === 404;
  }
}

interface ErrorBody {
  error?: { code?: string; message?: string };
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, {
      method,
      headers: body === undefined ? {} : { 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body)
    });
  } catch {
    // A network failure is not a server answer, and telling a user their
    // credentials were wrong when the wifi dropped sends them to reset
    // something that was never broken.
    throw new ApiError(0, 'network', 'Could not reach the server. Check your connection.');
  }

  if (response.status === 204) {
    return undefined as T;
  }

  const text = await response.text();
  const parsed: unknown = text.length > 0 ? safeJson(text) : undefined;

  if (!response.ok) {
    const described = parsed as ErrorBody | undefined;
    throw new ApiError(
      response.status,
      described?.error?.code ?? 'unknown',
      described?.error?.message ?? `The server answered ${response.status}.`
    );
  }

  return parsed as T;
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

/**
 * Build a query string, dropping anything absent.
 *
 * Absent, not empty: `?repo=` would ask the server to resolve the empty slug,
 * and a filter nobody set must not narrow anything.
 */
function query(params: Record<string, string | number | boolean | undefined>): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== '') search.set(key, String(value));
  }
  const rendered = search.toString();
  return rendered.length > 0 ? `?${rendered}` : '';
}

const get = <T>(path: string) => request<T>('GET', path);
const post = <T>(path: string, body?: unknown) => request<T>('POST', path, body);
const patch = <T>(path: string, body?: unknown) => request<T>('PATCH', path, body);
const put = <T>(path: string, body?: unknown) => request<T>('PUT', path, body);
const del = <T>(path: string) => request<T>('DELETE', path);

/** Percent-encode one path segment. A slug is user-chosen; a repo named `a/b` must not become two segments. */
const seg = (value: string) => encodeURIComponent(value);

export const api = {
  // ----------------------------------------------------------------- auth
  /**
   * Create the account and start enrolling an authenticator.
   *
   * Returns the enrollment itself — there is no mailbox to send it to — so the
   * caller must render the QR and recovery codes immediately. No session
   * exists until `confirmSignup`.
   */
  signup: (email: string, name?: string) =>
    post<Enrollment>('/api/auth/signup', { email, name: name || null }),

  /** Finish signup with a code from the authenticator. Opens the first session. */
  confirmSignup: (email: string, code: string) =>
    post<SessionOpened>('/api/auth/signup/confirm', { email, code }),

  login: (email: string, code: string) => post<SessionOpened>('/api/auth/login', { email, code }),
  loginWithRecoveryCode: (email: string, code: string) =>
    post<SessionOpened>('/api/auth/login/recovery', { email, code }),

  logout: () => post<void>('/api/auth/logout'),

  // ------------------------------------------------------------------- me
  me: () => get<Me>('/api/me'),
  sessions: () => get<BrowserSession[]>('/api/me/sessions'),
  signOutEverywhere: () => del<{ revoked: number }>('/api/me/sessions'),

  beginTotp: () => post<Enrollment>('/api/me/totp'),
  confirmTotp: (code: string) => post<void>('/api/me/totp/confirm', { code }),
  reissueRecoveryCodes: () => post<{ codes: string[] }>('/api/me/recovery-codes'),

  // ----------------------------------------------------------------- orgs
  orgs: () => get<Membership[]>('/api/orgs'),
  createOrg: (slug: string, name: string) => post<Org>('/api/orgs', { slug, name }),
  org: (org: string) => get<Joined>(`/api/orgs/${seg(org)}`),

  members: (org: string) => get<OrgMember[]>(`/api/orgs/${seg(org)}/members`),
  setMemberRole: (org: string, user: string, role: Role) =>
    patch<void>(`/api/orgs/${seg(org)}/members/${seg(user)}`, { role }),
  removeMember: (org: string, user: string) =>
    del<void>(`/api/orgs/${seg(org)}/members/${seg(user)}`),
  forceLogout: (org: string, user: string) =>
    post<void>(`/api/orgs/${seg(org)}/members/${seg(user)}/logout`),

  invites: (org: string) => get<Invite[]>(`/api/orgs/${seg(org)}/invites`),
  /** Returns the one-time code, shown once — the admin delivers it themselves. */
  invite: (org: string, email: string, role: Role) =>
    post<CreatedInvite>(`/api/orgs/${seg(org)}/invites`, { email, role }),
  resetMemberAuthenticator: (org: string, user: string) =>
    post<void>(`/api/orgs/${seg(org)}/members/${seg(user)}/reset-authenticator`),
  revokeInvite: (org: string, id: string) => del<void>(`/api/orgs/${seg(org)}/invites/${seg(id)}`),
  acceptInvite: (org: string, token: string) =>
    post<Joined>(`/api/orgs/${seg(org)}/invites/accept`, { token }),

  // ---------------------------------------------------------------- teams
  teams: (org: string) => get<Team[]>(`/api/orgs/${seg(org)}/teams`),
  createTeam: (org: string, slug: string, name?: string) =>
    post<Team>(`/api/orgs/${seg(org)}/teams`, { slug, name: name || null }),
  renameTeam: (org: string, team: string, name: string) =>
    patch<Team>(`/api/orgs/${seg(org)}/teams/${seg(team)}`, { name }),
  deleteTeam: (org: string, team: string) => del<void>(`/api/orgs/${seg(org)}/teams/${seg(team)}`),
  teamMembers: (org: string, team: string) =>
    get<TeamMember[]>(`/api/orgs/${seg(org)}/teams/${seg(team)}/members`),
  addTeamMember: (org: string, team: string, user: string) =>
    put<void>(`/api/orgs/${seg(org)}/teams/${seg(team)}/members/${seg(user)}`),
  removeTeamMember: (org: string, team: string, user: string) =>
    del<void>(`/api/orgs/${seg(org)}/teams/${seg(team)}/members/${seg(user)}`),

  // ---------------------------------------------------------------- repos
  repos: (org: string, includeInactive = false) =>
    get<Repo[]>(`/api/orgs/${seg(org)}/repos${query({ includeInactive })}`),
  registerRepo: (org: string, body: Record<string, unknown>) =>
    post<Repo>(`/api/orgs/${seg(org)}/repos`, body),
  updateRepo: (org: string, repo: string, body: Record<string, unknown>) =>
    patch<Repo>(`/api/orgs/${seg(org)}/repos/${seg(repo)}`, body),
  leases: (org: string, repo: string) =>
    get<Lease[]>(`/api/orgs/${seg(org)}/repos/${seg(repo)}/leases`),

  // ---------------------------------------------------------------- queue
  jobs: (
    org: string,
    filters: {
      status?: JobStatus;
      repo?: string;
      team?: string;
      mine?: boolean;
      limit?: number;
    } = {}
  ) => get<Job[]>(`/api/orgs/${seg(org)}/jobs${query({ ...filters })}`),
  queueStats: (org: string, repo?: string) =>
    get<QueueStats>(`/api/orgs/${seg(org)}/jobs/stats${query({ repo })}`),
  job: (org: string, id: string) => get<JobDetail>(`/api/orgs/${seg(org)}/jobs/${seg(id)}`),

  // ------------------------------------------------------- tokens & usage
  tokens: (org: string) => get<TokenSummary[]>(`/api/orgs/${seg(org)}/tokens`),
  mintToken: (org: string, name: string, scopes: string[], ttlDays?: number) =>
    post<MintedToken>(`/api/orgs/${seg(org)}/tokens`, { name, scopes, ttlDays: ttlDays ?? null }),
  revokeToken: (org: string, id: string) => del<void>(`/api/orgs/${seg(org)}/tokens/${seg(id)}`),

  usage: (org: string) => get<UsageStatus>(`/api/orgs/${seg(org)}/usage`),
  audit: (org: string, limit = 100) =>
    get<AuditEvent[]>(`/api/orgs/${seg(org)}/audit${query({ limit })}`),

  // ------------------------------------------------------------ discovery
  /**
   * Where the MCP endpoint and the grantable scopes come from.
   *
   * Read from RFC 9728 discovery rather than baked into the bundle: the console
   * is a static artifact that has to work against whatever origin serves it, and
   * a hard-coded MCP URL is how a self-hosted or staging deployment ends up
   * printing a connect command that points at production.
   */
  resourceMetadata: () => get<ProtectedResourceMetadata>('/.well-known/oauth-protected-resource')
};
