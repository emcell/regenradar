import { expect, test } from '@playwright/test';
import { waitReady } from './helpers';

/**
 * The timeline is fed by two sources that are deliberately *not* blended: radar
 * extrapolation out to +2 h, then the ICON-D2 model. These tests are about that
 * handover being both correct and visible.
 */
test.describe('two sources, one timeline', () => {
  test('the axis reaches past the radar and is split into segments', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);

    const t = await page.evaluate(async () => {
      const res = await fetch('/graphql', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          query:
            '{ radar { serverTime timeline { start end segments { origin start end label run } } } }',
        }),
      });
      const { radar } = (await res.json()).data;
      return {
        minutes: (Date.parse(radar.timeline.end) - Date.parse(radar.timeline.start)) / 60_000,
        startsInPast: Date.parse(radar.timeline.start) < Date.parse(radar.serverTime) - 90_000,
        segments: radar.timeline.segments,
      };
    });

    expect(t.startsInPast, 'the timeline must not start before now').toBe(false);
    // Radar alone is two hours; with the model it reaches the configured six.
    expect(t.minutes).toBeGreaterThan(110);
    expect(t.minutes).toBeLessThanOrEqual(6 * 60 + 5);

    expect(t.segments.length).toBeGreaterThanOrEqual(1);
    expect(t.segments[0].origin).toBe('RADAR');

    if (t.segments.length === 1) {
      test.info().annotations.push({ type: 'note', description: 'model part unavailable' });
      return;
    }

    const [radar, model] = t.segments;
    expect(model.origin).toBe('MODEL');
    expect(model.label).toBe('Modell');
    expect(radar.label).toBe('Radar');
    // A hard handover: the model starts strictly after the radar's last frame, and
    // the two never overlap.
    expect(Date.parse(model.start)).toBeGreaterThan(Date.parse(radar.end));
    // The radar reaches about two hours.
    const radarMinutes = (Date.parse(radar.end) - Date.parse(radar.start)) / 60_000;
    expect(radarMinutes).toBeGreaterThan(100);
    expect(radarMinutes).toBeLessThanOrEqual(125);
  });

  test('frames carry their origin and never interleave', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);

    const l = await page.evaluate(async () => {
      const res = await fetch('/graphql', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          query: '{ radar { layers { frames { time origin } flowUrls } } }',
        }),
      });
      return (await res.json()).data.radar.layers[0];
    });

    expect(l.frames.length).toBeGreaterThanOrEqual(25);
    expect(l.flowUrls.length).toBe(l.frames.length - 1);

    // Strictly increasing in time.
    for (let i = 1; i < l.frames.length; i++) {
      expect(Date.parse(l.frames[i].time)).toBeGreaterThan(Date.parse(l.frames[i - 1].time));
    }
    // All radar, then all model — one transition, never back.
    const changes = l.frames.filter(
      (f: any, i: number) => i > 0 && f.origin !== l.frames[i - 1].origin,
    );
    expect(changes.length).toBeLessThanOrEqual(1);
    expect(l.frames[0].origin).toBe('RADAR');
    if (changes.length === 1) {
      expect(changes[0].origin).toBe('MODEL');
      expect(l.frames[l.frames.length - 1].origin).toBe('MODEL');
    }
  });

  test('the slider is coloured by source and the badge follows the playhead', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);

    const badge = page.locator('#origin');
    await expect(badge).toBeVisible();
    await expect(badge).toHaveText('Radar');

    const track = () =>
      page.evaluate(() =>
        getComputedStyle(document.querySelector('#slider') as HTMLElement).getPropertyValue('--track'),
      );
    const atNow = await track();
    expect(atNow, 'the radar stretch must be painted').toContain('#49b7ff');

    const segments = await page.evaluate(async () => {
      const res = await fetch('/graphql', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ query: '{ radar { timeline { segments { origin } } } }' }),
      });
      return (await res.json()).data.radar.timeline.segments;
    });
    test.skip(segments.length < 2, 'model part unavailable');

    // The model colour must appear on the track too.
    expect(atNow, 'the model stretch must be painted').toContain('#a78bfa');
    // No neutral sliver between them: the handover is an edge, not a gap.
    const bands = atNow.split('rgba(255,255,255,0.14)').length - 1;
    expect(bands, 'segments must abut').toBeLessThanOrEqual(2);

    // Scrub well past the radar's two hours.
    const slider = page.locator('#slider');
    await slider.fill('900');
    await slider.dispatchEvent('input');
    await expect(badge).toHaveText('Modell');
    await expect(page.locator('#offset')).toHaveText(/^\+\d/);

    // Back to the start.
    await slider.fill('0');
    await slider.dispatchEvent('input');
    await expect(badge).toHaveText('Radar');
  });

  test('both sources are named under the slider', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);
    const sources = page.locator('#sources');
    await expect(sources).toContainText('Radar');
    await expect(sources).toContainText(/bis \d{1,2}[:.]\d{2}/);

    const chips = await sources.locator('span').count();
    expect(chips).toBeGreaterThanOrEqual(1);
    if (chips > 1) {
      await expect(sources).toContainText('ICON-D2');
    }
  });

  test('model frames decode at the same size as radar frames', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);

    const sizes = await page.evaluate(async () => {
      const res = await fetch('/graphql', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          query: `{ radar { layers { width height frames { index origin }
            tiling { overviewUrlTemplate overviewWidth overviewHeight } } } }`,
        }),
      });
      const l = (await res.json()).data.radar.layers[0];
      const first = (o: string) => l.frames.find((f: any) => f.origin === o);
      const measure = async (f: any) => {
        if (!f) return null;
        const url = l.tiling.overviewUrlTemplate.replace(
          '{frame}',
          String(f.index).padStart(2, '0'),
        );
        const bmp = await createImageBitmap(await (await fetch(url)).blob());
        const size = { w: bmp.width, h: bmp.height };
        bmp.close();
        return size;
      };
      return {
        declared: { w: l.tiling.overviewWidth, h: l.tiling.overviewHeight },
        radar: await measure(first('RADAR')),
        model: await measure(first('MODEL')),
      };
    });

    // Both sources are resampled into the same raster and cut with the same grid,
    // which is what lets the client treat them as one continuous stack.
    expect(sizes.radar).toEqual(sizes.declared);
    if (sizes.model) expect(sizes.model).toEqual(sizes.declared);
  });
});
