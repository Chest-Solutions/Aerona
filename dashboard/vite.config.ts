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
