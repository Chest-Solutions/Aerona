import { fileURLToPath } from 'node:url'

import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

// One origin in production: `aerona-api` serves `dist/` and `/api/v1/*` from the same
// port, so cookies are same-site and there is no CORS to misconfigure. In dev the API
// is proxied so the same holds.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  // `@/` is what shadcn's generated components import (`@/lib/utils`, `@/components/ui/*`).
  // The alias is declared twice on purpose: tsconfig for `tsc`, vite for the build.
  resolve: { alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) } },
  build: {
    target: 'es2022',
    sourcemap: false,
    assetsInlineLimit: 2048,
    // Vite 8 builds with rolldown, which only accepts a function here; its own
    // chunking already splits react and motion out, so nothing is overridden.
    //
    // One stylesheet, on purpose. With the default `cssCodeSplit`, each lazily loaded chunk gets its
    // own `assets/<chunk>-<hash>.css`, and the runtime reaches it through `__vitePreload`, which
    // injects a `<link>` at route-change time and logs "Unable to preload CSS" when that one request
    // fails. So a single lost 13 KB file cost a guild page its entire styling, silently, with a
    // console line as the only trace - while the JS for the same chunk arrived fine. The styles are
    // 148 KB together, the panel is one page, and a `<link>` in the document is fetched by the
    // browser's own loader: no preload machinery, no second chance to fail, no route that can come up
    // unstyled.
    cssCodeSplit: false,
  },
  ssr: { noExternal: ['framer-motion'] },
  server: {
    host: '0.0.0.0',
    port: 5173,
    strictPort: false,
    proxy: { '/api': { target: 'http://127.0.0.1:8790', changeOrigin: false } },
  },
  preview: { host: '0.0.0.0', port: 4173 },
})
