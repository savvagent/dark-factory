<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';

  import { api, ApiError } from '$lib/api';
  import { completeConnect } from '$lib/trackerState';
  import Alert from '$lib/components/Alert.svelte';
  import Loading from '$lib/components/Loading.svelte';

  /**
   * Where GitHub and Atlassian send the browser back.
   *
   * **Not under `/o/[org]/`, and it cannot be.** A redirect URI is one static
   * string registered with the provider; it cannot carry an org slug that
   * varies per customer. The org rides in the OAuth `state` instead, alongside
   * a nonce this browser minted before it left — see `$lib/trackerState`.
   *
   * **The page renders; the button-equivalent POSTs.** The authorization code
   * arrives in this URL, and everything that spends a single-use credential in
   * this product is a POST, so that a preview fetcher following the link burns
   * nothing. Here the POST is issued by script on arrival rather than by a
   * click, which is the same trade the rest of the console makes: a GET
   * rendered this page, and only a script the page ran can spend the code.
   */

  let error = $state<string | undefined>(undefined);
  let done = $state(false);

  $effect(() => {
    const params = page.url.searchParams;
    if (done) return;
    done = true;

    // A provider can refuse before we ever see a code — an admin who cancelled
    // on the consent screen lands here with `error` and nothing else.
    const denied = params.get('error_description') ?? params.get('error');
    if (denied) {
      error = `${denied}. Nothing was connected.`;
      return;
    }

    const pending = completeConnect(params.get('state'));
    if (!pending) {
      error =
        'This connect flow did not start in this tab, or it has already been completed. ' +
        'Open your organization’s Trackers page and start it again.';
      return;
    }

    const code = params.get('code');
    if (!code) {
      error =
        pending.provider === 'github'
          ? 'GitHub sent no authorization code. The App must have "Request user authorization ' +
            '(OAuth) during installation" enabled before an installation can be verified.'
          : 'The provider sent no authorization code. Start the connect flow again.';
      return;
    }

    // GitHub sends the installation alongside the code; JIRA has no equivalent
    // and the site is read from Atlassian on the server side.
    const rawInstallation = params.get('installation_id');
    const installationId = rawInstallation ? Number(rawInstallation) : undefined;

    void (async () => {
      try {
        await api.connectTracker(pending.org, pending.provider, {
          code,
          ...(Number.isSafeInteger(installationId) ? { installationId } : {})
        });
        await goto(`/o/${pending.org}/trackers`, { replaceState: true });
      } catch (e) {
        error = e instanceof ApiError ? e.message : 'Could not finish connecting that tracker.';
      }
    })();
  });
</script>

<div class="mx-auto max-w-md py-10">
  {#if error}
    <h1 class="text-lg font-semibold">That did not connect</h1>
    <div class="mt-3"><Alert>{error}</Alert></div>
    <p class="mt-4 text-sm text-faint">
      <a class="underline hover:text-ink" href="/orgs">Back to your organizations</a>
    </p>
  {:else}
    <Loading what="Finishing the connection" />
  {/if}
</div>
