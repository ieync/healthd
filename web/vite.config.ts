import { defineConfig } from 'vitest/config';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// The production build is embedded into the daemon binary (src/web.rs), which
// serves it from a static table. That table cannot know content hashes, so the
// output file names below are fixed. src/web.rs has a test that fails if
// index.html ever references a file the table does not contain.
export default defineConfig({
  plugins: [svelte()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2022',
    // One stylesheet, one script: two requests plus the page, no waterfalls.
    cssCodeSplit: false,
    sourcemap: false,
    // Never inline assets as data: URLs: the daemon's CSP only allows them
    // for images, and we have none.
    assetsInlineLimit: 0,
    rolldownOptions: {
      output: {
        entryFileNames: 'assets/app.js',
        chunkFileNames: 'assets/[name].js',
        assetFileNames: (info) =>
          info.names.some((n) => n.endsWith('.css')) ? 'assets/app.css' : 'assets/[name][extname]',
      },
    },
  },
  server: {
    // `npm run dev` against a daemon that is already running. changeOrigin
    // rewrites the Host header to the target's: the daemon rejects any other
    // Host (its DNS-rebinding defence), including the dev server's own.
    proxy: { '/api': { target: 'http://127.0.0.1:8808', changeOrigin: true } },
  },
  test: {
    environment: 'node',
    include: ['src/**/*.test.ts'],
  },
});
