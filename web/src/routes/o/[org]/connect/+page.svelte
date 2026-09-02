<script lang="ts">
  import { api, ApiError } from '$lib/api';
  import { useOrg } from '$lib/org.svelte';
  import { CLIENTS } from '$lib/clients';
  import { relative } from '$lib/format';
  import type { MintedToken, ProtectedResourceMetadata, TokenSummary } from '$lib/types';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';
  import Card from '$lib/components/Card.svelte';
  import CopyField from '$lib/components/CopyField.svelte';
  import Empty from '$lib/components/Empty.svelte';
  import Field from '$lib/components/Field.svelte';
  import Loading from '$lib/components/Loading.svelte';

  /**
   * Connect an agent.
   *
   * The MCP endpoint comes from `/.well-known/oauth-protected-resource`, not
   * from a constant in this bundle. The console is a static artifact that has to
   * work against whatever origin serves it, and a hard-coded URL is how a
   * staging or self-hosted deployment ends up printing a connect command that
   * points at production. The same document supplies the grantable scopes, so
   * the checkboxes below cannot drift from what the authorization server will
   * actually accept.
   *
   * OAuth is the path shown first; the token path is offered second and
   * explains itself. A PAT is not a weaker credential — same table, same
   * audience, same scopes, same per-request introspection — but it is a secret
   * a human has to hold, and that is a real cost worth stating before someone
   * mints one out of habit.
   */

  const org = useOrg();

  let metadata = $state<ProtectedResourceMetadata | undefined>(undefined);
  let tokens = $state<TokenSummary[]>([]);
  let loading = $state(true);
  let error = $state<string | undefined>(undefined);

  let client = $state(CLIENTS[0]!.id);
  let usingToken = $state(false);

  let name = $state('');
  let scopes = $state<string[]>(['jobs:read', 'jobs:write', 'repos:read', 'messages']);
  let ttlDays = $state(90);
  let minting = $state(false);
  let mintError = $state<string | undefined>(undefined);
  let minted = $state<MintedToken | undefined>(undefined);
  let busy = $state<string | undefined>(undefined);

  $effect(() => {
    const slug = org.slug;
    if (!slug) return;

    loading = true;
    error = undefined;

    void (async () => {
      try {
        const [m, t] = await Promise.all([api.resourceMetadata(), api.tokens(slug)]);
        if (org.slug !== slug) return;
        metadata = m;
        tokens = t;
      } catch (e) {
        error = e instanceof ApiError ? e.message : 'Could not read the server configuration.';
      } finally {
        loading = false;
      }
    })();
  });

  const mcpUrl = $derived(metadata?.resource ?? '');
  const grantable = $derived(metadata?.scopes_supported ?? []);
  const recipe = $derived(CLIENTS.find((c) => c.id === client) ?? CLIENTS[0]!);

  const snippet = $derived(
    !mcpUrl ? '' : usingToken ? recipe.token(mcpUrl, minted?.token ?? '') : recipe.oauth(mcpUrl)
  );

  function toggleScope(scope: string, on: boolean) {
    scopes = on ? [...new Set([...scopes, scope])] : scopes.filter((s) => s !== scope);
  }

  async function mint(event: SubmitEvent) {
    event.preventDefault();
    minting = true;
    mintError = undefined;
    try {
      minted = await api.mintToken(org.slug, name.trim(), scopes, ttlDays);
      usingToken = true;
      name = '';
      tokens = await api.tokens(org.slug);
    } catch (e) {
      mintError = e instanceof ApiError ? e.message : 'Could not mint that token.';
    } finally {
      minting = false;
    }
  }

  async function revoke(id: string) {
    busy = id;
    error = undefined;
    try {
      await api.revokeToken(org.slug, id);
      tokens = await api.tokens(org.slug);
      if (minted?.id === id) minted = undefined;
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Could not revoke that token.';
    } finally {
      busy = undefined;
    }
  }
</script>

<div class="space-y-5">
  <div>
    <h1 class="text-lg font-semibold">Connect an agent</h1>
    <p class="mt-0.5 text-sm text-faint">
      Any client that speaks MCP over Streamable HTTP works. Nothing here depends on a particular
      agent's plugins, hooks, or skills.
    </p>
  </div>

  {#if error}<Alert>{error}</Alert>{/if}

  {#if loading && !metadata}
    <Loading what="Reading the server configuration" />
  {:else if !mcpUrl}
    <Alert>
      The server did not report an MCP endpoint. Its
      <code class="df-mono">/.well-known/oauth-protected-resource</code> document is what this page reads.
    </Alert>
  {:else}
    <Card title="Endpoint">
      <CopyField value={mcpUrl} />
      <p class="mt-2 text-xs text-faint">
        Tokens are audienced for exactly this URI. A token minted here is refused anywhere else, and
        its organization —
        <code class="df-mono">{org.slug}</code> — is fixed when it is issued and cannot be changed.
      </p>
    </Card>

    <Card title="Your client">
      {#snippet actions()}
        <div class="flex rounded-md border border-edge text-xs">
          <button
            class="rounded-l-md px-2.5 py-1 transition"
            class:bg-raised={!usingToken}
            class:text-ink={!usingToken}
            class:text-muted={usingToken}
            onclick={() => (usingToken = false)}
          >
            OAuth
          </button>
          <button
            class="rounded-r-md px-2.5 py-1 transition"
            class:bg-raised={usingToken}
            class:text-ink={usingToken}
            class:text-muted={!usingToken}
            onclick={() => (usingToken = true)}
          >
            Access token
          </button>
        </div>
      {/snippet}

      <div class="flex flex-wrap gap-1.5">
        {#each CLIENTS as option (option.id)}
          <button
            class="rounded-md border px-2.5 py-1 text-xs transition"
            class:border-accent={client === option.id}
            class:text-ink={client === option.id}
            class:border-edge={client !== option.id}
            class:text-muted={client !== option.id}
            onclick={() => (client = option.id)}
          >
            {option.name}
          </button>
        {/each}
      </div>

      <div class="mt-4">
        {#if recipe.location}
          <p class="mb-1.5 text-xs text-faint">
            Put this in <code class="df-mono">{recipe.location}</code>:
          </p>
        {/if}
        <CopyField value={snippet} />
      </div>

      {#if recipe.note}
        <p class="mt-2 text-xs text-faint">{recipe.note}</p>
      {/if}

      {#if usingToken && !minted}
        <p class="mt-2 text-xs text-warn">
          Replace the placeholder with a token — mint one below. The token is shown once.
        </p>
      {:else if !usingToken}
        <p class="mt-2 text-xs text-faint">
          The client registers itself and sends you here to approve it. Nothing is pasted, and you
          can revoke it from this page afterwards.
        </p>
      {/if}
    </Card>

    {#if minted}
      <Card title="Your new token">
        <Alert tone="warn">
          This is shown once. Only a SHA-256 hash of it is stored, so nobody — including us — can
          show it to you again.
        </Alert>
        <div class="mt-3">
          <CopyField value={minted.token} />
        </div>
        <p class="mt-2 text-xs text-faint">
          Scopes: <span class="df-mono">{minted.scopes.join(' ')}</span>
        </p>
      </Card>
    {/if}

    <Card
      title="Personal access tokens"
      description="The compatibility path, for clients whose OAuth support is partial."
    >
      <form class="space-y-4" onsubmit={mint}>
        <div class="grid gap-4 sm:grid-cols-3">
          <div class="sm:col-span-2">
            <Field
              label="What is it for"
              hint="Shown in the list below. It is all you will have to go on when deciding what to revoke."
            >
              <input class="df-input" placeholder="laptop, CI runner" required bind:value={name} />
            </Field>
          </div>
          <Field label="Expires in" hint="Days. 1–365.">
            <input class="df-input" type="number" min="1" max="365" bind:value={ttlDays} />
          </Field>
        </div>

        <fieldset>
          <legend class="df-label">Scopes</legend>
          <div class="flex flex-wrap gap-x-5 gap-y-2">
            {#each grantable as scope (scope)}
              <label class="flex items-center gap-2 text-sm text-muted">
                <input
                  type="checkbox"
                  checked={scopes.includes(scope)}
                  onchange={(e) => toggleScope(scope, e.currentTarget.checked)}
                />
                <span class="df-mono">{scope}</span>
              </label>
            {/each}
          </div>
          <p class="mt-2 text-xs text-faint">
            You can only grant scopes you hold. <code class="df-mono">org:admin</code> needs an owner
            or admin of this organization.
          </p>
        </fieldset>

        {#if mintError}<Alert>{mintError}</Alert>{/if}

        <Button type="submit" pending={minting}>Mint a token</Button>
      </form>

      <div class="mt-6 border-t border-edge/50 pt-4">
        <h3 class="text-xs font-medium tracking-wide text-muted uppercase">
          Live credentials in {org.slug}
        </h3>
        <p class="mt-1 text-xs text-faint">
          Yours only, OAuth grants included. Revoking one stops that agent on its next call, not at
          some later expiry.
        </p>

        {#if tokens.length === 0}
          <div class="mt-3"><Empty title="No live tokens." /></div>
        {:else}
          <ul class="mt-3 divide-y divide-edge/40">
            {#each tokens as token (token.id)}
              <li class="flex flex-wrap items-center gap-3 py-2.5 text-sm">
                <div class="min-w-0 flex-1">
                  <span class="text-ink">{token.name ?? token.clientId ?? 'unnamed'}</span>
                  <span class="ml-2 rounded-full border border-edge px-2 py-0.5 text-xs text-faint">
                    {token.kind}
                  </span>
                  <p class="df-mono mt-0.5 text-xs text-faint">
                    {token.scopes.join(' ') || 'no scopes'}
                  </p>
                  <p class="text-xs text-faint">
                    last used {relative(token.lastUsedAt)} · expires {relative(token.expiresAt)}
                  </p>
                </div>
                <Button tone="danger" pending={busy === token.id} onclick={() => revoke(token.id)}>
                  Revoke
                </Button>
              </li>
            {/each}
          </ul>
        {/if}
      </div>
    </Card>
  {/if}
</div>
