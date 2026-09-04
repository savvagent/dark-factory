<script lang="ts">
  import { api, ApiError } from '$lib/api';
  import { useOrg } from '$lib/org.svelte';
  import { relative } from '$lib/format';
  import type { Job, QueueStats, Repo, UsageStatus } from '$lib/types';
  import Alert from '$lib/components/Alert.svelte';
  import Card from '$lib/components/Card.svelte';
  import Empty from '$lib/components/Empty.svelte';
  import Loading from '$lib/components/Loading.svelte';
  import Meter from '$lib/components/Meter.svelte';
  import StatusPill from '$lib/components/StatusPill.svelte';

  /**
   * The overview: what the queue is doing, what it is doing it to, and how much
   * of the month's bucket that has cost.
   *
   * `blocked` gets a tile of its own even though those jobs are also counted in
   * `pending`. A queue with four pending jobs where three are waiting on a
   * dependency is not idle — it is stuck, and that is a different thing to go
   * and fix.
   */

  const org = useOrg();

  let stats = $state<QueueStats | undefined>(undefined);
  let recent = $state<Job[]>([]);
  let repos = $state<Repo[]>([]);
  let usage = $state<UsageStatus | undefined>(undefined);
  let loading = $state(true);
  let error = $state<string | undefined>(undefined);

  $effect(() => {
    const slug = org.slug;
    if (!slug) return;

    loading = true;
    error = undefined;

    void (async () => {
      try {
        const [s, jobs, r, u] = await Promise.all([
          api.queueStats(slug),
          api.jobs(slug, { limit: 8 }),
          api.repos(slug),
          api.usage(slug)
        ]);
        if (org.slug !== slug) return;
        stats = s;
        recent = jobs;
        repos = r;
        usage = u;
      } catch (e) {
        error = e instanceof ApiError ? e.message : 'Could not load this organization.';
      } finally {
        loading = false;
      }
    })();
  });

  const tiles = $derived(
    stats
      ? [
          { label: 'Pending', value: stats.pending, tone: 'text-muted' },
          { label: 'In progress', value: stats.inProgress, tone: 'text-busy' },
          { label: 'Active', value: stats.active, tone: 'text-accent' },
          {
            label: 'Blocked',
            value: stats.blocked,
            tone: stats.blocked > 0 ? 'text-warn' : 'text-faint'
          },
          {
            label: 'Failed',
            value: stats.failed,
            tone: stats.failed > 0 ? 'text-bad' : 'text-faint'
          }
        ]
      : []
  );
</script>

<div class="space-y-6">
  <div>
    <h1 class="text-lg font-semibold">{org.title}</h1>
    <p class="mt-0.5 text-sm text-faint">
      <code class="df-mono">{org.slug}</code> · {org.role ?? '—'} · {org.org?.plan ?? '—'} plan
    </p>
  </div>

  {#if error}
    <Alert>{error}</Alert>
  {:else if loading && !stats}
    <Loading what="Loading the overview" />
  {:else}
    <div class="grid grid-cols-2 gap-3 sm:grid-cols-4">
      {#each tiles as tile (tile.label)}
        <div class="df-card px-4 py-3">
          <div class="text-2xl font-semibold {tile.tone}">{tile.value.toLocaleString()}</div>
          <div class="mt-0.5 text-xs text-faint">{tile.label}</div>
        </div>
      {/each}
    </div>

    {#if stats && stats.blocked > 0}
      <Alert tone="warn">
        {stats.blocked} pending
        {stats.blocked === 1 ? 'job is' : 'jobs are'} waiting on a dependency and cannot be claimed yet.
        They are counted under Pending as well.
      </Alert>
    {/if}

    <div class="grid gap-6 lg:grid-cols-2">
      <Card title="Recent jobs" description="Newest first, across every repo.">
        {#snippet actions()}
          <a class="text-xs text-muted underline hover:text-ink" href="/o/{org.slug}/queue">
            Open the queue
          </a>
        {/snippet}

        {#if recent.length === 0}
          <Empty title="Nothing has been queued yet.">
            Jobs are created by agents over MCP.
            <a class="text-muted underline hover:text-ink" href="/o/{org.slug}/connect">
              Connect one
            </a>.
          </Empty>
        {:else}
          <ul class="divide-y divide-edge/40">
            {#each recent as job (job.id)}
              <li class="flex items-center gap-3 py-2">
                <span class="w-24 shrink-0"><StatusPill status={job.status} /></span>
                <a
                  class="min-w-0 flex-1 truncate text-sm text-ink hover:underline"
                  href="/o/{org.slug}/queue/{job.id}"
                >
                  {job.title}
                </a>
                <span class="shrink-0 text-xs text-faint">{relative(job.createdAt)}</span>
              </li>
            {/each}
          </ul>
        {/if}
      </Card>

      <div class="space-y-6">
        <Card title="This period" description="Metered tool calls against the plan.">
          {#snippet actions()}
            <a class="text-xs text-muted underline hover:text-ink" href="/o/{org.slug}/usage">
              Details
            </a>
          {/snippet}

          {#if usage}
            <Meter {usage} compact />
          {:else}
            <Loading what="Reading the meter" />
          {/if}
        </Card>

        <Card title="Repos" description="Coordination is anchored on these.">
          {#snippet actions()}
            <a class="text-xs text-muted underline hover:text-ink" href="/o/{org.slug}/repos">
              Manage
            </a>
          {/snippet}

          {#if repos.length === 0}
            <Empty title="No repos registered.">
              A job has to belong to one, so nothing can be queued until a repo exists.
            </Empty>
          {:else}
            <ul class="flex flex-wrap gap-2">
              {#each repos as repo (repo.id)}
                <a
                  class="df-mono rounded-md border border-edge px-2 py-1 text-xs text-muted transition hover:bg-raised hover:text-ink"
                  href="/o/{org.slug}/queue?repo={encodeURIComponent(repo.slug)}"
                >
                  {repo.slug}
                </a>
              {/each}
            </ul>
          {/if}
        </Card>
      </div>
    </div>
  {/if}
</div>
