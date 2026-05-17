<script lang="ts">
  import { api, type Group } from '$lib/api';
  import { reloadTree } from '$lib/tree-state.svelte';
  import Modal from './Modal.svelte';

  let { onClose }: { onClose: () => void } = $props();

  let groups = $state<Group[]>([]);
  let parentUuid = $state<string | null>(null);
  let displayName = $state('');
  let busy = $state(false);
  let err = $state<string | null>(null);
  let lastCreated = $state<string | null>(null);

  $effect(() => {
    void api.listGroups().then((g) => (groups = g));
  });

  function breadcrumb(g: Group): string {
    const byUuid = new Map<string, Group>(groups.map((x) => [x.uuid, x]));
    const parts: string[] = [];
    let cur: Group | undefined = g;
    while (cur) {
      parts.unshift(cur.display_name);
      cur = cur.parent_uuid != null ? byUuid.get(cur.parent_uuid) : undefined;
    }
    return parts.join(' > ');
  }

  // Bump the display name's " (N)" suffix - so back-to-back "create + add
  // another" clicks don't all submit the same string. Duplicates are
  // technically allowed but operators almost always want to distinguish.
  function bumpName(name: string): string {
    const m = name.match(/^(.*?) \((\d+)\)$/);
    if (m) return `${m[1]} (${Number(m[2]) + 1})`;
    return `${name} (2)`;
  }

  async function doCreate(): Promise<boolean> {
    err = null;
    if (!displayName.trim()) {
      err = 'Name is required.';
      return false;
    }
    busy = true;
    try {
      await api.createGroup(displayName.trim(), parentUuid);
      // Refresh the in-modal group list (so the freshly added group can be
      // picked as a parent in the next iteration) and the sidebar tree.
      groups = await api.listGroups();
      reloadTree();
      lastCreated = displayName.trim();
      return true;
    } catch (e) {
      err = e instanceof Error ? e.message : String(e);
      return false;
    } finally {
      busy = false;
    }
  }

  async function createAndClose() {
    if (await doCreate()) onClose();
  }

  async function createAndAddAnother() {
    if (await doCreate()) {
      displayName = bumpName(lastCreated ?? displayName);
    }
  }

  const inputStyle =
    'background: var(--bg); border-color: var(--border); color: var(--fg)';
</script>

<Modal title="Create group" {onClose}>
  <form
    onsubmit={(e) => {
      e.preventDefault();
      void createAndClose();
    }}
    class="space-y-3 text-xs"
  >
    <label class="block">
      <span style="color: var(--muted)">Parent</span>
      <select
        bind:value={parentUuid}
        class="w-full mt-0.5 px-2 py-1 rounded border"
        style={inputStyle}
      >
        <option value={null}>- root (no parent) -</option>
        {#each groups as g (g.uuid)}
          <option value={g.uuid}>{breadcrumb(g)}</option>
        {/each}
      </select>
    </label>

    <label class="block">
      <span style="color: var(--muted)">Name</span>
      <!-- svelte-ignore a11y_autofocus -->
      <input
        bind:value={displayName}
        type="text"
        required
        autofocus
        placeholder="e.g. London datacentre"
        class="w-full mt-0.5 px-2 py-1 rounded border"
        style={inputStyle}
      />
    </label>

    {#if err}
      <p style="color: var(--latency-bad)">{err}</p>
    {/if}

    <div class="flex justify-end gap-2 pt-1">
      <button
        type="button"
        onclick={onClose}
        class="px-3 py-1 rounded border"
        style="border-color: var(--border)"
      >
        Cancel
      </button>
      <button
        type="button"
        disabled={busy}
        onclick={() => void createAndAddAnother()}
        class="px-3 py-1 rounded border"
        style="border-color: var(--border); opacity: {busy ? 0.6 : 1}"
      >
        Create + add another
      </button>
      <button
        type="submit"
        disabled={busy}
        class="px-3 py-1 rounded font-medium"
        style="background: var(--btn-bg); color: var(--btn-text); opacity: {busy ? 0.6 : 1}"
      >
        {busy ? 'Creating…' : 'Create'}
      </button>
    </div>
  </form>
</Modal>
