<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';

  import { api, ApiError } from '$lib/api';
  import { session } from '$lib/session.svelte';
  import * as webauthn from '$lib/webauthn';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';

  /**
   * Sign in.
   *
   * **There is no field on this page**, and that is the whole design. The
   * passkey is discoverable, so the browser resolves which account is signing
   * in and nothing is submitted beforehand. A login form that took an address
   * would be an oracle for which of an enterprise's employees hold accounts —
   * a target list for the phishing campaign that comes next — and every
   * previous version of this page needed careful, fragile machinery to avoid
   * being one.
   */

  const next = $derived(page.url.searchParams.get('next'));

  let pending = $state(false);
  let error = $state<string | undefined>(undefined);
  let supported = $state(true);

  $effect(() => {
    supported = webauthn.isSupported();
  });

  async function signIn() {
    pending = true;
    error = undefined;
    try {
      const started = await api.loginStart();
      const credential = await webauthn.authenticate(started.challenge as never);
      await api.loginFinish(started.ceremonyId, credential);
      await session.refresh();
      await goto(next ?? '/', { replaceState: true });
    } catch (e) {
      error =
        e instanceof webauthn.WebauthnError || e instanceof ApiError
          ? e.message
          : 'Could not sign you in.';
    } finally {
      pending = false;
    }
  }
</script>

<svelte:head><title>Sign in · dark-factory</title></svelte:head>

<div class="mx-auto max-w-sm py-8">
  <h1 class="text-lg font-semibold">Sign in</h1>
  <p class="mt-1 text-sm text-faint">
    {#if next}
      Sign in to continue.
    {:else}
      Your passkey knows who you are — there is nothing to type.
    {/if}
  </p>

  {#if !supported}
    <div class="mt-4">
      <Alert>
        This browser does not support passkeys. Try a current version of Safari, Chrome, Edge or
        Firefox.
      </Alert>
    </div>
  {:else}
    {#if error}<div class="mt-4"><Alert>{error}</Alert></div>{/if}

    <div class="mt-5">
      <Button {pending} onclick={signIn}>Sign in with a passkey</Button>
    </div>
  {/if}

  <div class="mt-6 space-y-2 border-t border-edge/50 pt-4 text-xs text-faint">
    <p>
      Lost every device you registered? There is no email to recover through, so an admin of an
      organization you belong to can issue you a one-time code to register a new passkey. If you
      have one, <a class="text-muted underline hover:text-ink" href="/claim">use it here</a>.
    </p>
    <p class="pt-2">
      No account? <a class="text-muted underline hover:text-ink" href="/signup">Create one</a>.
    </p>
  </div>
</div>
