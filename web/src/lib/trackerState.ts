/**
 * The `state` a tracker connect flow carries through the provider and back.
 *
 * Two jobs, and only one of them is CSRF.
 *
 * **It carries the org.** A provider redirect URI is one static string
 * registered with GitHub and Atlassian, so it cannot contain an org slug that
 * varies per customer — which is why the return page lives at
 * `/trackers/callback` and not under `/o/[org]/`. The org has to travel
 * somewhere, and `state` is the parameter that exists for carrying something
 * back unchanged.
 *
 * **It carries a nonce.** The redemption POST is already session-authenticated
 * and admin-gated, so the nonce is not what stops an outsider calling it. What
 * it stops is the login-CSRF shape: an attacker sending an admin a link that
 * returns *the attacker's* authorization code, binding the attacker's JIRA site
 * into the victim's org. The server cannot tell those apart — both arrive on a
 * real admin's session — so the check belongs here, where the flow was started.
 *
 * `sessionStorage`, not `localStorage`: a connect flow that outlives the tab
 * that began it is one nobody in this tab started. The cost is that finishing
 * in a different tab fails the check and asks the admin to start again.
 */
import type { TrackerProvider } from './types';

const KEY = 'df:tracker-connect';

export interface PendingConnect {
  org: string;
  provider: TrackerProvider;
  nonce: string;
}

function randomNonce(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

/**
 * Record a flow about to start, and return the `state` to hand the provider.
 *
 * Storage failures are swallowed on purpose — a browser with `sessionStorage`
 * blocked should still be able to connect a tracker; it will simply fail the
 * check on return and be told to start again, which is the safe direction.
 */
export function beginConnect(org: string, provider: TrackerProvider): string {
  const pending: PendingConnect = { org, provider, nonce: randomNonce() };
  try {
    sessionStorage.setItem(KEY, JSON.stringify(pending));
  } catch {
    // Nothing to do: the return trip will refuse rather than proceed blind.
  }
  return `${provider}:${pending.nonce}`;
}

/**
 * Match a returned `state` against what this tab stored, and consume it.
 *
 * Returns the org to complete against, or `null` for anything that does not
 * match — a missing entry, a different provider, a nonce that is not ours.
 * The entry is cleared either way: a `state` is single-use, and leaving a
 * spent one behind is how a stale flow gets replayed by a reload.
 */
export function completeConnect(state: string | null): PendingConnect | null {
  let raw: string | null = null;
  try {
    raw = sessionStorage.getItem(KEY);
    sessionStorage.removeItem(KEY);
  } catch {
    return null;
  }
  if (!raw || !state) return null;

  let pending: PendingConnect;
  try {
    pending = JSON.parse(raw) as PendingConnect;
  } catch {
    return null;
  }

  return state === `${pending.provider}:${pending.nonce}` ? pending : null;
}
