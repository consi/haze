import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/kit').Config} */
const config = {
  preprocess: vitePreprocess(),
  kit: {
    // Sentinel base path. SvelteKit bakes this into every emitted HTML
    // asset URL and the `base` export from `$app/paths`. The Rust server's
    // static-asset handler rewrites the literal `__HAZE_BASE__` to the
    // configured `HAZE_BASE_URL` (empty string in root mode) at serve
    // time, so the same compiled bundle can deploy under any URL path
    // without a rebuild.
    paths: { base: '/__HAZE_BASE__' },
    adapter: adapter({
      pages: 'build',
      assets: 'build',
      fallback: 'index.html',
      precompress: false,
      strict: false
    }),
    files: {
      assets: 'static'
    }
  }
};

export default config;
