<script lang="ts">
  import { api, ApiError } from '$lib/api';
  import { useOrg } from '$lib/org.svelte';
  import { relative, slugPreview } from '$lib/format';
  import type { Lease, Repo, Team, TrackerBinding, TrackerProvider } from '$lib/types';
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
   * render something nobody had asked to see. The tracker bindings in the same
   * expander are loaded the same way and for the same reason.
   *
   * A binding lives here rather than on a page of its own because it is a fact
   * about one repo: which project its tickets come from, and what label on an
   * issue there means "queue this". The connection those bindings hang off is
   * per-org and lives on the Trackers page.
   */

  const org = useOrg();

  let repos = $state<Repo[]>([]);
  let teams = $state<Team[]>([]);
  let includeInactive = $state(false);
  let loading = $state(true);
  let error = $state<string | undefined>(undefined);

  let expanded = $state<string | undefined>(undefined);
  let leases = $state<Record<string, Lease[] | 'loading' | 'failed'>>({});
  let bindings = $state<Record<string, TrackerBinding[] | 'loading' | 'failed'>>({});

  // Keyed by `${repo}:${provider}` so two providers on one repo edit
  // independently and a third repo's draft never lands in this one's field.
  let refDraft = $state<Record<string, string>>({});
  let labelDraft = $state<Record<string, string>>({});
  let bindingBusy = $state<string | undefined>(undefined);
  let bindingError = $state<Record<string, string | undefined>>({});

  const PROVIDERS: { provider: TrackerProvider; name: string; hint: string }[] = [
    { provider: 'github', name: 'GitHub', hint: 'owner/repo' },
    { provider: 'jira', name: 'JIRA', hint: 'PROJECT KEY' }
  ];

  let showForm = $state(false);
  let slug = $state('');
  let name = $state('');
  let remotes = $state('');
  let teamId = $state('');
  let creating = $state(false);
  let formError = $state<string | undefined>(undefined);

  // Not $state: it identifies a request and is never rendered.
  let latest = 0;

  $effect(() => {
    const org_ = org.slug;
    const withInactive = includeInactive;
    if (!org_) return;

    // Sequence rather than an org comparison, because the include-inactive
    // toggle is a dependency too: flipped twice quickly, the first response can
    // land after the second and show the list the toggle no longer asks for.
    const seq = ++latest;

    loading = true;
    error = undefined;

    void (async () => {
      try {
        const [r, t] = await Promise.all([api.repos(org_, withInactive), api.teams(org_)]);
        if (seq !== latest) return;
        repos = r;
        teams = t;
      } catch (e) {
        if (seq !== latest) return;
        error = e instanceof ApiError ? e.message : 'Could not load repos.';
      } finally {
        if (seq === latest) loading = false;
      }
    })();
  });

  async function toggle(repo: Repo) {
    if (expanded === repo.slug) {
      expanded = undefined;
      return;
    }
    expanded = repo.slug;
    void loadLeases(repo.slug);
    void loadBindings(repo.slug);
  }

  async function loadLeases(slug_: string) {
    if (leases[slug_] && leases[slug_] !== 'failed') return;
    leases = { ...leases, [slug_]: 'loading' };
    try {
      const found = await api.leases(org.slug, slug_);
      leases = { ...leases, [slug_]: found };
    } catch {
      leases = { ...leases, [slug_]: 'failed' };
    }
  }

  async function loadBindings(slug_: string) {
    if (bindings[slug_] && bindings[slug_] !== 'failed') return;
    bindings = { ...bindings, [slug_]: 'loading' };
    try {
      const found = await api.trackerBindings(org.slug, slug_);
      bindings = { ...bindings, [slug_]: found };
      // Seed the drafts from what is stored, so the field an admin edits shows
      // what is live rather than an empty box beside a bound repo.
      for (const binding of found) {
        const key = `${slug_}:${binding.provider}`;
        refDraft[key] = binding.externalRef;
        labelDraft[key] = binding.triggerLabel;
      }
    } catch {
      bindings = { ...bindings, [slug_]: 'failed' };
    }
  }

  function bindingFor(slug_: string, provider: TrackerProvider): TrackerBinding | undefined {
    const found = bindings[slug_];
    return Array.isArray(found) ? found.find((b) => b.provider === provider) : undefined;
  }

  /**
   * Re-read after a write rather than patching the local array.
   *
   * The server decides two of the fields shown here — the default trigger
   * label, and whether the binding is `live` — so a locally patched row would
   * show what was typed instead of what was stored.
   */
  async function reloadBindings(slug_: string) {
    bindings = { ...bindings, [slug_]: 'failed' };
    await loadBindings(slug_);
  }

  async function saveBinding(slug_: string, provider: TrackerProvider) {
    const key = `${slug_}:${provider}`;
    bindingBusy = key;
    bindingError = { ...bindingError, [key]: undefined };
    try {
      const label = (labelDraft[key] ?? '').trim();
      await api.bindRepo(org.slug, slug_, provider, {
        externalRef: (refDraft[key] ?? '').trim(),
        ...(label ? { triggerLabel: label } : {})
      });
      await reloadBindings(slug_);
    } catch (e) {
      bindingError = {
        ...bindingError,
        [key]: e instanceof ApiError ? e.message : 'Could not save that binding.'
      };
    } finally {
      bindingBusy = undefined;
    }
  }

  async function removeBinding(slug_: string, provider: TrackerProvider) {
    const key = `${slug_}:${provider}`;
    bindingBusy = key;
    bindingError = { ...bindingError, [key]: undefined };
    try {
      await api.unbindRepo(org.slug, slug_, provider);
      refDraft[key] = '';
      labelDraft[key] = '';
      await reloadBindings(slug_);
    } catch (e) {
      bindingError = {
        ...bindingError,
        [key]: e instanceof ApiError ? e.message : 'Could not remove that binding.'
      };
    } finally {
      bindingBusy = undefined;
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

            <div class="border-t border-edge/60 px-4 py-3">
              <h3 class="text-xs font-semibold tracking-wide text-muted uppercase">Trackers</h3>

              {#if bindings[repo.slug] === 'loading'}
                <Loading what="Reading tracker bindings" />
              {:else if bindings[repo.slug] === 'failed'}
                <Alert>Could not read this repo's tracker bindings.</Alert>
              {:else}
                <ul class="mt-2 space-y-3">
                  {#each PROVIDERS as { provider, name, hint } (provider)}
                    {@const binding = bindingFor(repo.slug, provider)}
                    {@const key = `${repo.slug}:${provider}`}
                    <li>
                      <div class="flex flex-wrap items-end gap-2">
                        <span class="w-14 shrink-0 text-sm text-ink">{name}</span>

                        {#if org.isAdmin}
                          <label class="min-w-0 flex-1">
                            <span class="sr-only">{name} project</span>
                            <input
                              class="df-input df-mono"
                              placeholder={hint}
                              bind:value={refDraft[key]}
                            />
                          </label>
                          <label class="w-32 shrink-0">
                            <span class="sr-only">{name} trigger label</span>
                            <input
                              class="df-input df-mono"
                              placeholder="dark-factory"
                              bind:value={labelDraft[key]}
                            />
                          </label>
                          <Button
                            pending={bindingBusy === key}
                            onclick={() => saveBinding(repo.slug, provider)}
                          >
                            {binding ? 'Update' : 'Bind'}
                          </Button>
                          {#if binding}
                            <Button
                              tone="quiet"
                              pending={bindingBusy === key}
                              onclick={() => removeBinding(repo.slug, provider)}
                            >
                              Remove
                            </Button>
                          {/if}
                        {:else if binding}
                          <span class="df-mono text-sm text-muted">{binding.externalRef}</span>
                          <span class="text-xs text-faint">label {binding.triggerLabel}</span>
                        {:else}
                          <span class="text-xs text-faint">not bound</span>
                        {/if}
                      </div>

                      {#if binding && !binding.live}
                        <p class="mt-1 text-xs text-faint">
                          Stored, but nothing syncs until {name} is connected on the
                          <a class="underline hover:text-ink" href="/o/{org.slug}/trackers">
                            Trackers
                          </a>
                          page.
                        </p>
                      {/if}
                      {#if bindingError[key]}
                        <p class="mt-1 text-xs text-rose-400">{bindingError[key]}</p>
                      {/if}
                    </li>
                  {/each}
                </ul>

                <p class="mt-3 text-xs text-faint">
                  An issue in the bound project carrying the trigger label becomes a job in this
                  repo. GitHub takes <code class="df-mono">owner/repo</code>; JIRA takes a project
                  key. Both are matched exactly against what the provider sends.
                </p>
              {/if}
            </div>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</div>
