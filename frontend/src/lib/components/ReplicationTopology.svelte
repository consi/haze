<script lang="ts">
  // DAG of every upstream instance that contributes data to THIS one.
  // Each peer P contributes a path:
  //   P.upstream_chain[0] → … → P.upstream_chain[last] → P.source → this
  // We deduplicate nodes that appear across multiple peers (a common
  // ancestor in cascaded topologies is drawn once with multiple edges
  // converging on it), assign each node a "depth from this instance",
  // and lay nodes out column-by-column.
  import type { ReplicationPeer } from '$lib/api';

  let { peers, onClose }: { peers: ReplicationPeer[]; onClose: () => void } = $props();

  function short(u: string): string {
    return u.length >= 8 ? u.slice(0, 8) : u;
  }

  type Node = {
    id: string; // 'this' or instance UUID
    label: string;
    kind: 'upstream' | 'peer' | 'self';
    depth: number; // hops away from 'this' (this = 0)
    column?: number; // assigned during layout
  };
  type Edge = { from: string; to: string };

  // Build the graph: edges are upstream -> downstream, ending at 'this'.
  const graph = $derived.by(() => {
    const nodes = new Map<string, Node>();
    const edges: Edge[] = [];
    const SELF = 'this';
    nodes.set(SELF, { id: SELF, label: 'this instance', kind: 'self', depth: 0 });

    for (const p of peers) {
      const chain = [
        ...p.upstream_chain,
        ...(p.source_instance_uuid ? [p.source_instance_uuid] : [])
      ];
      // Walk chain from oldest (leftmost) to newest (rightmost). Add
      // edges between consecutive nodes; the rightmost connects to SELF.
      for (let i = 0; i < chain.length; i++) {
        const uuid = chain[i];
        if (!nodes.has(uuid)) {
          const isPeer = i === chain.length - 1 && uuid === p.source_instance_uuid;
          nodes.set(uuid, {
            id: uuid,
            label: isPeer ? p.name : short(uuid),
            kind: isPeer ? 'peer' : 'upstream',
            depth: 1 // recomputed below
          });
        } else if (i === chain.length - 1 && uuid === p.source_instance_uuid) {
          // If a UUID later appears as a configured peer's direct
          // source, upgrade its label so the operator sees the friendly
          // name instead of just "upstream <hex>".
          const n = nodes.get(uuid)!;
          if (n.kind !== 'peer') {
            n.kind = 'peer';
            n.label = p.name;
          }
        }
        const next = i + 1 < chain.length ? chain[i + 1] : SELF;
        if (!edges.some((e) => e.from === uuid && e.to === next)) {
          edges.push({ from: uuid, to: next });
        }
      }
    }

    // BFS from SELF in the reverse-edge direction to assign depths.
    // Each node's depth is the longest path back to SELF; that puts
    // common ancestors at consistent levels even when they're shared
    // across branches of different lengths.
    const incoming = new Map<string, string[]>();
    for (const e of edges) {
      if (!incoming.has(e.to)) incoming.set(e.to, []);
      incoming.get(e.to)!.push(e.from);
    }
    // Compute depth via DFS-with-memo (graph is a DAG so this terminates).
    function depthOf(id: string, seen: Set<string>): number {
      if (id === SELF) return 0;
      if (seen.has(id)) return 0;
      seen.add(id);
      const parents = incoming.get(id) ?? [];
      if (parents.length === 0) return 1;
      let max = 0;
      for (const p of parents) {
        const d = depthOf(p, seen) + 1;
        if (d > max) max = d;
      }
      seen.delete(id);
      return max;
    }
    // The "depth" we want is hops from SELF going upstream. Cheaper to
    // compute: do BFS from SELF using a reversed-edge adjacency.
    const reverseAdj = new Map<string, string[]>();
    for (const e of edges) {
      if (!reverseAdj.has(e.to)) reverseAdj.set(e.to, []);
      reverseAdj.get(e.to)!.push(e.from);
    }
    const depths = new Map<string, number>();
    depths.set(SELF, 0);
    const q: string[] = [SELF];
    while (q.length > 0) {
      const cur = q.shift()!;
      const d = depths.get(cur)!;
      for (const p of reverseAdj.get(cur) ?? []) {
        const next = d + 1;
        if (!depths.has(p) || depths.get(p)! < next) {
          depths.set(p, next);
          q.push(p);
        }
      }
    }
    for (const [id, d] of depths) {
      const n = nodes.get(id);
      if (n) n.depth = d;
    }
    void depthOf; // silence unused

    return { nodes, edges };
  });

  // Bucket by depth, then assign a column to each node in that bucket so
  // siblings within the same depth band don't overlap.
  const laidOut = $derived.by(() => {
    const byDepth = new Map<number, Node[]>();
    for (const n of graph.nodes.values()) {
      if (!byDepth.has(n.depth)) byDepth.set(n.depth, []);
      byDepth.get(n.depth)!.push(n);
    }
    let maxColumns = 1;
    for (const list of byDepth.values()) {
      list.sort((a, b) => a.label.localeCompare(b.label));
      list.forEach((n, idx) => {
        n.column = idx;
      });
      if (list.length > maxColumns) maxColumns = list.length;
    }
    const maxDepth = Math.max(0, ...Array.from(byDepth.keys()));
    return { byDepth, maxColumns, maxDepth };
  });

  const BOX_W = 90;
  const BOX_H = 40;
  const V_GAP = 44;
  const H_GAP = 22;
  const PAD = 16;

  // Pan + zoom state. Mouse wheel adjusts zoom around the cursor;
  // mouse drag pans the SVG viewport. The transform is applied to a
  // wrapping <g> so the canvas itself doesn't need re-laying-out.
  let zoom = $state(1);
  let panX = $state(0);
  let panY = $state(0);
  let dragging = $state(false);
  let dragStart = { x: 0, y: 0, panX: 0, panY: 0 };

  function onWheel(ev: WheelEvent) {
    ev.preventDefault();
    const factor = ev.deltaY < 0 ? 1.1 : 1 / 1.1;
    const newZoom = Math.min(4, Math.max(0.3, zoom * factor));
    // Zoom around the cursor: keep the pointed-at point stable.
    const rect = (ev.currentTarget as SVGElement).getBoundingClientRect();
    const cx = ev.clientX - rect.left;
    const cy = ev.clientY - rect.top;
    panX = cx - (cx - panX) * (newZoom / zoom);
    panY = cy - (cy - panY) * (newZoom / zoom);
    zoom = newZoom;
  }
  function onMouseDown(ev: MouseEvent) {
    dragging = true;
    dragStart = { x: ev.clientX, y: ev.clientY, panX, panY };
  }
  function onMouseMove(ev: MouseEvent) {
    if (!dragging) return;
    panX = dragStart.panX + (ev.clientX - dragStart.x);
    panY = dragStart.panY + (ev.clientY - dragStart.y);
  }
  function onMouseUp() {
    dragging = false;
  }
  function resetView() {
    zoom = 1;
    panX = 0;
    panY = 0;
  }

  const totalWidth = $derived(PAD * 2 + laidOut.maxColumns * BOX_W + (laidOut.maxColumns - 1) * H_GAP);
  const totalHeight = $derived(PAD * 2 + (laidOut.maxDepth + 1) * BOX_H + laidOut.maxDepth * V_GAP);

  function xFor(node: Node): number {
    const col = node.column ?? 0;
    const list = laidOut.byDepth.get(node.depth) ?? [];
    // Centre this depth's row inside the SVG by offsetting from the half-width
    // that the row's nodes actually occupy.
    const rowWidth = list.length * BOX_W + Math.max(0, list.length - 1) * H_GAP;
    const rowStart = (totalWidth - rowWidth) / 2;
    return rowStart + col * (BOX_W + H_GAP);
  }
  function yFor(node: Node): number {
    // depth 0 = bottom (this instance). Higher depth = farther upstream
    // = higher up in the SVG.
    const fromTop = laidOut.maxDepth - node.depth;
    return PAD + fromTop * (BOX_H + V_GAP);
  }

  function boxColor(kind: Node['kind']): string {
    if (kind === 'self') return 'rgba(78, 161, 255, 0.18)';
    if (kind === 'peer') return 'rgba(88, 196, 122, 0.16)';
    return 'rgba(128, 128, 128, 0.12)';
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div
  class="fixed inset-0 z-50 flex items-center justify-center"
  style="background: rgba(0,0,0,0.45)"
  onclick={onClose}
  role="dialog"
  aria-modal="true"
  tabindex="-1"
  onkeydown={(e) => e.key === 'Escape' && onClose()}
>
  <div
    class="rounded border p-4"
    style="background: var(--bg); border-color: var(--border); color: var(--fg); max-width: 95vw; width: 720px"
    onclick={(e) => e.stopPropagation()}
    role="document"
  >
    <div class="flex items-center justify-between mb-3">
      <div>
        <h3 class="text-sm font-semibold">Replication topology</h3>
        <p class="text-[11px] mt-0.5" style="color: var(--muted)">
          Scroll to zoom, drag to pan. Shared ancestors merge into one
          node with multiple converging arrows.
        </p>
      </div>
      <div class="flex items-center gap-2">
        <button
          type="button"
          class="text-[11px] underline"
          style="color: var(--muted)"
          onclick={resetView}
        >
          reset
        </button>
        <button
          type="button"
          onclick={onClose}
          class="text-base px-2 leading-none"
          style="color: var(--muted)"
          aria-label="Close"
        >
          ✕
        </button>
      </div>
    </div>

    {#if peers.length === 0}
      <p class="text-xs" style="color: var(--muted)">No peers configured.</p>
    {:else}
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div
        style="height: 420px; overflow: hidden; border: 1px solid var(--border); border-radius: 4px; background: rgba(0,0,0,0.04); cursor: {dragging ? 'grabbing' : 'grab'}"
        onmousedown={onMouseDown}
        onmousemove={onMouseMove}
        onmouseup={onMouseUp}
        onmouseleave={onMouseUp}
        onwheel={onWheel}
      >
        <svg
          width="100%"
          height="100%"
          role="img"
          aria-label="Replication topology graph"
        >
          <g transform="translate({panX} {panY}) scale({zoom})">
          <defs>
            <marker
              id="topo-arrow"
              viewBox="0 0 10 10"
              refX="9"
              refY="5"
              markerWidth="6"
              markerHeight="6"
              orient="auto-start-reverse"
            >
              <path d="M 0 0 L 10 5 L 0 10 z" fill="var(--muted)" />
            </marker>
          </defs>

          {#each graph.edges as e (e.from + e.to)}
            {@const from = graph.nodes.get(e.from)}
            {@const to = graph.nodes.get(e.to)}
            {#if from && to}
              {@const x1 = xFor(from) + BOX_W / 2}
              {@const y1 = yFor(from) + BOX_H}
              {@const x2 = xFor(to) + BOX_W / 2}
              {@const y2 = yFor(to)}
              <line
                x1={x1}
                y1={y1}
                x2={x2}
                y2={y2 - 4}
                stroke="var(--muted)"
                stroke-width="1.5"
                marker-end="url(#topo-arrow)"
              />
            {/if}
          {/each}

          {#each Array.from(graph.nodes.values()) as n (n.id)}
            <g>
              <rect
                x={xFor(n)}
                y={yFor(n)}
                width={BOX_W}
                height={BOX_H}
                rx="4"
                fill={boxColor(n.kind)}
                stroke="var(--border)"
              />
              <text
                x={xFor(n) + BOX_W / 2}
                y={yFor(n) + BOX_H / 2 - 2}
                text-anchor="middle"
                style="font-size: 10px; font-weight: 600; fill: var(--fg)"
              >
                {n.label}
              </text>
              <text
                x={xFor(n) + BOX_W / 2}
                y={yFor(n) + BOX_H / 2 + 10}
                text-anchor="middle"
                style="font-size: 8px; fill: var(--muted); font-family: monospace"
              >
                {n.kind === 'self' ? '(local)' : short(n.id)}
              </text>
            </g>
          {/each}
          </g>
        </svg>
      </div>

      <div class="mt-3 text-[11px] grid grid-cols-3 gap-2" style="color: var(--muted)">
        <span class="flex items-center gap-1">
          <span
            class="inline-block rounded"
            style="width: 10px; height: 10px; background: rgba(128, 128, 128, 0.12); border: 1px solid var(--border)"
          ></span>
          upstream chain
        </span>
        <span class="flex items-center gap-1">
          <span
            class="inline-block rounded"
            style="width: 10px; height: 10px; background: rgba(88, 196, 122, 0.16); border: 1px solid var(--border)"
          ></span>
          configured peer
        </span>
        <span class="flex items-center gap-1">
          <span
            class="inline-block rounded"
            style="width: 10px; height: 10px; background: rgba(78, 161, 255, 0.18); border: 1px solid var(--border)"
          ></span>
          this instance
        </span>
      </div>
    {/if}
  </div>
</div>
