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
  import Loading from '$lib/components/Loading.svelte';

  /**
   * Enrol an authenticator.
   *
   * Two properties of this page are load-bearing, and both are about the
   * recovery codes rather than the QR code.
   *
   * **They are shown exactly once.** The server stores only hashes, so there is
   * no second chance by construction. That is why the flow is two steps with an
   * explicit acknowledgement between them: a console that showed the codes
   * beside the confirm field would let someone confirm and navigate away in one
   * motion, and would have quietly shipped an account-recovery story of "email
   * support".
   *
   * **Enrollment is not finished until the code is confirmed.** `POST
   * /api/me/totp` writes an unconfirmed credential; until `confirm` proves
   * possession, the credential cannot sign anyone in. Someone who abandons this
   * page has not locked themselves out — they can start again, and the new
   * enrollment supersedes the old.
   */

  type Step = 'starting' | 'codes' | 'confirm' | 'done';

  let step = $state<Step>('starting');
  let enrollment = $state<Enrollment | undefined>(undefined);
  let qr = $state<string | undefined>(undefined);
  let saved = $state(false);
  let code = $state('');
  let pending = $state(false);
  let error = $state<string | undefined>(undefined);

  $effect(() => {
    if (step === 'starting' && !enrollment) void begin();
  });

  async function begin() {
    error = undefined;
    try {
      const started = await api.beginTotp();
      enrollment = started;
      step = 'codes';
      // Rendered locally, never fetched. A QR code built by a third-party image
      // service would hand the TOTP secret to that service.
      qr = await QRCode.toString(started.provisioningUri, {
        type: 'svg',
        margin: 1,
        color: { dark: '#0f172a', light: '#f8fafc' }
      });
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Could not start enrollment.';
    }
  }

  async function confirm(event: SubmitEvent) {
    event.preventDefault();
    pending = true;
    error = undefined;
    try {
      await api.confirmTotp(code.trim());
      await session.refresh();
      step = 'done';
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
    // The codes are already on screen, so this is a convenience rather than the
    // only way to keep them — but it has to admit failure. A "Copied" that
    // copied nothing, for a credential shown exactly once, is a lost account.
    copied = (await copy((enrollment?.recoveryCodes ?? []).join('\n'))) ? 'copied' : 'failed';
    setTimeout(() => (copied = 'idle'), 2500);
  }
</script>

<svelte:head><title>Set up your authenticator · dark-factory</title></svelte:head>

<div class="mx-auto max-w-lg py-8">
  <h1 class="text-lg font-semibold">Set up your authenticator</h1>

  {#if error && !enrollment}
    <div class="mt-4"><Alert>{error}</Alert></div>
    <div class="mt-4"><Button onclick={begin}>Try again</Button></div>
  {:else if !enrollment}
    <Loading what="Preparing your credential" />
  {:else if step === 'codes'}
    <p class="mt-1 text-sm text-faint">
      Save these recovery codes first. They are shown once and stored only as hashes — nobody,
      including us, can show them to you again.
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
      Each code works once. If you lose both your authenticator and these codes, the only way back
      in is a recovery link emailed to your address.
    </p>

    <label class="mt-5 flex items-start gap-2 text-sm text-muted">
      <input type="checkbox" class="mt-0.5" bind:checked={saved} />
      <span>I have saved these codes somewhere I can get to without my phone.</span>
    </label>

    <div class="mt-4">
      <Button disabled={!saved} onclick={() => (step = 'confirm')}>Continue</Button>
    </div>
  {:else}
    <p class="mt-1 text-sm text-faint">
      Scan this with your authenticator app, then type the code it shows.
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

          <Button type="submit" {pending}>Finish setting up</Button>
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
