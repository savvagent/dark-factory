<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';

  import { api, ApiError } from '$lib/api';
  import { session } from '$lib/session.svelte';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';

  /**
   * What an invitation link opens.
   *
   * Behind the sign-in guard in the root layout, deliberately. The server
   * requires a session whose address matches the one invited — otherwise a code
   * forwarded to the wrong person is a way into someone else's org — and a page
   * that redeemed on load would burn the token for whoever the browser happened
   * to be signed in as. Codes now travel through chat, which unfurls links just
   * as eagerly as a mail scanner did.
   *
   * The mismatch case (`invite_wrong_account`, `403`) gets its own message,
   * because the fix is "sign in as the invited address", which is not something
   * the generic error text can know.
   */

  const org = $derived(page.params.org ?? '');
  const token = $derived(page.url.searchParams.get('token') ?? '');

  let pending = $state(false);
  let error = $state<string | undefined>(undefined);
  let wrongAccount = $state(false);

  async function accept() {
    pending = true;
    error = undefined;
    wrongAccount = false;
    try {
      const joined = await api.acceptInvite(org, token);
      await session.refresh();
      await goto(`/o/${joined.org.slug}`, { replaceState: true });
    } catch (e) {
      if (e instanceof ApiError) {
        wrongAccount = e.status === 403;
        error = e.message;
      } else {
        error = 'That invitation could not be accepted.';
      }
    } finally {
      pending = false;
    }
  }
</script>

<svelte:head><title>Join {org} · dark-factory</title></svelte:head>

<div class="mx-auto max-w-sm py-8">
  <h1 class="text-lg font-semibold">Join {org}</h1>

  {#if !token}
    <Alert>This link is missing its token. Ask whoever invited you to send another.</Alert>
  {:else}
    <p class="mt-1 text-sm text-faint">
      You are signed in as <span class="text-muted"
        >{session.me?.user.email ?? 'an account with no email set'}</span
      >. An invitation can only be accepted by the address it was sent to.
    </p>

    {#if error}
      <div class="mt-4">
        <Alert>
          {error}
          {#if wrongAccount}
            <span class="mt-1 block">
              Sign out and sign in as the invited address, then open this link again.
            </span>
          {/if}
        </Alert>
      </div>
    {/if}

    <div class="mt-6">
      <Button {pending} onclick={accept}>Accept the invitation</Button>
    </div>
  {/if}
</div>
