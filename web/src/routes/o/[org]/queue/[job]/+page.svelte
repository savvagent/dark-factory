<script lang="ts">
  import { page } from '$app/state';

  import { api, ApiError } from '$lib/api';
  import { useOrg } from '$lib/org.svelte';
  import { absolute, relative } from '$lib/format';
  import type { JobDetail, Repo } from '$lib/types';
  import Alert from '$lib/components/Alert.svelte';
  import Card from '$lib/components/Card.svelte';
  import Loading from '$lib/components/Loading.svelte';
  import StatusPill from '$lib/components/StatusPill.svelte';

  /**
   * One job.
   *
   * `metadata` is rendered as raw JSON on purpose. dark-factory never interprets
   * it — it is where a customer's own skill keeps whatever its methodology needs
   * — so any attempt to lay it out prettily would be the console inventing a
   * schema the server promised not to have. Showing it verbatim is the honest
   * rendering of an opaque field.
   */

  const org = useOrg();
  const id = $derived(page.params.job ?? '');

  let job = $state<JobDetail | undefined>(undefined);
  let repo = $state<Repo | undefined>(undefined);
  let loading = $state(true);
  let missing = $state(false);
  let error = $state<string | undefined>(undefined);

  $effect(() => {
    const slug = org.slug;
    const wanted = id;
    if (!slug || !wanted) return;

    loading = true;
    missing = false;
    error = undefined;

    void (async () => {
      try {
        const found = await api.job(slug, wanted);
        if (org.slug !== slug || id !== wanted) return;
        job = found;

        // The job carries a repo *id*; every link and label in this console
        // uses slugs, so the repo list is what turns one into the other.
        const repos = await api.repos(slug, true);
        repo = repos.find((r) => r.id === found.repoId);
      } catch (e) {
        if (e instanceof ApiError && e.isNotFound) missing = true;
        else error = e instanceof ApiError ? e.message : 'Could not load that job.';
      } finally {
        loading = false;
      }
    })();
  });

  const metadata = $derived(
    job && Object.keys(job.metadata ?? {}).length > 0
      ? JSON.stringify(job.metadata, null, 2)
      : undefined
  );
</script>

<div class="space-y-5">
  <p class="text-xs">
    <a class="text-muted underline hover:text-ink" href="/o/{org.slug}/queue">← Queue</a>
  </p>

  {#if missing}
    <div class="py-10 text-center">
      <h1 class="text-lg font-semibold">No such job</h1>
      <p class="mt-2 text-sm text-faint">
        Nothing in {org.title} is called <code class="df-mono">{id}</code>. Job ids are counted per
        organization, so the same id in another org is a different job.
      </p>
    </div>
  {:else if error}
    <Alert>{error}</Alert>
  {:else if !job}
    <Loading what="Loading {id}" />
  {:else}
    <div>
      <div class="flex flex-wrap items-center gap-3">
        <h1 class="text-lg font-semibold">{job.title}</h1>
        <StatusPill status={job.status} />
      </div>
      <p class="df-mono mt-1 text-xs text-faint">{job.id}</p>
    </div>

    {#if job.description}
      <Card title="Description">
        <p class="text-sm whitespace-pre-wrap text-muted">{job.description}</p>
      </Card>
    {/if}

    {#if job.status === 'failed' && job.error}
      <Alert>{job.error}</Alert>
    {/if}

    {#if job.status === 'completed' && job.result}
      <Card title="Result">
        <p class="text-sm whitespace-pre-wrap text-muted">{job.result}</p>
      </Card>
    {/if}

    <Card title="Details">
      <dl class="grid grid-cols-1 gap-x-8 gap-y-3 text-sm sm:grid-cols-2">
        <div>
          <dt class="df-label">Repo</dt>
          <dd>
            {#if repo}
              <a
                class="df-mono text-muted underline hover:text-ink"
                href="/o/{org.slug}/queue?repo={encodeURIComponent(repo.slug)}"
              >
                {repo.slug}
              </a>
            {:else}
              <span class="text-faint">—</span>
            {/if}
          </dd>
        </div>
        <div>
          <dt class="df-label">Ticket</dt>
          <dd class="text-muted">{job.ticketRef ?? '—'}{job.tracker ? ` (${job.tracker})` : ''}</dd>
        </div>
        <div>
          <dt class="df-label">Claimed by</dt>
          <dd class="text-muted">{job.claimedByLabel ?? '—'}</dd>
        </div>
        <div>
          <dt class="df-label">Agent type</dt>
          <dd class="text-muted">{job.agentType ?? 'any'}</dd>
        </div>
        <div>
          <dt class="df-label">Queued</dt>
          <dd class="text-muted" title={absolute(job.createdAt)}>{relative(job.createdAt)}</dd>
        </div>
        <div>
          <dt class="df-label">Started</dt>
          <dd class="text-muted" title={absolute(job.startedAt)}>{relative(job.startedAt)}</dd>
        </div>
        <div>
          <dt class="df-label">Finished</dt>
          <dd class="text-muted" title={absolute(job.completedAt)}>{relative(job.completedAt)}</dd>
        </div>
        <div>
          <dt class="df-label">Attempts</dt>
          <dd class="text-muted">{job.attempts}</dd>
        </div>
      </dl>
    </Card>

    {#if job.dependsOn.length > 0}
      <Card
        title="Waiting on"
        description="This job cannot be claimed until all of these are completed."
      >
        <ul class="flex flex-wrap gap-2">
          {#each job.dependsOn as dependency (dependency)}
            <a
              class="df-mono rounded-md border border-edge px-2 py-1 text-xs text-muted transition hover:bg-raised hover:text-ink"
              href="/o/{org.slug}/queue/{dependency}"
            >
              {dependency}
            </a>
          {/each}
        </ul>
      </Card>
    {/if}

    {#if metadata}
      <Card
        title="Metadata"
        description="Opaque to dark-factory — whatever the queueing skill put here."
      >
        <pre class="df-mono overflow-x-auto whitespace-pre text-muted">{metadata}</pre>
      </Card>
    {/if}
  {/if}
</div>
