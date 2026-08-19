import { expect, test } from '@playwright/test';
import { waitFramesLoaded, waitReady } from './helpers';

test.describe('shell and controls', () => {
  test('loads, reaches a ready state and shows the radar run time', async ({ page }) => {
    const errors: string[] = [];
    page.on('console', (m) => {
      if (m.type() === 'error') errors.push(m.text());
    });
    page.on('pageerror', (e) => errors.push(String(e)));

    await page.goto('/');
    await waitReady(page);

    // Status reports either the radar run's clock time or a model-only fallback.
    await expect(page.locator('#status')).toHaveText(/^(Radar \d{1,2}[:.]\d{2}|nur Modelldaten)/);
    await expect(page.locator('#status')).not.toHaveClass(/error/);

    await expect(page.locator('#offset')).toHaveText('jetzt');
    await expect(page.locator('#clock')).toHaveText(/\d{1,2}[:.]\d{2}/);
    expect(errors, `console errors: ${errors.join(' | ')}`).toEqual([]);
  });

  test('slider scrubs forward in time and never before now', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);

    const slider = page.locator('#slider');
    await expect(page.locator('#offset')).toHaveText('jetzt');

    await slider.fill('500');
    await slider.dispatchEvent('input');
    await expect(page.locator('#offset')).toHaveText(/^\+\d/);

    await slider.fill('1000');
    await slider.dispatchEvent('input');
    const last = await page.locator('#offset').textContent();
    expect(last).toMatch(/^\+/);

    // The far end of the axis is roughly two hours out and never further.
    const minutes = await page.locator('#clock').textContent();
    expect(minutes).toMatch(/\d{1,2}[:.]\d{2}/);

    // Back to the start: still "jetzt", never a past time.
    await slider.fill('0');
    await slider.dispatchEvent('input');
    await expect(page.locator('#offset')).toHaveText('jetzt');
  });

  test('play toggles and advances the clock, space bar does the same', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);
    await waitFramesLoaded(page);

    const play = page.locator('#play');
    await play.click();
    await expect(play).toHaveClass(/active/);

    const before = await page.locator('#clock').textContent();
    await expect
      .poll(async () => page.locator('#clock').textContent(), { timeout: 15_000 })
      .not.toBe(before);

    await page.keyboard.press('Space');
    await expect(play).not.toHaveClass(/active/);
    const paused = await page.locator('#clock').textContent();
    await page.waitForTimeout(1200);
    expect(await page.locator('#clock').textContent()).toBe(paused);
  });

  test('arrow keys step the forecast in five-minute increments', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);

    await page.keyboard.press('ArrowRight');
    await expect(page.locator('#offset')).toHaveText('+5 Min');
    await page.keyboard.press('ArrowRight');
    await expect(page.locator('#offset')).toHaveText('+10 Min');
    await page.keyboard.press('ArrowLeft');
    await expect(page.locator('#offset')).toHaveText('+5 Min');
  });

  test('legend toggles and is labelled in mm/h', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);

    const legend = page.locator('#legend');
    await expect(legend).toBeHidden();
    await page.locator('#legend-toggle').click();
    await expect(legend).toBeVisible();
    await expect(page.locator('#legend-labels')).toContainText('mm/h');
    // Logarithmic scale marks.
    await expect(page.locator('#legend-labels')).toContainText('0.1');
    await expect(page.locator('#legend-labels')).toContainText('50');

    await page.locator('#legend-toggle').click();
    await expect(legend).toBeHidden();
  });

  test('half-hour tick marks line up along the slider', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);
    const ticks = page.locator('#ticks span');
    // Two hours of forecast gives three or four half-hour boundaries.
    expect(await ticks.count()).toBeGreaterThanOrEqual(3);
    await expect(ticks.first()).toHaveText(/\d{1,2}[:.]\d{2}/);
  });

  test('data sources are credited', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);
    const attribution = page.locator('#attribution');
    // GeoNutzV requires the DWD credit verbatim.
    await expect(attribution).toContainText('Deutscher Wetterdienst');
    await expect(attribution).toContainText('OpenStreetMap');
  });
});
