<script lang="ts">
  import { onMount } from 'svelte';

  let {
    title,
    onClose,
    children
  }: {
    title: string;
    onClose: () => void;
    children: import('svelte').Snippet;
  } = $props();

  // Close on Esc. Mounted on window so the modal doesn't have to be focused.
  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });

  // Track where the press started. A drag whose mousedown lands inside the
  // dialog (e.g. selecting text inside a number input) but whose mouseup
  // happens on the backdrop fires a `click` on the common ancestor — the
  // backdrop — with `e.target === e.currentTarget`. Without remembering the
  // press origin we'd close the modal on every such drag. We close only
  // when *both* ends of the click are on the backdrop itself.
  let pressTarget: EventTarget | null = null;

  function onBackdropMouseDown(e: MouseEvent) {
    pressTarget = e.target;
  }

  function onBackdrop(e: MouseEvent) {
    const both = e.target === e.currentTarget && pressTarget === e.currentTarget;
    pressTarget = null;
    if (both) onClose();
  }
</script>

<!-- The backdrop is a dismissive overlay: clicking outside the dialog
     closes the modal. Esc handles keyboard dismissal (see onMount), so
     we suppress the static-interaction lint here rather than adding a
     redundant role. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
  class="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto"
  style="background: rgba(0,0,0,0.45); padding: 4rem 1rem"
  onmousedown={onBackdropMouseDown}
  onclick={onBackdrop}
>
  <div
    class="w-full max-w-2xl rounded border shadow-lg"
    style="background: var(--bg); border-color: var(--border)"
    role="dialog"
    aria-modal="true"
    aria-label={title}
  >
    <header
      class="flex items-center justify-between px-3 py-2 border-b"
      style="border-color: var(--border)"
    >
      <h2 class="text-xs font-semibold" style="color: var(--fg)">{title}</h2>
      <button
        type="button"
        onclick={onClose}
        class="text-xs px-1.5"
        style="color: var(--muted)"
        aria-label="Close"
      >
        ✕
      </button>
    </header>
    <div class="p-3">
      {@render children()}
    </div>
  </div>
</div>
