<script lang="ts">
  import { api, ApiError } from '$lib/api';
  import { useOrg } from '$lib/org.svelte';
  import { relative } from '$lib/format';
  import { beginConnect } from '$lib/trackerState';
  import type {
    ProviderSetup,
    TrackerConnection,
    TrackerConnections,
    TrackerProvider
  } from '$lib/types';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';
  import Card from '$lib/components/Card.svelte';
  import Loading from '$lib/components/Loading.svelte';

  /**
   * Connecting a tracker to this org.
   *
   * Nothing about either provider is baked into this bundle. The install and
   * consent URLs come from the server, which builds them from the App slug and
   * OAuth client id *this* deployment was configured with — a hard-coded slug
   * is how a staging console sends an admin to install the production App.
   *
   * `configured` is likewise the server's answer, not a guess: a Connect button
   * on a deployment with no OAuth client would walk an admin through installing
   * an App they then have to uninstall by hand.
   */

  const org = useOrg();

  let data = $state<TrackerConnections | undefined>(undefined);
  let loading = $state(true);
  let error = $state<string | undefined>(undefined);
  let busy = $state<TrackerProvider | undefined>(undefined);

  let latest = 0;

  $effect(() => {
    const org_ = org.slug;
    if (!org_ || !org.isAdmin) return;

    const seq = ++latest;
    loading = true;
    error = undefined;

    void (async () => {
      try {
        const loaded = await api.trackerConnections(org_);
        if (seq !== latest) return;
        data = loaded;
      } catch (e) {
        if (seq !== latest) return;
        error = e instanceof ApiError ? e.message : 'Could not load tracker connections.';
      } finally {
        if (seq === latest) loading = false;
      }
    })();
  });

  const connections = $derived(data?.connections ?? []);
  const connectionFor = $derived((provider: TrackerProvider): TrackerConnection | undefined =>
    connections.find((c) => c.provider === provider)
  );

  function connect(provider: TrackerProvider, setup: ProviderSetup) {
    if (!setup.startUrl) return;
    // The nonce is stored before we navigate, never after: a redirect that
    // beat the write would come back to a check with nothing to check against.
    const state = beginConnect(org.slug, provider);
    const url = new URL(setup.startUrl);
    url.searchParams.set('state', state);
    window.location.href = url.toString();
  }

  async function disconnect(provider: TrackerProvider) {
    busy = provider;
    error = undefined;
    try {
      await api.disconnectTracker(org.slug, provider);
      data = await api.trackerConnections(org.slug);
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Could not disconnect that tracker.';
    } finally {
      busy = undefined;
    }
  }

  const providers: { provider: TrackerProvider; name: string; identifies: string }[] = [
    { provider: 'github', name: 'GitHub', identifies: 'Installation' },
    { provider: 'jira', name: 'JIRA', identifies: 'Site' }
  ];
</script>

<div class="space-y-5">
  <div>
    <h1 class="text-lg font-semibold">Trackers</h1>
    <p class="mt-0.5 text-sm text-faint">
      Connect an issue tracker once per organization, then point individual repos at a project on
      the <a class="underline hover:text-ink" href="/o/{org.slug}/repos">Repos</a> page. A labelled issue
      becomes a job; finishing the job writes back a comment and moves the ticket.
    </p>
  </div>

  {#if !org.isAdmin}
    <Alert>Only an owner or admin can connect a tracker.</Alert>
  {:else if error}
    <Alert>{error}</Alert>
  {/if}

  {#if org.isAdmin}
    {#if loading && !data}
      <Loading what="Loading tracker connections" />
    {:else if data}
      {#each providers as { provider, name, identifies } (provider)}
        {@const setup = data[provider]}
        {@const connection = connectionFor(provider)}
        <Card title={name}>
          {#if connection}
            <div class="flex flex-wrap items-center gap-3">
              <div class="min-w-0 flex-1">
                <p class="text-sm text-ink">
                  {identifies}
                  <code class="df-mono">{connection.externalId}</code>
                </p>
                <p class="mt-0.5 text-xs text-faint">
                  Connected {relative(connection.createdAt)}{connection.hasCredentials
                    ? ' · credentials stored'
                    : ''}
                </p>
              </div>
              <Button tone="quiet" pending={busy === provider} onclick={() => disconnect(provider)}>
                Disconnect
              </Button>
            </div>
            <p class="mt-3 text-xs text-faint">
              Disconnecting leaves each repo's binding in place but inert — nothing syncs until
              {name} is connected again.
            </p>
          {:else if setup.configured}
            <p class="text-sm text-muted">
              Not connected. {#if provider === 'github'}
                You will install the dark-factory GitHub App on the organization whose issues this
                org works from, and come back here.
              {:else}
                You will authorize dark-factory against one Atlassian site. Grant access to a single
                site — one JIRA site per organization is what this stores.
              {/if}
            </p>
            <div class="mt-3">
              <Button onclick={() => connect(provider, setup)}>Connect {name}</Button>
            </div>
          {:else}
            <p class="text-sm text-muted">
              This deployment does not offer {name} sync. An operator configures it with the
              {provider === 'github' ? 'GitHub App' : 'Atlassian OAuth'} credentials.
            </p>
          {/if}
        </Card>
      {/each}
    {/if}
  {/if}
</div>
