<script lang="ts">
  import { api, ApiError } from '$lib/api';
  import { useOrg } from '$lib/org.svelte';
  import { absolute, relative } from '$lib/format';
  import type { AuditEvent } from '$lib/types';
  import Alert from '$lib/components/Alert.svelte';
  import Empty from '$lib/components/Empty.svelte';
  import Loading from '$lib/components/Loading.svelte';

  /**
   * The security log.
   *
   * Admin-only, unlike every other read in this console. Membership changes,
   * token issuance, and failed logins are exactly the trail an attacker holding
   * a low-privilege session would read before choosing whom to target — so the
   * server refuses a member, and this page is only in the sidebar for an admin.
   * The `403` is still the server's to give; hiding the link is a courtesy.
   */

  const org = useOrg();

  let events = $state<AuditEvent[]>([]);
  let loading = $state(true);
  let error = $state<string | undefined>(undefined);

  $effect(() => {
    const slug = org.slug;
    if (!slug) return;

    loading = true;
    error = undefined;

    void (async () => {
      try {
        const found = await api.audit(slug, 200);
        if (org.slug !== slug) return;
        events = found;
      } catch (e) {
        error = e instanceof ApiError ? e.message : 'Could not read the audit log.';
      } finally {
        loading = false;
      }
    })();
  });

  function detail(event: AuditEvent): string {
    const keys = Object.keys(event.detail ?? {});
    return keys.length > 0 ? JSON.stringify(event.detail) : '';
  }
</script>

<div class="space-y-5">
  <div>
    <h1 class="text-lg font-semibold">Audit log</h1>
    <p class="mt-0.5 text-sm text-faint">
      Who did what in {org.title}. Admins only.
    </p>
  </div>

  {#if error}
    <Alert>{error}</Alert>
  {:else if loading && events.length === 0}
    <Loading what="Reading the audit log" />
  {:else if events.length === 0}
    <Empty title="Nothing recorded yet." />
  {:else}
    <div class="df-card overflow-x-auto">
      <table class="w-full text-sm">
        <thead class="border-b border-edge/60 text-left text-xs text-faint">
          <tr>
            <th class="px-4 py-2 font-medium">When</th>
            <th class="px-4 py-2 font-medium">Action</th>
            <th class="px-4 py-2 font-medium">Actor</th>
            <th class="px-4 py-2 font-medium">Target</th>
            <th class="px-4 py-2 font-medium">Detail</th>
          </tr>
        </thead>
        <tbody class="divide-y divide-edge/40">
          {#each events as event (event.id)}
            <tr class="hover:bg-raised/40">
              <td class="px-4 py-2 whitespace-nowrap text-faint" title={absolute(event.createdAt)}>
                {relative(event.createdAt)}
              </td>
              <td class="df-mono px-4 py-2 whitespace-nowrap text-ink">{event.action}</td>
              <td class="px-4 py-2 text-muted">{event.actorLabel ?? event.actorUserId ?? '—'}</td>
              <td class="px-4 py-2 text-muted">
                {event.targetType ? `${event.targetType} ${event.targetId ?? ''}` : '—'}
              </td>
              <td class="df-mono max-w-64 truncate px-4 py-2 text-faint" title={detail(event)}>
                {detail(event)}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</div>
