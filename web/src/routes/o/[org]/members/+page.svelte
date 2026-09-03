<script lang="ts">
  import { api, ApiError } from '$lib/api';
  import { useOrg } from '$lib/org.svelte';
  import { session } from '$lib/session.svelte';
  import { relative } from '$lib/format';
  import type { CreatedInvite, Invite, OrgMember, Role } from '$lib/types';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';
  import Card from '$lib/components/Card.svelte';
  import CopyField from '$lib/components/CopyField.svelte';
  import Empty from '$lib/components/Empty.svelte';
  import Field from '$lib/components/Field.svelte';
  import Loading from '$lib/components/Loading.svelte';

  /**
   * Members and outstanding invitations.
   *
   * The roster is open to any member — who else is in your own org is not
   * privileged — while every button that changes something is admin-only. That
   * split is the server's; this page mirrors it so a member is not offered
   * controls that would fail, but the server is what enforces it.
   *
   * Two rules the server keeps and this page has to explain rather than
   * re-implement: only an owner may create or demote another owner, and the
   * last owner can be neither demoted nor removed. Both arrive as errors with
   * messages written to be read, so they are shown as-is.
   */

  const org = useOrg();

  let members = $state<OrgMember[]>([]);
  let invites = $state<Invite[]>([]);
  let loading = $state(true);
  let error = $state<string | undefined>(undefined);
  let busy = $state<string | undefined>(undefined);

  let inviteEmail = $state('');
  let inviteRole = $state<Role>('member');
  let inviting = $state(false);
  let inviteError = $state<string | undefined>(undefined);
  let minted = $state<CreatedInvite | undefined>(undefined);

  $effect(() => {
    const slug = org.slug;
    const admin = org.isAdmin;
    if (!slug) return;

    loading = true;
    error = undefined;

    void (async () => {
      try {
        members = await api.members(slug);
        // Invitations are admin-only. A member asking for them gets a 403, and
        // spending a request to be told that on every page load is noise.
        invites = admin ? await api.invites(slug) : [];
      } catch (e) {
        error = e instanceof ApiError ? e.message : 'Could not load members.';
      } finally {
        loading = false;
      }
    })();
  });

  async function reload() {
    members = await api.members(org.slug);
    if (org.isAdmin) invites = await api.invites(org.slug);
  }

  async function act(key: string, action: () => Promise<unknown>) {
    busy = key;
    error = undefined;
    try {
      await action();
      await reload();
      // Removing yourself changes your own memberships, and the org switcher in
      // the header reads them.
      await session.refresh();
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'That did not work.';
    } finally {
      busy = undefined;
    }
  }

  async function invite(event: SubmitEvent) {
    event.preventDefault();
    inviting = true;
    inviteError = undefined;
    minted = undefined;
    try {
      // The code comes back here and nowhere else — only its hash is stored,
      // so it cannot be read back and must stay on screen until the admin has
      // actually delivered it.
      minted = await api.invite(org.slug, inviteEmail.trim(), inviteRole);
      inviteEmail = '';
      inviteRole = 'member';
      invites = await api.invites(org.slug);
    } catch (e) {
      inviteError = e instanceof ApiError ? e.message : 'Could not create that invitation.';
    } finally {
      inviting = false;
    }
  }

  const roles: Role[] = ['owner', 'admin', 'member'];
</script>

<div class="space-y-5">
  <div>
    <h1 class="text-lg font-semibold">Members</h1>
    <p class="mt-0.5 text-sm text-faint">
      Everyone who can sign in to {org.title}.
    </p>
  </div>

  {#if error}<Alert>{error}</Alert>{/if}

  {#if org.isAdmin}
    <Card
      title="Invite someone"
      description="A single-use code, good for 14 days. You deliver it — nothing is emailed."
    >
      <form class="flex flex-wrap items-end gap-3" onsubmit={invite}>
        <div class="min-w-56 flex-1">
          <Field label="Email">
            <input class="df-input" type="email" required bind:value={inviteEmail} />
          </Field>
        </div>
        <div class="w-36">
          <Field label="Role">
            <select class="df-input" bind:value={inviteRole}>
              <option value="member">member</option>
              <option value="admin">admin</option>
              {#if org.isOwner}<option value="owner">owner</option>{/if}
            </select>
          </Field>
        </div>
        <div class="pb-0.5">
          <Button type="submit" pending={inviting}>Create invitation</Button>
        </div>
      </form>

      {#if inviteError}<div class="mt-3"><Alert>{inviteError}</Alert></div>{/if}
      {#if minted}
        <div class="mt-4 space-y-3 rounded-lg border border-ok/40 bg-ok/5 p-4">
          <p class="text-sm text-ink">
            Invitation for <span class="df-mono">{minted.email}</span>. Send them one of these —
            <span class="text-muted"
              >it is shown only now, and cannot be looked up again. If you lose it, invite them
              again.</span
            >
          </p>
          <CopyField label="Link" value={minted.link} />
          <CopyField label="Code" value={minted.code} />
          <p class="text-xs text-faint">
            Only an account signed in as {minted.email} can redeem it, so a code that goes astray is not
            a free seat.
          </p>
        </div>
      {/if}
    </Card>
  {/if}

  {#if loading && members.length === 0}
    <Loading what="Loading members" />
  {:else}
    <Card title="Roster">
      <ul class="divide-y divide-edge/40">
        {#each members as member (member.id)}
          {@const isMe = member.id === session.me?.user.id}
          <li class="flex flex-wrap items-center gap-3 py-2.5">
            <div class="min-w-0 flex-1">
              <div class="flex items-center gap-2 text-sm">
                <span class="text-ink">{member.name ?? member.email}</span>
                {#if isMe}<span class="text-xs text-faint">(you)</span>{/if}
                {#if member.disabledAt}
                  <span class="rounded-full border border-bad/50 px-2 py-0.5 text-xs text-bad">
                    disabled
                  </span>
                {/if}
              </div>
              <p class="text-xs text-faint">{member.email} · joined {relative(member.joinedAt)}</p>
            </div>

            {#if org.isAdmin}
              <select
                class="df-input w-28"
                value={member.role}
                disabled={busy === member.id}
                onchange={(e) =>
                  act(member.id, () =>
                    api.setMemberRole(org.slug, member.id, e.currentTarget.value as Role)
                  )}
              >
                {#each roles as role (role)}
                  <!-- Only an owner may create or demote an owner; the server
                       refuses otherwise, and offering the option would be a
                       button that exists only to fail. -->
                  <option
                    value={role}
                    disabled={!org.isOwner && (role === 'owner' || member.role === 'owner')}
                  >
                    {role}
                  </option>
                {/each}
              </select>

              <Button
                tone="quiet"
                pending={busy === `${member.id}:logout`}
                title="End every browser session this account holds. Leaves their tokens alone."
                onclick={() =>
                  act(`${member.id}:logout`, () => api.forceLogout(org.slug, member.id))}
              >
                Force sign-out
              </Button>

              <!-- The only assisted account recovery there is: no email means no
                   recovery link, so an admin is the last resort for someone who
                   has lost both their authenticator and their codes. It grants
                   nothing — they enrol again themselves. -->
              {#if org.isOwner || member.role !== 'owner'}
                <Button
                  tone="quiet"
                  pending={busy === `${member.id}:reset`}
                  title="Clear their authenticator so they can enrol a new one. Ends their sessions. Gives you no access to the account."
                  onclick={() => {
                    if (
                      confirm(
                        `Clear the authenticator for ${member.email}? They will be signed out everywhere and must enrol again from scratch.`
                      )
                    ) {
                      act(`${member.id}:reset`, () =>
                        api.resetMemberAuthenticator(org.slug, member.id)
                      );
                    }
                  }}
                >
                  Reset authenticator
                </Button>
              {/if}
            {/if}

            {#if org.isAdmin || isMe}
              <Button
                tone="danger"
                pending={busy === `${member.id}:remove`}
                onclick={() =>
                  act(`${member.id}:remove`, () => api.removeMember(org.slug, member.id))}
              >
                {isMe ? 'Leave' : 'Remove'}
              </Button>
            {/if}
          </li>
        {/each}
      </ul>

      <p class="mt-3 text-xs text-faint">
        Removing someone also clears their team memberships and revokes the tokens they held in this
        org — their agents stop on their next call.
      </p>
    </Card>

    {#if org.isAdmin}
      <Card title="Outstanding invitations">
        {#if invites.length === 0}
          <Empty title="Nobody is waiting on an invitation." />
        {:else}
          <ul class="divide-y divide-edge/40">
            {#each invites as pending (pending.id)}
              <li class="flex items-center gap-3 py-2.5 text-sm">
                <div class="min-w-0 flex-1">
                  <span class="text-ink">{pending.email}</span>
                  <p class="text-xs text-faint">
                    {pending.role} · expires {relative(pending.expiresAt)}
                  </p>
                </div>
                <Button
                  tone="quiet"
                  pending={busy === pending.id}
                  onclick={() => act(pending.id, () => api.revokeInvite(org.slug, pending.id))}
                >
                  Withdraw
                </Button>
              </li>
            {/each}
          </ul>
        {/if}
      </Card>
    {/if}
  {/if}
</div>
