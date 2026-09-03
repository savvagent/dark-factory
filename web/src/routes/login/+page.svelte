<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';

  import { api, ApiError } from '$lib/api';
  import { session } from '$lib/session.svelte';
  import { isServerRoute, safeNext } from '$lib/next';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';
  import Field from '$lib/components/Field.svelte';

  /**
   * Sign in. Email plus a six-digit authenticator code — there is no password
   * field here because there is no password anywhere in the product.
   *
   * **Failures are one answer.** `df-auth` answers an unknown address, a
   * disabled account, a wrong code, and a replayed code identically, so that
   * signing in cannot be used to find out who has an account. This page renders
   * whatever the server said and adds nothing: a client that helpfully
   * distinguished "no such account" would reinstate the oracle the server went
   * to some trouble to remove.
   */

  type Mode = 'totp' | 'recovery';

  let email = $state('');
  let code = $state('');
  let mode = $state<Mode>('totp');
  let pending = $state(false);
  let error = $state<string | undefined>(undefined);

  const next = $derived(safeNext(page.url.searchParams.get('next')));

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    pending = true;
    error = undefined;

    try {
      const opened =
        mode === 'totp'
          ? await api.login(email.trim(), code.trim())
          : await api.loginWithRecoveryCode(email.trim(), code.trim());

      await session.refresh();

      if (opened.mustEnrollTotp) {
        // A recovery-code sign-in leaves TOTP intact; a *recovery link* removes
        // it. Either way the server is the one that decides, and it says so
        // here rather than the console guessing from which button was pressed.
        await goto('/enroll', { replaceState: true });
        return;
      }

      if (next && isServerRoute(next)) {
        // A full navigation, not `goto`: `/oauth/authorize` is rendered by
        // `df-web`, and the client router has no such page.
        location.assign(next);
        return;
      }

      await goto(next ?? '/', { replaceState: true });
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Sign-in failed.';
      code = '';
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
      Your email address and a code from your authenticator app.
    {/if}
  </p>

  <form class="mt-6 space-y-4" onsubmit={submit}>
    <Field label="Email">
      <input
        class="df-input"
        type="email"
        name="email"
        autocomplete="username"
        required
        bind:value={email}
      />
    </Field>

    <Field
      label={mode === 'totp' ? 'Authenticator code' : 'Recovery code'}
      hint={mode === 'totp'
        ? 'Six digits from your authenticator app.'
        : 'One of the codes issued when you enrolled. Each works once.'}
    >
      <input
        class="df-input df-mono"
        type="text"
        name="code"
        inputmode={mode === 'totp' ? 'numeric' : 'text'}
        autocomplete="one-time-code"
        autocapitalize="off"
        spellcheck="false"
        required
        bind:value={code}
      />
    </Field>

    {#if error}<Alert>{error}</Alert>{/if}

    <Button type="submit" {pending}>Sign in</Button>
  </form>

  <div class="mt-6 space-y-2 border-t border-edge/50 pt-4 text-xs text-faint">
    <p>
      <button
        class="text-muted underline hover:text-ink"
        onclick={() => {
          mode = mode === 'totp' ? 'recovery' : 'totp';
          code = '';
          error = undefined;
        }}
      >
        {mode === 'totp' ? 'Use a recovery code instead' : 'Use my authenticator instead'}
      </button>
    </p>
    <p>
      Lost your authenticator? Use a recovery code above. We send no email, so there is no recovery
      link — if the codes are gone too, an admin of an organization you belong to can clear your
      authenticator so you can enrol a new one.
    </p>
    <p class="pt-2">
      No account? <a class="text-muted underline hover:text-ink" href="/signup">Create one</a>.
    </p>
  </div>
</div>
