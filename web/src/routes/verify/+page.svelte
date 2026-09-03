<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';

  import { api, ApiError } from '$lib/api';
  import { session } from '$lib/session.svelte';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';

  /**
   * What a verification link opens.
   *
   * **This page spends nothing on load.** The token sits in the query string
   * and is redeemed only when the button is pressed, by a `POST`. Mail
   * scanners, link-preview fetchers, and corporate URL rewriters follow every
   * link in every message; a page that redeemed on mount would be burned before
   * the human ever saw it, and the failure would look exactly like an attack.
   * `df-web`'s `every_single_use_redemption_is_a_post` holds the other half of
   * this bargain.
   */

  const token = $derived(page.url.searchParams.get('token') ?? '');

  let pending = $state(false);
  let error = $state<string | undefined>(undefined);
  let done = $state(false);

  async function confirm() {
    pending = true;
    error = undefined;
    try {
      const verified = await api.verify(token);
      done = true;

      // A session comes back only for an account with no confirmed
      // authenticator — which is exactly the account that still has to enrol
      // one. Anyone else confirmed an address and now signs in normally.
      if (verified.signedIn) {
        await session.refresh();
        await goto(verified.mustEnrollTotp ? '/enroll' : '/', { replaceState: true });
      }
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'That link could not be used.';
    } finally {
      pending = false;
    }
  }
</script>

<svelte:head><title>Confirm your email · dark-factory</title></svelte:head>

<div class="mx-auto max-w-sm py-8">
  <h1 class="text-lg font-semibold">Confirm your email</h1>

  {#if !token}
    <Alert>
      This link is missing its token. Ask for a new one from the
      <a class="underline" href="/login">sign-in page</a>.
    </Alert>
  {:else if done && !error}
    <Alert tone="ok">Your address is confirmed.</Alert>
    <p class="mt-4 text-sm text-muted">
      <a class="underline hover:text-ink" href="/login">Sign in</a> to continue.
    </p>
  {:else}
    <p class="mt-1 text-sm text-faint">
      Press the button to confirm. The link works once and expires in ten minutes.
    </p>

    {#if error}<div class="mt-4"><Alert>{error}</Alert></div>{/if}

    <div class="mt-6">
      <Button {pending} onclick={confirm}>Confirm my address</Button>
    </div>

    <p class="mt-6 text-xs text-faint">
      Expired? Ask for another from the
      <a class="text-muted underline hover:text-ink" href="/login">sign-in page</a>.
    </p>
  {/if}
</div>
