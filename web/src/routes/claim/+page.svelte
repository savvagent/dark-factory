<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';

  import { api, ApiError } from '$lib/api';
  import { session } from '$lib/session.svelte';
  import * as webauthn from '$lib/webauthn';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';
  import Field from '$lib/components/Field.svelte';

  /**
   * Register a new passkey after an admin cleared this account's.
   *
   * The assisted half of recovery, and the reason it exists: an account with no
   * passkeys and no outstanding claim would be claimable by whoever reached
   * registration first. The code is what makes it re-registrable only by
   * whoever the admin handed it to.
   *
   * The code arrives in the URL when someone follows the link an admin sent
   * them, but **nothing is spent on load** — the server consumes it only at
   * `claim/finish`, so a link preview that fetches this page burns nothing, and
   * an abandoned ceremony leaves the code still usable.
   */

  let code = $state(page.url.searchParams.get('code') ?? '');
  let pending = $state(false);
  let error = $state<string | undefined>(undefined);

  async function claim(event: SubmitEvent) {
    event.preventDefault();
    pending = true;
    error = undefined;
    try {
      const started = await api.claimStart(code.trim());
      const credential = await webauthn.register(started.challenge as never);
      await api.claimFinish(started.ceremonyId, code.trim(), credential, 'Replacement device');
      await session.refresh();
      await goto('/', { replaceState: true });
    } catch (e) {
      error =
        e instanceof webauthn.WebauthnError || e instanceof ApiError
          ? e.message
          : 'That code was not accepted.';
    } finally {
      pending = false;
    }
  }
</script>

<svelte:head><title>Register a new passkey · dark-factory</title></svelte:head>

<div class="mx-auto max-w-sm py-8">
  <h1 class="text-lg font-semibold">Register a new passkey</h1>
  <p class="mt-1 text-sm text-faint">
    Use the one-time code an admin gave you. It works once and replaces every passkey the account
    had.
  </p>

  <form class="mt-6 space-y-4" onsubmit={claim}>
    <Field label="Recovery code">
      <input
        class="df-input df-mono"
        type="text"
        autocapitalize="off"
        spellcheck="false"
        required
        bind:value={code}
      />
    </Field>

    {#if error}<Alert>{error}</Alert>{/if}

    <Button type="submit" {pending}>Create a passkey</Button>
  </form>

  <p class="mt-6 text-xs text-faint">
    <a class="text-muted underline hover:text-ink" href="/login">Back to sign in</a>
  </p>
</div>
