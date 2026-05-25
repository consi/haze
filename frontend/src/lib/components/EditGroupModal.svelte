<script lang="ts">
  import { api, ApiError, type Group } from '$lib/api';
  import { reloadTree } from '$lib/tree-state.svelte';
  import Modal from './Modal.svelte';

  let {
    group,
    allGroups,
    onClose
  }: {
    group: Group;
    /** Snapshot of every group, needed to populate the parent selector
     *  and to filter out cycles. */
    allGroups: Group[];
    onClose: () => void;
  } = $props();

  // svelte-ignore state_referenced_locally
  // Editor state is seeded from the snapshot we were mounted with;
  // remounting with a different `group` is what swaps the form.
  let displayName = $state(group.display_name);
  // svelte-ignore state_referenced_locally
  let parentUuid = $state<string | null>(group.parent_uuid);
  let busy = $state(false);
  let err = $state<string | null>(null);

  // Replicated groups are owned by an upstream peer: only the local
  // display name can be changed (it's a label on this instance), the
  // parent placement is fixed because the replication worker enforces
  // it on every reconcile pass.
  // svelte-ignore state_referenced_locally
  const isReplicated = group.replication_peer_id != null;

  // Forbidden parents: the group itself plus everything in its subtree.
  // Moving X under one of X's descendants is a cycle and the backend
  // rejects it with 422; hide it from the dropdown so the user doesn't
  // run into the error.
  const forbidden = $derived.by<Set<string>>(() => {
    const childrenOf = new Map<string, string[]>();
    for (const g of allGroups) {
      if (g.parent_uuid != null) {
        const arr = childrenOf.get(g.parent_uuid) ?? [];
        arr.push(g.uuid);
        childrenOf.set(g.parent_uuid, arr);
      }
    }
    const out = new Set<string>([group.uuid]);
    const stack = [group.uuid];
    while (stack.length) {
      const u = stack.pop()!;
      for (const c of childrenOf.get(u) ?? []) {
        if (!out.has(c)) {
          out.add(c);
          stack.push(c);
        }
      }
    }
    return out;
  });

  function breadcrumb(g: Group): string {
    const byUuid = new Map<string, Group>(allGroups.map((x) => [x.uuid, x]));
    const parts: string[] = [];
    let cur: Group | undefined = g;
    while (cur) {
      parts.unshift(cur.display_name);
      cur = cur.parent_uuid != null ? byUuid.get(cur.parent_uuid) : undefined;
    }
    return parts.join(' > ');
  }

  async function save() {
    err = null;
    const name = displayName.trim();
    if (!name) {
      err = 'Name is required.';
      return;
    }
    busy = true;
    // Build a tri-state-aware PATCH: only include `parent_uuid` if it
    // actually changed, so we never send null (which means "move to
    // root") when the user just renamed and left the parent alone.
    const patch: { display_name?: string; parent_uuid?: string | null } = {};
    if (name !== group.display_name) patch.display_name = name;
    if (parentUuid !== group.parent_uuid) patch.parent_uuid = parentUuid;
    try {
      if (Object.keys(patch).length > 0) {
        await api.updateGroup(group.uuid, patch);
        reloadTree();
      }
      onClose();
    } catch (e) {
      err = e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function doDelete() {
    const yes = window.confirm(
      `Delete group "${group.display_name}"?\n\n` +
        'This removes the group and every group nested under it. Hosts ' +
        'attached only to this subtree become root-level (their data is ' +
        'not deleted).'
    );
    if (!yes) return;
    err = null;
    busy = true;
    try {
      await api.deleteGroup(group.uuid);
      reloadTree();
      onClose();
    } catch (e) {
      err = e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  const inputStyle =
    'background: var(--bg); border-color: var(--border); color: var(--fg)';
</script>

<Modal title="Edit group" {onClose}>
  <form
    onsubmit={(e) => {
      e.preventDefault();
      void save();
    }}
    class="space-y-3 text-xs"
  >
    <label class="block">
      <span style="color: var(--muted)">Name</span>
      <input
        bind:value={displayName}
        type="text"
        required
        class="w-full mt-0.5 px-2 py-1 rounded border"
        style={inputStyle}
      />
    </label>

    {#if isReplicated}
      <div
        class="rounded border p-2 text-[11px]"
        style="border-color: var(--border); background: rgba(78, 161, 255, 0.06)"
      >
        <strong>Managed by replication.</strong> Placement in the tree is
        controlled by the replication rule on the source. You can rename
        this group locally; the rename is preserved across reconciles
        unless the source itself renames it, in which case the local
        rename is overwritten.
      </div>
    {:else}
      <label class="block">
        <span style="color: var(--muted)">Parent</span>
        <select
          bind:value={parentUuid}
          class="w-full mt-0.5 px-2 py-1 rounded border"
          style={inputStyle}
        >
          <option value={null}>- root (no parent) -</option>
          {#each allGroups as g (g.uuid)}
            {#if !forbidden.has(g.uuid)}
              <option value={g.uuid}>{breadcrumb(g)}</option>
            {/if}
          {/each}
        </select>
        <span class="block text-[10px] mt-1" style="color: var(--muted)">
          Cycles are filtered out: you can't move a group under itself or one of its descendants.
          Moving rewrites the path of every group and host underneath, in a single transaction.
        </span>
      </label>
    {/if}

    {#if err}
      <p style="color: var(--latency-bad)">{err}</p>
    {/if}

    <div class="flex items-center justify-between gap-2 pt-1">
      <button
        type="button"
        onclick={doDelete}
        disabled={busy}
        class="px-3 py-1 rounded border"
        style="border-color: var(--latency-bad); color: var(--latency-bad); opacity: {busy ? 0.6 : 1}"
      >
        Delete
      </button>
      <div class="flex gap-2">
        <button
          type="button"
          onclick={onClose}
          class="px-3 py-1 rounded border"
          style="border-color: var(--border)"
        >
          Cancel
        </button>
        <button
          type="submit"
          disabled={busy}
          class="px-3 py-1 rounded font-medium"
          style="background: var(--btn-bg); color: var(--btn-text); opacity: {busy ? 0.6 : 1}"
        >
          {busy ? 'Saving…' : 'Save'}
        </button>
      </div>
    </div>
  </form>
</Modal>
