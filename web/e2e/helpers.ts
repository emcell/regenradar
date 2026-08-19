import type { Page } from '@playwright/test';

export interface WetSpot {
  lng: number;
  lat: number;
  byte: number;
}

/** Wait until the app has applied a backend state. */
export async function waitReady(page: Page): Promise<void> {
  await page.waitForFunction(() => document.body.dataset.state === 'ready', null, {
    timeout: 60_000,
  });
}

/**
 * Wait until at least `count` frames are on the GPU.
 *
 * Deliberately an absolute count rather than a fraction: the stack grew from 25
 * frames to 40-odd when the model part was added, and any fraction that worked for
 * one is wrong for the other.
 */
export async function waitFramesLoaded(page: Page, count = 3): Promise<void> {
  await page.waitForFunction(
    (n) => Number(document.body.dataset.framesReady ?? 0) >= n,
    count,
    { timeout: 60_000 },
  );
}

/**
 * Pull the entire frame stack down and return to "now".
 *
 * Frames load lazily around the playhead, so at rest only a handful are resident.
 * Pressing play is the app's own trigger for fetching the rest; this uses that
 * rather than reaching into internals.
 */
export async function primeAllFrames(page: Page): Promise<void> {
  await page.locator('#play').click();
  await page.waitForFunction(
    () => Number(document.body.dataset.loaded ?? 0) > 0.9,
    null,
    { timeout: 90_000 },
  );
  await page.locator('#play').click();
  const slider = page.locator('#slider');
  await slider.fill('0');
  await slider.dispatchEvent('input');
}

/**
 * Open the app looking at a particular place.
 *
 * Uses the app's own `?at=` parameter rather than a test-only entry point. The view
 * is not persisted between visits, so this is also the only way to reproduce a
 * position.
 */
export async function openAt(
  page: Page,
  lng: number,
  lat: number,
  zoom: number,
  opts: { capture?: boolean } = {},
): Promise<void> {
  const capture = opts.capture ? 'capture&' : '';
  await page.goto(`/?${capture}at=${lat},${lng},${zoom}`);
}

/**
 * Find the wettest pixel in the radar layer's first frame and convert it back to
 * lon/lat, so tests can point the map at somewhere it will actually rain.
 *
 * Doubles as an end-to-end check of the georeferencing: the returned coordinate is
 * derived from the layer bounds the backend advertises.
 */
export async function findWetSpot(page: Page): Promise<WetSpot | null> {
  return page.evaluate(async () => {
    const query = `{ radar { layers { id width height bounds { west south east north }
      tiling { cols rows tileWidth tileHeight overviewUrlTemplate } frames { index } } } }`;
    const res = await fetch('/graphql', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ query }),
    });
    const body = await res.json();
    const layer = body.data.radar.layers[0];
    if (!layer) return null;

    // The overview is enough to find a shower and it is one small request.
    const url = layer.tiling.overviewUrlTemplate.replace('{frame}', '00');
    const blob = await (await fetch(url)).blob();
    const bmp = await createImageBitmap(blob, {
      premultiplyAlpha: 'none',
      colorSpaceConversion: 'none',
    });
    const cv = document.createElement('canvas');
    cv.width = bmp.width;
    cv.height = bmp.height;
    const ctx = cv.getContext('2d', { willReadFrequently: true })!;
    ctx.drawImage(bmp, 0, 0);
    bmp.close();
    const data = ctx.getImageData(0, 0, cv.width, cv.height).data;

    // Prefer a spot with wet neighbours: an isolated bright pixel could vanish at
    // map zoom, a broad shower will not.
    let best = { x: -1, y: -1, score: 0, byte: 0 };
    const step = 3;
    for (let y = step; y < cv.height - step; y += step) {
      for (let x = step; x < cv.width - step; x += step) {
        const c = data[(y * cv.width + x) * 4];
        if (c < 40) continue;
        let score = 0;
        for (let dy = -step; dy <= step; dy += step) {
          for (let dx = -step; dx <= step; dx += step) {
            score += data[((y + dy) * cv.width + (x + dx)) * 4];
          }
        }
        if (score > best.score) best = { x, y, score, byte: c };
      }
    }
    if (best.x < 0) return null;

    // Pixel centre -> web mercator -> lon/lat, using the advertised bounds.
    const R = 6378137;
    const mercX = (lon: number) => (R * lon * Math.PI) / 180;
    const mercY = (lat: number) => R * Math.log(Math.tan(Math.PI / 4 + (lat * Math.PI) / 360));
    const x0 = mercX(layer.bounds.west);
    const x1 = mercX(layer.bounds.east);
    const y0 = mercY(layer.bounds.south);
    const y1 = mercY(layer.bounds.north);

    const mx = x0 + ((best.x + 0.5) / cv.width) * (x1 - x0);
    const my = y1 - ((best.y + 0.5) / cv.height) * (y1 - y0);
    return {
      lng: (mx / R) * (180 / Math.PI),
      lat: (2 * Math.atan(Math.exp(my / R)) - Math.PI / 2) * (180 / Math.PI),
      byte: best.byte,
    };
  });
}

export interface CanvasStats {
  total: number;
  saturated: number;
  saturatedFraction: number;
  maxSaturation: number;
}

/**
 * Measure how much of the map canvas is coloured precipitation.
 *
 * The dark basemap is nearly monochrome, so a high-saturation pixel is a strong
 * signal that the radar layer drew. Requires `?capture` so the drawing buffer is
 * still readable after the frame.
 */
export async function canvasStats(page: Page): Promise<CanvasStats> {
  return page.evaluate(() => {
    const src = document.querySelector('.maplibregl-canvas') as HTMLCanvasElement;
    const cv = document.createElement('canvas');
    cv.width = src.width;
    cv.height = src.height;
    const ctx = cv.getContext('2d', { willReadFrequently: true })!;
    ctx.drawImage(src, 0, 0);
    const d = ctx.getImageData(0, 0, cv.width, cv.height).data;

    let saturated = 0;
    let maxSaturation = 0;
    const total = cv.width * cv.height;
    for (let i = 0; i < d.length; i += 4) {
      const r = d[i];
      const g = d[i + 1];
      const b = d[i + 2];
      const max = Math.max(r, g, b);
      const min = Math.min(r, g, b);
      if (max < 40) continue; // too dark to judge
      const sat = (max - min) / max;
      if (sat > maxSaturation) maxSaturation = sat;
      if (sat > 0.45 && max > 70) saturated++;
    }
    return { total, saturated, saturatedFraction: saturated / total, maxSaturation };
  });
}

/** Pixel-difference ratio between two data URLs of equal size. */
export async function canvasSignature(page: Page): Promise<number[]> {
  return page.evaluate(() => {
    const src = document.querySelector('.maplibregl-canvas') as HTMLCanvasElement;
    // Downsample hard: we care about "did the picture change", not about detail.
    const cv = document.createElement('canvas');
    cv.width = 32;
    cv.height = 32;
    const ctx = cv.getContext('2d', { willReadFrequently: true })!;
    ctx.drawImage(src, 0, 0, 32, 32);
    return Array.from(ctx.getImageData(0, 0, 32, 32).data);
  });
}

export function meanAbsDiff(a: number[], b: number[]): number {
  let sum = 0;
  for (let i = 0; i < a.length; i++) sum += Math.abs(a[i] - b[i]);
  return sum / a.length;
}
