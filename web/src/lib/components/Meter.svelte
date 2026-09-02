<script lang="ts">
  import type { UsageStatus } from '$lib/types';
  import { plural } from '$lib/format';

  /**
   * The usage meter.
   *
   * Two numbers matter and they are not the same one. `billableUsed` is what
   * counts against the bucket; `totalCalls` is every call the org made. The
   * gap between them is `watch` — a continuous long poll that would bill an
   * idle agent tens of thousands of calls a month if it were charged flat — and
   * a meter that showed only the billable figure would leave a customer unable
   * to reconcile it against what their agents actually did.
   *
   * The bar can exceed 100%. It is clamped for drawing and *not* for the
   * caption, because an org that has run over needs to see by how much.
   */
  interface Props {
    usage: UsageStatus;
    compact?: boolean;
  }

  let { usage, compact = false }: Props = $props();

  const fraction = $derived(usage.includedOps > 0 ? usage.billableUsed / usage.includedOps : 0);
  const percent = $derived(Math.round(fraction * 100));
  const width = $derived(Math.min(100, Math.max(0, percent)));
  const tone = $derived(fraction >= 1 ? 'bg-bad' : usage.warning ? 'bg-warn' : 'bg-accent');
</script>

<div>
  <div class="flex items-baseline justify-between gap-3">
    <span class="text-sm text-muted">
      {usage.billableUsed.toLocaleString()} / {usage.includedOps.toLocaleString()} billable
    </span>
    <span class="text-xs text-faint">{percent}%</span>
  </div>

  <div
    class="mt-1.5 h-2 overflow-hidden rounded-full bg-raised"
    role="meter"
    aria-valuenow={usage.billableUsed}
    aria-valuemin={0}
    aria-valuemax={usage.includedOps}
    aria-label="Billable operations used this period"
  >
    <div class="h-full rounded-full transition-all {tone}" style="width: {width}%"></div>
  </div>

  {#if !compact}
    <p class="mt-2 text-xs text-faint">
      {plural(usage.totalCalls, 'tool call')} recorded this period, {plural(
        usage.billableUsed,
        'billable call'
      )} of them. Continuous polls such as <code class="df-mono">watch</code> are free, which is why the
      two numbers differ.
    </p>
  {/if}
</div>
