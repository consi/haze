<script lang="ts">
  import { onMount } from 'svelte';
  import { startRegistration } from '@simplewebauthn/browser';
  import { goto } from '$app/navigation';
  import { base } from '$app/paths';
  import { api } from '$lib/api';
  import { auth } from '$lib/auth.svelte';
  import { reloadKeys, disconnectEvents } from '$lib/events.svelte';

  // ─── Account info ────────────────────────────────────────────────────────

  // ─── Change password ────────────────────────────────────────────────────
  let pwCurrent = $state('');
  let pwNew = $state('');
  let pwConfirm = $state('');
  let pwBusy = $state(false);
  let pwErr = $state<string | null>(null);

  async function submitPassword(e: SubmitEvent) {
    e.preventDefault();
    pwErr = null;
    if (pwNew !== pwConfirm) {
      pwErr = 'New password and confirmation do not match.';
      return;
    }
    if (pwNew.length < 8) {
      pwErr = 'New password must be at least 8 characters.';
      return;
    }
    pwBusy = true;
    try {
      await api.changePassword(pwCurrent, pwNew);
      // The backend revokes all sessions on password change (it's the safe
      // default; see haze_auth::user::set_password_hash). Even though the
      // /api/v1/user/password route currently keeps the current session
      // alive, an admin-initiated reset elsewhere would not — and we'd
      // rather force a single, predictable re-login path than have the
      // user discover staleness mid-action. Disconnect the SSE stream
      // ourselves so it doesn't race with the impending /login navigation.
      disconnectEvents();
      auth.user = null;
      pwCurrent = '';
      pwNew = '';
      pwConfirm = '';
      void goto(`${base}/login?reason=password-changed`);
      return;
    } catch (e) {
      pwErr = e instanceof Error ? e.message : String(e);
    } finally {
      pwBusy = false;
    }
  }

  // ─── Passkeys ────────────────────────────────────────────────────────────
  type Passkey = {
    id: number;
    label: string | null;
    created_at: number;
    last_used_at: number | null;
  };
  let passkeys = $state<Passkey[]>([]);
  let passkeyLabel = $state('');
  let passkeyBusy = $state(false);
  let passkeyErr = $state<string | null>(null);
  let passkeyMsg = $state<string | null>(null);

  async function refreshPasskeys() {
    try {
      passkeys = await api.listMyPasskeys();
    } catch (e) {
      passkeyErr = e instanceof Error ? e.message : String(e);
    }
  }

  async function addPasskey() {
    passkeyBusy = true;
    passkeyErr = null;
    passkeyMsg = null;
    try {
      const begin = await api.passkeyRegisterBegin();
      // webauthn-rs wraps PublicKeyCredentialCreationOptions inside
      // `{ publicKey: { ... } }`; @simplewebauthn/browser's optionsJSON
      // expects that inner object directly.
      const optionsJSON = (begin.challenge as { publicKey: unknown }).publicKey;
      const cred = await startRegistration({ optionsJSON: optionsJSON as never });
      await api.passkeyRegisterFinish(begin.token, cred, passkeyLabel || undefined);
      passkeyMsg = 'Passkey added.';
      passkeyLabel = '';
      await refreshPasskeys();
    } catch (e) {
      passkeyErr = e instanceof Error ? e.message : String(e);
    } finally {
      passkeyBusy = false;
    }
  }

  async function removePasskey(id: number) {
    try {
      await api.deleteMyPasskey(id);
      await refreshPasskeys();
    } catch (e) {
      passkeyErr = e instanceof Error ? e.message : String(e);
    }
  }

  // ─── API tokens ──────────────────────────────────────────────────────────
  type Token = {
    id: number;
    name: string;
    created_at: number;
    expires_at: number | null;
    last_used_at: number | null;
  };
  let tokens = $state<Token[]>([]);
  let tokenName = $state('');
  let tokenBusy = $state(false);
  let tokenErr = $state<string | null>(null);
  let tokenPlaintext = $state<{ name: string; plaintext: string } | null>(null);

  async function refreshTokens() {
    try {
      tokens = await api.listMyTokens();
    } catch (e) {
      tokenErr = e instanceof Error ? e.message : String(e);
    }
  }

  async function createToken() {
    if (!tokenName.trim()) {
      tokenErr = 'Name is required';
      return;
    }
    tokenBusy = true;
    tokenErr = null;
    try {
      const r = await api.createMyToken(tokenName, null);
      tokenPlaintext = { name: r.name, plaintext: r.plaintext };
      tokenName = '';
      await refreshTokens();
    } catch (e) {
      tokenErr = e instanceof Error ? e.message : String(e);
    } finally {
      tokenBusy = false;
    }
  }

  async function revokeToken(id: number) {
    try {
      await api.deleteMyToken(id);
      await refreshTokens();
    } catch (e) {
      tokenErr = e instanceof Error ? e.message : String(e);
    }
  }

  async function copyToClipboard(text: string) {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // ignore
    }
  }

  function fmtTs(ts: number | null | undefined): string {
    if (ts == null) return '-';
    return new Date(ts * 1000).toLocaleString();
  }

  onMount(async () => {
    await Promise.all([refreshPasskeys(), refreshTokens()]);
  });

  // Refetch passkeys + tokens whenever the SSE stream signals a user-scoped
  // change (admin reset our password from another tab, another tab added a
  // passkey, etc.). Reading the reactive counter inside the effect wires
  // the dependency automatically.
  $effect(() => {
    const _key = reloadKeys.users;
    void _key;
    if (auth.user) {
      void refreshPasskeys();
      void refreshTokens();
    }
  });
</script>

<div class="p-6 max-w-3xl space-y-4">
  <h1 class="text-base font-semibold">User</h1>

  <section class="border rounded p-3" style="border-color: var(--border)">
    <h2 class="text-xs font-semibold mb-1">Account</h2>
    <div class="text-xs mono" style="color: var(--muted)">
      Username: <span style="color: var(--fg)">{auth.user?.username ?? '-'}</span>
    </div>
    <div class="text-xs mono" style="color: var(--muted)">
      Role: <span style="color: var(--fg)">{auth.user?.role ?? '-'}</span>
    </div>
  </section>

  <!-- Change password -->
  <section class="border rounded p-3" style="border-color: var(--border)">
    <h2 class="text-xs font-semibold mb-2">Change password</h2>
    <form onsubmit={submitPassword} class="space-y-2 text-xs max-w-sm">
      <label class="block">
        <span style="color: var(--muted)">Current password</span>
        <input
          bind:value={pwCurrent}
          type="password"
          autocomplete="current-password"
          required
          class="w-full mt-0.5 px-2 py-1 rounded border"
          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
        />
      </label>
      <label class="block">
        <span style="color: var(--muted)">New password</span>
        <input
          bind:value={pwNew}
          type="password"
          autocomplete="new-password"
          required
          minlength="8"
          class="w-full mt-0.5 px-2 py-1 rounded border"
          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
        />
      </label>
      <label class="block">
        <span style="color: var(--muted)">Confirm new password</span>
        <input
          bind:value={pwConfirm}
          type="password"
          autocomplete="new-password"
          required
          minlength="8"
          class="w-full mt-0.5 px-2 py-1 rounded border"
          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
        />
      </label>
      {#if pwErr}<p style="color: var(--latency-bad)">{pwErr}</p>{/if}
      <button
        type="submit"
        disabled={pwBusy}
        class="px-2 py-1 rounded font-medium"
        style="background: var(--btn-bg); color: var(--btn-text); opacity: {pwBusy ? 0.6 : 1}"
      >
        {pwBusy ? 'Updating…' : 'Update password'}
      </button>
    </form>
  </section>

  <!-- Passkeys -->
  <section class="border rounded p-3" style="border-color: var(--border)">
    <h2 class="text-xs font-semibold mb-2">Passkeys</h2>
    <p class="text-[11px] mb-2" style="color: var(--muted)">
      Sign in without a password using a biometric or security key.
      Requires <code>HAZE_ORIGIN</code> to be set on the server.
    </p>
    {#if passkeys.length}
      <table class="w-full text-xs mono mb-2">
        <thead style="color: var(--muted)">
          <tr class="text-left">
            <th class="py-1 pr-2 font-normal">Label</th>
            <th class="py-1 pr-2 font-normal">Created</th>
            <th class="py-1 pr-2 font-normal">Last used</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {#each passkeys as p}
            <tr class="border-t" style="border-color: var(--border)">
              <td class="py-1 pr-2">{p.label ?? '(unlabelled)'}</td>
              <td class="py-1 pr-2" style="color: var(--muted)">{fmtTs(p.created_at)}</td>
              <td class="py-1 pr-2" style="color: var(--muted)">{fmtTs(p.last_used_at)}</td>
              <td class="py-1 text-right">
                <button
                  onclick={() => removePasskey(p.id)}
                  class="px-1 py-0.5"
                  style="color: var(--latency-bad)"
                >
                  remove
                </button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    {:else}
      <p class="text-[11px] mb-2" style="color: var(--muted)">No passkeys registered.</p>
    {/if}

    <div class="flex gap-2 items-center text-xs mt-2">
      <input
        bind:value={passkeyLabel}
        type="text"
        placeholder="Label (optional, e.g. MacBook Touch ID)"
        class="px-2 py-1 rounded border flex-1"
        style="background: var(--bg); border-color: var(--border); color: var(--fg)"
      />
      <button
        type="button"
        onclick={addPasskey}
        disabled={passkeyBusy}
        class="px-2 py-1 rounded font-medium"
        style="background: var(--btn-bg); color: var(--btn-text); opacity: {passkeyBusy ? 0.6 : 1}"
      >
        {passkeyBusy ? 'Adding…' : 'Add passkey'}
      </button>
    </div>
    {#if passkeyMsg}<p class="text-xs mt-1" style="color: var(--latency-good)">{passkeyMsg}</p>{/if}
    {#if passkeyErr}<p class="text-xs mt-1" style="color: var(--latency-bad)">{passkeyErr}</p>{/if}
  </section>

  <!-- API tokens -->
  <section class="border rounded p-3" style="border-color: var(--border)">
    <h2 class="text-xs font-semibold mb-2">Access tokens</h2>
    <p class="text-[11px] mb-2" style="color: var(--muted)">
      Generate a personal access token to authenticate API requests:
      <code>Authorization: Bearer hzt_…</code>. Plaintext is shown <em>once</em>; copy it immediately.
    </p>

    {#if tokenPlaintext}
      <div
        class="rounded border p-2 mb-2 text-xs"
        style="background: rgba(0,200,80,0.08); border-color: var(--latency-good)"
      >
        <div class="font-medium mb-1">
          New token <span class="mono">{tokenPlaintext.name}</span> - copy now, it won't be shown again.
        </div>
        <div class="flex gap-2 items-center">
          <code
            class="flex-1 px-2 py-1 rounded mono break-all"
            style="background: var(--bg); border: 1px solid var(--border)"
          >{tokenPlaintext.plaintext}</code>
          <button
            type="button"
            onclick={() => copyToClipboard(tokenPlaintext!.plaintext)}
            class="px-2 py-1 rounded border"
            style="border-color: var(--border)"
          >
            Copy
          </button>
        </div>
      </div>
    {/if}

    {#if tokens.length}
      <table class="w-full text-xs mono mb-2">
        <thead style="color: var(--muted)">
          <tr class="text-left">
            <th class="py-1 pr-2 font-normal">Name</th>
            <th class="py-1 pr-2 font-normal">Created</th>
            <th class="py-1 pr-2 font-normal">Last used</th>
            <th class="py-1 pr-2 font-normal">Expires</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {#each tokens as t}
            <tr class="border-t" style="border-color: var(--border)">
              <td class="py-1 pr-2">{t.name}</td>
              <td class="py-1 pr-2" style="color: var(--muted)">{fmtTs(t.created_at)}</td>
              <td class="py-1 pr-2" style="color: var(--muted)">{fmtTs(t.last_used_at)}</td>
              <td class="py-1 pr-2" style="color: var(--muted)">{fmtTs(t.expires_at)}</td>
              <td class="py-1 text-right">
                <button
                  onclick={() => revokeToken(t.id)}
                  class="px-1 py-0.5"
                  style="color: var(--latency-bad)"
                >
                  revoke
                </button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    {:else}
      <p class="text-[11px] mb-2" style="color: var(--muted)">No tokens.</p>
    {/if}

    <div class="flex gap-2 items-center text-xs mt-2">
      <input
        bind:value={tokenName}
        type="text"
        placeholder="Token name (e.g. My API Client)"
        class="px-2 py-1 rounded border flex-1"
        style="background: var(--bg); border-color: var(--border); color: var(--fg)"
      />
      <button
        type="button"
        onclick={createToken}
        disabled={tokenBusy}
        class="px-2 py-1 rounded font-medium"
        style="background: var(--btn-bg); color: var(--btn-text); opacity: {tokenBusy ? 0.6 : 1}"
      >
        {tokenBusy ? 'Creating…' : 'Generate token'}
      </button>
    </div>
    {#if tokenErr}<p class="text-xs mt-1" style="color: var(--latency-bad)">{tokenErr}</p>{/if}
  </section>
</div>
