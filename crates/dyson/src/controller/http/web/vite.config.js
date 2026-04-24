import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Bundled CSS is ~38 KiB, and Vite emits it as a <link rel="stylesheet">
// in <head> — that's render-blocking on mobile slow-4G for ~600 ms.
// Inline it into the HTML instead: the stylesheet arrives in the same
// response as the document, FCP stops waiting for the CSS round-trip,
// and the font @font-face URLs (already rewritten to /assets/… by
// Vite's asset pipeline) still resolve because they're absolute paths.
// The trade-off is no shared cache between pages, but this is a
// single-page SPA so there's nothing to share.
function inlineCss() {
  return {
    name: 'dyson-inline-css',
    apply: 'build',
    enforce: 'post',
    transformIndexHtml: {
      order: 'post',
      handler(html, ctx) {
        if (!ctx || !ctx.bundle) return html;
        let out = html;
        for (const [fileName, chunk] of Object.entries(ctx.bundle)) {
          if (chunk.type !== 'asset' || !fileName.endsWith('.css')) continue;
          const source = typeof chunk.source === 'string'
            ? chunk.source
            : Buffer.from(chunk.source).toString('utf8');
          const escaped = fileName.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
          const linkRe = new RegExp(
            `\\s*<link[^>]*href="[^"]*${escaped}"[^>]*>`,
            'g');
          if (linkRe.test(out)) {
            out = out.replace(linkRe, `<style>${source}</style>`);
            delete ctx.bundle[fileName];
          }
        }
        return out;
      },
    },
  };
}

// Dev server proxies the dyson HTTP controller running on :7878 so the
// frontend can be iterated on with HMR while talking to a real backend.
// Production build emits to ./dist, which build.rs bakes into the Rust
// binary via include_bytes!.
export default defineConfig({
  plugins: [react(), inlineCss()],
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
