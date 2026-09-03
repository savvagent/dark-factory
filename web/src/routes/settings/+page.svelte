<script lang="ts">
  import { api, ApiError } from '$lib/api';
  import { relative } from '$lib/format';
  import { session } from '$lib/session.svelte';
  import * as webauthn from '$lib/webauthn';
  import type { Passkey } from '$lib/types';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';
  import Card from '$lib/components/Card.svelte';
  import Field from '$lib/components/Field.svelte';
  import Loading from '$lib/components/Loading.svelte';

  /**
   * The account: its passkeys, and its profile.
   *
   * **The second passkey is the recovery story**, so this page leads with it
   * rather than burying it. There is no email, so an account with one passkey
   * is one lost device away from needing an admin — and the owner of a
   * single-person org has no admin above them.
   */

  let keys = $state<Passkey[]>([]);
  let loading = $state(true);
  let error = $state<string | undefined>(undefined);
  let busy = $state<string | undefined>(undefined);

  let email = $state('');
  let name = $state('');
  let savingProfile = $state(false);
  let profileError = $state<string | undefined>(undefined);
  let profileSaved = $state(false);

  $effect(() => {
    void load();
  });

  $effect(() => {
    const me = session.me?.user;
    if (me) {
      email = me.email ?? '';
      name = me.name ?? '';
    }
  });

  async function load() {
    try {
      keys = await api.passkeys();
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Could not load your passkeys.';
    } finally {
      loading = false;
    }
  }

  async function addPasskey() {
    busy = 'add';
    error = undefined;
    try {
      const started = await api.addPasskeyStart();
      const credential = await webauthn.register(started.challenge as never);
      await api.addPasskeyFinish(started.ceremonyId, credential, 'New device');
      keys = await api.passkeys();
      await session.refresh();
    } catch (e) {
      error =
        e instanceof webauthn.WebauthnError || e instanceof ApiError
          ? e.message
          : 'Could not add that passkey.';
    } finally {
      busy = undefined;
    }
  }

  async function act(id: string, fn: () => Promise<unknown>) {
    busy = id;
    error = undefined;
    try {
      await fn();
      keys = await api.passkeys();
      await session.refresh();
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'That did not work.';
    } finally {
      busy = undefined;
    }
  }

  async function saveProfile(event: SubmitEvent) {
    event.preventDefault();
    savingProfile = true;
    profileError = undefined;
    profileSaved = false;
    try {
      await api.setProfile({ email: email.trim(), name: name.trim() });
      await session.refresh();
      profileSaved = true;
    } catch (e) {
      profileError = e instanceof ApiError ? e.message : 'Could not save that.';
    } finally {
      savingProfile = false;
    }
  }
</script>

<svelte:head><title>Settings · dark-factory</title></svelte:head>

<div class="space-y-5">
  <div>
    <h1 class="text-lg font-semibold">Settings</h1>
    <p class="mt-0.5 text-sm text-faint">Your passkeys and how you are addressed.</p>
  </div>

  {#if error}<Alert>{error}</Alert>{/if}

  {#if keys.length === 1 && !loading}
    <Alert tone="warn">
      You have one passkey, which means one device. We send no email, so if you lose it the only way
      back in is an admin of an organization you belong to — and if you are the only owner of yours,
      there is none. Add a second.
    </Alert>
  {/if}

  <Card
    title="Passkeys"
    description="Each one signs you in on its own. Add your phone as well as this device."
  >
    {#if loading}
      <Loading what="Loading your passkeys" />
    {:else}
      <ul class="divide-y divide-edge/50">
        {#each keys as key (key.id)}
          <li class="flex items-center gap-3 py-2.5">
            <div class="min-w-0 flex-1">
              <div class="text-sm text-ink">{key.nickname ?? 'Unnamed passkey'}</div>
              <p class="text-xs text-faint">
                Added {relative(key.createdAt)}
                {#if key.lastUsedAt}· last used {relative(key.lastUsedAt)}{:else}· never used{/if}
              </p>
            </div>

            <Button
              tone="quiet"
              pending={busy === `${key.id}:rename`}
              onclick={() => {
                const nickname = prompt('Name this passkey', key.nickname ?? '');
                if (nickname) act(`${key.id}:rename`, () => api.renamePasskey(key.id, nickname));
              }}
            >
              Rename
            </Button>

            <!-- The server refuses to remove the last one; hiding the button
                 too means a person is not offered a click that would lock them
                 out and then be told no. -->
            {#if keys.length > 1}
              <Button
                tone="danger"
                pending={busy === `${key.id}:remove`}
                onclick={() => act(`${key.id}:remove`, () => api.removePasskey(key.id))}
              >
                Remove
              </Button>
            {/if}
          </li>
        {/each}
      </ul>

      <div class="mt-4 border-t border-edge/50 pt-3">
        <Button pending={busy === 'add'} onclick={addPasskey}>Add a passkey</Button>
      </div>
    {/if}
  </Card>

  <Card title="Profile" description="Your email is an identifier here. Nothing is sent to it.">
    <form class="max-w-sm space-y-4" onsubmit={saveProfile}>
      <Field label="Email" hint="How colleagues invite you to an organization.">
        <input class="df-input" type="email" autocomplete="username" required bind:value={email} />
      </Field>

      <Field label="Name">
        <input class="df-input" type="text" autocomplete="name" bind:value={name} />
      </Field>

      {#if profileError}<Alert>{profileError}</Alert>{/if}
      {#if profileSaved}<Alert tone="ok">Saved.</Alert>{/if}

      <Button type="submit" pending={savingProfile}>Save</Button>
    </form>
  </Card>
</div>
