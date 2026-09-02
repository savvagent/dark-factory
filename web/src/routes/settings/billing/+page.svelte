<script lang="ts">
  import { goto } from '$app/navigation';
  import { session } from '$lib/session.svelte';
  import Empty from '$lib/components/Empty.svelte';
  import Loading from '$lib/components/Loading.svelte';

  /**
   * The org-less billing URL, which exists because something else already
   * points at it.
   *
   * `df-billing`'s quota error and `df-mcp`'s upgrade prompt are built as
   * `{public_url}/settings/billing` — an agent that runs out of bucket puts that
   * exact string in front of a human. It has no org segment because the meter
   * builds it from configuration, once, at startup. So this page resolves the
   * org from the session and forwards; it is a signpost for a URL the rest of
   * the system already prints.
   */

  $effect(() => {
    if (!session.ready || !session.signedIn) return;
    const home = session.homeOrg;
    if (home) void goto(`/o/${home}/usage`, { replaceState: true });
  });
</script>

<svelte:head><title>Usage · dark-factory</title></svelte:head>

{#if session.ready && session.signedIn && !session.homeOrg}
  <Empty title="You are not in an organization yet.">
    Usage is measured per organization.
    <a class="text-muted underline hover:text-ink" href="/orgs/new">Create one</a>.
  </Empty>
{:else}
  <Loading what="Opening your usage" />
{/if}
