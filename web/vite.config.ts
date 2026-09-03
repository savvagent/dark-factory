import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

/**
 * In production the console and the API are one origin: `df-server` serves the
 * built bundle beside `/api`, `/oauth`, and `/.well-known`.
 *
 * Development has to reproduce that, not merely approximate it. The session
 * cookie carries the `__Host-` prefix, which browsers refuse to store unless
 * the cookie has `Path=/` and **no `Domain`** — so it is bound to whatever
 * origin set it and cannot be sent to a different port. A dev server that
 * pointed `fetch` at `http://localhost:8080` would never send the session, and
 * CORS could not rescue it. Proxying instead keeps every request on the Vite
 * origin, where the cookie lives.
 *
 * `secure: false` only tells the proxy not to verify an upstream TLS
 * certificate; it has nothing to do with the cookie's `Secure` attribute, which
 * browsers honour on `localhost` regardless.
 */
const api = process.env.DF_API_ORIGIN ?? 'http://127.0.0.1:8080';

const proxied = {
  target: api,
  changeOrigin: false,
  secure: false
};

export default defineConfig({
  plugins: [tailwindcss(), sveltekit()],
  server: {
    port: 5173,
    proxy: {
      '/api': proxied,
      '/oauth': proxied,
      '/.well-known': proxied
    }
  }
});
