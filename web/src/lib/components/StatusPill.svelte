<script lang="ts">
  import type { JobStatus } from '$lib/types';

  /**
   * Colour *and* the word, never colour alone. A queue read by someone with a
   * colour vision deficiency has to say "failed", not merely be red.
   */
  interface Props {
    status: JobStatus;
  }

  let { status }: Props = $props();

  const tones: Record<JobStatus, string> = {
    pending: 'border-faint/40 bg-faint/10 text-muted',
    'in-progress': 'border-busy/50 bg-busy/10 text-busy',
    completed: 'border-ok/50 bg-ok/10 text-ok',
    failed: 'border-bad/50 bg-bad/10 text-bad'
  };
</script>

<span
  class="inline-flex items-center rounded-full border px-2 py-0.5 text-xs font-medium whitespace-nowrap {tones[
    status
  ]}"
>
  {status}
</span>
