/**
 * The console API's wire types.
 *
 * Hand-written, and mirroring `crates/df-web/src/openapi.rs` rather than
 * generated from it. Generation would be the reflex; it is the wrong trade at
 * this size. A generator has to run in CI to be worth anything, and until
 * `df-server` binds a port there is no document to fetch — so the "generated"
 * file would in practice be a checked-in artifact nobody regenerates, which is
 * the same hand-written file with a comment claiming otherwise.
 * `GET /api/openapi.json` is the authority either way; this is a transcription
 * of it, and `npm run check` fails when a page reads a field not declared here.
 *
 * Every name is camelCase because every response body is: `df-web`'s structs
 * carry `#[serde(rename_all = "camelCase")]`.
 */

export type Role = 'owner' | 'admin' | 'member';
export type JobStatus = 'pending' | 'in-progress' | 'completed' | 'failed';
export type Provider = 'github' | 'gitlab' | 'bitbucket' | 'other';
export type TokenKind = 'oauth' | 'pat';

export interface User {
  id: string;
  /** Absent until the account sets one — a passkey creates the account. */
  email: string | null;
  name: string | null;
  createdAt: string;
  disabledAt: string | null;
}

export interface Org {
  id: string;
  slug: string;
  name: string;
  plan: string;
  enforceSso: boolean;
  createdAt: string;
}

/** One org this account belongs to, and the role it holds there. */
export interface Membership {
  orgId: string;
  userId: string;
  role: Role;
  orgSlug: string;
  orgName: string;
  plan: string;
}

export interface Me {
  user: User;
  orgs: Membership[];
  shouldAddPasskey: boolean;
  passkeyCount: number;
}

export interface Joined {
  org: Org;
  role: Role;
}

export interface SessionOpened {
  user: User;
  shouldAddPasskey: boolean;
}

export interface OrgMember {
  id: string;
  email: string | null;
  name: string | null;
  role: Role;
  joinedAt: string;
  disabledAt: string | null;
}

export interface Invite {
  id: string;
  orgId: string;
  email: string;
  role: Role;
  invitedBy: string | null;
  expiresAt: string;
  acceptedAt: string | null;
  createdAt: string;
}

/**
 * The response from minting an invitation: the invite, plus the one-time code.
 *
 * `code` and `link` are the same secret twice and are returned **only** here —
 * nothing is emailed, and only the hash is stored, so an admin who loses the
 * code re-invites rather than looking it up.
 */
export interface CreatedInvite extends Invite {
  code: string;
  link: string;
}

/** A registered authenticator, as the console lists it. */
export interface Passkey {
  id: string;
  nickname: string | null;
  createdAt: string;
  lastUsedAt: string | null;
}

/** A WebAuthn challenge plus the id that lets the server find its own state. */
export interface RegistrationChallenge {
  ceremonyId: string;
  challenge: unknown;
}

export interface AuthenticationChallenge {
  ceremonyId: string;
  challenge: unknown;
}

/**
 * A one-time code letting an account register a passkey again, returned once
 * to the admin who cleared them. Nothing is emailed.
 */
export interface ClaimCode {
  code: string;
  link: string;
}

export interface Team {
  id: string;
  orgId: string;
  slug: string;
  name: string;
  createdAt: string;
}

export interface TeamMember {
  userId: string;
  email: string;
  name: string | null;
  joinedAt: string;
}

export interface Repo {
  id: string;
  orgId: string;
  slug: string;
  name: string;
  provider: Provider;
  defaultBranch: string;
  teamId: string | null;
  defaultAgentType: string | null;
  trackerBinding: Record<string, unknown>;
  active: boolean;
  createdAt: string;
  createdBy: string | null;
}

/**
 * An advisory, time-bounded claim on one branch. The server cannot enforce it
 * against a git operation it cannot see; it makes collisions visible rather
 * than impossible.
 */
export interface Lease {
  id: string;
  repoId: string;
  branch: string;
  holderUserId: string;
  holderLabel: string | null;
  jobId: string | null;
  acquiredAt: string;
  renewedAt: string;
  expiresAt: string;
}

export interface Job {
  id: string;
  orgId: string;
  repoId: string;
  teamId: string | null;
  title: string;
  description: string | null;
  status: JobStatus;
  ticketRef: string | null;
  tracker: 'jira' | 'github' | null;
  agentType: string | null;
  /** Opaque to dark-factory. A customer's own skill owns the shape. */
  metadata: Record<string, unknown>;
  createdAt: string;
  startedAt: string | null;
  completedAt: string | null;
  attempts: number;
  result: string | null;
  error: string | null;
  createdBy: string | null;
  claimedBy: string | null;
  claimedByLabel: string | null;
}

export interface JobDetail extends Job {
  dependsOn: string[];
}

/**
 * `blocked` overlaps `pending` rather than partitioning it: it counts the
 * pending jobs still waiting on a dependency. Two pending jobs where one cannot
 * start is not the same queue as two that can.
 */
export interface QueueStats {
  pending: number;
  inProgress: number;
  completed: number;
  failed: number;
  blocked: number;
  total: number;
}

export interface UsageStatus {
  plan: string;
  includedOps: number;
  billableUsed: number;
  remaining: number;
  totalCalls: number;
  periodStart: string;
  warning: boolean;
  hardStop: boolean;
  /** Whether the server is currently refusing billable calls over the bucket. */
  enforced: boolean;
}

/** A live credential. Never includes the token itself. */
export interface TokenSummary {
  id: string;
  name: string | null;
  kind: TokenKind;
  clientId: string | null;
  scopes: string[];
  createdAt: string;
  lastUsedAt: string | null;
  expiresAt: string;
}

/** Shown once. Only a SHA-256 hash of `token` is stored. */
export interface MintedToken {
  token: string;
  id: string;
  name: string;
  scopes: string[];
  /** The MCP endpoint this token is audienced for. */
  resource: string;
}

export interface BrowserSession {
  id: string;
  userId: string;
  expiresAt: string;
  createdAt: string;
}

export interface AuditEvent {
  id: number;
  orgId: string | null;
  actorUserId: string | null;
  actorLabel: string | null;
  action: string;
  targetType: string | null;
  targetId: string | null;
  ip: string | null;
  userAgent: string | null;
  detail: Record<string, unknown>;
  createdAt: string;
}

/** RFC 9728, as `/.well-known/oauth-protected-resource` serves it. */
export interface ProtectedResourceMetadata {
  resource: string;
  authorization_servers: string[];
  scopes_supported: string[];
  bearer_methods_supported: string[];
}
