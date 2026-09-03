<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import type { Snippet } from 'svelte';

  import '../app.css';
  import { api, ApiError } from '$lib/api';
  import { session } from '$lib/session.svelte';
  import Alert from '$lib/components/Alert.svelte';
  import Loading from '$lib/components/Loading.svelte';

  let { children }: { children: Snippet } = $props();

  /** A failure resolving the session that is *not* "signed out". */
  let fatal = $state<string | undefined>(undefined);
  let signingOut = $state(false);

  /**
   * Pages reachable without a session.
   *
   * Just the two doors. `/signup` carries the whole account-creation flow now —
   * address, recovery codes, and authenticator enrollment in one visit — because
   * there is no email and so no second visit to come back from.
   *
   * `/invite/…` is deliberately *not* here: redeeming an invitation requires a
   * session whose address matches the one invited, so it sends the visitor to
   * sign in first. That check is what keeps a code that goes astray from being
   * a free seat.
   */
  const PUBLIC = ['/login', '/signup'];

  const isPublic = $derived(PUBLIC.some((p) => page.url.pathname === p));
  const onEnrollment = $derived(page.url.pathname === '/enroll');

  $effect(() => {
    void resolve();
  });

  async function resolve() {
    if (session.ready) return;
    try {
      await session.refresh();
    } catch (error) {
      fatal =
        error instanceof ApiError ? error.message : 'Something went wrong resolving your session.';
    }
  }

  /**
   * The routing guard, in one place.
   *
   * Written as an effect over `session.ready` and the current path rather than
   * as a check in each page: a page that forgets is a page that renders a
   * skeleton to a signed-out visitor and then flashes it away, and the
   * enrollment gate in particular has to hold everywhere at once — an account
   * with no confirmed authenticator can reach the API, so leaving one route
   * ungated would leave a usable console behind a half-finished login.
   */
  $effect(() => {
    // `signingOut` suppresses the guard for the moment between clearing the
    // session and arriving at `/login`. Without it the guard fires first, from
    // whatever org page the button was pressed on, and rewrites the destination
    // to `/login?next=/o/acme` — so someone who deliberately signed out is told
    // to "sign in to continue" and sent back where they left.
    if (!session.ready || fatal || signingOut) return;

    if (!session.signedIn) {
      if (!isPublic) {
        const next = page.url.pathname + page.url.search;
        void goto(`/login?next=${encodeURIComponent(next)}`, { replaceState: true });
      }
      return;
    }

    if (session.me?.mustEnrollTotp && !onEnrollment) {
      void goto('/enroll', { replaceState: true });
      return;
    }

    if (isPublic && !session.me?.mustEnrollTotp) {
      void goto('/', { replaceState: true });
    }
  });

  async function signOut() {
    signingOut = true;
    try {
      await api.logout();
    } finally {
      // Cleared even if the request failed. The server clears the cookie on
      // success and treats an unknown one as already gone, so the only way to
      // reach here with a live session is a network error — and leaving the
      // console looking signed in after someone pressed "sign out" is the worse
      // of the two wrong answers.
      session.clear();
      await goto('/login', { replaceState: true });
      signingOut = false;
    }
  }
</script>

<div class="flex min-h-full flex-col">
  <header class="border-b border-edge/60 bg-surface/40">
    <div class="mx-auto flex w-full max-w-6xl items-center gap-4 px-4 py-3">
      <a href="/" class="flex items-center gap-2 text-sm font-semibold tracking-tight">
        <span class="inline-block size-2.5 rounded-sm bg-accent"></span>
        dark-factory
      </a>

      {#if session.signedIn && !session.me?.mustEnrollTotp}
        <nav class="ml-2 hidden gap-1 text-sm sm:flex" aria-label="Organizations">
          {#each session.orgs as membership (membership.orgId)}
            <a
              href="/o/{membership.orgSlug}"
              class="rounded-md px-2.5 py-1 text-muted transition hover:bg-raised hover:text-ink"
              class:bg-raised={page.url.pathname.startsWith(`/o/${membership.orgSlug}`)}
              class:text-ink={page.url.pathname.startsWith(`/o/${membership.orgSlug}`)}
            >
              {membership.orgName}
            </a>
          {/each}
          <a
            href="/orgs/new"
            class="rounded-md px-2.5 py-1 text-faint transition hover:bg-raised hover:text-ink"
            title="Create an organization"
          >
            +
          </a>
        </nav>
      {/if}

      <div class="ml-auto flex items-center gap-3 text-sm">
        {#if session.me}
          <span class="hidden text-faint sm:inline">{session.me.user.email}</span>
          <button
            class="rounded-md border border-edge px-2.5 py-1 text-muted transition hover:bg-raised hover:text-ink disabled:opacity-50"
            onclick={signOut}
            disabled={signingOut}
          >
            Sign out
          </button>
        {/if}
      </div>
    </div>
  </header>

  <main class="mx-auto w-full max-w-6xl flex-1 px-4 py-6">
    {#if fatal}
      <Alert>
        {fatal}
        <button class="ml-2 underline" onclick={() => location.reload()}>Try again</button>
      </Alert>
    {:else if !session.ready}
      <Loading what="Checking your session" />
    {:else}
      {@render children()}
    {/if}
  </main>

  <footer class="border-t border-edge/40 px-4 py-4 text-center text-xs text-faint">
    <a class="hover:text-muted" href="/api/openapi.json">API reference</a>
  </footer>
</div>
