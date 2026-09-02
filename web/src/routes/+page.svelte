<script lang="ts">
  import { goto } from '$app/navigation';
  import { session } from '$lib/session.svelte';
  import Loading from '$lib/components/Loading.svelte';

  /**
   * `/` is a signpost, not a page.
   *
   * There is no useful org-less view: everything the console shows — the queue,
   * repos, members, the meter — is scoped to one org, because everything in
   * `df-core` is. So this resolves where to go and goes there, and a brand new
   * account with no memberships is sent to create an org rather than shown an
   * empty shell that explains nothing.
   */
  $effect(() => {
    if (!session.ready || !session.signedIn) return;
    const home = session.homeOrg;
    void goto(home ? `/o/${home}` : '/orgs/new', { replaceState: true });
  });
</script>

<Loading what="Finding your organization" />
