<script lang="ts">
  import { api, ApiError } from '$lib/api';
  import { useOrg } from '$lib/org.svelte';
  import { day } from '$lib/format';
  import type { UsageStatus } from '$lib/types';
  import Alert from '$lib/components/Alert.svelte';
  import Card from '$lib/components/Card.svelte';
  import Loading from '$lib/components/Loading.svelte';
  import Meter from '$lib/components/Meter.svelte';

  /**
   * The usage meter.
   *
   * Free to read, and readable by an org that has run out — the same
   * `df_billing::Meter::report` the `usage` MCP tool calls, so the figure here
   * and the figure an agent sees cannot disagree. Nothing on this page charges
   * anything: billing a customer for looking at what they have been billed
   * costs more in trust than it could ever earn.
   */

  const org = useOrg();

  let usage = $state<UsageStatus | undefined>(undefined);
  let loading = $state(true);
  let error = $state<string | undefined>(undefined);

  $effect(() => {
    const slug = org.slug;
    if (!slug) return;

    loading = true;
    error = undefined;

    void (async () => {
      try {
        const status = await api.usage(slug);
        if (org.slug !== slug) return;
        usage = status;
      } catch (e) {
        error = e instanceof ApiError ? e.message : 'Could not read the meter.';
      } finally {
        loading = false;
      }
    })();
  });

  const free = $derived(usage ? usage.totalCalls - usage.billableUsed : 0);
</script>

<div class="space-y-5">
  <div>
    <h1 class="text-lg font-semibold">Usage</h1>
    <p class="mt-0.5 text-sm text-faint">The billable unit is one MCP tool call.</p>
  </div>

  {#if error}
    <Alert>{error}</Alert>
  {:else if !usage}
    <Loading what="Reading the meter" />
  {:else}
    {#if usage.remaining === 0 && usage.hardStop}
      <Alert tone={usage.enforced ? 'error' : 'warn'}>
        {#if usage.enforced}
          This organization is out of its included operations and billable tools are being refused.
          Reads keep working — the queue stays visible, and nothing already queued is lost.
        {:else}
          This organization is past its included operations. Nothing is being refused yet.
        {/if}
      </Alert>
    {:else if usage.warning}
      <Alert tone="warn">Most of this period's included operations have been used.</Alert>
    {/if}

    <Card title="This period" description="Since {day(usage.periodStart)}.">
      <Meter {usage} />
    </Card>

    <div class="grid gap-3 sm:grid-cols-3">
      <div class="df-card px-4 py-3">
        <div class="text-2xl font-semibold text-ink">{usage.totalCalls.toLocaleString()}</div>
        <div class="mt-0.5 text-xs text-faint">Tool calls recorded</div>
      </div>
      <div class="df-card px-4 py-3">
        <div class="text-2xl font-semibold text-ink">{usage.billableUsed.toLocaleString()}</div>
        <div class="mt-0.5 text-xs text-faint">Billable</div>
      </div>
      <div class="df-card px-4 py-3">
        <div class="text-2xl font-semibold text-muted">{free.toLocaleString()}</div>
        <div class="mt-0.5 text-xs text-faint">Free</div>
      </div>
    </div>

    <Card title="Why the two numbers differ">
      <p class="text-sm text-muted">
        Every call is recorded, but not every call is billed. <code class="df-mono">watch</code> is a
        continuous long poll — an agent waiting for work holds one open more or less permanently — and
        charging it flat would bill an idle agent tens of thousands of calls a month for doing nothing.
        Reads that answer "what is going on", including this page, are free too.
      </p>
      <p class="mt-3 text-sm text-muted">
        The full history is kept regardless of how a call was classified, so a change to what counts
        can be applied without losing what happened.
      </p>
      <dl class="mt-4 grid grid-cols-2 gap-y-2 text-sm sm:grid-cols-4">
        <div>
          <dt class="df-label">Plan</dt>
          <dd class="text-muted">{usage.plan}</dd>
        </div>
        <div>
          <dt class="df-label">Included</dt>
          <dd class="text-muted">{usage.includedOps.toLocaleString()}</dd>
        </div>
        <div>
          <dt class="df-label">Remaining</dt>
          <dd class="text-muted">{usage.remaining.toLocaleString()}</dd>
        </div>
        <div>
          <dt class="df-label">Over the bucket</dt>
          <dd class="text-muted">
            {usage.hardStop ? 'stops billable work' : 'metered as overage'}
          </dd>
        </div>
      </dl>
    </Card>
  {/if}
</div>
