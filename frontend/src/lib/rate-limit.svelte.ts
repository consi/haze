// Reactive throttling state surfaced by the API client when a request
// hits the per-IP rate limit. The api.ts `req()` helper updates this
// whenever a 429 lands and clears it once the retry completes (success or
// final failure). Layout components can render a banner / inline notice
// based on it so the user knows why the page paused.

export const rateLimitState = $state<{
  throttled: boolean;
  /** Epoch ms when the throttled request will retry. */
  retryAtMs: number | null;
  message: string | null;
}>({
  throttled: false,
  retryAtMs: null,
  message: null
});

export function noteThrottled(retryAfterSecs: number) {
  rateLimitState.throttled = true;
  rateLimitState.retryAtMs = Date.now() + retryAfterSecs * 1000;
  rateLimitState.message = `Rate limited; retrying in ${retryAfterSecs}s`;
}

export function clearThrottled() {
  rateLimitState.throttled = false;
  rateLimitState.retryAtMs = null;
  rateLimitState.message = null;
}
