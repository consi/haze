<script lang="ts">
  let { label = 'Loading graph data…' }: { label?: string } = $props();
</script>

<div
  class="absolute inset-0 z-10 flex items-center justify-center pointer-events-none"
  style="background: color-mix(in srgb, var(--bg) 55%, transparent)"
  role="status"
  aria-label={label}
>
  <span
    class="block w-7 h-7 rounded-full animate-spin"
    style="border: 3px solid var(--border); border-top-color: var(--accent)"
    aria-hidden="true"
  ></span>
</div>
