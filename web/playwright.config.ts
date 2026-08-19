import { defineConfig, devices } from '@playwright/test';

/**
 * The suite runs against the real backend serving the real build, hitting live DWD
 * and Open-Meteo data. That is deliberate: the parts most likely to break — RADOLAN
 * decoding, georeferencing, texture upload formats — are exactly the parts a mocked
 * fixture would paper over.
 */
const PORT = Number(process.env.REGENRADAR_PORT ?? 8787);
const BASE = process.env.REGENRADAR_BASE ?? `http://127.0.0.1:${PORT}`;

export default defineConfig({
  testDir: './e2e',
  // Live data and WebGL warm-up are both slower than a unit test.
  timeout: 90_000,
  expect: { timeout: 20_000 },
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 1 : 0,
  reporter: [['list']],
  outputDir: './e2e/.artifacts',
  use: {
    baseURL: BASE,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    // Headless Chromium falls back to SwiftShader, which renders our custom layer
    // correctly, just slowly.
    launchOptions: { args: ['--enable-unsafe-swiftshader'] },
  },
  projects: [
    {
      name: 'desktop',
      use: { ...devices['Desktop Chrome'], viewport: { width: 1280, height: 860 } },
    },
    {
      name: 'mobile',
      use: { ...devices['Pixel 7'] },
    },
  ],
});
