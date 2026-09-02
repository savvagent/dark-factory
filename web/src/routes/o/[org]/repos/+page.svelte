<script lang="ts">
  import { api, ApiError } from '$lib/api';
  import { useOrg } from '$lib/org.svelte';
  import { relative, slugPreview } from '$lib/format';
  import type { Lease, Repo, Team } from '$lib/types';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';
  import Card from '$lib/components/Card.svelte';
  import Empty from '$lib/components/Empty.svelte';
  import Field from '$lib/components/Field.svelte';
  import Loading from '$lib/components/Loading.svelte';

  /**
   * Repos, and who is holding a lease on one right now.
   *
   * The leases are the reason this page is worth opening. A lease is advisory —
   * the server cannot see a git operation and cannot stop one — so its whole
   * value is being *visible*: "why is my agent waiting?" is answered by a name
   * and a branch, not by a lock.
   *
   * Leases are loaded per repo and only when a row is expanded. Fetching every
   * repo's leases up front would be one request per repo on every page load, to
   * render something nobody had asked to see.
   */

  const org = useOrg();

  let repos = $state<Repo[]>([]);
  let teams = $state<Team[]>([]);
  let includeInactive = $state(false);
  let loading = $state(true);
  let error = $state<string | undefined>(undefined);

  let expanded = $state<string | undefined>(undefined);
  let leases = $state<Record<string, Lease[] | 'loading' | 'failed'>>({});

  let showForm = $state(false);
  let slug = $state('');
  let name = $state('');
  let remotes = $state('');
  let teamId = $state('');
  let creating = $state(false);
  let formError = $state<string | undefined>(undefined);

  $effect(() => {
    const org_ = org.slug;
    const withInactive = includeInactive;
    if (!org_) return;

    loading = true;
    error = undefined;

    void (async () => {
      try {
        const [r, t] = await Promise.all([api.repos(org_, withInactive), api.teams(org_)]);
        if (org.slug !== org_) return;
        repos = r;
        teams = t;
      } catch (e) {
        error = e instanceof ApiError ? e.message : 'Could not load repos.';
      } finally {
        loading = false;
      }
    })();
  });

  async function toggle(repo: Repo) {
    if (expanded === repo.slug) {
      expanded = undefined;
      return;
    }
    expanded = repo.slug;
    if (leases[repo.slug] && leases[repo.slug] !== 'failed') return;

    leases = { ...leases, [repo.slug]: 'loading' };
    try {
      const found = await api.leases(org.slug, repo.slug);
      leases = { ...leases, [repo.slug]: found };
    } catch {
      leases = { ...leases, [repo.slug]: 'failed' };
    }
  }

  async function register(event: SubmitEvent) {
    event.preventDefault();
    creating = true;
    formError = undefined;
    try {
      // One remote per line. The server normalizes them, so the SSH and HTTPS
      // spellings of one repository collapse to a single row and either
      // resolves to it — which is why pasting all of them is the right advice.
      const parsed = remotes
        .split('\n')
        .map((line) => line.trim())
        .filter((line) => line.length > 0);

      await api.registerRepo(org.slug, {
        slug: slugPreview(slug),
        name: name.trim() || null,
        remotes: parsed,
        teamId: teamId || null
      });

      slug = '';
      name = '';
      remotes = '';
      teamId = '';
      showForm = false;
      repos = await api.repos(org.slug, includeInactive);
    } catch (e) {
      formError = e instanceof ApiError ? e.message : 'Could not register that repo.';
    } finally {
      creating = false;
    }
  }

  async function setActive(repo: Repo, active: boolean) {
    try {
      await api.updateRepo(org.slug, repo.slug, { active });
      repos = await api.repos(org.slug, includeInactive);
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Could not update that repo.';
    }
  }

  const teamName = $derived((id: string | null) =>
    id ? (teams.find((t) => t.id === id)?.slug ?? 'unknown team') : 'org-wide'
  );
</script>

<div class="space-y-5">
  <div class="flex flex-wrap items-center justify-between gap-3">
    <div>
      <h1 class="text-lg font-semibold">Repos</h1>
      <p class="mt-0.5 text-sm text-faint">
        Every job belongs to one of these. An agent that cannot resolve its checkout to a registered
        repo gets an error naming these slugs, never a guess.
      </p>
    </div>
    {#if org.isAdmin}
      <Button onclick={() => (showForm = !showForm)}>
        {showForm ? 'Cancel' : 'Register a repo'}
      </Button>
    {/if}
  </div>

  {#if showForm}
    <Card title="Register a repo">
      <form class="space-y-4" onsubmit={register}>
        <div class="grid gap-4 sm:grid-cols-2">
          <Field label="Slug" hint="What agents will type. It cannot be changed later.">
            <input class="df-input df-mono" required bind:value={slug} />
          </Field>
          <Field label="Name" hint="Optional. Defaults to the slug.">
            <input class="df-input" bind:value={name} />
          </Field>
        </div>

        <Field
          label="Remotes"
          hint="One per line, in any form git prints. SSH and HTTPS spellings of one repository collapse to a single row."
        >
          <textarea class="df-input df-mono h-24" bind:value={remotes}></textarea>
        </Field>

        {#if teams.length > 0}
          <Field
            label="Team"
            hint="Leave org-wide unless this repo should only be visible to one team."
          >
            <select class="df-input" bind:value={teamId}>
              <option value="">Org-wide</option>
              {#each teams as team (team.id)}
                <option value={team.id}>{team.slug}</option>
              {/each}
            </select>
          </Field>
        {/if}

        {#if formError}<Alert>{formError}</Alert>{/if}

        <Button type="submit" pending={creating}>Register</Button>
      </form>
    </Card>
  {/if}

  <label class="flex items-center gap-2 text-xs text-muted">
    <input type="checkbox" bind:checked={includeInactive} />
    Include retired repos
  </label>

  {#if error}
    <Alert>{error}</Alert>
  {:else if loading && repos.length === 0}
    <Loading what="Loading repos" />
  {:else if repos.length === 0}
    <Empty title="No repos registered yet.">
      {#if org.isAdmin}
        Register one above, or let an agent do it with the <code class="df-mono">register_repo</code
        >
        tool.
      {:else}
        Ask an owner or admin to register one.
      {/if}
    </Empty>
  {:else}
    <ul class="space-y-2">
      {#each repos as repo (repo.id)}
        <li class="df-card">
          <div class="flex flex-wrap items-center gap-3 px-4 py-3">
            <div class="min-w-0 flex-1">
              <div class="flex items-center gap-2">
                <span class="df-mono text-sm text-ink">{repo.slug}</span>
                {#if !repo.active}
                  <span class="rounded-full border border-edge px-2 py-0.5 text-xs text-faint">
                    retired
                  </span>
                {/if}
              </div>
              <p class="mt-0.5 text-xs text-faint">
                {repo.name} · {repo.provider} · {repo.defaultBranch} · {teamName(repo.teamId)}
              </p>
            </div>

            <button
              class="text-xs text-muted underline hover:text-ink"
              onclick={() => toggle(repo)}
              aria-expanded={expanded === repo.slug}
            >
              {expanded === repo.slug ? 'Hide leases' : 'Who is in here?'}
            </button>

            <a
              class="text-xs text-muted underline hover:text-ink"
              href="/o/{org.slug}/queue?repo={encodeURIComponent(repo.slug)}"
            >
              Queue
            </a>

            {#if org.isAdmin}
              <Button tone="quiet" onclick={() => setActive(repo, !repo.active)}>
                {repo.active ? 'Retire' : 'Reinstate'}
              </Button>
            {/if}
          </div>

          {#if expanded === repo.slug}
            <div class="border-t border-edge/60 px-4 py-3">
              {#if leases[repo.slug] === 'loading'}
                <Loading what="Reading leases" />
              {:else if leases[repo.slug] === 'failed'}
                <Alert>Could not read the leases on this repo.</Alert>
              {:else if (leases[repo.slug] as Lease[]).length === 0}
                <p class="text-xs text-faint">
                  Nobody holds a lease on {repo.slug} right now.
                </p>
              {:else}
                <ul class="space-y-1.5">
                  {#each leases[repo.slug] as Lease[] as lease (lease.id)}
                    <li class="flex flex-wrap items-baseline gap-x-3 text-sm">
                      <span class="df-mono text-ink">{lease.branch}</span>
                      <span class="text-muted">{lease.holderLabel ?? 'an agent'}</span>
                      {#if lease.jobId}
                        <a
                          class="df-mono text-xs text-muted underline hover:text-ink"
                          href="/o/{org.slug}/queue/{lease.jobId}"
                        >
                          {lease.jobId}
                        </a>
                      {/if}
                      <span class="text-xs text-faint">expires {relative(lease.expiresAt)}</span>
                    </li>
                  {/each}
                </ul>
                <p class="mt-2 text-xs text-faint">
                  Leases are advisory. The server cannot see a git push, so a lease makes a
                  collision visible — it does not prevent one.
                </p>
              {/if}
            </div>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</div>
