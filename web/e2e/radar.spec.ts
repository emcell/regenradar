import { expect, test } from '@playwright/test';
import {
  canvasSignature,
  canvasStats,
  findWetSpot,
  meanAbsDiff,
  openAt,
  primeAllFrames,
  waitFramesLoaded,
  waitReady,
} from './helpers';

test.describe('precipitation rendering', () => {
  test('the custom WebGL layer is present and draws coloured precipitation', async ({ page }) => {
    // Find real rain first, so the assertion is about rendering rather than weather.
    await page.goto('/?capture');
    await waitReady(page);
    const spot = await findWetSpot(page);
    test.skip(!spot, 'no precipitation anywhere in the radar composite right now');

    await openAt(page, spot!.lng, spot!.lat, 7.5, { capture: true });
    await waitReady(page);
    await waitFramesLoaded(page);
    // Let the basemap tiles and the first frames settle.
    await page.waitForTimeout(3500);

    const stats = await canvasStats(page);
    expect(
      stats.saturatedFraction,
      `only ${(stats.saturatedFraction * 100).toFixed(3)}% saturated pixels (max sat ${stats.maxSaturation.toFixed(2)})`,
    ).toBeGreaterThan(0.002);
  });

  test('scrubbing the timeline changes the picture', async ({ page }) => {
    await page.goto('/?capture');
    await waitReady(page);
    const spot = await findWetSpot(page);
    test.skip(!spot, 'no precipitation available');

    await openAt(page, spot!.lng, spot!.lat, 7, { capture: true });
    await waitReady(page);
    // The comparison needs both ends of the axis resident.
    await primeAllFrames(page);
    await page.waitForTimeout(2000);

    const atNow = await canvasSignature(page);
    await page.locator('#slider').fill('1000');
    await page.locator('#slider').dispatchEvent('input');
    await page.waitForTimeout(1500);
    const atEnd = await canvasSignature(page);

    // Two hours apart, the rain field must look different.
    expect(meanAbsDiff(atNow, atEnd)).toBeGreaterThan(1.0);
  });

  test('motion interpolation produces distinct in-between frames', async ({ page }) => {
    await page.goto('/?capture');
    await waitReady(page);
    const spot = await findWetSpot(page);
    test.skip(!spot, 'no precipitation available');

    await openAt(page, spot!.lng, spot!.lat, 8, { capture: true });
    await waitReady(page);
    await primeAllFrames(page);
    await page.waitForTimeout(2500);

    // Three moments strictly inside one source interval. Derived from the real frame
    // times rather than assumed: the axis is two hours when only the radar is up and
    // six once the model part is stitched on, so a hardcoded slider offset would mean
    // something different in each case.
    const positions = await page.evaluate(async () => {
      const res = await fetch('/graphql', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          query: '{ radar { timeline { start end } layers { frames { time } } } }',
        }),
      });
      const { timeline, layers } = (await res.json()).data.radar;
      const start = Date.parse(timeline.start);
      const end = Date.parse(timeline.end);
      const times = layers[0].frames.map((f: { time: string }) => Date.parse(f.time));

      // The first whole interval that lies after the axis begins. Staying near the
      // start also keeps us where the wet spot was actually wet.
      const i = times.findIndex((t: number, k: number) => t >= start && k + 1 < times.length);
      if (i < 0) return null;
      const [a, b] = [times[i], times[i + 1]];
      const slider = (t: number) => String(Math.round(((t - start) / (end - start)) * 1000));
      return {
        gapMinutes: (b - a) / 60_000,
        at: [0.2, 0.5, 0.8].map((f) => slider(a + (b - a) * f)),
      };
    });
    test.skip(!positions, 'no usable frame interval');
    // Distinct slider positions are a precondition for the comparison to mean anything.
    expect(new Set(positions!.at).size, 'slider resolution too coarse').toBe(3);

    const sample = async (value: string) => {
      const slider = page.locator('#slider');
      await slider.fill(value);
      await slider.dispatchEvent('input');
      await page.waitForTimeout(700);
      return canvasSignature(page);
    };

    const [a, b, c] = [
      await sample(positions!.at[0]),
      await sample(positions!.at[1]),
      await sample(positions!.at[2]),
    ];

    // Without interpolation these three would be pixel-identical: they all sit
    // between the same two source frames.
    expect(meanAbsDiff(a, b), 'sub-frame times should differ').toBeGreaterThan(0.05);
    expect(meanAbsDiff(b, c), 'sub-frame times should differ').toBeGreaterThan(0.05);
  });

  test('tiles and overviews are served immutably and cover the raster', async ({ page }) => {
    await page.goto('/');
    await waitReady(page);

    const result = await page.evaluate(async () => {
      const query = `{ radar { layers { width height frames { index } flowUrls
        tiling { cols rows tileWidth tileHeight overviewWidth overviewHeight
                 tileUrlTemplate overviewUrlTemplate maskUrlTemplate } } } }`;
      const res = await fetch('/graphql', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ query }),
      });
      const l = (await res.json()).data.radar.layers[0];
      const t = l.tiling;
      const fetchImage = async (url: string) => {
        const r = await fetch(url);
        if (!r.ok) return null;
        const bmp = await createImageBitmap(await r.blob());
        const size = { w: bmp.width, h: bmp.height, bytes: (await (await fetch(url)).blob()).size };
        bmp.close();
        return { size, cacheControl: r.headers.get('cache-control'), type: r.headers.get('content-type') };
      };

      const overview = await fetchImage(t.overviewUrlTemplate.replace('{frame}', '00'));
      // Corner tiles: the first is nominal, the last may be clipped.
      const first = await fetchImage(
        t.tileUrlTemplate.replace('{frame}', '00').replace('{x}', '0').replace('{y}', '0'),
      );
      const last = await fetchImage(
        t.tileUrlTemplate
          .replace('{frame}', '00')
          .replace('{x}', String(t.cols - 1))
          .replace('{y}', String(t.rows - 1)),
      );
      const mask = t.maskUrlTemplate
        ? await fetchImage(t.maskUrlTemplate.replace('{x}', '0').replace('{y}', '0'))
        : null;

      return { layer: { width: l.width, height: l.height, frames: l.frames.length, flows: l.flowUrls.length }, t, overview, first, last, mask };
    });

    const { layer, t, overview, first, last, mask } = result;
    expect(layer.frames).toBeGreaterThanOrEqual(25);
    expect(layer.flows).toBe(layer.frames - 1);

    expect(overview!.type).toBe('image/webp');
    expect(overview!.cacheControl).toContain('immutable');
    expect(overview!.size.w).toBe(t.overviewWidth);
    expect(overview!.size.h).toBe(t.overviewHeight);

    expect(first!.size.w).toBe(t.tileWidth);
    expect(first!.size.h).toBe(t.tileHeight);
    // The grid must span the raster: last tile start plus its size is the full width.
    expect((t.cols - 1) * t.tileWidth + last!.size.w).toBe(layer.width);
    expect((t.rows - 1) * t.tileHeight + last!.size.h).toBe(layer.height);

    // The point of tiling: a tile is a fraction of the frame.
    expect(overview!.size.bytes).toBeLessThan(60_000);
    expect(first!.size.bytes).toBeLessThan(120_000);
    if (mask) expect(mask.size.w).toBe(t.tileWidth);
  });

  test('a stale run id is rejected rather than served', async ({ page }) => {
    await page.goto('/');
    const status = await page.evaluate(async () => {
      const res = await fetch('/data/dwd/19990101T0000/o00.webp');
      return res.status;
    });
    expect(status).toBe(404);
  });
});
