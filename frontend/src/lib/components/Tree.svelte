<script lang="ts">
  import type { Group, Host } from '$lib/api';
  import { goto } from '$app/navigation';
  import { base } from '$app/paths';
  import { page } from '$app/state';
  import { treeState, toggle as toggleNode } from '$lib/tree-state.svelte';

  let {
    groups,
    hosts,
    search = '',
    onEditGroup,
    onEditHost
  }: {
    groups: Group[];
    hosts: Host[];
    search?: string;
    /** Pencil-icon callback hooks. Layout owns the edit modals so they
     *  render at the page level and have access to the full host/group
     *  snapshots (parent selectors, group chip pickers). */
    onEditGroup?: (g: Group) => void;
    onEditHost?: (h: Host) => void;
  } = $props();

  type Node = {
    group: Group;
    children: Node[];
    hosts: Host[];
  };

  const tree = $derived.by(() => {
    const byUuid = new Map<string, Node>();
    for (const g of groups) byUuid.set(g.uuid, { group: g, children: [], hosts: [] });
    const roots: Node[] = [];
    for (const g of groups) {
      const node = byUuid.get(g.uuid)!;
      if (g.parent_uuid == null) roots.push(node);
      else byUuid.get(g.parent_uuid)?.children.push(node);
    }
    for (const h of hosts) {
      for (const gu of h.group_uuids) {
        byUuid.get(gu)?.hosts.push(h);
      }
    }
    return roots;
  });

  const rootHosts = $derived(hosts.filter((h) => h.group_uuids.length === 0));

  const expanded = $derived(treeState.expanded);

  function toggle(uuid: string) {
    toggleNode(uuid);
  }

  function isActiveHost(uuid: string): boolean {
    return page.url.pathname === `/hosts/${uuid}`;
  }
  function isActiveGroup(uuid: string): boolean {
    return page.url.pathname === `/groups/${uuid}`;
  }

  function pickHost(uuid: string) {
    void goto(`${base}/hosts/${uuid}`);
  }
  function pickGroup(uuid: string) {
    void goto(`${base}/groups/${uuid}`);
  }

  function totalHostsIn(node: Node): number {
    let n = isSearching ? node.hosts.filter(showHost).length : node.hosts.length;
    for (const c of node.children) n += totalHostsIn(c);
    return n;
  }

  // ─── Search ────────────────────────────────────────────────────────────
  const terms = $derived(
    search.trim().toLowerCase().split(/\s+/).filter((t) => t.length > 0)
  );
  const isSearching = $derived(terms.length > 0);

  function hostMatches(h: Host): boolean {
    if (!isSearching) return true;
    const haystack = `${h.display_name} ${h.probe_type}`.toLowerCase();
    return terms.every((t) => haystack.includes(t));
  }

  const matchedGroupUuids = $derived.by<Set<string>>(() => {
    const result = new Set<string>();
    if (!isSearching) return result;
    const parent = new Map<string, string | null>();
    for (const g of groups) parent.set(g.uuid, g.parent_uuid);
    for (const h of hosts) {
      if (!hostMatches(h)) continue;
      for (const gu of h.group_uuids) {
        let g: string | null = gu;
        while (g != null && !result.has(g)) {
          result.add(g);
          g = parent.get(g) ?? null;
        }
      }
    }
    return result;
  });

  function showGroup(node: Node): boolean {
    return !isSearching || matchedGroupUuids.has(node.group.uuid);
  }
  function showHost(h: Host): boolean {
    return hostMatches(h);
  }
  function nodeOpen(g: Group): boolean {
    if (isSearching) return matchedGroupUuids.has(g.uuid);
    return expanded.has(g.uuid);
  }
</script>

<nav class="text-xs leading-tight select-none">
  {#each rootHosts as host (host.uuid)}
    {#if showHost(host)}
      {@render hostRow(host, 0.5)}
    {/if}
  {/each}
  {#each tree as node (node.group.uuid)}
    {#if showGroup(node)}
      {@render renderNode(node, 0)}
    {/if}
  {/each}
</nav>

<!-- Monochromatic pencil glyph. Inlined to avoid pulling in an icon set. -->
{#snippet pencilIcon()}
  <svg
    viewBox="0 0 16 16"
    width="11"
    height="11"
    fill="currentColor"
    aria-hidden="true"
  >
    <path d="M11.36 1.64a1.5 1.5 0 0 1 2.12 0l.88.88a1.5 1.5 0 0 1 0 2.12L6.8 12.2 3 13l.8-3.8L11.36 1.64Zm.7.71L4.74 9.66 4.3 11.7l2.04-.44 7.32-7.32-1.6-1.6Z" />
  </svg>
{/snippet}

{#snippet editPencil(onClick: () => void, label: string)}
  <!-- Hidden until the cursor enters the parent row (which carries the
       `group` Tailwind marker). On hover, snaps to 60 % muted; pencil's
       own hover bumps to 100 % so the user gets a "ready to click" cue. -->
  <button
    type="button"
    onclick={(e) => {
      e.stopPropagation();
      onClick();
    }}
    class="opacity-0 group-hover:opacity-60 hover:!opacity-100 focus:!opacity-100"
    style="color: var(--muted); padding: 2px"
    aria-label={label}
    title={label}
  >
    {@render pencilIcon()}
  </button>
{/snippet}

{#snippet hostRow(host: Host, paddingRem: number)}
  <div
    class="group w-full flex items-center gap-2 px-2 py-0.5 hover:bg-white/5"
    style="padding-left: {paddingRem}rem; {isActiveHost(host.uuid)
      ? 'background: rgba(78, 161, 255, 0.12);'
      : ''}"
  >
    <span style="color: var(--muted)">•</span>
    <button
      type="button"
      onclick={() => pickHost(host.uuid)}
      class="flex-1 text-left"
    >
      {host.display_name}
    </button>
    {#if onEditHost}
      {@render editPencil(() => onEditHost?.(host), `Edit ${host.display_name}`)}
    {/if}
    <span class="text-[10px]" style="color: var(--muted)">{host.probe_type}</span>
  </div>
{/snippet}

{#snippet renderNode(node: Node, depth: number)}
  {@const isOpen = nodeOpen(node.group)}
  {@const totalHosts = totalHostsIn(node)}
  {@const hasChildren = node.children.length > 0 || node.hosts.length > 0}
  <div>
    <div
      class="group w-full flex items-center gap-1 px-1.5 py-0.5 hover:bg-white/5"
      style="padding-left: {0.375 + depth * 0.75}rem; {isActiveGroup(node.group.uuid)
        ? 'background: rgba(78, 161, 255, 0.12);'
        : ''}"
    >
      <button
        type="button"
        onclick={() => toggle(node.group.uuid)}
        class="inline-flex items-center justify-center"
        style="width: 14px; height: 14px; border: 1px solid var(--border); color: var(--muted); font-size: 10px; line-height: 1; border-radius: 2px; {hasChildren
          ? 'cursor: pointer'
          : 'visibility: hidden'}"
        aria-label={isOpen ? 'Collapse' : 'Expand'}
        tabindex={hasChildren ? 0 : -1}
      >
        {isOpen ? '−' : '+'}
      </button>
      <button
        type="button"
        onclick={() => pickGroup(node.group.uuid)}
        class="flex-1 text-left font-medium"
        style="color: var(--fg)"
      >
        {node.group.display_name}
      </button>
      {#if onEditGroup}
        {@render editPencil(() => onEditGroup?.(node.group), `Edit ${node.group.display_name}`)}
      {/if}
      <span class="text-[10px]" style="color: var(--muted)">{totalHosts}</span>
    </div>
    {#if isOpen}
      {#each node.hosts as host (host.uuid)}
        {#if showHost(host)}
          {@render hostRow(host, 1.5 + depth * 0.75)}
        {/if}
      {/each}
      {#each node.children as child (child.group.uuid)}
        {#if showGroup(child)}
          {@render renderNode(child, depth + 1)}
        {/if}
      {/each}
    {/if}
  </div>
{/snippet}
