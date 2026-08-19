/**
 * Point readout: "is it going to rain *here*, and when".
 *
 * The frame data is already on the GPU, but reading it back per pixel would mean a
 * render pass per frame. Instead the one tile containing the point is fetched — a
 * sixteenth of a frame — and decoded with a one-pixel crop rectangle, which
 * `createImageBitmap` supports directly. Those tiles are usually in the HTTP cache
 * already, because the map just drew them.
 */

import { MercatorCoordinate } from 'maplibre-gl';
import { decodeIntensity, type IntensityScale } from './api';
import type { FrameStore } from './frames';
import { mercBox } from './radarLayer';

export interface ProbeSample {
  time: number;
  mmh: number;
}

export interface ProbeResult {
  /** Layer that supplied the values. */
  layerLabel: string;
  layerId: string;
  samples: ProbeSample[];
}

let scratch: OffscreenCanvas | HTMLCanvasElement | null = null;
let scratchCtx: OffscreenCanvasRenderingContext2D | CanvasRenderingContext2D | null = null;

function ctx() {
  if (!scratchCtx) {
    scratch =
      typeof OffscreenCanvas !== 'undefined'
        ? new OffscreenCanvas(1, 1)
        : Object.assign(document.createElement('canvas'), { width: 1, height: 1 });
    scratchCtx = (scratch as HTMLCanvasElement).getContext('2d', {
      willReadFrequently: true,
    }) as CanvasRenderingContext2D;
  }
  return scratchCtx!;
}

/** Read the red channel of a single pixel out of a PNG blob. */
async function readPixel(blob: Blob, x: number, y: number): Promise<number | null> {
  try {
    const bmp = await createImageBitmap(blob, x, y, 1, 1, {
      premultiplyAlpha: 'none',
      colorSpaceConversion: 'none',
    });
    const c = ctx();
    c.clearRect(0, 0, 1, 1);
    c.drawImage(bmp as CanvasImageSource, 0, 0);
    bmp.close();
    return c.getImageData(0, 0, 1, 1).data[0];
  } catch {
    return null;
  }
}

/** Pixel coordinates of `lng`/`lat` in a store's raster, or null if outside. */
function pixelFor(store: FrameStore, lng: number, lat: number): { x: number; y: number } | null {
  const m = MercatorCoordinate.fromLngLat({ lng, lat });
  const [w, n, e, s] = mercBox(store.layer);
  const u = (m.x - w) / (e - w);
  const v = (m.y - n) / (s - n);
  if (u < 0 || u > 1 || v < 0 || v > 1) return null;
  return {
    x: Math.min(store.layer.width - 1, Math.max(0, Math.floor(u * store.layer.width))),
    y: Math.min(store.layer.height - 1, Math.max(0, Math.floor(v * store.layer.height))),
  };
}

/**
 * Sample the precipitation time series at one location.
 *
 * `stores` is searched from the end, so the sharpest layer wins where it has
 * coverage; a masked-out point falls through to the continental layer instead of
 * silently reporting "dry".
 */
export async function probe(
  stores: FrameStore[],
  lng: number,
  lat: number,
  scale: IntensityScale,
): Promise<ProbeResult | null> {
  for (let s = stores.length - 1; s >= 0; s--) {
    const store = stores[s];
    const px = pixelFor(store, lng, lat);
    if (!px) continue;

    if (store.layer.tiling.maskUrlTemplate) {
      // Wait for the mask rather than treating "not downloaded yet" as "no
      // coverage" — that would report no data while standing in a downpour.
      const tile = await store.maskTile(px.x, px.y);
      if (!tile) continue;
      const cover = await readPixel(tile.blob, tile.x, tile.y);
      if (cover === null || cover < 128) continue;
    }

    const tiles = await Promise.all(
      Array.from({ length: store.length }, (_, i) => store.frameTile(i, px.x, px.y)),
    );
    const samples: ProbeSample[] = [];
    for (let i = 0; i < store.length; i++) {
      const tile = tiles[i];
      if (!tile) continue;
      const byte = await readPixel(tile.blob, tile.x, tile.y);
      if (byte === null) continue;
      samples.push({ time: store.times[i], mmh: decodeIntensity(byte, scale) });
    }
    if (samples.length) {
      return { layerLabel: store.layer.label, layerId: store.layer.id, samples };
    }
  }
  return null;
}

/**
 * Turn a series into a sentence: when rain starts, when it stops, or that none is
 * expected. Anything under 0.1 mm/h is not worth calling rain.
 */
export function summarise(samples: ProbeSample[], now: number): string {
  const WET = 0.1;
  const future = samples.filter((s) => s.time >= now - 60_000);
  if (!future.length) return 'Keine Daten für diesen Punkt.';

  const minutes = (t: number) => Math.max(0, Math.round((t - now) / 60_000));
  const raining = future[0].mmh >= WET;

  if (raining) {
    const stop = future.find((s) => s.mmh < WET);
    const peak = Math.max(...future.map((s) => s.mmh));
    if (!stop) return `Regen, bis mindestens ${fmtClock(future[future.length - 1].time)} (max ${fmtRate(peak)}).`;
    return `Regen hört in ca. ${minutes(stop.time)} Min auf (max ${fmtRate(peak)}).`;
  }

  const start = future.find((s) => s.mmh >= WET);
  if (!start) return 'Kein Regen in den nächsten zwei Stunden.';
  const peak = Math.max(...future.filter((s) => s.time >= start.time).map((s) => s.mmh));
  return `Regen in ca. ${minutes(start.time)} Min (bis ${fmtRate(peak)}).`;
}

export function fmtRate(mmh: number): string {
  if (mmh < 0.1) return '0 mm/h';
  if (mmh < 1) return `${mmh.toFixed(2)} mm/h`;
  if (mmh < 10) return `${mmh.toFixed(1)} mm/h`;
  return `${Math.round(mmh)} mm/h`;
}

export function fmtClock(ms: number): string {
  // The UI is German throughout, so the clock is too — a 12-hour "10:45 AM" would
  // look wrong next to "+35 Min".
  return new Date(ms).toLocaleTimeString('de-DE', { hour: '2-digit', minute: '2-digit' });
}
