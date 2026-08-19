import { defineConfig } from 'vite';
import { VitePWA } from 'vite-plugin-pwa';

/** Where the Rust backend listens in development. */
const API = process.env.REGENRADAR_API ?? 'http://127.0.0.1:8787';

export default defineConfig({
  server: {
    port: 5173,
    // Same-origin in production, so proxy in dev to keep the code path identical.
    proxy: {
      '/graphql': { target: API, changeOrigin: true },
      '/data': { target: API, changeOrigin: true },
      '/healthz': { target: API, changeOrigin: true },
    },
  },
  build: {
    target: 'es2022',
    sourcemap: true,
  },
  // MapLibre starts its worker with `{ type: 'module' }`.
  worker: { format: 'es' },
  plugins: [
    VitePWA({
      registerType: 'autoUpdate',
      includeAssets: ['icons/apple-touch-icon.png', 'icons/favicon.png'],
      manifest: {
        name: 'Regenradar',
        short_name: 'Regenradar',
        description: 'Werbefreies Regenradar mit Vorhersage für die nächsten zwei Stunden.',
        lang: 'de',
        theme_color: '#0b1017',
        background_color: '#0b1017',
        display: 'standalone',
        orientation: 'any',
        start_url: '/',
        scope: '/',
        categories: ['weather', 'utilities'],
        icons: [
          { src: '/icons/icon-192.png', sizes: '192x192', type: 'image/png' },
          { src: '/icons/icon-512.png', sizes: '512x512', type: 'image/png' },
          {
            src: '/icons/icon-maskable-512.png',
            sizes: '512x512',
            type: 'image/png',
            purpose: 'maskable',
          },
        ],
      },
      workbox: {
        // MapLibre's worker bundle is large; the default 2 MB limit would skip it.
        maximumFileSizeToCacheInBytes: 6 * 1024 * 1024,
        globPatterns: ['**/*.{js,mjs,css,html,png,svg,webp,woff2}'],
        navigateFallbackDenylist: [/^\/graphql/, /^\/data\//, /^\/healthz/],
        runtimeCaching: [
          {
            // Frame PNGs are immutable — their run id is in the path — but they turn
            // over every five minutes, so the cache is capped and short-lived to
            // keep it from growing without bound.
            urlPattern: /\/data\/.*\.webp$/,
            handler: 'CacheFirst',
            options: {
              cacheName: 'radar-frames',
              expiration: { maxEntries: 240, maxAgeSeconds: 60 * 60 },
              cacheableResponse: { statuses: [0, 200] },
            },
          },
          {
            urlPattern: /^https:\/\/tiles\.openfreemap\.org\/.*/,
            handler: 'StaleWhileRevalidate',
            options: {
              cacheName: 'basemap',
              expiration: { maxEntries: 800, maxAgeSeconds: 60 * 60 * 24 * 14 },
              cacheableResponse: { statuses: [0, 200] },
            },
          },
        ],
      },
      devOptions: { enabled: false },
    }),
  ],
});
