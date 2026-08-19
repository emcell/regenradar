import { Map as MapLibreMap, Marker, NavigationControl, setWorkerUrl } from 'maplibre-gl';
import 'maplibre-gl/dist/maplibre-gl.css';
// MapLibre v6 locates its worker with `new URL('./maplibre-gl-worker.mjs',
// import.meta.url)`. Bundlers cannot see through that, so the file is never emitted
// and the worker dies on startup — the map keeps rendering but never requests a
// single tile. `?worker&url` makes Vite build the worker properly, which matters
// because it in turn imports `maplibre-gl-shared.mjs`; copying the file verbatim
// would leave that dangling.
import workerUrl from 'maplibre-gl/dist/maplibre-gl-worker.mjs?worker&url';
import './style.css';

setWorkerUrl(workerUrl);

import { fetchRadar, type RadarState } from './api';
import { RadarLayer } from './radarLayer';
import { probe, summarise, type ProbeResult } from './probe';
import {
  drawChart,
  drawLegend,
  drawSources,
  drawTicks,
  el,
  fmtClock,
  fmtRate,
  formatOffset,
  legendLabels,
  ORIGIN_COLOR,
  originAt,
  trackGradient,
  valueAt,
} from './ui';

/** Free, keyless vector tiles. */
const BASEMAP = 'https://tiles.openfreemap.org/styles/dark';
/** Seconds of wall clock for one full sweep of the forecast during playback. */
const PLAY_SECONDS = 16;

const dom = {
  status: el<HTMLSpanElement>('status'),
  play: el<HTMLButtonElement>('play'),
  offset: el<HTMLSpanElement>('offset'),
  clock: el<HTMLSpanElement>('clock'),
  slider: el<HTMLInputElement>('slider'),
  ticks: el<HTMLDivElement>('ticks'),
  legend: el<HTMLDivElement>('legend'),
  legendCanvas: el<HTMLCanvasElement>('legend-canvas'),
  legendLabels: el<HTMLDivElement>('legend-labels'),
  legendToggle: el<HTMLButtonElement>('legend-toggle'),
  origin: el<HTMLSpanElement>('origin'),
  sources: el<HTMLDivElement>('sources'),
  attribution: el<HTMLDivElement>('attribution'),
  locate: el<HTMLButtonElement>('locate'),
  loading: el<HTMLDivElement>('loading'),
  readout: el<HTMLElement>('readout'),
  readoutClose: el<HTMLButtonElement>('readout-close'),
  readoutPlace: el<HTMLElement>('readout-place'),
  readoutSource: el<HTMLElement>('readout-source'),
  readoutSummary: el<HTMLElement>('readout-summary'),
  readoutChart: el<HTMLCanvasElement>('readout-chart'),
};

// ---------------------------------------------------------------------------
// Map
// ---------------------------------------------------------------------------

/** Zoom used when the map moves to the user's own position. */
const LOCATE_ZOOM = 9;
/**
 * How long the radar waits for the browser's location before giving up and drawing
 * the overview instead.
 *
 * The wait is worth it: at country zoom the first frames pull the whole tile grid,
 * and all of it is thrown away the moment the map jumps to wherever you are. Bounded,
 * though — a permission prompt can sit unanswered forever, and an empty map is worse
 * than the wrong one.
 */
const LOCATE_GATE_MS = 5_000;

/** The whole radar composite, which is where the map starts unless told otherwise. */
const DEFAULT_VIEW: { center: [number, number]; zoom: number } = {
  center: [10.4, 51.0],
  zoom: 5.2,
};

/**
 * Where to open the map.
 *
 * Deliberately *not* remembered between visits. A rain radar is a question about
 * here and now, so it starts at the overview and then moves to wherever the browser
 * says you are — restoring last week's viewport would just be in the way.
 *
 * `?at=<lat>,<lng>,<zoom>` overrides it for a single load. Read once, never written
 * back, so a shared link points somewhere specific without the app becoming sticky.
 */
let startedFromLink = false;

function initialView(): { center: [number, number]; zoom: number } {
  const raw = new URLSearchParams(location.search).get('at');
  if (raw) {
    const [lat, lng, zoom] = raw.split(',').map(Number);
    if (Number.isFinite(lat) && Number.isFinite(lng)) {
      startedFromLink = true;
      return {
        center: [lng, lat],
        zoom: Number.isFinite(zoom) ? zoom : LOCATE_ZOOM,
      };
    }
  }
  return DEFAULT_VIEW;
}

const start = initialView();
const map = new MapLibreMap({
  container: 'map',
  style: BASEMAP,
  center: start.center,
  zoom: start.zoom,
  minZoom: 2,
  maxZoom: 12,
  attributionControl: false,
  dragRotate: false,
  pitchWithRotate: false,
  // The radar quad is drawn in flat mercator.
  hash: false,
  // Readable pixels cost a little performance, so it is opt-in: `?capture` allows
  // the map to be exported to an image (and lets automated tests inspect it).
  canvasContextAttributes: {
    preserveDrawingBuffer: new URLSearchParams(location.search).has('capture'),
  },
});
map.touchZoomRotate.disableRotation();
map.addControl(new NavigationControl({ showCompass: false }), 'bottom-right');

const radar = new RadarLayer();

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

let state: RadarState | null = null;
let timeStart = Date.now();
let timeEnd = timeStart + 2 * 3600_000;
let cursor = timeStart;
let playing = false;
let lastFrame = 0;
let inFlight: AbortController | null = null;
let probeMarker: Marker | null = null;
let probeLngLat: { lng: number; lat: number } | null = null;
let probeResult: ProbeResult | null = null;
/** The pin stays put when the panel is dismissed, so `hidden` is not the whole
 *  story — this is what says whether the readout should be showing. */
let readoutOpen = false;
/** Set once the user pans or zooms themselves, so a slow geolocation fix cannot
 *  yank the map out from under them. */
let userMovedMap = false;

async function refresh(): Promise<void> {
  inFlight?.abort();
  const ctl = new AbortController();
  inFlight = ctl;
  try {
    const next = await fetchRadar(ctl.signal);
    if (ctl.signal.aborted) return;
    applyState(next);
  } catch (err) {
    if ((err as Error).name === 'AbortError') return;
    console.error(err);
    dom.status.textContent = 'offline';
    dom.status.classList.add('error');
  } finally {
    if (inFlight === ctl) inFlight = null;
  }
}

function applyState(next: RadarState): void {
  const first = !state;
  state = next;
  dom.status.classList.remove('error');

  if (!next.ready || !next.timeline) {
    document.body.dataset.state = 'waiting';
    dom.status.textContent = 'warte auf Daten …';
    return;
  }
  document.body.dataset.state = 'ready';

  const newStart = Date.parse(next.timeline.start);
  const newEnd = Date.parse(next.timeline.end);
  // Keep the user's place on the timeline across a refresh, as an offset from now.
  const offset = first ? 0 : cursor - timeStart;
  timeStart = newStart;
  timeEnd = newEnd;
  cursor = Math.min(timeEnd, Math.max(timeStart, timeStart + offset));

  radar.setState(next);
  radar.setTime(cursor);

  drawLegend(dom.legendCanvas, next.colorScale);
  legendLabels(dom.legendLabels, next.intensity);
  drawTicks(dom.ticks, next.timeline);
  drawSources(dom.sources, next.timeline.segments);
  dom.attribution.innerHTML = [
    ...next.attribution,
    '© <a href="https://openfreemap.org/" target="_blank" rel="noopener">OpenFreeMap</a>',
    '© <a href="https://www.openstreetmap.org/copyright" target="_blank" rel="noopener">OpenStreetMap</a>',
  ].join(' · ');

  showRunStatus();

  syncTime();
  if (probeLngLat) void runProbe(probeLngLat.lng, probeLngLat.lat, readoutOpen);
}

function syncTime(): void {
  const span = Math.max(1, timeEnd - timeStart);
  const t = (cursor - timeStart) / span;
  dom.slider.value = String(Math.round(t * 1000));
  dom.offset.textContent = formatOffset(cursor - Date.now());
  dom.clock.textContent = fmtClock(cursor);

  const segments = state?.timeline?.segments ?? [];
  dom.slider.style.setProperty('--track', trackGradient(segments, timeStart, timeEnd, cursor));
  const seg = originAt(segments, cursor);
  if (seg) {
    const color = ORIGIN_COLOR[seg.origin];
    dom.origin.hidden = false;
    dom.origin.textContent = seg.label;
    dom.origin.style.setProperty('--chip', color);
    dom.slider.style.setProperty('--thumb', color);
  } else {
    dom.origin.hidden = true;
  }

  radar.setTime(cursor);
  updateReadoutValue();
}

function setCursor(ms: number): void {
  cursor = Math.min(timeEnd, Math.max(timeStart, ms));
  syncTime();
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

function tick(now: number): void {
  requestAnimationFrame(tick);
  if (!playing) {
    lastFrame = now;
    return;
  }
  const dt = Math.min(200, now - lastFrame);
  lastFrame = now;
  const span = timeEnd - timeStart;
  const next = cursor + (dt / 1000 / PLAY_SECONDS) * span;
  // Pause on the last frame briefly, then loop back to now.
  setCursor(next >= timeEnd ? timeStart : next);
}
requestAnimationFrame(tick);

function setPlaying(on: boolean): void {
  playing = on;
  dom.play.classList.toggle('active', on);
  dom.play.title = on ? 'Pause' : 'Abspielen';
  dom.play.innerHTML = on
    ? '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M7 5h4v14H7zM13 5h4v14h-4z" class="fill"/></svg>'
    : '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M8 5l11 7-11 7z" class="fill"/></svg>';
  if (on) radar.prefetchAll();
}

// ---------------------------------------------------------------------------
// Point readout
// ---------------------------------------------------------------------------

async function runProbe(lng: number, lat: number, open = true): Promise<void> {
  probeLngLat = { lng, lat };
  if (!probeMarker) {
    const node = document.createElement('div');
    node.className = 'probe-pin';
    node.title = 'Regen an diesem Punkt';
    // Re-opening the panel from the pin is the natural way back after dismissing
    // it; without stopping propagation the map's own click would fire too.
    node.addEventListener('click', (e) => {
      e.stopPropagation();
      if (probeLngLat) void runProbe(probeLngLat.lng, probeLngLat.lat, true);
    });
    probeMarker = new Marker({ element: node }).setLngLat([lng, lat]).addTo(map);
  } else {
    probeMarker.setLngLat([lng, lat]);
  }

  if (open) {
    readoutOpen = true;
    dom.readout.hidden = false;
    dom.readoutPlace.textContent = `${lat.toFixed(3)}°, ${lng.toFixed(3)}°`;
    dom.readoutSummary.textContent = 'lese Daten …';
  }

  const s = state;
  if (!s) return;
  const result = await probe(radar.stores(), lng, lat, s.intensity);
  if (!probeLngLat || probeLngLat.lng !== lng || probeLngLat.lat !== lat) return;

  probeResult = result;
  if (!readoutOpen) return;
  if (!result) {
    dom.readoutSource.textContent = '';
    dom.readoutSummary.textContent = 'Für diesen Punkt liegen keine Daten vor.';
    dom.readoutChart.getContext('2d')?.clearRect(0, 0, dom.readoutChart.width, dom.readoutChart.height);
    return;
  }
  const segs = state?.timeline?.segments ?? [];
  dom.readoutSource.textContent = segs.length
    ? segs.map((sg) => sg.label).join(' + ')
    : result.layerLabel;
  updateReadoutValue();
}

function updateReadoutValue(): void {
  if (!probeResult || !state || dom.readout.hidden) return;
  const now = Date.now();
  const rate = valueAt(probeResult.samples, cursor);
  dom.readoutSummary.textContent = `${summarise(probeResult.samples, now)} — ${fmtClock(cursor)}: ${fmtRate(rate)}`;
  // The dashed marker in the chart is the same handover the slider colours show.
  const segs = state.timeline?.segments ?? [];
  const seam = segs.length > 1 ? Date.parse(segs[1].start) : undefined;
  drawChart(
    dom.readoutChart,
    probeResult.samples,
    state.intensity,
    now,
    cursor,
    state.colorScale,
    seam,
  );
}

/** Dismiss the panel but leave the pin — it is a marker of "the place I care
 *  about", and losing it every time the panel is closed would mean re-aiming at the
 *  same spot over and over. Tap the pin to bring the panel back. */
function closeReadout(): void {
  dom.readout.hidden = true;
  readoutOpen = false;
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

dom.slider.addEventListener('input', () => {
  setPlaying(false);
  setCursor(timeStart + (Number(dom.slider.value) / 1000) * (timeEnd - timeStart));
});

dom.play.addEventListener('click', () => setPlaying(!playing));

dom.legendToggle.addEventListener('click', () => {
  dom.legend.hidden = !dom.legend.hidden;
  dom.legendToggle.classList.toggle('active', !dom.legend.hidden);
});

dom.readoutClose.addEventListener('click', closeReadout);

map.on('click', (e) => void runProbe(e.lngLat.lng, e.lngLat.lat));

/**
 * Ask the browser where we are.
 *
 * The button path always recentres and reports failures. The startup path is
 * different in two ways: it stays quiet about a declined permission — that is a
 * choice, not an error — and it declines to move the map if the user has already
 * started navigating somewhere themselves.
 */
function locateUser({ automatic }: { automatic: boolean }): Promise<boolean> {
  return new Promise((resolve) => {
    if (!navigator.geolocation) {
      if (!automatic) dom.status.textContent = 'Standort nicht verfügbar';
      resolve(false);
      return;
    }
    dom.locate.classList.add('active');
    navigator.geolocation.getCurrentPosition(
      (pos) => {
        dom.locate.classList.remove('active');
        const { longitude, latitude } = pos.coords;
        if (!automatic || !userMovedMap) {
          // Before the layer is attached there is nothing to animate away from, and
          // jumping means the very first frame is fetched for the right place.
          const target = { center: [longitude, latitude] as [number, number], zoom: LOCATE_ZOOM };
          if (map.getLayer(radar.id)) {
            map.easeTo({ ...target, zoom: Math.max(map.getZoom(), LOCATE_ZOOM) });
          } else {
            map.jumpTo(target);
          }
        }
        void runProbe(longitude, latitude, true);
        resolve(true);
      },
      () => {
        dom.locate.classList.remove('active');
        if (!automatic) dom.status.textContent = 'Standort nicht verfügbar';
        resolve(false);
      },
      { enableHighAccuracy: false, timeout: 10_000, maximumAge: 60_000 },
    );
  });
}

dom.locate.addEventListener('click', () => locateUser({ automatic: false }));

// Only user-driven moves carry an originalEvent; our own easeTo does not.
map.on('movestart', (e) => {
  if ((e as { originalEvent?: unknown }).originalEvent) userMovedMap = true;
});

// Basemap tiles and precipitation data are independent: if the vector tiles stall
// on a flaky connection, the radar should still appear. So the data fetch starts
// immediately, and the layer is attached as soon as the *style* is ready rather
// than waiting for `load`, which also waits on tiles.
/**
 * Where the precipitation layer belongs in the basemap's stack.
 *
 * Just under the administrative boundaries: borders and every label stay readable on
 * top of the rain, while roads, buildings and landuse sit beneath it so a downpour
 * reads as one clean surface. Appending the layer at the top instead would bury the
 * place names — and this style's first symbol layer is `water_name`, very early on,
 * so "below all symbols" would leave every road drawn over the rain.
 */
function radarInsertBefore(): string | undefined {
  const layers = map.getStyle().layers ?? [];
  const boundary = layers.find((l) => l.id.startsWith('boundary'));
  if (boundary) return boundary.id;
  return layers.find((l) => l.type === 'symbol')?.id;
}

let styleReady = false;
let viewSettled = false;
/** True while the startup location request is still outstanding. */
let locating = false;

/**
 * Attach the precipitation layer once both the basemap style and the starting view
 * are settled.
 *
 * Waiting on the view matters because frames are fetched for whatever is on screen:
 * attaching at the overview and then jumping to the user's position would download
 * one viewport's worth of tiles and immediately discard them.
 */
function attachRadar(): void {
  if (!styleReady || !viewSettled || map.getLayer(radar.id)) return;
  map.addLayer(radar, radarInsertBefore());
  restyleLabels();
}

function settleView(): void {
  if (viewSettled) return;
  viewSettled = true;
  locating = false;
  showRunStatus();
  attachRadar();
}

/** Name the radar run in the status chip, unless something more urgent is there. */
function showRunStatus(): void {
  if (locating || !state?.layers.length) return;
  const first = state.layers[0].frames[0];
  if (first) dom.status.textContent = `Radar ${fmtClock(Date.parse(first.time))}`;
}

/**
 * Prefer German place names, and strengthen label halos.
 *
 * OpenFreeMap's dark style labels everything in `name:latin`, so an app that speaks
 * German otherwise would still say "CZECHIA" and "MUNICH". The halo matters because
 * the style assumes a dark background — over a bright orange rain cell, grey text
 * without one is unreadable.
 */
function restyleLabels(): void {
  for (const layer of map.getStyle().layers ?? []) {
    if (layer.type !== 'symbol') continue;
    const field = (layer.layout as { 'text-field'?: unknown } | undefined)?.['text-field'];
    if (field === undefined) continue;
    try {
      map.setLayoutProperty(layer.id, 'text-field', [
        'coalesce',
        ['get', 'name:de'],
        ['get', 'name:latin'],
        ['get', 'name'],
      ]);
      map.setPaintProperty(layer.id, 'text-halo-color', 'rgba(6,10,15,0.9)');
      map.setPaintProperty(layer.id, 'text-halo-width', 1.6);
      map.setPaintProperty(layer.id, 'text-color', '#d6e0ec');
    } catch {
      // A layer whose label is not a plain name lookup: leave it as the style had it.
    }
  }
}
if (map.isStyleLoaded()) {
  styleReady = true;
} else {
  map.once('style.load', () => {
    styleReady = true;
    attachRadar();
  });
}
map.on('error', (e) => {
  console.warn('map error:', e.error?.message ?? e);
});
void refresh();

// A shared link names its own place, so the browser's idea of "here" would only get
// in the way. Otherwise: locate first, draw second.
if (startedFromLink) {
  settleView();
} else {
  locating = true;
  dom.status.textContent = 'Standort …';
  void locateUser({ automatic: true }).then(settleView);
  window.setTimeout(settleView, LOCATE_GATE_MS);
}

// New radar runs land every five minutes; only chase them while visible.
window.setInterval(() => {
  if (document.visibilityState === 'visible') void refresh();
}, 60_000);
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible') void refresh();
});

// Keep the "+X Min" label honest as real time advances past a static cursor.
window.setInterval(() => {
  if (!playing) dom.offset.textContent = formatOffset(cursor - Date.now());
}, 15_000);

// Spinner while frames are in flight. Frames load lazily around the playhead, so
// "everything downloaded" is not the right condition — "nothing outstanding" is.
window.setInterval(() => {
  dom.loading.hidden = !radar.isLoading();
  document.body.dataset.loaded = radar.loadProgress().toFixed(3);
  document.body.dataset.framesReady = String(radar.loadedFrames());
}, 300);

window.addEventListener('keydown', (e) => {
  if (e.key === ' ') {
    e.preventDefault();
    setPlaying(!playing);
  } else if (e.key === 'ArrowRight') {
    setPlaying(false);
    setCursor(cursor + 5 * 60_000);
  } else if (e.key === 'ArrowLeft') {
    setPlaying(false);
    setCursor(cursor - 5 * 60_000);
  } else if (e.key === 'Escape') {
    closeReadout();
  }
});
