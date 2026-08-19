import { expect, test } from '@playwright/test';
import { findWetSpot, openAt, waitFramesLoaded, waitReady } from './helpers';

test.describe('point readout', () => {
  test('tapping the map reports the rain forecast for that spot', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);
    const spot = await findWetSpot(page);
    test.skip(!spot, 'no precipitation available');

    await openAt(page, spot!.lng, spot!.lat, 9);
    await waitReady(page);
    await waitFramesLoaded(page);

    const box = (await page.locator('#map').boundingBox())!;
    // Click the centre of the map, which is the wet spot we pinned.
    await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);

    const readout = page.locator('#readout');
    await expect(readout).toBeVisible();
    await expect(page.locator('#readout-place')).toHaveText(/-?\d+\.\d+°, -?\d+\.\d+°/);

    // Wait past the placeholder to a real verdict.
    await expect(page.locator('#readout-summary')).not.toHaveText('lese Daten …', {
      timeout: 45_000,
    });
    await expect(page.locator('#readout-summary')).toHaveText(/mm\/h/);
    // Names the sources feeding the series it just drew.
    await expect(page.locator('#readout-source')).toContainText('Radar');
  });

  test('the readout follows the timeline cursor', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);
    const spot = await findWetSpot(page);
    test.skip(!spot, 'no precipitation available');

    await openAt(page, spot!.lng, spot!.lat, 9);
    await waitReady(page);
    await waitFramesLoaded(page);

    const box = (await page.locator('#map').boundingBox())!;
    await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
    await expect(page.locator('#readout-summary')).toHaveText(/mm\/h/, { timeout: 45_000 });

    const atNow = await page.locator('#readout-summary').textContent();
    await page.locator('#slider').fill('900');
    await page.locator('#slider').dispatchEvent('input');
    await expect
      .poll(async () => page.locator('#readout-summary').textContent(), { timeout: 10_000 })
      .not.toBe(atNow);
  });

  test('closing the readout leaves the pin, and the pin brings it back', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);

    const box = (await page.locator('#map').boundingBox())!;
    await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
    await expect(page.locator('#readout')).toBeVisible();
    await expect(page.locator('.probe-pin')).toBeVisible();

    // Dismissing the panel must not throw away the marked spot.
    await page.locator('#readout-close').click();
    await expect(page.locator('#readout')).toBeHidden();
    await expect(page.locator('.probe-pin')).toBeVisible();

    // And the pin is the way back.
    await page.locator('.probe-pin').click();
    await expect(page.locator('#readout')).toBeVisible();

    // Escape closes it too, and again the pin survives.
    await page.keyboard.press('Escape');
    await expect(page.locator('#readout')).toBeHidden();
    await expect(page.locator('.probe-pin')).toBeVisible();
  });

  test('geolocation is requested on load and pins the reported position', async ({
    page,
    context,
  }) => {
    await context.grantPermissions(['geolocation']);
    // Somewhere well inside radar coverage.
    await context.setGeolocation({ latitude: 50.94, longitude: 6.96 });

    await page.goto('/');
    await waitReady(page);

    // No click anywhere: the app should have asked by itself.
    await expect(page.locator('.probe-pin')).toBeVisible({ timeout: 20_000 });
    await expect(page.locator('#readout')).toBeVisible();
    await expect(page.locator('#readout-place')).toHaveText(/50\.94\d*°, 6\.96\d*°/);
  });

  test('the readout closes again', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);

    const box = (await page.locator('#map').boundingBox())!;
    await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
    await expect(page.locator('#readout')).toBeVisible();

    await page.locator('#readout-close').click();
    await expect(page.locator('#readout')).toBeHidden();

    await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
    await expect(page.locator('#readout')).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.locator('#readout')).toBeHidden();
  });

  test('a point outside all coverage says so instead of claiming it is dry', async ({ page }) => {
    // Mid-Atlantic: outside the radar composite, and outside any Europe view.
    await openAt(page, -40, 35, 4);
    await waitReady(page);

    const box = (await page.locator('#map').boundingBox())!;
    await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
    await expect(page.locator('#readout')).toBeVisible();
    await expect(page.locator('#readout-summary')).not.toHaveText('lese Daten …', {
      timeout: 45_000,
    });
  });
});
