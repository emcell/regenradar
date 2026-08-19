/**
 * Loads a layer's frame stack onto the GPU, on demand and only where it is looked at.
 *
 * Every frame exists in two forms on the server: a downscaled **overview** and a grid
 * of full-resolution **tiles**. Which one a client needs depends entirely on the map:
 *
 * * Zoomed out, the whole composite is squeezed into fewer screen pixels than the
 *   overview has, so the extra detail is invisible — one small request per frame.
 * * Zoomed in, the viewport covers a few tiles out of sixteen. Fetching the whole
 *   composite there would throw away most of the download.
 *
 * Tiles land in one full-size texture through `texSubImage2D`, which keeps the shader
 * oblivious: it samples by uv and never learns the picture arrived in pieces. Panning
 * fills in more tiles as they come into view.
 *
 * Textures go up as single-channel `R8`. As RGBA a 41-frame stack would be 250 MB of
 * texture memory, which no phone will thank you for.
 *
 * Frames are also fetched lazily around the playhead: someone who never scrubs past
 * "+15 min" should not pay to download six hours of forecast.
 */

import { maskUrl, overviewUrl, tileUrl, type Layer } from './api';

/** Which form of a frame is currently wanted. */
export type Detail = 'overview' | 'tiles';

/** A rectangle of tile indices, inclusive. */
export interface TileRect {
  x0: number;
  y0: number;
  x1: number;
  y1: number;
}

interface FrameSlot {
  overviewTex: WebGLTexture | null;
  overviewLoading: boolean;
  fullTex: WebGLTexture | null;
  /** Tile keys already uploaded into `fullTex`. */
  haveTiles: Set<string>;
  /** Tile keys currently in flight. */
  pendingTiles: Set<string>;
  failed: boolean;
}

function emptySlot(): FrameSlot {
  return {
    overviewTex: null,
    overviewLoading: false,
    fullTex: null,
    haveTiles: new Set(),
    pendingTiles: new Set(),
    failed: false,
  };
}

interface FlowSlot {
  tex: WebGLTexture | null;
  loading: boolean;
  failed: boolean;
}

export class FrameStore {
  private slots: FrameSlot[];
  private flow: FlowSlot[];
  private maskTiles = new Map<string, Blob>();
  private maskPending = new Map<string, Promise<Blob | null>>();
  private disposed = false;

  /** Frame valid times, as epoch milliseconds. */
  readonly times: number[];

  constructor(
    private gl: WebGL2RenderingContext,
    readonly layer: Layer,
    private onLoaded: () => void,
  ) {
    this.slots = layer.frames.map(emptySlot);
    this.flow = layer.flowUrls.map(() => ({ tex: null, loading: false, failed: false }));
    this.times = layer.frames.map((f) => Date.parse(f.time));
  }

  get length(): number {
    return this.slots.length;
  }

  /**
   * Where `timeMs` falls on this layer's frame axis, as a fractional index.
   * The stack mixes five-minute radar steps with quarter-hour model ones, so the
   * spacing is not uniform.
   */
  positionAt(timeMs: number): number {
    const t = this.times;
    if (t.length === 0) return 0;
    if (timeMs <= t[0]) return 0;
    if (timeMs >= t[t.length - 1]) return t.length - 1;
    let i = 0;
    while (i < t.length - 2 && t[i + 1] <= timeMs) i++;
    const span = t[i + 1] - t[i];
    return i + (span > 0 ? (timeMs - t[i]) / span : 0);
  }

  /**
   * Texture for frame `i` at the requested detail, starting any missing loads.
   *
   * Returns whatever is usable right now: while tiles are still arriving the
   * overview stands in, so scrubbing never shows a hole.
   */
  frameTex(i: number, detail: Detail, rect: TileRect): WebGLTexture | null {
    const s = this.slots[i];
    if (!s || s.failed) return null;

    if (detail === 'tiles') {
      this.ensureTiles(i, rect);
      // Only switch once the visible area is actually covered, otherwise the map
      // would flash through a half-filled texture.
      if (s.fullTex && this.rectCovered(s, rect)) return s.fullTex;
    }
    this.ensureOverview(i);
    return s.overviewTex ?? (detail === 'tiles' ? s.fullTex : null);
  }

  /** Motion field between frames `i` and `i + 1`. */
  flowTex(i: number): WebGLTexture | null {
    const url = this.layer.flowUrls[i];
    const s = this.flow[i];
    if (!s || !url || s.failed) return null;
    if (!s.tex && !s.loading) {
      s.loading = true;
      void this.load(url, 'rgb')
        .then((tex) => {
          s.tex = tex;
          if (tex) this.onLoaded();
          else s.failed = true;
        })
        .finally(() => {
          s.loading = false;
        });
    }
    return s.tex;
  }

  isLoaded(i: number): boolean {
    const s = this.slots[i];
    return !!s && (!!s.overviewTex || s.haveTiles.size > 0);
  }

  /** Nearest frame index that has something on the GPU, or -1. */
  nearestLoaded(i: number): number {
    for (let d = 0; d < this.slots.length; d++) {
      if (this.isLoaded(i - d)) return i - d;
      if (this.isLoaded(i + d)) return i + d;
    }
    return -1;
  }

  /** Ask for a window of frames around the playhead. */
  prefetch(from: number, to: number, detail: Detail, rect: TileRect): void {
    for (let i = Math.max(0, from); i <= Math.min(this.slots.length - 1, to); i++) {
      this.frameTex(i, detail, rect);
      if (i < this.flow.length) this.flowTex(i);
    }
  }

  /**
   * Pull in the whole stack for smooth playback.
   *
   * Overviews always — they are cheap and guarantee every frame has something to
   * show. Tiles as well when the map is zoomed in past them, so playing does not
   * stay on the blurry version; the overview stands in until each tile lands.
   */
  prefetchAll(detail: Detail, rect: TileRect): void {
    for (let i = 0; i < this.slots.length; i++) {
      this.ensureOverview(i);
      if (detail === 'tiles') this.ensureTiles(i, rect);
    }
    for (let i = 0; i < this.flow.length; i++) this.flowTex(i);
  }

  get loadedCount(): number {
    return this.slots.filter((s) => s.overviewTex || s.haveTiles.size > 0).length;
  }

  get pendingCount(): number {
    let n = 0;
    for (const s of this.slots) {
      if (s.overviewLoading) n++;
      n += s.pendingTiles.size;
    }
    for (const f of this.flow) if (f.loading) n++;
    return n;
  }

  // --- point readout -------------------------------------------------------

  /**
   * The tile of frame `i` containing raster pixel `(px, py)`, as bytes.
   *
   * The readout decodes one pixel, so it fetches one tile rather than the frame.
   */
  async frameTile(i: number, px: number, py: number): Promise<{ blob: Blob; x: number; y: number } | null> {
    const t = this.layer.tiling;
    const tx = Math.min(t.cols - 1, Math.floor(px / t.tileWidth));
    const ty = Math.min(t.rows - 1, Math.floor(py / t.tileHeight));
    const blob = await this.fetchBlob(tileUrl(t, this.layer.frames[i].index, tx, ty));
    if (!blob) return null;
    return { blob, x: px - tx * t.tileWidth, y: py - ty * t.tileHeight };
  }

  /** The coverage-mask tile containing a raster pixel. */
  async maskTile(px: number, py: number): Promise<{ blob: Blob; x: number; y: number } | null> {
    const t = this.layer.tiling;
    if (!t.maskUrlTemplate) return null;
    const tx = Math.min(t.cols - 1, Math.floor(px / t.tileWidth));
    const ty = Math.min(t.rows - 1, Math.floor(py / t.tileHeight));
    const key = `${tx},${ty}`;
    let blob = this.maskTiles.get(key) ?? null;
    if (!blob) {
      let pending = this.maskPending.get(key);
      if (!pending) {
        pending = this.fetchBlob(maskUrl(t, tx, ty)!);
        this.maskPending.set(key, pending);
      }
      blob = await pending;
      if (blob) this.maskTiles.set(key, blob);
    }
    if (!blob) return null;
    return { blob, x: px - tx * t.tileWidth, y: py - ty * t.tileHeight };
  }

  // --- loading -------------------------------------------------------------

  private ensureOverview(i: number): void {
    const s = this.slots[i];
    if (!s || s.overviewTex || s.overviewLoading || s.failed) return;
    s.overviewLoading = true;
    void this.load(overviewUrl(this.layer.tiling, this.layer.frames[i].index), 'r8')
      .then((tex) => {
        if (tex) {
          s.overviewTex = tex;
          this.onLoaded();
        }
      })
      .finally(() => {
        s.overviewLoading = false;
      });
  }

  private ensureTiles(i: number, rect: TileRect): void {
    const s = this.slots[i];
    if (!s) return;
    const t = this.layer.tiling;
    for (let y = Math.max(0, rect.y0); y <= Math.min(t.rows - 1, rect.y1); y++) {
      for (let x = Math.max(0, rect.x0); x <= Math.min(t.cols - 1, rect.x1); x++) {
        const key = `${x},${y}`;
        if (s.haveTiles.has(key) || s.pendingTiles.has(key)) continue;
        s.pendingTiles.add(key);
        void this.loadTile(i, x, y, key);
      }
    }
  }

  private async loadTile(i: number, x: number, y: number, key: string): Promise<void> {
    const s = this.slots[i];
    const t = this.layer.tiling;
    try {
      const bitmap = await this.fetchBitmap(tileUrl(t, this.layer.frames[i].index, x, y));
      if (!bitmap || this.disposed) return;
      const gl = this.gl;
      if (!s.fullTex) s.fullTex = this.allocateFull();
      gl.bindTexture(gl.TEXTURE_2D, s.fullTex);
      gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
      gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
      gl.texSubImage2D(
        gl.TEXTURE_2D,
        0,
        x * t.tileWidth,
        y * t.tileHeight,
        gl.RED,
        gl.UNSIGNED_BYTE,
        bitmap,
      );
      bitmap.close();
      s.haveTiles.add(key);
      this.onLoaded();
    } catch (err) {
      console.warn('tile load failed', i, key, err);
    } finally {
      s.pendingTiles.delete(key);
    }
  }

  private rectCovered(s: FrameSlot, rect: TileRect): boolean {
    const t = this.layer.tiling;
    for (let y = Math.max(0, rect.y0); y <= Math.min(t.rows - 1, rect.y1); y++) {
      for (let x = Math.max(0, rect.x0); x <= Math.min(t.cols - 1, rect.x1); x++) {
        if (!s.haveTiles.has(`${x},${y}`)) return false;
      }
    }
    return true;
  }

  /** A zero-filled texture the tiles are written into. */
  private allocateFull(): WebGLTexture {
    const gl = this.gl;
    const tex = gl.createTexture()!;
    gl.bindTexture(gl.TEXTURE_2D, tex);
    gl.texStorage2D(gl.TEXTURE_2D, 1, gl.R8, this.layer.width, this.layer.height);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    return tex;
  }

  private async fetchBlob(url: string): Promise<Blob | null> {
    try {
      const res = await fetch(url);
      if (!res.ok) throw new Error(`${res.status} ${url}`);
      return await res.blob();
    } catch (err) {
      console.warn('fetch failed', url, err);
      return null;
    }
  }

  private async fetchBitmap(url: string): Promise<ImageBitmap | null> {
    const blob = await this.fetchBlob(url);
    if (!blob) return null;
    // No premultiplication and no colour management: these bytes are data, not
    // colour, and any "helpful" conversion would corrupt them.
    return createImageBitmap(blob, {
      premultiplyAlpha: 'none',
      colorSpaceConversion: 'none',
    });
  }

  private async load(url: string, fmt: 'r8' | 'rgb'): Promise<WebGLTexture | null> {
    const bitmap = await this.fetchBitmap(url);
    if (!bitmap) return null;
    if (this.disposed) {
      bitmap.close();
      return null;
    }
    const gl = this.gl;
    const tex = gl.createTexture()!;
    gl.bindTexture(gl.TEXTURE_2D, tex);
    gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
    if (fmt === 'r8') {
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.R8, gl.RED, gl.UNSIGNED_BYTE, bitmap);
    } else {
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGB8, gl.RGB, gl.UNSIGNED_BYTE, bitmap);
    }
    bitmap.close();
    // Clamped so the shader's own bounds test is the only edge behaviour, and
    // linear so both the raster and the motion field interpolate smoothly.
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    return tex;
  }

  dispose(): void {
    this.disposed = true;
    const gl = this.gl;
    for (const s of this.slots) {
      if (s.overviewTex) gl.deleteTexture(s.overviewTex);
      if (s.fullTex) gl.deleteTexture(s.fullTex);
      s.overviewTex = null;
      s.fullTex = null;
      s.haveTiles.clear();
    }
    for (const f of this.flow) {
      if (f.tex) gl.deleteTexture(f.tex);
      f.tex = null;
    }
    this.maskTiles.clear();
  }
}
