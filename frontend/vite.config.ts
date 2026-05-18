import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

// No build-time gzip/brotli pre-compression: every text asset must be
// rewritten at serve time to replace SvelteKit's `__HAZE_BASE__`
// placeholder with the configured `HAZE_BASE_URL`, so cached `.gz`/`.br`
// siblings would be stale. The backend wraps the asset branch in
// `CompressionLayer` instead — compressed once at first request, then
// served from the rewrite cache.
export default defineConfig({
  plugins: [tailwindcss(), sveltekit()],
  server: {
    port: 5173,
    proxy: {
      // SvelteKit serves the dev app under `kit.paths.base` (the
      // `/__HAZE_BASE__` sentinel), so browser fetches go to
      // `/__HAZE_BASE__/api/v1/...` and `/__HAZE_BASE__/healthz`. Forward
      // those to the backend after stripping the sentinel — that way
      // `just backend` can stay at root in dev without setting
      // `HAZE_BASE_URL`.
      '/__HAZE_BASE__/api': {
        target: 'http://127.0.0.1:4420',
        changeOrigin: false,
        rewrite: (p) => p.replace('/__HAZE_BASE__', '')
      },
      '/__HAZE_BASE__/healthz': {
        target: 'http://127.0.0.1:4420',
        changeOrigin: false,
        rewrite: (p) => p.replace('/__HAZE_BASE__', '')
      }
    }
  },
  build: {
    target: 'es2022',
    sourcemap: false
  }
});
