/** Small DOM helpers for the control bar, legend and point readout. */

import type { ColorStop, IntensityScale, Origin, Segment, Timeline } from './api';
import { fmtClock, fmtRate, type ProbeSample } from './probe';

export function el<T extends HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (!node) throw new Error(`missing element #${id}`);
  return node as T;
}

/** "jetzt", "+25 Min", "+1:30 Std". */
export function formatOffset(ms: number): string {
  const min = Math.round(ms / 60_000);
  if (min <= 0) return 'jetzt';
  if (min < 60) return `+${min} Min`;
  const h = Math.floor(min / 60);
  const m = min % 60;
  return m === 0 ? `+${h} Std` : `+${h}:${String(m).padStart(2, '0')} Std`;
}

/** Paint the shared colour ramp into the legend strip. */
export function drawLegend(canvas: HTMLCanvasElement, stops: ColorStop[]): void {
  const ctx = canvas.getContext('2d');
  if (!ctx) return;
  canvas.width = 256;
  canvas.height = 1;
  const grad = ctx.createLinearGradient(0, 0, 256, 0);
  for (const s of [...stops].sort((a, b) => a.at - b.at)) {
    grad.addColorStop(Math.min(1, Math.max(0, s.at)), `rgba(${s.r},${s.g},${s.b},${s.a})`);
  }
  ctx.clearRect(0, 0, 256, 1);
  ctx.fillStyle = grad;
  ctx.fillRect(0, 0, 256, 1);
}

/**
 * Label the legend at meaningful rain rates rather than at even pixel spacings —
 * the scale is logarithmic, so evenly spaced labels would read as arbitrary numbers.
 */
export function legendLabels(host: HTMLElement, scale: IntensityScale): void {
  const marks = [0.1, 0.5, 2, 10, 50];
  const lo = Math.log(scale.minMmh);
  const hi = Math.log(scale.maxMmh);
  host.innerHTML = '';
  for (const mmh of marks) {
    if (mmh < scale.minMmh || mmh > scale.maxMmh) continue;
    const t = (Math.log(mmh) - lo) / (hi - lo);
    const span = document.createElement('span');
    span.textContent = mmh < 1 ? `${mmh}` : `${mmh}`;
    span.style.left = `${(t * 100).toFixed(2)}%`;
    host.appendChild(span);
  }
  const unit = document.createElement('span');
  unit.textContent = 'mm/h';
  unit.style.left = '100%';
  unit.style.transform = 'translateX(-100%)';
  host.appendChild(unit);
}

/** Colour per data source, matching the CSS custom properties. */
export const ORIGIN_COLOR: Record<Origin, string> = {
  RADAR: '#49b7ff',
  MODEL: '#a78bfa',
};

/** Tick marks along the slider, spaced so the labels stay readable. */
export function drawTicks(host: HTMLElement, timeline: Timeline): void {
  const start = Date.parse(timeline.start);
  const end = Date.parse(timeline.end);
  const span = end - start;
  host.innerHTML = '';
  if (span <= 0) return;

  // The axis grew from two hours to six when the model part arrived; a fixed
  // half-hour spacing would crowd a dozen labels onto a phone-width slider.
  const hours = span / 3_600_000;
  const stepMs = (hours > 8 ? 180 : hours > 4 ? 120 : hours > 2 ? 60 : 30) * 60_000;

  let t = Math.ceil(start / stepMs) * stepMs;
  for (; t <= end; t += stepMs) {
    const span_ = document.createElement('span');
    span_.textContent = fmtClock(t);
    span_.style.left = `${(((t - start) / span) * 100).toFixed(3)}%`;
    host.appendChild(span_);
  }
}

/**
 * Paint the slider track by provenance.
 *
 * Radar and model are never blended, so the track says plainly which is which:
 * hard-edged bands per source, brighter up to the playhead and dimmed beyond it.
 * The boundaries move with every run, so this is built here rather than in CSS.
 */
export function trackGradient(
  segments: Segment[],
  start: number,
  end: number,
  cursor: number,
): string {
  const span = Math.max(1, end - start);
  const pct = (t: number) => Math.min(100, Math.max(0, ((t - start) / span) * 100));
  const idle = 'rgba(255,255,255,0.14)';
  if (!segments.length) return idle;

  const play = pct(cursor);
  const cuts: { from: number; to: number; color: string }[] = [];
  for (const [i, seg] of segments.entries()) {
    const a = pct(Date.parse(seg.start));
    // Run each band up to where the next one begins. The few minutes between the
    // radar's last frame and the model's first are a handover, not a hole, and a
    // neutral sliver there would read as a rendering fault.
    const next = segments[i + 1];
    const b = next ? pct(Date.parse(next.start)) : pct(Date.parse(seg.end));
    if (b <= a) continue;
    const color = ORIGIN_COLOR[seg.origin] ?? idle;
    // Split where the playhead falls so past and future read differently.
    if (play > a && play < b) {
      cuts.push({ from: a, to: play, color });
      cuts.push({ from: play, to: b, color });
    } else {
      cuts.push({ from: a, to: b, color });
    }
  }
  if (!cuts.length) return idle;

  const stops: string[] = [];
  let at = 0;
  for (const c of cuts) {
    if (c.from > at + 0.01) {
      stops.push(`${idle} ${at.toFixed(2)}%`, `${idle} ${c.from.toFixed(2)}%`);
    }
    const dim = c.to > play + 0.01;
    const paint = dim ? `color-mix(in srgb, ${c.color} 32%, transparent)` : c.color;
    stops.push(`${paint} ${c.from.toFixed(2)}%`, `${paint} ${c.to.toFixed(2)}%`);
    at = c.to;
  }
  if (at < 99.99) {
    stops.push(`${idle} ${at.toFixed(2)}%`, `${idle} 100%`);
  }
  return `linear-gradient(90deg, ${stops.join(', ')})`;
}

/** Which source covers `timeMs`. */
export function originAt(segments: Segment[], timeMs: number): Segment | null {
  let best: Segment | null = null;
  for (const s of segments) {
    const a = Date.parse(s.start);
    const b = Date.parse(s.end);
    if (timeMs >= a && timeMs <= b) return s;
    // Between segments (the handover gap) the following one is the honest answer.
    if (timeMs < a && (!best || Date.parse(best.start) > a)) best = s;
  }
  return best ?? segments[segments.length - 1] ?? null;
}

/** A chip per source, naming what it is and how far it reaches. */
export function drawSources(host: HTMLElement, segments: Segment[]): void {
  host.innerHTML = '';
  const detail: Record<Origin, string> = {
    RADAR: 'Radar, 1 km',
    MODEL: 'Modell ICON-D2, 2 km',
  };
  for (const seg of segments) {
    const chip = document.createElement('span');
    chip.style.color = ORIGIN_COLOR[seg.origin] ?? 'inherit';
    const dot = document.createElement('i');
    const text = document.createElement('em');
    text.style.fontStyle = 'normal';
    text.style.color = 'var(--muted)';
    text.textContent = `${detail[seg.origin] ?? seg.label} bis ${fmtClock(Date.parse(seg.end))}`;
    chip.append(dot, text);
    host.appendChild(chip);
  }
}

/**
 * Sparkline of rain rate over the forecast window.
 *
 * Bars on a logarithmic scale, matching the map's colour ramp, with a marker at the
 * currently displayed time so the chart and the map agree.
 */
export function drawChart(
  canvas: HTMLCanvasElement,
  samples: ProbeSample[],
  scale: IntensityScale,
  nowMs: number,
  cursorMs: number,
  stops: ColorStop[],
  seamMs?: number,
): void {
  const ctx = canvas.getContext('2d');
  if (!ctx || !samples.length) return;

  const dpr = Math.min(3, window.devicePixelRatio || 1);
  const cssW = canvas.clientWidth || 600;
  const cssH = canvas.clientHeight || 60;
  canvas.width = Math.round(cssW * dpr);
  canvas.height = Math.round(cssH * dpr);
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, cssW, cssH);

  const t0 = samples[0].time;
  const t1 = samples[samples.length - 1].time;
  const span = Math.max(1, t1 - t0);
  const lo = Math.log(scale.minMmh);
  const hi = Math.log(scale.maxMmh);
  const norm = (mmh: number) => (mmh < scale.minMmh ? 0 : (Math.log(mmh) - lo) / (hi - lo));

  const sorted = [...stops].sort((a, b) => a.at - b.at);
  const colourAt = (t: number) => {
    let a = sorted[0];
    let b = sorted[sorted.length - 1];
    for (let i = 0; i < sorted.length - 1; i++) {
      if (t >= sorted[i].at && t <= sorted[i + 1].at) {
        a = sorted[i];
        b = sorted[i + 1];
        break;
      }
    }
    const d = b.at - a.at;
    const f = d > 0 ? (t - a.at) / d : 0;
    return `rgb(${Math.round(a.r + (b.r - a.r) * f)},${Math.round(a.g + (b.g - a.g) * f)},${Math.round(
      a.b + (b.b - a.b) * f,
    )})`;
  };

  const pad = 2;
  const h = cssH - pad * 2;
  // Bar width follows each sample's own spacing, so the coarser model steps read as
  // coarser instead of leaving unexplained gaps.
  const gapOf = (i: number) =>
    i + 1 < samples.length
      ? samples[i + 1].time - samples[i].time
      : samples[i].time - samples[Math.max(0, i - 1)].time;
  const widest = Math.max(...samples.map((_, i) => gapOf(i)));
  const scaleX = (cssW - 2) / span;

  // Baseline
  ctx.strokeStyle = 'rgba(255,255,255,0.12)';
  ctx.beginPath();
  ctx.moveTo(0, cssH - pad);
  ctx.lineTo(cssW, cssH - pad);
  ctx.stroke();

  const barW = Math.max(2, widest * scaleX * 0.72);
  samples.forEach((s, i) => {
    const w = Math.max(2, gapOf(i) * scaleX * 0.72);
    const x = ((s.time - t0) / span) * (cssW - 2) + 1;
    const t = Math.min(1, norm(s.mmh));
    // A visible stub even for a trace, so "almost nothing" is distinguishable
    // from "no data at all".
    const barH = s.mmh > 0 ? Math.max(2, t * h) : 0;
    if (barH <= 0) return;
    ctx.fillStyle = colourAt(t);
    ctx.fillRect(x - w / 2, cssH - pad - barH, w, barH);
  });

  // Where radar hands over to the model.
  if (seamMs !== undefined && seamMs > t0 && seamMs < t1) {
    const x = ((seamMs - t0) / span) * (cssW - 2) + 1;
    ctx.strokeStyle = ORIGIN_COLOR.MODEL;
    ctx.setLineDash([2, 3]);
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, cssH);
    ctx.stroke();
    ctx.setLineDash([]);
  }

  // "now" divider
  if (nowMs > t0 && nowMs < t1) {
    const x = ((nowMs - t0) / span) * (cssW - barW) + barW / 2;
    ctx.strokeStyle = 'rgba(255,255,255,0.28)';
    ctx.setLineDash([3, 3]);
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, cssH);
    ctx.stroke();
    ctx.setLineDash([]);
  }

  // playhead
  const cx = ((Math.min(t1, Math.max(t0, cursorMs)) - t0) / span) * (cssW - barW) + barW / 2;
  ctx.strokeStyle = '#49b7ff';
  ctx.lineWidth = 1.5;
  ctx.beginPath();
  ctx.moveTo(cx, 0);
  ctx.lineTo(cx, cssH);
  ctx.stroke();
}

export function valueAt(samples: ProbeSample[], timeMs: number): number {
  if (!samples.length) return 0;
  let best = samples[0];
  for (const s of samples) {
    if (Math.abs(s.time - timeMs) < Math.abs(best.time - timeMs)) best = s;
  }
  return best.mmh;
}

export { fmtRate, fmtClock };
