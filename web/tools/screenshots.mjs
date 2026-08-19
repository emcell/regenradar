/**
 * Regenerate the README screenshots against a running backend.
 *
 * Usage:  node tools/screenshots.mjs [baseURL]
 *
 * Finds the wettest spot in the current radar composite so the shots always show
 * actual weather, rather than whatever happens to be over Germany today.
 */
import { chromium, devices } from '@playwright/test';

const BASE = process.argv[2] ?? 'http://127.0.0.1:8787';
const OUT = new URL('../../shots/', import.meta.url).pathname;

const browser = await chromium.launch({ args: ['--enable-unsafe-swiftshader'] });

async function ready(page) {
  await page.waitForFunction(() => document.body.dataset.state === 'ready', null, { timeout: 90_000 });
}

/** Press play so the whole stack downloads, then return to "now". */
async function primeFrames(page) {
  await page.locator('#play').click();
  await page.waitForFunction(() => Number(document.body.dataset.loaded || 0) > 0.9, null, {
    timeout: 120_000,
  });
  await page.locator('#play').click();
  await page.evaluate(() => {
    const s = document.querySelector('#slider');
    s.value = '0';
    s.dispatchEvent(new Event('input', { bubbles: true }));
  });
  await page.waitForTimeout(3000);
}

/** The app reads `?at=` once at startup and never writes it back. */
function viewUrl(base, lng, lat, zoom) {
  return `${base}/?capture&at=${lat},${lng},${zoom}`;
}

// --- locate real precipitation ---
const scout = await browser.newPage({ viewport: { width: 1280, height: 860 } });
await scout.goto(`${BASE}/?capture`);
await ready(scout);
const wet = await scout.evaluate(async () => {
  const res = await fetch('/graphql', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      query: `{ radar { layers { id width height bounds { west south east north }
        tiling { overviewUrlTemplate } frames { index } } } }`,
    }),
  });
  const layer = (await res.json()).data.radar.layers.find((l) => l.id === 'dwd');
  if (!layer) return null;
  const url = layer.tiling.overviewUrlTemplate.replace('{frame}', '00');
  const bmp = await createImageBitmap(await (await fetch(url)).blob());
  const cv = document.createElement('canvas');
  cv.width = bmp.width;
  cv.height = bmp.height;
  const ctx = cv.getContext('2d', { willReadFrequently: true });
  ctx.drawImage(bmp, 0, 0);
  bmp.close();
  const d = ctx.getImageData(0, 0, cv.width, cv.height).data;
  let best = { x: 0, y: 0, s: 0 };
  for (let y = 6; y < cv.height - 6; y += 3) {
    for (let x = 6; x < cv.width - 6; x += 3) {
      let s = 0;
      for (let dy = -6; dy <= 6; dy += 3) for (let dx = -6; dx <= 6; dx += 3) s += d[((y + dy) * cv.width + (x + dx)) * 4];
      if (s > best.s) best = { x, y, s };
    }
  }
  const R = 6378137;
  const mx = (lon) => (R * lon * Math.PI) / 180;
  const my = (lat) => R * Math.log(Math.tan(Math.PI / 4 + (lat * Math.PI) / 360));
  const x0 = mx(layer.bounds.west), x1 = mx(layer.bounds.east);
  const y0 = my(layer.bounds.south), y1 = my(layer.bounds.north);
  const X = x0 + ((best.x + 0.5) / cv.width) * (x1 - x0);
  const Y = y1 - ((best.y + 0.5) / cv.height) * (y1 - y0);
  return {
    lng: (X / R) * (180 / Math.PI),
    lat: (2 * Math.atan(Math.exp(Y / R)) - Math.PI / 2) * (180 / Math.PI),
  };
});
await scout.close();
console.log('wettest spot:', wet);

const shots = [
  { name: 'germany', lng: 10.4, lat: 51.0, zoom: 5.4, legend: true },
  // Scrubbed into the model stretch, so the slider shows both provenances.
  { name: 'timeline-model', lng: 10.4, lat: 51.0, zoom: 5.4, slider: '900' },
];
if (wet) shots.push({ name: 'rain-zoom', lng: wet.lng, lat: wet.lat, zoom: 8 });

for (const s of shots) {
  const page = await browser.newPage({ viewport: { width: 1280, height: 860 } });
  await page.goto(viewUrl(BASE, s.lng, s.lat, s.zoom));
  await ready(page);
  await primeFrames(page);
  if (s.legend) {
    await page.locator('#legend-toggle').click();
    await page.waitForTimeout(400);
  }
  if (s.slider) {
    await page.evaluate((v) => {
      const el = document.querySelector('#slider');
      el.value = v;
      el.dispatchEvent(new Event('input', { bubbles: true }));
    }, s.slider);
    await page.waitForTimeout(2500);
  }
  await page.screenshot({ path: `${OUT}${s.name}.png` });
  console.log('wrote', s.name);
  await page.close();
}

if (wet) {
  const page = await browser.newPage({ ...devices['Pixel 7'] });
  await page.goto(viewUrl(BASE, wet.lng, wet.lat, 8));
  await ready(page);
  await primeFrames(page);
  const box = await page.locator('#map').boundingBox();
  await page.mouse.click(box.width / 2, box.height / 2);
  await page.waitForTimeout(6000);
  await page.screenshot({ path: `${OUT}mobile.png` });
  console.log('wrote mobile');
  await page.close();
}

await browser.close();
