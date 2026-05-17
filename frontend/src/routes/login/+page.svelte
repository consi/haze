<script lang="ts">
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { startAuthentication } from '@simplewebauthn/browser';
  import { login as pwLogin, auth } from '$lib/auth.svelte';
  import { api } from '$lib/api';

  let username = $state('');
  let password = $state('');
  let err = $state<string | null>(null);
  let submitting = $state(false);
  let passkeysEnabled = $state(false);

  onMount(async () => {
    try {
      const info = await api.serverInfo();
      passkeysEnabled = info.passkeys_enabled;
    } catch {
      // Leave as false if the server-info endpoint is unreachable.
    }
  });

  async function passwordSubmit(e: SubmitEvent) {
    e.preventDefault();
    err = null;
    submitting = true;
    try {
      await pwLogin(username, password);
      void goto('/');
    } catch (e) {
      err = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }

  async function passkeyLogin() {
    err = null;
    submitting = true;
    try {
      const begin = await api.passkeyLoginBegin();
      // Same wrapper unwrap as registration - see comment in /user/+page.svelte.
      const optionsJSON = (begin.challenge as { publicKey: unknown }).publicKey;
      const cred = await startAuthentication({ optionsJSON: optionsJSON as never });
      const user = await api.passkeyLoginFinish(begin.token, cred);
      auth.user = user;
      void goto('/');
    } catch (e) {
      err = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }
</script>

<div class="min-h-[60vh] flex items-center justify-center">
  <form
    onsubmit={passwordSubmit}
    class="w-80 rounded border p-4 space-y-3"
    style="border-color: var(--border); background: rgba(255,255,255,0.02)"
  >
    <h1 class="font-semibold text-sm" style="color: var(--fg)">Sign in</h1>

    <label class="block">
      <span class="text-xs" style="color: var(--muted)">Username</span>
      <input
        bind:value={username}
        type="text"
        autocomplete="username webauthn"
        required
        class="w-full mt-0.5 px-2 py-1 rounded border text-sm"
        style="background: var(--bg); border-color: var(--border); color: var(--fg)"
      />
    </label>

    <label class="block">
      <span class="text-xs" style="color: var(--muted)">Password</span>
      <input
        bind:value={password}
        type="password"
        autocomplete="current-password"
        class="w-full mt-0.5 px-2 py-1 rounded border text-sm"
        style="background: var(--bg); border-color: var(--border); color: var(--fg)"
      />
    </label>

    {#if err}
      <p class="text-xs" style="color: var(--latency-bad)">{err}</p>
    {/if}

    <button
      type="submit"
      disabled={submitting}
      class="w-full px-3 py-1.5 rounded text-sm font-medium"
      style="background: var(--fg); color: var(--bg); opacity: {submitting ? 0.6 : 1}"
    >
      {submitting ? 'Signing in…' : 'Sign in with password'}
    </button>

    {#if passkeysEnabled}
      <button
        type="button"
        onclick={passkeyLogin}
        disabled={submitting}
        class="w-full px-3 py-1.5 rounded text-sm font-medium border"
        style="border-color: var(--border); color: var(--fg)"
      >
        Sign in with passkey
      </button>
    {/if}
  </form>
</div>
