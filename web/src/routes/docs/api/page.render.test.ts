// @vitest-environment jsdom
import { mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { fixtureDoc } from '$lib/openapi.fixtures';
import Page from './+page.svelte';

describe('/docs/api page', () => {
  let container: HTMLElement;

  beforeEach(() => {
    container = document.createElement('div');
    document.body.appendChild(container);
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue({
        ok: true,
        json: () => Promise.resolve(fixtureDoc)
      })
    );
  });

  afterEach(() => {
    container.remove();
    vi.unstubAllGlobals();
  });

  it('renders one element per fixture endpoint operationId', async () => {
    const instance = mount(Page, { target: container });
    await vi.waitFor(() => {
      expect(container.querySelector('#listRepos')).not.toBeNull();
    });
    expect(container.querySelector('#createRepo')).not.toBeNull();
    expect(container.querySelector('#ingestWebhook')).not.toBeNull();
    unmount(instance);
  });

  it('surfaces the auth level in the rendered text for each endpoint', async () => {
    const instance = mount(Page, { target: container });
    await vi.waitFor(() => {
      expect(container.querySelector('#createRepo')).not.toBeNull();
    });
    expect(container.querySelector('#createRepo')?.textContent).toContain('org admin');
    expect(container.querySelector('#ingestWebhook')?.textContent).toContain('public');
    unmount(instance);
  });

  it('shows a retry-capable alert when the fetch fails', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new Error('network down')));
    const instance = mount(Page, { target: container });
    await vi.waitFor(() => {
      expect(container.querySelector('[role="alert"]')).not.toBeNull();
    });
    expect(container.querySelector('button')).not.toBeNull();
    unmount(instance);
  });
});
