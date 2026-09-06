<script lang="ts">
  import { onMount } from 'svelte';
  import { startViewportTracking, viewport } from '$lib/viewport.svelte';

  let {
    title,
    onClose,
    children,
    wide = false,
    contained = false
  }: {
    wide?: boolean;
    contained?: boolean;
    title: string;
    onClose: () => void;
    children: import('svelte').Snippet;
  } = $props();

  let dialog: HTMLDivElement;

  // Close on Esc. Mounted on window so the modal doesn't have to be focused.
  onMount(() => {
    const stopViewport = startViewportTracking();
    const priorFocus = document.activeElement as HTMLElement | null;
    const previousOverflow = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
    dialog?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented) return;
      if (e.key === 'Escape') { e.preventDefault(); onClose(); }
      if (e.key === 'Tab' && dialog) {
        const items = Array.from(dialog.querySelectorAll<HTMLElement>('button:not(:disabled),a[href],input:not(:disabled),select:not(:disabled),textarea:not(:disabled),summary,[tabindex="0"]')).filter(el => el.getClientRects().length);
        const first=items[0],last=items[items.length-1];
        if(!first){e.preventDefault();dialog.focus();}
        else if(e.shiftKey && (document.activeElement===first||document.activeElement===dialog)){e.preventDefault();last.focus();}
        else if(!e.shiftKey && (document.activeElement===last||document.activeElement===dialog)){e.preventDefault();first.focus();}
      }
    };
    window.addEventListener('keydown', onKey);
    return () => {
      window.removeEventListener('keydown', onKey);
      stopViewport();
      document.body.style.overflow = previousOverflow;
      if (priorFocus?.isConnected && priorFocus.getClientRects().length) priorFocus.focus();
      else {
        const fallback = Array.from(document.querySelectorAll<HTMLElement>('[data-modal-focus-fallback], main')).find(el => el.getClientRects().length);
        fallback?.focus();
      }
    };
  });

  // Track where the press started. A drag whose mousedown lands inside the
  // dialog (e.g. selecting text inside a number input) but whose mouseup
  // happens on the backdrop fires a `click` on the common ancestor - the
  // backdrop - with `e.target === e.currentTarget`. Without remembering the
  // press origin we'd close the modal on every such drag. We close only
  // when *both* ends of the click are on the backdrop itself.
  //
  // On mobile we don't dismiss on backdrop at all - the dialog goes
  // full-screen so any stray touch on the "backdrop" is actually a
  // user trying to scroll the dialog body. The X button is the only
  // explicit dismissal there.
  let pressTarget: EventTarget | null = null;

  function onBackdropMouseDown(e: MouseEvent) {
    pressTarget = e.target;
  }

  function onBackdrop(e: MouseEvent) {
    if (viewport.isMobile) {
      pressTarget = null;
      return;
    }
    const both = e.target === e.currentTarget && pressTarget === e.currentTarget;
    pressTarget = null;
    if (both) onClose();
  }
</script>

<!-- The backdrop is a dismissive overlay: clicking outside the dialog
     closes the modal on desktop. Esc handles keyboard dismissal (see
     onMount), so we suppress the static-interaction lint here rather than
     adding a redundant role. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
  class="fixed inset-0 z-50 flex items-stretch md:items-start justify-center md:overflow-y-auto"
  style="background: rgba(0,0,0,0.45);"
  onmousedown={onBackdropMouseDown}
  onclick={onBackdrop}
>
  <div
    class="
      w-full {wide ? 'md:my-3 md:mx-3 md:h-[calc(100dvh-1.5rem)] md:max-h-[calc(100dvh-1.5rem)]' : 'md:max-w-2xl md:my-16 md:mx-4 md:max-h-[calc(100vh-8rem)]'}
      md:rounded md:border md:shadow-lg
      flex flex-col min-w-0 min-h-0
      max-h-full
    "
    style="background: var(--bg); border-color: var(--border)"
    bind:this={dialog}
    tabindex="-1"
    role="dialog"
    aria-modal="true"
    aria-label={title}
  >
    <header
      class="flex items-center justify-between gap-2 px-3 py-1 border-b shrink-0"
      style="border-color: var(--border)"
    >
      <h2 class="text-sm md:text-xs font-semibold truncate pr-2" style="color: var(--fg)">{title}</h2>
      <button
        type="button"
        onclick={onClose}
        class="icon-button icon-button"
        style="color: var(--muted)"
        aria-label="Close"
        title="Close (Escape)"
      >
        ✕
      </button>
    </header>
    <div class="modal-body p-3 min-w-0 min-h-0 flex-1 overflow-y-auto" class:contained>
      {@render children()}
    </div>
  </div>
</div>

<style>
  @media (min-width: 768px) {
    .modal-body.contained { display: flex; flex-direction: column; overflow: hidden; }
  }
</style>
