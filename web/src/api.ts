/** GraphQL client for the radar backend. */

export interface Bounds {
  west: number;
  south: number;
  east: number;
  north: number;
}

export type Origin = 'RADAR' | 'MODEL';

export interface Frame {
  /** Position in the stack; substituted into the layer's URL templates. */
  index: number;
  /** ISO 8601 */
  time: string;
  forecast: boolean;
  origin: Origin;
}

/**
 * How frames are cut up for delivery.
 *
 * Each frame ships as `cols * rows` full-resolution tiles plus one downscaled
 * overview. Zoomed out, the overview is all the screen can resolve; zoomed in, only
 * the tiles the viewport covers are fetched.
 */
export interface Tiling {
  cols: number;
  rows: number;
  tileWidth: number;
  tileHeight: number;
  overviewWidth: number;
  overviewHeight: number;
  /** Contains `{frame}`, `{x}`, `{y}`. */
  tileUrlTemplate: string;
  /** Contains `{frame}`. */
  overviewUrlTemplate: string;
  /** Contains `{x}`, `{y}`. Absent when the layer has no coverage mask. */
  maskUrlTemplate: string | null;
}

/** A contiguous stretch of the timeline fed by one source. */
export interface Segment {
  origin: Origin;
  start: string;
  end: string;
  label: string;
  run: string;
}

export interface Layer {
  id: string;
  label: string;
  attribution: string;
  run: string;
  width: number;
  height: number;
  resolutionMeters: number;
  bounds: Bounds;
  frames: Frame[];
  flowUrls: string[];
  flowMaxPixels: number;
  tiling: Tiling;
}

/** Fill a URL template. */
export function tileUrl(t: Tiling, frame: number, x: number, y: number): string {
  return t.tileUrlTemplate
    .replace('{frame}', String(frame).padStart(2, '0'))
    .replace('{x}', String(x))
    .replace('{y}', String(y));
}

export function overviewUrl(t: Tiling, frame: number): string {
  return t.overviewUrlTemplate.replace('{frame}', String(frame).padStart(2, '0'));
}

export function maskUrl(t: Tiling, x: number, y: number): string | null {
  if (!t.maskUrlTemplate) return null;
  return t.maskUrlTemplate.replace('{x}', String(x)).replace('{y}', String(y));
}

export interface Timeline {
  start: string;
  end: string;
  stepSeconds: number;
  segments: Segment[];
}

export interface ColorStop {
  at: number;
  r: number;
  g: number;
  b: number;
  a: number;
}

export interface IntensityScale {
  minMmh: number;
  maxMmh: number;
}

export interface RadarState {
  ready: boolean;
  serverTime: string;
  timeline: Timeline | null;
  layers: Layer[];
  intensity: IntensityScale;
  colorScale: ColorStop[];
  attribution: string[];
}

const ENDPOINT = import.meta.env.VITE_API_URL ?? '/graphql';

const QUERY = `query Radar {
  radar {
    ready
    serverTime
    timeline { start end stepSeconds segments { origin start end label run } }
    intensity { minMmh maxMmh }
    colorScale { at r g b a }
    attribution
    layers {
      id label attribution run
      width height resolutionMeters
      bounds { west south east north }
      frames { index time forecast origin }
      flowUrls
      flowMaxPixels
      tiling {
        cols rows tileWidth tileHeight overviewWidth overviewHeight
        tileUrlTemplate overviewUrlTemplate maskUrlTemplate
      }
    }
  }
}`;

export async function fetchRadar(signal?: AbortSignal): Promise<RadarState> {
  const res = await fetch(ENDPOINT, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ query: QUERY }),
    signal,
  });
  if (!res.ok) throw new Error(`API ${res.status}`);
  const body = await res.json();
  if (body.errors?.length) throw new Error(body.errors[0].message);
  return body.data.radar as RadarState;
}

/**
 * Decode an intensity byte back into mm/h.
 *
 * Mirrors the backend's logarithmic encoding: byte 0 is "no precipitation",
 * 1..255 spans `minMmh`..`maxMmh` geometrically.
 */
export function decodeIntensity(byte: number, scale: IntensityScale): number {
  if (byte <= 0) return 0;
  const lo = Math.log(scale.minMmh);
  const hi = Math.log(scale.maxMmh);
  return Math.exp(lo + ((byte - 1) / 254) * (hi - lo));
}
