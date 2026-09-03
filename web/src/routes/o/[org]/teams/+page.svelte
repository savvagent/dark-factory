<script lang="ts">
  import { api, ApiError } from '$lib/api';
  import { useOrg } from '$lib/org.svelte';
  import { slugPreview } from '$lib/format';
  import type { OrgMember, Team, TeamMember } from '$lib/types';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';
  import Card from '$lib/components/Card.svelte';
  import Empty from '$lib/components/Empty.svelte';
  import Field from '$lib/components/Field.svelte';
  import Loading from '$lib/components/Loading.svelte';

  /**
   * Teams — the visibility scope inside an org.
   *
   * Deleting a team is refused while repos are still scoped to it, and the
   * refusal names them. That is not a validation nicety: a null team means
   * org-wide, so a delete that cascaded would quietly publish a team's repos to
   * everybody. The error arrives with the repo names in it and is shown as-is.
   */

  const org = useOrg();

  let teams = $state<Team[]>([]);
  let members = $state<OrgMember[]>([]);
  let rosters = $state<Record<string, TeamMember[]>>({});
  let expanded = $state<string | undefined>(undefined);
  let loading = $state(true);
  let error = $state<string | undefined>(undefined);
  let busy = $state<string | undefined>(undefined);

  let slug = $state('');
  let name = $state('');
  let creating = $state(false);
  let formError = $state<string | undefined>(undefined);

  $effect(() => {
    const org_ = org.slug;
    if (!org_) return;

    loading = true;
    error = undefined;

    void (async () => {
      try {
        const [t, m] = await Promise.all([api.teams(org_), api.members(org_)]);
        if (org.slug !== org_) return;
        teams = t;
        members = m;
      } catch (e) {
        error = e instanceof ApiError ? e.message : 'Could not load teams.';
      } finally {
        loading = false;
      }
    })();
  });

  async function openTeam(team: Team) {
    if (expanded === team.slug) {
      expanded = undefined;
      return;
    }
    expanded = team.slug;
    await loadRoster(team.slug);
  }

  async function loadRoster(teamSlug: string) {
    try {
      rosters = { ...rosters, [teamSlug]: await api.teamMembers(org.slug, teamSlug) };
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Could not read that team.';
    }
  }

  async function create(event: SubmitEvent) {
    event.preventDefault();
    creating = true;
    formError = undefined;
    try {
      await api.createTeam(org.slug, slugPreview(slug), name.trim());
      slug = '';
      name = '';
      teams = await api.teams(org.slug);
    } catch (e) {
      formError = e instanceof ApiError ? e.message : 'Could not create that team.';
    } finally {
      creating = false;
    }
  }

  async function act(key: string, action: () => Promise<unknown>, teamSlug?: string) {
    busy = key;
    error = undefined;
    try {
      await action();
      teams = await api.teams(org.slug);
      if (teamSlug) await loadRoster(teamSlug);
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'That did not work.';
    } finally {
      busy = undefined;
    }
  }

  /** Org members not already on the expanded team. */
  const candidates = $derived((teamSlug: string) => {
    const on = new Set((rosters[teamSlug] ?? []).map((m) => m.userId));
    return members.filter((m) => !on.has(m.id));
  });
</script>

<div class="space-y-5">
  <div>
    <h1 class="text-lg font-semibold">Teams</h1>
    <p class="mt-0.5 text-sm text-faint">
      A team scopes repos and their queues. A repo with no team is visible to the whole org.
    </p>
  </div>

  {#if error}<Alert>{error}</Alert>{/if}

  {#if org.isAdmin}
    <Card title="New team">
      <form class="flex flex-wrap items-end gap-3" onsubmit={create}>
        <div class="w-44">
          <Field label="Slug">
            <input class="df-input df-mono" required bind:value={slug} />
          </Field>
        </div>
        <div class="min-w-48 flex-1">
          <Field label="Name" hint="Optional. Defaults to the slug.">
            <input class="df-input" bind:value={name} />
          </Field>
        </div>
        <div class="pb-0.5">
          <Button type="submit" pending={creating}>Create</Button>
        </div>
      </form>
      {#if formError}<div class="mt-3"><Alert>{formError}</Alert></div>{/if}
    </Card>
  {/if}

  {#if loading && teams.length === 0}
    <Loading what="Loading teams" />
  {:else if teams.length === 0}
    <Empty title="No teams yet.">
      Teams are optional. Without one, every repo is visible to every member.
    </Empty>
  {:else}
    <ul class="space-y-2">
      {#each teams as team (team.id)}
        <li class="df-card">
          <div class="flex flex-wrap items-center gap-3 px-4 py-3">
            <div class="min-w-0 flex-1">
              <span class="df-mono text-sm text-ink">{team.slug}</span>
              <p class="text-xs text-faint">{team.name}</p>
            </div>

            <button
              class="text-xs text-muted underline hover:text-ink"
              onclick={() => openTeam(team)}
              aria-expanded={expanded === team.slug}
            >
              {expanded === team.slug ? 'Hide members' : 'Members'}
            </button>

            <a
              class="text-xs text-muted underline hover:text-ink"
              href="/o/{org.slug}/queue?team={encodeURIComponent(team.slug)}"
            >
              Queue
            </a>

            {#if org.isAdmin}
              <Button
                tone="danger"
                pending={busy === `${team.id}:delete`}
                onclick={() => act(`${team.id}:delete`, () => api.deleteTeam(org.slug, team.slug))}
              >
                Delete
              </Button>
            {/if}
          </div>

          {#if expanded === team.slug}
            {@const roster = rosters[team.slug]}
            <div class="space-y-3 border-t border-edge/60 px-4 py-3">
              {#if !roster}
                <Loading what="Loading the roster" />
              {:else if roster.length === 0}
                <p class="text-xs text-faint">Nobody is on this team yet.</p>
              {:else}
                <ul class="divide-y divide-edge/40">
                  {#each roster as member (member.userId)}
                    <li class="flex items-center gap-3 py-2 text-sm">
                      <span class="min-w-0 flex-1 truncate text-muted">
                        {member.name ?? member.email}
                      </span>
                      {#if org.isAdmin}
                        <Button
                          tone="quiet"
                          pending={busy === `${team.id}:${member.userId}`}
                          onclick={() =>
                            act(
                              `${team.id}:${member.userId}`,
                              () => api.removeTeamMember(org.slug, team.slug, member.userId),
                              team.slug
                            )}
                        >
                          Remove
                        </Button>
                      {/if}
                    </li>
                  {/each}
                </ul>
              {/if}

              {#if org.isAdmin && candidates(team.slug).length > 0}
                <label class="flex items-end gap-2">
                  <span class="sr-only">Add a member to {team.slug}</span>
                  <select
                    class="df-input w-64"
                    value=""
                    onchange={(e) => {
                      const user = e.currentTarget.value;
                      e.currentTarget.value = '';
                      if (user) {
                        void act(
                          `${team.id}:add`,
                          () => api.addTeamMember(org.slug, team.slug, user),
                          team.slug
                        );
                      }
                    }}
                  >
                    <option value="">Add a member…</option>
                    {#each candidates(team.slug) as member (member.id)}
                      <option value={member.id}>{member.name ?? member.email}</option>
                    {/each}
                  </select>
                </label>
              {/if}
            </div>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</div>
