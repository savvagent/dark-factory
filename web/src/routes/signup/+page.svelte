<script lang="ts">
  import { api, ApiError } from '$lib/api';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';
  import Field from '$lib/components/Field.svelte';

  /**
   * Create an account.
   *
   * The success state is the same whether or not the address was already
   * registered — `POST /api/auth/signup` answers identically on purpose, and
   * this page shows the server's own sentence rather than composing a friendlier
   * one that would leak the difference.
   */

  let email = $state('');
  let name = $state('');
  let pending = $state(false);
  let error = $state<string | undefined>(undefined);
  let sent = $state<string | undefined>(undefined);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    pending = true;
    error = undefined;
    try {
      const accepted = await api.signup(email.trim(), name.trim());
      sent = accepted.message;
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Could not create that account.';
    } finally {
      pending = false;
    }
  }
</script>

<svelte:head><title>Create an account · dark-factory</title></svelte:head>

<div class="mx-auto max-w-sm py-8">
  {#if sent}
    <h1 class="text-lg font-semibold">Check your email</h1>
    <p class="mt-2 text-sm text-muted">{sent}</p>
    <p class="mt-4 text-xs text-faint">
      The link opens a page with a button on it. Nothing is confirmed until you press it — so a mail
      scanner following the link cannot use it up before you do.
    </p>
    <p class="mt-6 text-xs text-faint">
      <a class="text-muted underline hover:text-ink" href="/login">Back to sign in</a>
    </p>
  {:else}
    <h1 class="text-lg font-semibold">Create an account</h1>
    <p class="mt-1 text-sm text-faint">
      No password. You confirm your address, then enrol an authenticator app.
    </p>

    <form class="mt-6 space-y-4" onsubmit={submit}>
      <Field label="Email">
        <input class="df-input" type="email" autocomplete="username" required bind:value={email} />
      </Field>

      <Field label="Name" hint="Optional. Shown to the other people in your organizations.">
        <input class="df-input" type="text" autocomplete="name" bind:value={name} />
      </Field>

      {#if error}<Alert>{error}</Alert>{/if}

      <Button type="submit" {pending}>Send me a confirmation link</Button>
    </form>

    <p class="mt-6 text-xs text-faint">
      Already have an account? <a class="text-muted underline hover:text-ink" href="/login"
        >Sign in</a
      >.
    </p>
  {/if}
</div>
