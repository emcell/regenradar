import { expect, test } from '@playwright/test';
import { waitReady } from './helpers';

test.describe('PWA installability', () => {
  test('manifest declares an installable standalone app', async ({ page, request }) => {
    await page.goto('/');
    const href = await page.locator('link[rel="manifest"]').getAttribute('href');
    expect(href).toBeTruthy();

    const res = await request.get(href!);
    expect(res.ok()).toBe(true);
    const manifest = await res.json();

    expect(manifest.name).toBe('Regenradar');
    expect(manifest.display).toBe('standalone');
    expect(manifest.start_url).toBeTruthy();
    expect(manifest.theme_color).toBeTruthy();

    // Chrome requires a 192 and a 512 icon; a maskable one is what keeps the
    // Android launcher from framing it in a white box.
    const sizes = manifest.icons.map((i: any) => i.sizes);
    expect(sizes).toContain('192x192');
    expect(sizes).toContain('512x512');
    expect(manifest.icons.some((i: any) => i.purpose === 'maskable')).toBe(true);

    for (const icon of manifest.icons) {
      const iconRes = await request.get(icon.src);
      expect(iconRes.ok(), `${icon.src} must exist`).toBe(true);
      expect(iconRes.headers()['content-type']).toContain('png');
    }
  });

  test('iOS home-screen metadata is present', async ({ page, request }) => {
    await page.goto('/');
    await expect(page.locator('meta[name="apple-mobile-web-app-capable"]')).toHaveAttribute(
      'content',
      'yes',
    );
    await expect(page.locator('meta[name="apple-mobile-web-app-title"]')).toHaveAttribute(
      'content',
      'Regenradar',
    );
    const touchIcon = await page.locator('link[rel="apple-touch-icon"]').getAttribute('href');
    expect(touchIcon).toBeTruthy();
    expect((await request.get(touchIcon!)).ok()).toBe(true);
  });

  test('the service worker registers and takes control', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);

    const registered = await page.evaluate(async () => {
      if (!('serviceWorker' in navigator)) return false;
      const reg = await navigator.serviceWorker.getRegistration();
      return !!reg;
    });
    expect(registered).toBe(true);

    await page.waitForFunction(() => !!navigator.serviceWorker.controller, null, {
      timeout: 30_000,
    });
  });

  test('the app shell still renders after a reload with the worker in control', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);
    await page.waitForFunction(() => !!navigator.serviceWorker.controller, null, {
      timeout: 30_000,
    });

    await page.reload();
    await waitReady(page);
    await expect(page.locator('#slider')).toBeVisible();
    await expect(page.locator('#play')).toBeVisible();
  });

  test('a bundle from an older build 404s instead of returning the HTML shell', async ({
    request,
  }) => {
    // The SPA fallback used to answer every unknown path with index.html. A client
    // holding a cached entry point from a previous build would then receive HTML
    // where it expected JavaScript, fail to parse it, and show a blank map — a much
    // worse failure than an honest 404.
    const res = await request.get('/assets/index-FromAnOlderBuild.js');
    expect(res.status()).toBe(404);
    expect(res.headers()['content-type'] ?? '').not.toContain('html');

    for (const path of ['/assets/style-gone.css', '/icons/missing.png', '/sw-old.js']) {
      expect((await request.get(path)).status(), `${path} must 404`).toBe(404);
    }
  });

  test('unknown routes still get the app shell', async ({ request }) => {
    const res = await request.get('/some/deep/link');
    expect(res.status()).toBe(200);
    expect(res.headers()['content-type']).toContain('html');
  });

  test('the entry point revalidates while hashed assets are immutable', async ({
    page,
    request,
  }) => {
    // Without this the browser can pin a stale index.html and never see a new build.
    for (const path of ['/', '/sw.js', '/manifest.webmanifest']) {
      const cc = (await request.get(path)).headers()['cache-control'] ?? '';
      expect(cc, `${path} must revalidate`).toContain('no-cache');
    }

    await page.goto('/');
    const bundle = await page.evaluate(() =>
      [...document.scripts].map((s) => new URL(s.src).pathname).find((p) => p.startsWith('/assets/')),
    );
    expect(bundle).toBeTruthy();
    const cc = (await request.get(bundle!)).headers()['cache-control'] ?? '';
    expect(cc).toContain('immutable');
  });

  test('probe endpoints are never cached', async ({ request }) => {
    // A cached readiness answer routes traffic at an instance that has nothing to
    // serve, and a cached liveness answer hides a dead one.
    for (const path of ['/healthz', '/readyz']) {
      const res = await request.get(path);
      expect(res.headers()['cache-control'], `${path} must not be cached`).toContain('no-store');
    }
    // Readiness reports whether there is actually a forecast, not just a process.
    expect((await request.get('/readyz')).status()).toBe(200);
  });

  test('frame tiles keep their immutable caching', async ({ request }) => {
    // Their URLs carry the run id, so the bytes behind one never change. Losing this
    // would make every five-minute run re-download what the browser already has.
    const res = await request.post('/graphql', {
      data: { query: '{ radar { layers { frames { index } tiling { tileUrlTemplate overviewUrlTemplate } } } }' },
    });
    const l = (await res.json()).data.radar.layers[0];
    const urls = [
      l.tiling.overviewUrlTemplate.replace('{frame}', '00'),
      l.tiling.tileUrlTemplate.replace('{frame}', '00').replace('{x}', '0').replace('{y}', '0'),
    ];
    for (const url of urls) {
      const r = await request.get(url);
      expect(r.headers()['cache-control'], `${url}`).toContain('immutable');
      expect(r.headers()['content-type']).toBe('image/webp');
    }
  });

  test('the viewport is set up for full-bleed mobile use', async ({ page }) => {
    await page.goto('/');
    const viewport = await page.locator('meta[name="viewport"]').getAttribute('content');
    expect(viewport).toContain('width=device-width');
    // Needed for the safe-area insets the control bar relies on.
    expect(viewport).toContain('viewport-fit=cover');
  });

  test('controls stay on screen and reachable on a small viewport', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);

    const size = page.viewportSize()!;
    for (const id of ['#slider', '#play', '#locate', '#legend-toggle']) {
      const box = (await page.locator(id).boundingBox())!;
      expect(box, `${id} must be laid out`).toBeTruthy();
      expect(box.y).toBeGreaterThanOrEqual(0);
      expect(box.y + box.height).toBeLessThanOrEqual(size.height + 1);
      expect(box.x).toBeGreaterThanOrEqual(0);
      expect(box.x + box.width).toBeLessThanOrEqual(size.width + 1);
    }

    // Tap targets big enough for a thumb.
    for (const id of ['#play', '#locate', '#legend-toggle']) {
      const box = (await page.locator(id).boundingBox())!;
      expect(Math.min(box.width, box.height), `${id} tap target`).toBeGreaterThanOrEqual(40);
    }

    // The page itself must not scroll; the map owns the gestures.
    const scrolls = await page.evaluate(
      () => document.documentElement.scrollHeight > window.innerHeight + 1,
    );
    expect(scrolls).toBe(false);
  });
});
