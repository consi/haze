<script lang="ts">
  let { what = 'this page' }: { what?: string } = $props();
</script>

<div class="p-6">
  <h1 class="text-base font-semibold mb-1">Forbidden</h1>
  <p class="text-xs" style="color: var(--muted)">
    Your account doesn't have permission to view {what}.
  </p>
</div>
