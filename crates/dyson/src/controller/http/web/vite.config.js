import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Dev server proxies the dyson HTTP controller running on :7878 so the
// frontend can be iterated on with HMR while talking to a real backend.
// Production build emits to ./dist, which build.rs bakes into the Rust
// binary via include_bytes!.
export default defineConfig({
  plugins: [react()],
  // Vitest runs under jsdom so regression tests can mount components and
  // walk the resulting DOM — source-text greps missed the artefacts-tab
  // black-screen bug five times because they couldn't see what React
  // actually rendered.
  test: {
    environment: 'jsdom',
  },
  server: {
    port: 5173,
    proxy: {
      '/api': { target: 'http://127.0.0.1:7878', changeOrigin: false },
      '/artefacts': { target: 'http://127.0.0.1:7878', changeOrigin: false },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // Deterministic chunk layout — build.rs walks dist/ to generate
    // the Rust asset table, and hashed filenames invalidate the
    // Cargo cache cleanly when sources change.
    rollupOptions: {
      output: {
        entryFileNames: 'assets/[name]-[hash].js',
        chunkFileNames: 'assets/[name]-[hash].js',
        assetFileNames: 'assets/[name]-[hash][extname]',
      },
    },
  },
});
