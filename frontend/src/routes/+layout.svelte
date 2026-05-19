<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';
  import {
    auth,
    refresh,
    logout,
    publicMode,
    setPublicMode,
    canSeeAlerts,
    canSeeSettings,
    canEditGroups,
    canEditHosts
  } from '$lib/auth.svelte';
  import { api, setUnauthorizedHandler, type Group, type Host } from '$lib/api';
  import {
    connectEvents,
    disconnectEvents,
    reloadKeys,
    setEventsUnauthorizedHandler
  } from '$lib/events.svelte';
  import Tree from '$lib/components/Tree.svelte';
  import CreateGroupModal from '$lib/components/CreateGroupModal.svelte';
  import CreateHostModal from '$lib/components/CreateHostModal.svelte';
  import EditGroupModal from '$lib/components/EditGroupModal.svelte';
  import EditHostModal from '$lib/components/EditHostModal.svelte';
  import { collapseAll, expandAll, treeState } from '$lib/tree-state.svelte';
  // expandAll is still used by the "++" header button.
  import { goto } from '$app/navigation';
  import { base } from '$app/paths';
  import { page } from '$app/state';

  // Single redirect path for any code path that detects the session is
  // gone - both the central api.ts 401 trap and the EventSource terminal
  // error route through this. Guard against re-entrant calls (already on
  // /login) so we don't queue infinite navigations.
  //
  // In public mode an anonymous visitor can hit an endpoint that still
  // requires auth (e.g. an admin-only setting); we treat that as
  // "endpoint forbidden", not "session lost", and stay on the page they
  // were browsing. Without this guard, viewing a host or group page
  // bounced public visitors to /login because the storage-settings probe
  // returned 401.
  function handleUnauthorized() {
    const wasAuthenticated = auth.user !== null;
    // Anonymous public-mode visitor hit a still-protected endpoint
    // (e.g. an admin-only setting). Their SSE stream and the page they
    // were viewing are both still valid; don't tear them down and don't
    // navigate.
    if (!wasAuthenticated && publicMode.enabled) return;
    disconnectEvents();
    auth.user = null;
    if (page.url.pathname !== `${base}/login`) void goto(`${base}/login`);
  }
  setUnauthorizedHandler(handleUnauthorized);
  setEventsUnauthorizedHandler(handleUnauthorized);

  let { children } = $props();

  let groups = $state<Group[]>([]);
  let hosts = $state<Host[]>([]);
  let treeLoading = $state(false);

  // Search input (live) + debounced version that drives the tree filter.
  // 150 ms is enough to avoid recomputing on every keystroke without feeling
  // sluggish; filtering is O(n hosts) substring match, plenty fast for our
  // host counts.
  let searchInput = $state('');
  let searchDebounced = $state('');
  $effect(() => {
    const v = searchInput;
    const timer = setTimeout(() => {
      searchDebounced = v;
    }, 150);
    return () => clearTimeout(timer);
  });

  async function loadTree() {
    // Anonymous public-mode visitors also see the tree; the api gates the
    // call on the backend ViewerAccess extractor.
    if (!auth.user && !publicMode.enabled) return;
    treeLoading = true;
    try {
      // Initial expand-state is loaded from localStorage by tree-state.svelte;
      // we don't seed it here. Groups whose UUID isn't in the saved set
      // render collapsed, so brand-new installs start fully collapsed and
      // new groups appear collapsed by default. Host adds/removes have no
      // effect on the set.
      const tree = await api.getTree();
      groups = tree.groups;
      hosts = tree.hosts;
    } finally {
      treeLoading = false;
    }
  }

  // Ask the server whether public mode is on before deciding to redirect
  // an unauthenticated visitor. /server-info is always anonymous-accessible
  // and is the single source of truth for the flag.
  async function refreshPublicMode(): Promise<boolean> {
    try {
      const info = await api.serverInfo();
      setPublicMode(info.public_mode_enabled);
      return info.public_mode_enabled;
    } catch {
      // If /server-info is unreachable we fall back to "no public mode"
      // so the existing login-required flow stays in effect.
      setPublicMode(false);
      return false;
    }
  }

  onMount(async () => {
    await refreshPublicMode();
    await refresh();
    if (auth.user) await loadTree();
    else if (publicMode.enabled) await loadTree();
    else if (page.url.pathname !== `${base}/login`) void goto(`${base}/login`);
  });

  // When an admin flips the public-mode toggle elsewhere, the Settings SSE
  // event fires. Re-read /server-info so anonymous tabs either gain the
  // sidebar (turned on) or get bounced to /login (turned off).
  $effect(() => {
    void reloadKeys.settings;
    void refreshPublicMode();
  });

  // Re-fetch the tree whenever `auth.user` changes or another page calls
  // `reloadTree()` (which bumps `treeState.reloadKey`). The effect tracks the
  // reloadKey by reading it inside the dependency block so Svelte 5 wires
  // the reactivity automatically.
  $effect(() => {
    const _key = treeState.reloadKey;
    void _key;
    if (auth.user || publicMode.enabled) void loadTree();
  });

  // Open/close the SSE stream as the auth or public-mode state flips. Both
  // functions are idempotent, so calling them on every transition is safe.
  // Logging in from /login → connect; logout / 401 redirect / public mode
  // disabled → disconnect. Anonymous visitors in public mode also get the
  // stream so tree updates land live.
  $effect(() => {
    if (auth.user || publicMode.enabled) connectEvents();
    else disconnectEvents();
  });

  // If public mode flips off while we're anonymous, bounce to /login.
  // Gated on `publicMode.initialized` so the very first render doesn't
  // redirect against the default `enabled = false` before the
  // /server-info fetch resolves - that race was sending anonymous
  // visitors straight to /login on cold loads.
  $effect(() => {
    if (
      publicMode.initialized &&
      !auth.user &&
      !publicMode.enabled &&
      page.url.pathname !== `${base}/login`
    ) {
      void goto(`${base}/login`);
    }
  });

  async function doLogout() {
    disconnectEvents();
    await logout();
    // In public mode the dashboard is browsable without a session, so
    // logging out should drop the user onto the public welcome page
    // (where a "log in" link is still visible in the top bar). Without
    // this branch they'd land on /login with no way back to the public
    // view short of typing the URL by hand.
    if (publicMode.enabled) {
      void goto(`${base}/`);
    } else {
      void goto(`${base}/login`);
    }
  }

  // ─── Resizable tree pane ─────────────────────────────────────────────────
  // Width is persisted to localStorage so it survives reloads. The handle is
  // a 4 px-wide strip between <aside> and <main>; while dragging we listen on
  // window so the pointer can travel anywhere on screen, and we only commit
  // to localStorage on pointerup to avoid hammering it on every frame.
  const TREE_WIDTH_KEY = 'haze.treeWidth';
  const TREE_MIN = 160;
  const TREE_MAX = 640;
  const TREE_DEFAULT = 240;

  let treeWidth = $state(TREE_DEFAULT);
  let dragging = $state(false);

  // Modal state for the sidebar +G / +H buttons. Both open inline overlays
  // rather than navigating to a separate page, so the user keeps their tree
  // context (selected host, scroll position, expansion state) intact.
  let groupModalOpen = $state(false);
  let hostModalOpen = $state(false);
  // Edit modals are open when these hold a target. The Tree fires
  // `onEditGroup` / `onEditHost`, the layout snapshots the group/host,
  // and the modal renders against that snapshot. Modal save closes by
  // setting back to null and triggers a tree reload.
  let editGroupTarget = $state<import('$lib/api').Group | null>(null);
  let editHostTarget = $state<import('$lib/api').Host | null>(null);

  // Mobile burger drawer: full-screen overlay containing the action row
  // (user/alerts/settings/log out) plus the full tree. Closed by re-tapping
  // the burger or by any in-drawer navigation (tracked via pathname).
  let burgerOpen = $state(false);
  // Snapshot the pathname so we can auto-close the drawer when it changes.
  // Without this, tapping a tree node would navigate but leave the drawer
  // overlay covering the destination page.
  $effect(() => {
    const _path = page.url.pathname;
    void _path;
    burgerOpen = false;
  });

  onMount(() => {
    if (typeof localStorage === 'undefined') return;
    const raw = localStorage.getItem(TREE_WIDTH_KEY);
    if (raw == null) return;
    const v = parseInt(raw, 10);
    if (Number.isFinite(v)) treeWidth = clamp(v, TREE_MIN, TREE_MAX);
  });

  function clamp(v: number, lo: number, hi: number): number {
    return Math.min(hi, Math.max(lo, v));
  }

  function startResize(e: PointerEvent) {
    e.preventDefault();
    dragging = true;
    const startX = e.clientX;
    const startW = treeWidth;

    const onMove = (ev: PointerEvent) => {
      treeWidth = clamp(startW + (ev.clientX - startX), TREE_MIN, TREE_MAX);
    };
    const onUp = () => {
      dragging = false;
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onUp);
      try {
        localStorage.setItem(TREE_WIDTH_KEY, String(Math.round(treeWidth)));
      } catch {
        // localStorage can throw in private mode / when full - ignore.
      }
    };
    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', onUp);
  }
</script>

<!-- h-dvh (dynamic viewport height) instead of min-h-screen so the flex
     chain has a definite height. With min-h-screen the root could grow
     past the viewport, BODY would scroll, and `<main>`'s
     `overflow-y-auto` never kicked in - which broke the sticky compact
     header on host/group pages. The `shrink-0` on <header> + `min-h-0`
     on the flex-1 chain below are required because flex items default
     to `min-height: auto`, which lets long content push the row taller
     than the parent and re-introduces body scroll. -->
<div class="h-dvh flex flex-col overflow-hidden">
  <header class="shrink-0 border-b flex items-center gap-3 px-3 py-2" style="border-color: var(--border)">
    <a
      href={`${base}/`}
      class="font-display"
      style="color: var(--fg); font-weight: 700; font-size: 16px; line-height: 1"
    >
      Haze
    </a>
    <span class="font-display" style="color: var(--muted); font-weight: 400; font-size: 10px">
      network latency monitor
    </span>
    <span class="ml-auto text-xs flex items-center gap-3" style="color: var(--muted)">
      <!-- Desktop inline actions. Hidden on mobile (the burger drawer holds
           the same actions, just with bigger tap targets). -->
      <span class="hidden md:flex items-center gap-3">
        {#if auth.user}
          <a
            href={`${base}/user`}
            class="mono hover:text-[var(--fg)]"
            style="color: var(--fg); font-weight: {auth.user.role === 'admin' ? 600 : 400}"
          >
            {auth.user.username}
          </a>
          {#if canSeeAlerts()}
            <a href={`${base}/alerting`} class="hover:text-[var(--fg)]">alerting</a>
          {/if}
          {#if canSeeSettings()}
            <a href={`${base}/settings`} class="hover:text-[var(--fg)]">settings</a>
          {/if}
          <button onclick={doLogout} class="hover:text-[var(--fg)]">log out</button>
        {:else if publicMode.enabled}
          <a href={`${base}/login`} class="hover:text-[var(--fg)]">log in</a>
        {/if}
      </span>
      <!-- Mobile burger. Only shown when we actually have a drawer to open
           (i.e. on app pages, not /login when signed-out without public mode). -->
      {#if page.url.pathname !== `${base}/login` && (auth.user || publicMode.enabled)}
        <button
          type="button"
          class="md:hidden inline-flex flex-col gap-[3px] p-2 -mr-2"
          aria-label={burgerOpen ? 'Close menu' : 'Open menu'}
          aria-expanded={burgerOpen}
          onclick={() => (burgerOpen = !burgerOpen)}
        >
          <span class="block w-5 h-[2px]" style="background: var(--fg)"></span>
          <span class="block w-5 h-[2px]" style="background: var(--fg)"></span>
          <span class="block w-5 h-[2px]" style="background: var(--fg)"></span>
        </button>
      {/if}
    </span>
  </header>

  {#if page.url.pathname === `${base}/login` || (!auth.user && !publicMode.enabled)}
    <main class="flex-1">
      {@render children?.()}
    </main>
  {:else}
    <div class="flex-1 min-h-0 flex relative" style:user-select={dragging ? 'none' : null} style:cursor={dragging ? 'col-resize' : null}>
      <!-- Desktop sidebar. `hidden md:flex` so it doesn't compete with the
           mobile drawer for layout space. -->
      <aside
        class="hidden md:block overflow-y-auto shrink-0"
        style="width: {treeWidth}px; border-color: var(--border)"
      >
        <div class="px-2 py-1.5 flex items-center gap-2">
          <input
            bind:value={searchInput}
            type="search"
            placeholder="Search..."
            class="flex-1 text-xs px-1.5 py-0.5 rounded border outline-none"
            style="background: var(--bg); border-color: var(--border); color: var(--fg)"
          />
          <span class="flex gap-1 text-xs font-semibold">
            {#if canEditGroups()}
              <button
                type="button"
                title="Add Group"
                onclick={() => (groupModalOpen = true)}
                class="w-5 h-5 flex items-center justify-center rounded leading-none"
                style="background: var(--btn-bg); color: #fff"
              >G</button>
            {/if}
            {#if canEditHosts()}
              <button
                type="button"
                title="Add Host"
                onclick={() => (hostModalOpen = true)}
                class="w-5 h-5 flex items-center justify-center rounded leading-none"
                style="background: var(--btn-bg); color: #fff"
              >H</button>
            {/if}
            <button
              type="button"
              title="Expand All"
              onclick={() => expandAll(groups.map((g) => g.uuid))}
              class="w-5 h-5 flex items-center justify-center rounded leading-none"
              style="background: var(--btn-bg); color: #fff"
            >+</button>
            <button
              type="button"
              title="Collapse All"
              onclick={() => collapseAll()}
              class="w-5 h-5 flex items-center justify-center rounded leading-none"
              style="background: var(--btn-bg); color: #fff"
            >−</button>
          </span>
        </div>
        {#if !treeLoading && groups.length === 0 && hosts.length === 0}
          <p class="text-[11px] px-2 py-2 italic" style="color: var(--muted); opacity: 0.7">
            Nothing defined yet.
          </p>
        {:else}
          <Tree
            {groups}
            {hosts}
            search={searchDebounced}
            onEditGroup={(g) => (editGroupTarget = g)}
            onEditHost={(h) => (editHostTarget = h)}
          />
        {/if}
      </aside>
      <!-- Resize handle: 4 px hit zone, 1 px visible divider centred on it.
           role="separator" lets screen readers announce it as resizable.
           Hidden on mobile (no sidebar to resize). -->
      <div
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize tree pane"
        tabindex="-1"
        onpointerdown={startResize}
        class="hidden md:block shrink-0 cursor-col-resize"
        style="width: 4px; background: var(--border); opacity: {dragging ? 0.6 : 0.25}; transition: opacity 100ms"
        onmouseenter={(e) => (e.currentTarget.style.opacity = '0.6')}
        onmouseleave={(e) => {
          if (!dragging) e.currentTarget.style.opacity = '0.25';
        }}
      ></div>
      <main class="flex-1 overflow-y-auto min-w-0 min-h-0">
        {@render children?.()}
      </main>

      <!-- Mobile burger drawer: full-width overlay covering the main area.
           Renders the same actions and tree the desktop sidebar shows, but
           with bigger tap targets and edge-to-edge. Auto-closes on
           navigation via the $effect on page.url.pathname above. -->
      {#if burgerOpen}
        <div
          class="md:hidden absolute inset-0 z-40 flex flex-col overflow-y-auto"
          style="background: var(--bg)"
        >
          <!-- Action row: user / alerting / settings / log out (or log in). -->
          <div
            class="flex items-center gap-4 px-3 py-2 border-b text-sm"
            style="border-color: var(--border); color: var(--muted)"
          >
            {#if auth.user}
              <a
                href={`${base}/user`}
                class="mono"
                style="color: var(--fg); font-weight: {auth.user.role === 'admin' ? 600 : 400}"
              >{auth.user.username}</a>
              {#if canSeeAlerts()}
                <a href={`${base}/alerting`}>alerting</a>
              {/if}
              {#if canSeeSettings()}
                <a href={`${base}/settings`}>settings</a>
              {/if}
              <button onclick={doLogout} class="ml-auto">log out</button>
            {:else if publicMode.enabled}
              <a href={`${base}/login`} class="ml-auto">log in</a>
            {/if}
          </div>

          <!-- Search + add buttons -->
          <div class="px-3 py-2 flex items-center gap-2 border-b" style="border-color: var(--border)">
            <input
              bind:value={searchInput}
              type="search"
              placeholder="Search..."
              class="flex-1 text-sm px-2 py-1 rounded border outline-none"
              style="background: var(--bg); border-color: var(--border); color: var(--fg)"
            />
            <span class="flex gap-1.5 text-xs font-semibold">
              {#if canEditGroups()}
                <button
                  type="button"
                  title="Add Group"
                  onclick={() => {
                    groupModalOpen = true;
                    burgerOpen = false;
                  }}
                  class="w-7 h-7 flex items-center justify-center rounded leading-none"
                  style="background: var(--btn-bg); color: #fff"
                >G</button>
              {/if}
              {#if canEditHosts()}
                <button
                  type="button"
                  title="Add Host"
                  onclick={() => {
                    hostModalOpen = true;
                    burgerOpen = false;
                  }}
                  class="w-7 h-7 flex items-center justify-center rounded leading-none"
                  style="background: var(--btn-bg); color: #fff"
                >H</button>
              {/if}
              <button
                type="button"
                title="Expand All"
                onclick={() => expandAll(groups.map((g) => g.uuid))}
                class="w-7 h-7 flex items-center justify-center rounded leading-none"
                style="background: var(--btn-bg); color: #fff"
              >+</button>
              <button
                type="button"
                title="Collapse All"
                onclick={() => collapseAll()}
                class="w-7 h-7 flex items-center justify-center rounded leading-none"
                style="background: var(--btn-bg); color: #fff"
              >−</button>
            </span>
          </div>

          {#if !treeLoading && groups.length === 0 && hosts.length === 0}
            <p class="text-sm px-3 py-3 italic" style="color: var(--muted); opacity: 0.7">
              Nothing defined yet.
            </p>
          {:else}
            <div class="flex-1">
              <Tree
                {groups}
                {hosts}
                search={searchDebounced}
                onEditGroup={(g) => (editGroupTarget = g)}
                onEditHost={(h) => (editHostTarget = h)}
              />
            </div>
          {/if}
        </div>
      {/if}
    </div>
  {/if}
</div>

{#if groupModalOpen}
  <CreateGroupModal onClose={() => (groupModalOpen = false)} />
{/if}
{#if hostModalOpen}
  <CreateHostModal onClose={() => (hostModalOpen = false)} />
{/if}
{#if editGroupTarget}
  <EditGroupModal
    group={editGroupTarget}
    allGroups={groups}
    onClose={() => (editGroupTarget = null)}
  />
{/if}
{#if editHostTarget}
  <EditHostModal
    host={editHostTarget}
    allGroups={groups}
    onClose={() => (editHostTarget = null)}
  />
{/if}
