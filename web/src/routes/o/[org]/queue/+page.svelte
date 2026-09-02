<script lang="ts">
  import { page } from '$app/state';
  import { replaceState } from '$app/navigation';

  import { api, ApiError } from '$lib/api';
  import { useOrg } from '$lib/org.svelte';
  import { relative } from '$lib/format';
  import type { Job, JobStatus, Repo, Team } from '$lib/types';
  import Alert from '$lib/components/Alert.svelte';
  import Empty from '$lib/components/Empty.svelte';
  import Loading from '$lib/components/Loading.svelte';
  import StatusPill from '$lib/components/StatusPill.svelte';

  /**
   * The queue, read-only.
   *
   * There is no button on this page that changes a job, and that is not an
   * omission. A job is created and finished by the agent doing the work, over
   * MCP; a human pressing "mark complete" here would be telling the queue
   * something they cannot observe, and the audit trail would record it as fact.
   * The console's job is to answer "what is happening, and why is my agent
   * waiting" — see the leases on the Repos page for the second half of that.
   *
   * Filters live in the URL so a view can be linked to. `replaceState` rather
   * than `goto`: changing a filter is not a place in history to go back to, and
   * a dozen entries per session makes the browser's back button useless.
   */

  const org = useOrg();

  const STATUSES: JobStatus[] = ['pending', 'in-progress', 'completed', 'failed'];

  const status = $derived(
    (STATUSES as string[]).includes(page.url.searchParams.get('status') ?? '')
      ? (page.url.searchParams.get('status') as JobStatus)
      : undefined
  );
  const repo = $derived(page.url.searchParams.get('repo') ?? undefined);
  const team = $derived(page.url.searchParams.get('team') ?? undefined);
  const mine = $derived(page.url.searchParams.get('mine') === 'true');

  let jobs = $state<Job[]>([]);
  let repos = $state<Repo[]>([]);
  let teams = $state<Team[]>([]);
  let loading = $state(true);
  let error = $state<string | undefined>(undefined);

  $effect(() => {
    const slug = org.slug;
    if (!slug) return;
    void (async () => {
      try {
        const [r, t] = await Promise.all([api.repos(slug), api.teams(slug)]);
        if (org.slug !== slug) return;
        repos = r;
        teams = t;
      } catch {
        // The pickers are a convenience. Losing them is not worth replacing the
        // queue itself with an error box — the filters still work by URL.
      }
    })();
  });

  $effect(() => {
    const slug = org.slug;
    // Read inside the effect so each becomes a dependency.
    const filters = { status, repo, team, mine, limit: 200 };
    if (!slug) return;

    loading = true;
    error = undefined;

    void (async () => {
      try {
        const found = await api.jobs(slug, filters);
        if (org.slug !== slug) return;
        jobs = found;
      } catch (e) {
        // An unregistered repo or team slug is a 404 naming what *is*
        // registered — better than an empty table, which reads as a quiet
        // queue rather than as a question nobody asked.
        error = e instanceof ApiError ? e.message : 'Could not load the queue.';
        jobs = [];
      } finally {
        loading = false;
      }
    })();
  });

  function setFilter(key: string, value: string | undefined) {
    const url = new URL(page.url);
    if (value === undefined || value === '') url.searchParams.delete(key);
    else url.searchParams.set(key, value);
    replaceState(url, page.state);
  }

  const filtered = $derived(
    status !== undefined || repo !== undefined || team !== undefined || mine
  );
</script>

<div class="space-y-4">
  <div class="flex flex-wrap items-baseline justify-between gap-2">
    <h1 class="text-lg font-semibold">Queue</h1>
    <p class="text-xs text-faint">Read-only. Jobs are queued and completed by agents over MCP.</p>
  </div>

  <div class="df-card flex flex-wrap items-end gap-3 px-4 py-3">
    <label class="block">
      <span class="df-label">Status</span>
      <select
        class="df-input w-40"
        value={status ?? ''}
        onchange={(e) => setFilter('status', e.currentTarget.value)}
      >
        <option value="">Any</option>
        {#each STATUSES as option (option)}
          <option value={option}>{option}</option>
        {/each}
      </select>
    </label>

    <label class="block">
      <span class="df-label">Repo</span>
      <select
        class="df-input w-44"
        value={repo ?? ''}
        onchange={(e) => setFilter('repo', e.currentTarget.value)}
      >
        <option value="">Every repo</option>
        {#if repo && !repos.some((r) => r.slug === repo)}
          <!-- A slug from the URL that is not registered. Rendered so the
               picker shows what the page is actually filtering on; without it
               the select falls back to blank and the error above looks
               unrelated to anything the user can see. -->
          <option value={repo}>{repo} (not registered)</option>
        {/if}
        {#each repos as option (option.id)}
          <option value={option.slug}>{option.slug}</option>
        {/each}
      </select>
    </label>

    {#if teams.length > 0 || team}
      <label class="block">
        <span class="df-label">Team</span>
        <select
          class="df-input w-44"
          value={team ?? ''}
          onchange={(e) => setFilter('team', e.currentTarget.value)}
        >
          <option value="">Every team</option>
          {#if team && !teams.some((t) => t.slug === team)}
            <option value={team}>{team} (no such team)</option>
          {/if}
          {#each teams as option (option.id)}
            <option value={option.slug}>{option.slug}</option>
          {/each}
        </select>
      </label>
    {/if}

    <label class="flex items-center gap-2 pb-2 text-sm text-muted">
      <input
        type="checkbox"
        checked={mine}
        onchange={(e) => setFilter('mine', e.currentTarget.checked ? 'true' : undefined)}
      />
      Only what I queued
    </label>

    {#if filtered}
      <button
        class="ml-auto pb-2 text-xs text-muted underline hover:text-ink"
        onclick={() => replaceState(new URL(page.url.pathname, location.origin), page.state)}
      >
        Clear filters
      </button>
    {/if}
  </div>

  {#if error}
    <Alert>{error}</Alert>
  {:else if loading && jobs.length === 0}
    <Loading what="Loading the queue" />
  {:else if jobs.length === 0}
    <Empty title={filtered ? 'No jobs match these filters.' : 'Nothing has been queued yet.'}>
      {#if !filtered}
        Jobs are created by agents over MCP.
        <a class="text-muted underline hover:text-ink" href="/o/{org.slug}/connect">Connect one</a>.
      {/if}
    </Empty>
  {:else}
    <div class="df-card overflow-x-auto">
      <table class="w-full text-sm">
        <thead class="border-b border-edge/60 text-left text-xs text-faint">
          <tr>
            <th class="px-4 py-2 font-medium">Job</th>
            <th class="px-4 py-2 font-medium">Status</th>
            <th class="px-4 py-2 font-medium">Agent</th>
            <th class="px-4 py-2 font-medium">Ticket</th>
            <th class="px-4 py-2 font-medium">Queued</th>
          </tr>
        </thead>
        <tbody class="divide-y divide-edge/40">
          {#each jobs as job (job.id)}
            <tr class="hover:bg-raised/40">
              <td class="px-4 py-2">
                <a class="text-ink hover:underline" href="/o/{org.slug}/queue/{job.id}">
                  {job.title}
                </a>
                <div class="df-mono text-xs text-faint">{job.id}</div>
              </td>
              <td class="px-4 py-2"><StatusPill status={job.status} /></td>
              <td class="px-4 py-2 text-muted">
                {job.claimedByLabel ?? job.agentType ?? '—'}
              </td>
              <td class="px-4 py-2 text-muted">{job.ticketRef ?? '—'}</td>
              <td class="px-4 py-2 whitespace-nowrap text-faint">{relative(job.createdAt)}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>

    <p class="text-xs text-faint">
      Showing {jobs.length}
      {jobs.length === 1 ? 'job' : 'jobs'}{jobs.length === 200 ? ', the most recent 200' : ''}.
    </p>
  {/if}
</div>
