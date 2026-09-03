<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';

  import { api, ApiError } from '$lib/api';
  import { session } from '$lib/session.svelte';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';

  /**
   * What a recovery link opens, for someone whose authenticator is gone.
   *
   * Spending it removes the current authenticator and opens a session so a new
   * one can be enrolled — a genuinely destructive act, which is the second
   * reason it is behind a button rather than a page load. The first is the same
   * as `/verify`: mail scanners follow links.
   */

  const token = $derived(page.url.searchParams.get('token') ?? '');

  let pending = $state(false);
  let error = $state<string | undefined>(undefined);

  async function confirm() {
    pending = true;
    error = undefined;
    try {
      await api.recover(token);
      await session.refresh();
      await goto('/enroll', { replaceState: true });
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'That link could not be used.';
    } finally {
      pending = false;
    }
  }
</script>

<svelte:head><title>Recover your account · dark-factory</title></svelte:head>

<div class="mx-auto max-w-sm py-8">
  <h1 class="text-lg font-semibold">Recover your account</h1>

  {#if !token}
    <Alert>
      This link is missing its token. Ask for a new one from the
      <a class="underline" href="/login">sign-in page</a>.
    </Alert>
  {:else}
    <p class="mt-1 text-sm text-faint">
      This removes the authenticator currently on your account and signs you in so you can enrol a
      new one. Your existing recovery codes stop working.
    </p>

    {#if error}<div class="mt-4"><Alert>{error}</Alert></div>{/if}

    <div class="mt-6">
      <Button tone="danger" {pending} onclick={confirm}>
        Remove my authenticator and continue
      </Button>
    </div>

    <p class="mt-6 text-xs text-faint">
      Still have your authenticator? Close this page — nothing has happened yet — and
      <a class="text-muted underline hover:text-ink" href="/login">sign in</a> as usual.
    </p>
  {/if}
</div>
