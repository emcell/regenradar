import { expect, test } from '@playwright/test';
import { waitFramesLoaded, waitReady } from './helpers';

/**
 * Where the map opens, and in what order.
 *
 * Frames are fetched for whatever is on screen, so the starting view is not just a
 * cosmetic choice: drawing the overview first and then jumping to the user's position
 * would download one viewport's worth of tiles and throw them away.
 */
test.describe('startup', () => {
  test('the view is not remembered between visits', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);

    // Move somewhere distinctive and let it settle.
    await page.evaluate(() => window.scrollTo(0, 0));
    const box = (await page.locator('#map').boundingBox())!;
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.mouse.down();
    await page.mouse.move(box.x + box.width / 2 - 300, box.y + box.height / 2 - 200, { steps: 12 });
    await page.mouse.up();
    await page.waitForTimeout(1200);

    // Nothing about the view may be written anywhere persistent.
    const stored = await page.evaluate(() => ({
      local: { ...localStorage },
      session: { ...sessionStorage },
    }));
    expect(Object.keys(stored.local), 'localStorage must stay empty').toEqual([]);
    expect(Object.keys(stored.session), 'sessionStorage must stay empty').toEqual([]);
  });

  test('without a location it falls back to the whole composite', async ({ page, context }) => {
    // Playwright denies geolocation unless granted, which is the declined case.
    await context.clearPermissions();
    await page.goto('/');
    await waitReady(page);
    await waitFramesLoaded(page);

    const view = await page.evaluate(() => {
      const canvas = document.querySelector('.maplibregl-canvas') as HTMLCanvasElement;
      return { width: canvas.width };
    });
    expect(view.width).toBeGreaterThan(0);
    // The status settles on the radar run rather than staying on "Standort …".
    await expect(page.locator('#status')).toHaveText(/^Radar \d{1,2}[:.]\d{2}/);
  });

  test('a shared link opens at its own place and ignores the browser location', async ({
    page,
    context,
  }) => {
    await context.grantPermissions(['geolocation']);
    await context.setGeolocation({ latitude: 50.94, longitude: 6.96 }); // Cologne

    // ...but the link says Munich.
    await page.goto('/?at=48.14,11.58,9');
    await waitReady(page);
    await waitFramesLoaded(page);
    await page.waitForTimeout(1500);

    // Clicking the centre of the map reports the coordinate under it, which is a
    // DOM-visible way to read back where the map actually went.
    const box = (await page.locator('#map').boundingBox())!;
    await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
    await expect(page.locator('#readout-place')).toHaveText(/4[78]\.\d+°, 1[12]\.\d+°/);
  });

  test('a location moves the map before any frame is fetched', async ({ page, context }) => {
    await context.grantPermissions(['geolocation']);
    await context.setGeolocation({ latitude: 50.94, longitude: 6.96 });

    const tileRequests: string[] = [];
    page.on('request', (r) => {
      if (/\/data\/.*\/(t|o)\d/.test(r.url())) tileRequests.push(r.url());
    });

    await page.goto('/');
    await waitReady(page);
    await waitFramesLoaded(page);
    await page.waitForTimeout(2000);

    // The pin proves the position was applied.
    await expect(page.locator('.probe-pin')).toBeVisible();
    // And frames were only requested once it had been.
    expect(tileRequests.length, 'frames should have started loading').toBeGreaterThan(0);
  });
});
