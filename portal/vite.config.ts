/// <reference types="vitest/config" />
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// The server embeds dist/ and serves it at /admin/ (SPA fallback to index.html).
// `npm run dev` proxies /api to a local rconn-signaling; `npm run dev:mock` needs no server.
export default defineConfig({
  base: '/admin/',
  plugins: [react()],
  build: { outDir: 'dist', emptyOutDir: true, sourcemap: false },
  server: {
    port: 5180,
    strictPort: true,
    proxy: { '/api': { target: process.env.RCONN_API ?? 'http://127.0.0.1:8080' } },
  },
  preview: { port: 5181, strictPort: true },
  test: { environment: 'node', include: ['tests/**/*.test.{ts,tsx}'] },
});
