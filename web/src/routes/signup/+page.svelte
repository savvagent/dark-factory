<script lang="ts">
  import { goto } from '$app/navigation';
  import QRCode from 'qrcode';

  import { api, ApiError } from '$lib/api';
  import { copy } from '$lib/format';
  import { session } from '$lib/session.svelte';
  import type { Enrollment } from '$lib/types';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';
  import CopyField from '$lib/components/CopyField.svelte';
  import Field from '$lib/components/Field.svelte';

  /**
   * Create an account, in one sitting.
   *
   * There is no email anywhere in this product, so there is no "check your
   * inbox" step to break the flow into two visits: `POST /api/auth/signup`
   * hands the TOTP secret straight back, and the account exists the moment a
   * code from it is accepted.
   *
   * Three properties are load-bearing.
   *
   * **The recovery codes are shown exactly once**, before the QR, behind an
   * explicit acknowledgement. The server keeps only hashes, so there is no
   * second chance by construction — and with no emailed recovery link, these
   * codes are the *only* self-service way back into the account. A page that
   * showed them beside the confirm field would let someone finish and navigate
   * away in one motion.
   *
   * **Nothing is confirmed until a code proves possession.** Signup writes an
   * unconfirmed credential and opens no session. Someone who abandons this page
   * has not locked themselves out and has not taken the address hostage — they
   * start again, and the new enrollment supersedes the old.
   *
   * **An address that already has an authenticator is refused**, because
   * handing out a fresh enrollment for one would be account takeover. That is
   * why this page can say "already registered" where the old emailed flow
   * could not; see `df_web::routes::auth` for what it costs.
   */

  type Step = 'email' | 'codes' | 'confirm';

  let step = $state<Step>('email');
  let email = $state('');
  let name = $state('');
  let enrollment = $state<Enrollment | undefined>(undefined);
  let qr = $state<string | undefined>(undefined);
  let saved = $state(false);
  let code = $state('');
  let pending = $state(false);
  let error = $state<string | undefined>(undefined);
  let exists = $state(false);

  async function start(event: SubmitEvent) {
    event.preventDefault();
    pending = true;
    error = undefined;
    exists = false;
    try {
      const started = await api.signup(email.trim(), name.trim());
      enrollment = started;
      step = 'codes';
      // Rendered in this browser, never fetched. A QR built by a third-party
      // image service would hand that service the TOTP secret.
      qr = await QRCode.toString(started.provisioningUri, {
        type: 'svg',
        margin: 1,
        color: { dark: '#0f172a', light: '#f8fafc' }
      });
    } catch (e) {
      if (e instanceof ApiError && e.code === 'account_exists') {
        exists = true;
        error = e.message;
      } else {
        error = e instanceof ApiError ? e.message : 'Could not create that account.';
      }
    } finally {
      pending = false;
    }
  }

  async function confirm(event: SubmitEvent) {
    event.preventDefault();
    pending = true;
    error = undefined;
    try {
      await api.confirmSignup(email.trim(), code.trim());
      await session.refresh();
      await goto('/', { replaceState: true });
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'That code was not accepted.';
      code = '';
    } finally {
      pending = false;
    }
  }

  let copied = $state<'idle' | 'copied' | 'failed'>('idle');

  async function copyCodes() {
    // A convenience — the codes are already on screen — but it has to admit
    // failure. A "Copied" that copied nothing, for a credential shown exactly
    // once and with no emailed way back, is a lost account.
    copied = (await copy((enrollment?.recoveryCodes ?? []).join('\n'))) ? 'copied' : 'failed';
    setTimeout(() => (copied = 'idle'), 2500);
  }
</script>

<svelte:head><title>Create an account · dark-factory</title></svelte:head>

<div class="mx-auto max-w-lg py-8">
  {#if step === 'email'}
    <h1 class="text-lg font-semibold">Create an account</h1>
    <p class="mt-1 text-sm text-faint">
      No password and no email to confirm. You enrol an authenticator app now, and that is how you
      sign in from then on.
    </p>

    <form class="mt-6 max-w-sm space-y-4" onsubmit={start}>
      <Field label="Email" hint="Your unique identifier here. Nothing is ever sent to it.">
        <input class="df-input" type="email" autocomplete="username" required bind:value={email} />
      </Field>

      <Field label="Name" hint="Optional. Shown to the other people in your organizations.">
        <input class="df-input" type="text" autocomplete="name" bind:value={name} />
      </Field>

      {#if error}
        <Alert>
          {error}
          {#if exists}
            <a class="ml-1 underline hover:text-ink" href="/login">Sign in instead</a>.
          {/if}
        </Alert>
      {/if}

      <Button type="submit" {pending}>Continue</Button>
    </form>

    <p class="mt-6 text-xs text-faint">
      Already have an account? <a class="text-muted underline hover:text-ink" href="/login"
        >Sign in</a
      >.
    </p>
  {:else if step === 'codes' && enrollment}
    <h1 class="text-lg font-semibold">Save your recovery codes</h1>
    <p class="mt-1 text-sm text-faint">
      Shown once, and stored only as hashes — nobody, including us, can show them to you again.
    </p>

    <div class="df-card mt-5 p-4">
      <ul class="grid grid-cols-2 gap-x-6 gap-y-1.5">
        {#each enrollment.recoveryCodes as recoveryCode (recoveryCode)}
          <li class="df-mono text-ink">{recoveryCode}</li>
        {/each}
      </ul>
      <div class="mt-4 flex items-center gap-3 border-t border-edge/50 pt-3">
        <Button tone="quiet" onclick={copyCodes}>
          {copied === 'copied' ? 'Copied' : copied === 'failed' ? 'Select them above' : 'Copy all'}
        </Button>
        {#if copied === 'failed'}
          <span class="text-xs text-warn">
            The browser refused clipboard access. Select the codes above and copy them by hand.
          </span>
        {/if}
      </div>
    </div>

    <p class="mt-3 text-xs text-faint">
      Each code works once. We send no email, so there is no recovery link — if you lose both your
      authenticator and these codes, an admin of an organization you belong to is the only way back
      in, and for the last owner of an organization there is none.
    </p>

    <label class="mt-5 flex items-start gap-2 text-sm text-muted">
      <input type="checkbox" class="mt-0.5" bind:checked={saved} />
      <span>I have saved these codes somewhere I can get to without my phone.</span>
    </label>

    <div class="mt-4">
      <Button disabled={!saved} onclick={() => (step = 'confirm')}>Continue</Button>
    </div>
  {:else if enrollment}
    <h1 class="text-lg font-semibold">Set up your authenticator</h1>
    <p class="mt-1 text-sm text-faint">
      Scan this with any authenticator app, then type the code it shows. Your account is created
      when the code is accepted.
    </p>

    <div class="mt-5 flex flex-col gap-5 sm:flex-row sm:items-start">
      <div class="w-44 shrink-0 rounded-lg bg-ink p-2">
        {#if qr}
          <!-- Generated in this browser from the provisioning URI; no markup from the server. -->
          <!-- eslint-disable-next-line svelte/no-at-html-tags -->
          {@html qr}
        {/if}
      </div>

      <div class="min-w-0 flex-1 space-y-4">
        <CopyField label="Or enter this key by hand" value={enrollment.manualKey} />

        <form class="space-y-3" onsubmit={confirm}>
          <Field label="Code from your app" hint="Six digits. It changes every 30 seconds.">
            <input
              class="df-input df-mono"
              inputmode="numeric"
              autocomplete="one-time-code"
              autocapitalize="off"
              spellcheck="false"
              required
              bind:value={code}
            />
          </Field>

          {#if error}<Alert>{error}</Alert>{/if}

          <Button type="submit" {pending}>Create my account</Button>
        </form>
      </div>
    </div>

    <p class="mt-6 text-xs text-faint">
      <button class="text-muted underline hover:text-ink" onclick={() => (step = 'codes')}>
        Show my recovery codes again
      </button>
      — they are still on this page until you leave it.
    </p>
  {/if}
</div>
