<script lang="ts">
  import type { Group, Host } from '$lib/api';
  import { goto } from '$app/navigation';
  import { base } from '$app/paths';
  import { page } from '$app/state';
  import { tick, untrack } from 'svelte';
  import {
    expand as expandNodes,
    treeState,
    toggle as toggleNode
  } from '$lib/tree-state.svelte';
  import { groupHierarchy } from '$lib/group-order';

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

  const hierarchy = $derived(groupHierarchy(groups));

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
    const sortNodes = (nodes: Node[]) => {
      nodes.sort((a, b) => hierarchy.compare(a.group, b.group));
      for (const node of nodes) sortNodes(node.children);
    };
    sortNodes(roots);
    return roots;
  });

  const rootHosts = $derived(hosts.filter((h) => h.group_uuids.length === 0));

  const expanded = $derived(treeState.expanded);

  // Alert host links include the first group in which the host appears as a
  // `group` query parameter. Expand that group and every ancestor so the
  // selected host is visible in the sidebar, even when another search filter
  // is currently active. Keep the expansion in the normal persisted state.
  const revealGroupUuid = $derived(page.url.searchParams.get('group'));
  const revealHostUuid = $derived(
    page.url.pathname.startsWith(`${base}/hosts/`) ? page.params.uuid ?? null : null
  );
  const revealPathUuids = $derived.by<Set<string>>(() => {
    const result = new Set<string>();
    if (!revealGroupUuid || !revealHostUuid) return result;
    const host = hosts.find((candidate) => candidate.uuid === revealHostUuid);
    if (!host?.group_uuids.includes(revealGroupUuid)) return result;
    const byUuid = new Map(groups.map((group) => [group.uuid, group]));
    let current = byUuid.get(revealGroupUuid);
    while (current && !result.has(current.uuid)) {
      result.add(current.uuid);
      current = current.parent_uuid ? byUuid.get(current.parent_uuid) : undefined;
    }
    return result;
  });

  let treeNav: HTMLElement | undefined = $state();
  let lastReveal = '';
  $effect(() => {
    const groupUuid = revealGroupUuid;
    const hostUuid = revealHostUuid;
    const path = [...revealPathUuids];
    if (!groupUuid || !hostUuid || path.length === 0) return;
    const key = `${groupUuid}:${hostUuid}`;
    if (key === lastReveal) return;
    lastReveal = key;
    untrack(() => expandNodes(path));
    void tick().then(() => {
      const row = Array.from(
        treeNav?.querySelectorAll<HTMLElement>('[data-tree-host-uuid]') ?? []
      ).find(
        (candidate) =>
          candidate.dataset.treeHostUuid === hostUuid &&
          candidate.dataset.treeGroupUuid === groupUuid
      );
      row?.scrollIntoView({ block: 'nearest' });
    });
  });

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

  function groupMatches(g: Group): boolean {
    if (!isSearching) return true;
    const haystack = hierarchy.breadcrumb(g).toLowerCase();
    return terms.every((t) => haystack.includes(t));
  }

  const directlyMatchedGroupUuids = $derived.by<Set<string>>(() => {
    const result = new Set<string>();
    if (!isSearching) return result;
    for (const g of groups) {
      if (groupMatches(g)) result.add(g.uuid);
    }
    return result;
  });

  // Every descendant of a directly matched group remains browsable. A group
  // may be replicated or local; ownership does not change search ordering or
  // visibility semantics.
  const matchedSubtreeGroupUuids = $derived.by<Set<string>>(() => {
    const result = new Set<string>();
    if (!isSearching) return result;
    const parent = new Map<string, string | null>();
    for (const g of groups) parent.set(g.uuid, g.parent_uuid);
    for (const g of groups) {
      let current: string | null = g.uuid;
      while (current != null) {
        if (directlyMatchedGroupUuids.has(current)) {
          result.add(g.uuid);
          break;
        }
        current = parent.get(current) ?? null;
      }
    }
    return result;
  });

  const visibleGroupUuids = $derived.by<Set<string>>(() => {
    const result = new Set<string>();
    if (!isSearching) return result;
    const parent = new Map<string, string | null>();
    for (const g of groups) parent.set(g.uuid, g.parent_uuid);

    const includeWithAncestors = (uuid: string) => {
      let current: string | null = uuid;
      while (current != null && !result.has(current)) {
        result.add(current);
        current = parent.get(current) ?? null;
      }
    };
    for (const uuid of matchedSubtreeGroupUuids) includeWithAncestors(uuid);
    for (const h of hosts) {
      if (!hostMatches(h)) continue;
      for (const gu of h.group_uuids) includeWithAncestors(gu);
    }
    return result;
  });

  function showGroup(node: Node): boolean {
    return (
      !isSearching ||
      visibleGroupUuids.has(node.group.uuid) ||
      revealPathUuids.has(node.group.uuid)
    );
  }
  function showHost(h: Host): boolean {
    return (
      h.uuid === revealHostUuid ||
      hostMatches(h) || h.group_uuids.some((uuid) => matchedSubtreeGroupUuids.has(uuid))
    );
  }
  function nodeOpen(g: Group): boolean {
    if (revealPathUuids.has(g.uuid)) return true;
    if (isSearching) return visibleGroupUuids.has(g.uuid);
    return expanded.has(g.uuid);
  }
</script>

<nav bind:this={treeNav} class="text-xs leading-tight select-none">
  {#each rootHosts as host (host.uuid)}
    {#if showHost(host)}
      {@render hostRow(host, 0.5, null)}
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
  <!-- Hover-revealed on desktop (the `group` marker on the row), always
       visible on mobile where there's no hover. The desktop variant
       lands at 60 % muted; the pencil's own hover/focus bumps to 100 %
       so the user gets a "ready to click" cue. -->
  <button
    type="button"
    onclick={(e) => {
      e.stopPropagation();
      onClick();
    }}
    class="opacity-60 md:opacity-0 md:group-hover:opacity-60 hover:!opacity-100 focus:!opacity-100"
    style="color: var(--muted); padding: 2px"
    aria-label={label}
    title={label}
  >
    {@render pencilIcon()}
  </button>
{/snippet}

{#snippet hostRow(host: Host, paddingRem: number, groupUuid: string | null)}
  <div
    data-tree-host-uuid={host.uuid}
    data-tree-group-uuid={groupUuid ?? ''}
    class="group w-full flex items-center gap-2 px-2 py-1.5 md:py-0.5 hover:bg-white/5"
    style="padding-left: {paddingRem}rem; {isActiveHost(host.uuid)
      ? 'background: rgba(78, 161, 255, 0.12);'
      : ''}"
    title={host.replication_peer_id != null
      ? `${host.display_name} (replicated)`
      : host.display_name}
  >
    <span style="color: var(--muted)">•</span>
    <button
      type="button"
      onclick={() => pickHost(host.uuid)}
      class="flex-1 min-w-0 text-left truncate"
      style={host.replication_peer_id != null ? 'color: var(--fg-replicated)' : ''}
      title={host.display_name}
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
      class="group w-full flex items-center gap-1 px-1.5 py-1.5 md:py-0.5 hover:bg-white/5"
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
        class="flex-1 min-w-0 text-left font-medium truncate"
        style="color: var(--fg)"
        title={node.group.display_name}
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
          {@render hostRow(host, 1.5 + depth * 0.75, node.group.uuid)}
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
