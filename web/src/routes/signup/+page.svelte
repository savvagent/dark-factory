<script lang="ts">
  import { goto } from '$app/navigation';

  import { api, ApiError } from '$lib/api';
  import { session } from '$lib/session.svelte';
  import * as webauthn from '$lib/webauthn';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';
  import Field from '$lib/components/Field.svelte';

  /**
   * Create an account.
   *
   * **The passkey comes first and the address comes second**, and that ordering
   * is a security property rather than a style choice. Nothing is submitted
   * before the ceremony — no email, no username — so this page cannot be used
   * to ask whether an account exists. Every earlier version of signup could: a
   * password leaks through "already registered", and TOTP leaked because the
   * secret had to come back in the response and so had to be refused for an
   * address that already had one.
   *
   * The address is collected afterwards, by someone already holding the key,
   * where "that one is taken" costs nothing an attacker can use.
   */

  type Step = 'intro' | 'profile';

  let step = $state<Step>('intro');
  let email = $state('');
  let name = $state('');
  let pending = $state(false);
  let error = $state<string | undefined>(undefined);
  let supported = $state(true);
  let platform = $state(true);

  $effect(() => {
    supported = webauthn.isSupported();
    if (supported) void webauthn.hasPlatformAuthenticator().then((p) => (platform = p));
  });

  async function createAccount() {
    pending = true;
    error = undefined;
    try {
      const started = await api.signupStart();
      const credential = await webauthn.register(started.challenge as never);
      await api.signupFinish(started.ceremonyId, credential, deviceName());
      await session.refresh();
      step = 'profile';
    } catch (e) {
      error =
        e instanceof webauthn.WebauthnError || e instanceof ApiError
          ? e.message
          : 'Could not create that account.';
    } finally {
      pending = false;
    }
  }

  async function saveProfile(event: SubmitEvent) {
    event.preventDefault();
    pending = true;
    error = undefined;
    try {
      await api.setProfile({ email: email.trim(), name: name.trim() });
      await session.refresh();
      await goto('/', { replaceState: true });
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Could not save that.';
    } finally {
      pending = false;
    }
  }

  /** A first guess at a label, so the key list is not a row of blanks. */
  function deviceName(): string {
    const ua = navigator.userAgent;
    if (/iPhone|iPad/.test(ua)) return 'iPhone';
    if (/Android/.test(ua)) return 'Android device';
    if (/Mac OS X/.test(ua)) return 'Mac';
    if (/Windows/.test(ua)) return 'Windows PC';
    return 'This device';
  }
</script>

<svelte:head><title>Create an account · dark-factory</title></svelte:head>

<div class="mx-auto max-w-sm py-8">
  {#if step === 'intro'}
    <h1 class="text-lg font-semibold">Create an account</h1>
    <p class="mt-1 text-sm text-faint">
      No password, and no email to confirm. You create a passkey now, and that is how you sign in
      from then on.
    </p>

    {#if !supported}
      <div class="mt-4">
        <Alert>
          This browser does not support passkeys. Try a current version of Safari, Chrome, Edge or
          Firefox.
        </Alert>
      </div>
    {:else}
      <p class="mt-4 text-xs text-faint">
        {#if platform}
          Your device will ask for your fingerprint, face, or screen lock. A security key or your
          phone works too.
        {:else}
          You will need a security key or your phone — this device has no built-in authenticator.
        {/if}
      </p>

      {#if error}<div class="mt-4"><Alert>{error}</Alert></div>{/if}

      <div class="mt-5">
        <Button {pending} onclick={createAccount}>Create a passkey</Button>
      </div>
    {/if}

    <p class="mt-6 text-xs text-faint">
      Already have an account? <a class="text-muted underline hover:text-ink" href="/login"
        >Sign in</a
      >.
    </p>
  {:else}
    <h1 class="text-lg font-semibold">Your account is ready</h1>
    <p class="mt-1 text-sm text-faint">
      Tell us how to address you. Your email is how colleagues invite you to an organization — we
      never send anything to it.
    </p>

    <form class="mt-6 space-y-4" onsubmit={saveProfile}>
      <Field label="Email" hint="Your unique identifier here. Nothing is ever sent to it.">
        <input class="df-input" type="email" autocomplete="username" required bind:value={email} />
      </Field>

      <Field label="Name" hint="Optional. Shown to the other people in your organizations.">
        <input class="df-input" type="text" autocomplete="name" bind:value={name} />
      </Field>

      {#if error}<Alert>{error}</Alert>{/if}

      <Button type="submit" {pending}>Save and continue</Button>
    </form>

    <p class="mt-6 text-xs text-faint">
      Next, add a second passkey from <a
        class="text-muted underline hover:text-ink"
        href="/settings">your settings</a
      >. One passkey is one device, and there is no email to recover through if you lose it.
    </p>
  {/if}
</div>
