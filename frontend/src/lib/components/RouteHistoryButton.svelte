<script lang="ts">
  import type { Host } from '$lib/api';
  import RouteHistoryModal from './RouteHistoryModal.svelte';
  let {host,fromSecs,toSecs}: {host:Host;fromSecs:number;toSecs:number}=$props();
  let open=$state(false);
</script>
{#if host.probe_type === 'ping'}
  <button type="button" class="icon-button compact-icon-button route-button ml-auto shrink-0 inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px]" onclick={()=>open=true} title="Review route changes and packet loss" aria-label={`Route history for ${host.display_name}`}>
    <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" aria-hidden="true"><circle cx="3" cy="8" r="1.6"/><circle cx="12" cy="3" r="1.6"/><circle cx="12" cy="13" r="1.6"/><path d="M4.6 8H7V3h3.4M7 8v5h3.4"/></svg>
  </button>
  {#if open}<RouteHistoryModal {host} {fromSecs} {toSecs} onClose={()=>open=false}/>{/if}
{/if}
<style>
  .route-button {color:var(--muted);border:1px solid var(--border);background:var(--bg)}
  .route-button:hover,.route-button:focus-visible {color:var(--accent);border-color:var(--accent)}
</style>
