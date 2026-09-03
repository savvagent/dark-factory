<script lang="ts">
  import { goto } from '$app/navigation';

  import { api, ApiError } from '$lib/api';
  import { session } from '$lib/session.svelte';
  import { slugPreview } from '$lib/format';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';
  import Field from '$lib/components/Field.svelte';

  /**
   * Create an organization. The creator becomes its owner — an org with no
   * owner cannot be administered at all.
   *
   * A slug is a public identifier and is refused to an unverified address, so
   * the `403` case here is worth naming rather than dropping into the generic
   * error box: the reader has an action to take (confirm your email), and the
   * server's message says so.
   */

  let name = $state('');
  let slug = $state('');
  let touchedSlug = $state(false);
  let pending = $state(false);
  let error = $state<string | undefined>(undefined);

  // Suggested from the name until the user edits it themselves, and then left
  // alone — a slug that keeps re-deriving under a typing hand is unusable.
  const suggested = $derived(touchedSlug ? slug : slugPreview(name));

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    pending = true;
    error = undefined;
    try {
      const org = await api.createOrg(suggested, name.trim() || suggested);
      await session.refresh();
      session.lastOrg = org.slug;
      await goto(`/o/${org.slug}`, { replaceState: true });
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Could not create that organization.';
    } finally {
      pending = false;
    }
  }
</script>

<svelte:head><title>New organization · dark-factory</title></svelte:head>

<div class="mx-auto max-w-sm py-8">
  <h1 class="text-lg font-semibold">New organization</h1>
  <p class="mt-1 text-sm text-faint">
    {#if session.orgs.length === 0}
      Everything in dark-factory belongs to an organization — repos, the queue, your agents' tokens.
      Create one to get started.
    {:else}
      You will be its owner.
    {/if}
  </p>

  <form class="mt-6 space-y-4" onsubmit={submit}>
    <Field label="Name">
      <input class="df-input" type="text" required bind:value={name} />
    </Field>

    <Field
      label="Slug"
      hint="Appears in URLs and in every agent's configuration. It cannot be changed later."
    >
      <input
        class="df-input df-mono"
        type="text"
        value={suggested}
        oninput={(event) => {
          touchedSlug = true;
          slug = event.currentTarget.value;
        }}
        required
      />
    </Field>

    {#if error}<Alert>{error}</Alert>{/if}

    <Button type="submit" {pending} disabled={!suggested}>Create organization</Button>
  </form>
</div>
