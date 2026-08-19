/**
 * The precipitation layer: a MapLibre custom WebGL layer.
 *
 * What makes it worth a custom layer rather than a stack of image sources is
 * **motion interpolation**. The data only exists every five minutes. Cross-fading
 * between frames makes a shower dissolve in one place and condense in another; what
 * should happen is that it *travels*. The backend ships a displacement field per
 * frame pair, and the shader warps both neighbouring frames towards the requested
 * in-between time before blending — the same technique video frame interpolation
 * uses, applied to rain.
 *
 * No coverage mask is needed to draw: outside radar range the encoded intensity is
 * zero and the colour ramp is transparent there, so the layer ends exactly where the
 * radar does.
 */

import type { CustomLayerInterface, CustomRenderMethodInput, Map as MapLibreMap } from 'maplibre-gl';
import { MercatorCoordinate } from 'maplibre-gl';
import type { ColorStop, Layer, RadarState } from './api';
import { FrameStore, type Detail, type TileRect } from './frames';

const VERTEX_SRC = `#version 300 es
precision highp float;
in vec2 a_pos;              // web mercator, 0..1
uniform mat4 u_matrix;
out vec2 v_merc;
void main() {
  v_merc = a_pos;
  gl_Position = u_matrix * vec4(a_pos, 0.0, 1.0);
}`;

const FRAGMENT_SRC = `#version 300 es
precision highp float;
precision highp sampler2D;

in vec2 v_merc;
out vec4 fragColor;

uniform sampler2D u_ramp;
uniform float u_opacity;

uniform vec4  u_box;       // (west, north, east, south) in mercator 0..1
uniform float u_mix, u_useFlow;
uniform vec2  u_flowUV;
uniform sampler2D u_a, u_b, u_flow;

void main() {
  vec2 uv = (v_merc - u_box.xy) / (u_box.zw - u_box.xy);
  if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) discard;

  // The parcel of rain at uv at time f came from uv - f*flow and is heading for
  // uv + (1-f)*flow, so each frame is read at its own displaced position.
  vec2 d = vec2(0.0);
  if (u_useFlow > 0.5) {
    d = (texture(u_flow, uv).rg * 2.0 - 1.0) * u_flowUV;
  }
  float va = texture(u_a, clamp(uv - d * u_mix,         vec2(0.0), vec2(1.0))).r;
  float vb = texture(u_b, clamp(uv + d * (1.0 - u_mix), vec2(0.0), vec2(1.0))).r;
  float value = mix(va, vb, u_mix);

  // The ramp is premultiplied, matching MapLibre's blend function.
  fragColor = texture(u_ramp, vec2(clamp(value, 0.0, 1.0), 0.5)) * u_opacity;
}`;

/**
 * How many tiles per frame a play press may pull at full resolution.
 *
 * Beyond this the viewport is covering enough of the composite that the overview is
 * the better trade; the current frame still sharpens to tiles when playback stops.
 */
const PREFETCH_TILE_BUDGET = 9;

/** Subdivision of the quad. Flat mercator needs only two triangles, but a modest
 *  grid keeps a steeply pitched camera from clipping the geometry oddly. */
const GRID = 16;

interface Slot {
  store: FrameStore;
  /** (west, north, east, south) in mercator 0..1 */
  box: [number, number, number, number];
}

function compile(gl: WebGL2RenderingContext, type: number, src: string): WebGLShader {
  const sh = gl.createShader(type)!;
  gl.shaderSource(sh, src);
  gl.compileShader(sh);
  if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) {
    throw new Error(`shader: ${gl.getShaderInfoLog(sh)}`);
  }
  return sh;
}

/** Build the 256-entry colour ramp, premultiplied. */
function rampPixels(stops: ColorStop[]): Uint8Array {
  const out = new Uint8Array(256 * 4);
  const sorted = [...stops].sort((a, b) => a.at - b.at);
  for (let i = 0; i < 256; i++) {
    const t = i / 255;
    let lo = sorted[0];
    let hi = sorted[sorted.length - 1];
    for (let s = 0; s < sorted.length - 1; s++) {
      if (t >= sorted[s].at && t <= sorted[s + 1].at) {
        lo = sorted[s];
        hi = sorted[s + 1];
        break;
      }
    }
    const span = hi.at - lo.at;
    const f = span > 0 ? (t - lo.at) / span : 0;
    const r = lo.r + (hi.r - lo.r) * f;
    const g = lo.g + (hi.g - lo.g) * f;
    const b = lo.b + (hi.b - lo.b) * f;
    const a = lo.a + (hi.a - lo.a) * f;
    out[i * 4 + 0] = Math.round(r * a);
    out[i * 4 + 1] = Math.round(g * a);
    out[i * 4 + 2] = Math.round(b * a);
    out[i * 4 + 3] = Math.round(a * 255);
  }
  return out;
}

export class RadarLayer implements CustomLayerInterface {
  readonly id = 'precipitation';
  readonly type = 'custom' as const;
  readonly renderingMode = '2d' as const;

  private gl: WebGL2RenderingContext | null = null;
  private map: MapLibreMap | null = null;
  private program: WebGLProgram | null = null;
  private vao: WebGLVertexArrayObject | null = null;
  private vbo: WebGLBuffer | null = null;
  private ibo: WebGLBuffer | null = null;
  private indexCount = 0;
  private ramp: WebGLTexture | null = null;
  private blank: WebGLTexture | null = null;
  private uniforms = new Map<string, WebGLUniformLocation | null>();

  private slot: Slot | null = null;
  private pending: RadarState | null = null;

  /** Currently displayed time, epoch milliseconds. */
  private timeMs = Date.now();
  opacity = 1;

  onAdd(map: MapLibreMap, gl: WebGL2RenderingContext): void {
    this.map = map;
    this.gl = gl;

    const program = gl.createProgram()!;
    gl.attachShader(program, compile(gl, gl.VERTEX_SHADER, VERTEX_SRC));
    gl.attachShader(program, compile(gl, gl.FRAGMENT_SHADER, FRAGMENT_SRC));
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
      throw new Error(`link: ${gl.getProgramInfoLog(program)}`);
    }
    this.program = program;

    // Unit quad, subdivided.
    const verts: number[] = [];
    const idx: number[] = [];
    for (let y = 0; y <= GRID; y++) {
      for (let x = 0; x <= GRID; x++) verts.push(x / GRID, y / GRID);
    }
    for (let y = 0; y < GRID; y++) {
      for (let x = 0; x < GRID; x++) {
        const a = y * (GRID + 1) + x;
        const b = a + 1;
        const c = a + GRID + 1;
        const d = c + 1;
        idx.push(a, b, c, b, d, c);
      }
    }
    this.indexCount = idx.length;

    this.vao = gl.createVertexArray();
    gl.bindVertexArray(this.vao);
    this.vbo = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, this.vbo);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array(verts), gl.STATIC_DRAW);
    const loc = gl.getAttribLocation(program, 'a_pos');
    gl.enableVertexAttribArray(loc);
    gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
    this.ibo = gl.createBuffer();
    gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, this.ibo);
    gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, new Uint16Array(idx), gl.STATIC_DRAW);
    gl.bindVertexArray(null);

    // A 1x1 stand-in so every sampler has a valid binding even when a slot is idle.
    this.blank = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, this.blank);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, 1, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE, new Uint8Array([0, 0, 0, 0]));
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);

    if (this.pending) {
      const s = this.pending;
      this.pending = null;
      this.setState(s);
    }
  }

  onRemove(): void {
    const gl = this.gl;
    if (!gl) return;
    this.slot?.store.dispose();
    this.slot = null;
    if (this.program) gl.deleteProgram(this.program);
    if (this.vao) gl.deleteVertexArray(this.vao);
    if (this.vbo) gl.deleteBuffer(this.vbo);
    if (this.ibo) gl.deleteBuffer(this.ibo);
    if (this.ramp) gl.deleteTexture(this.ramp);
    if (this.blank) gl.deleteTexture(this.blank);
    this.gl = null;
  }

  /** Replace the layer data. The store is reused when the run has not changed. */
  setState(state: RadarState): void {
    const gl = this.gl;
    if (!gl) {
      this.pending = state;
      return;
    }
    this.setRamp(state.colorScale);

    const layer = state.layers[0];
    const same = layer && this.slot?.store.layer.run === layer.run;
    if (!same) {
      this.slot?.store.dispose();
      this.slot = layer ? { store: this.newStore(layer), box: mercBox(layer) } : null;
    }
    this.repaint();
  }

  setTime(ms: number): void {
    this.timeMs = ms;
    this.repaint();
  }

  /** Frame stores, for the point readout. */
  stores(): FrameStore[] {
    return this.slot ? [this.slot.store] : [];
  }

  /**
   * True while any frame is still downloading.
   *
   * This — not overall progress — is what the spinner reflects: frames load lazily
   * around the playhead, so the stack is deliberately incomplete most of the time
   * and a progress-based spinner would simply never stop.
   */
  isLoading(): boolean {
    return this.stores().some((s) => s.pendingCount > 0);
  }

  /** How many frames are on the GPU right now. */
  loadedFrames(): number {
    return this.stores().reduce((n, s) => n + s.loadedCount, 0);
  }

  /** Fraction of the whole frame stack already on the GPU, 0..1. */
  loadProgress(): number {
    const stores = this.stores();
    if (!stores.length) return 1;
    const total = stores.reduce((n, s) => n + s.length, 0);
    const done = stores.reduce((n, s) => n + s.loadedCount, 0);
    return total ? done / total : 1;
  }

  /**
   * Warm the whole stack for playback.
   *
   * Overviews always. Tiles only when the map is genuinely zoomed in — if the
   * viewport still covers most of the grid, "only the visible tiles" is the whole
   * composite, and pulling it for every frame would undo the point of tiling. Zoomed
   * out the overview is barely two upscales away from the raster anyway, and the
   * current frame still sharpens to tiles once playback stops.
   */
  prefetchAll(): void {
    if (!this.slot || !this.map) return;
    const { detail, rect } = this.viewport(0);
    const visible = (rect.x1 - rect.x0 + 1) * (rect.y1 - rect.y0 + 1);
    // An absolute cap, not a fraction of the grid: this bounds both the bytes and
    // the number of requests a single play press can start.
    const zoomedIn = detail === 'tiles' && visible <= PREFETCH_TILE_BUDGET;
    this.slot.store.prefetchAll(zoomedIn ? 'tiles' : 'overview', rect);
  }

  private newStore(layer: Layer): FrameStore {
    return new FrameStore(this.gl!, layer, () => this.repaint());
  }

  private repaint(): void {
    this.map?.triggerRepaint();
  }

  private setRamp(stops: ColorStop[]): void {
    const gl = this.gl!;
    if (!this.ramp) {
      this.ramp = gl.createTexture();
      gl.bindTexture(gl.TEXTURE_2D, this.ramp);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    }
    gl.bindTexture(gl.TEXTURE_2D, this.ramp);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, 256, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE, rampPixels(stops));
  }

  private uni(name: string): WebGLUniformLocation | null {
    if (!this.uniforms.has(name)) {
      this.uniforms.set(name, this.gl!.getUniformLocation(this.program!, name));
    }
    return this.uniforms.get(name)!;
  }

  /**
   * Which form of the frames this view needs, and which tiles it can see.
   *
   * The overview carries one tile's worth of pixels for the whole composite, so it is
   * enough until the layer is drawn larger than that on screen. Past that point the
   * viewport is clipped to the layer and turned into a tile rectangle, with a
   * one-tile margin so a small pan does not stall on a fetch.
   */
  private viewport(margin = 1): { detail: Detail; rect: TileRect } {
    const slot = this.slot!;
    const map = this.map!;
    const t = slot.store.layer.tiling;
    const [w, n, e, s] = slot.box;

    const left = map.project([slot.store.layer.bounds.west, slot.store.layer.bounds.north]);
    const right = map.project([slot.store.layer.bounds.east, slot.store.layer.bounds.south]);
    const onScreen = Math.abs(right.x - left.x);
    // A little headroom before committing to sixteen times the data.
    if (onScreen <= t.overviewWidth * 1.4) {
      return { detail: 'overview', rect: { x0: 0, y0: 0, x1: -1, y1: -1 } };
    }

    const b = map.getBounds();
    const nw = MercatorCoordinate.fromLngLat({ lng: b.getWest(), lat: b.getNorth() });
    const se = MercatorCoordinate.fromLngLat({ lng: b.getEast(), lat: b.getSouth() });
    const u0 = (nw.x - w) / (e - w);
    const u1 = (se.x - w) / (e - w);
    const v0 = (nw.y - n) / (s - n);
    const v1 = (se.y - n) / (s - n);

    const cols = t.cols;
    const rows = t.rows;
    const clamp = (v: number, max: number) => Math.min(max, Math.max(0, v));
    return {
      detail: 'tiles',
      rect: {
        x0: clamp(Math.floor(u0 * cols) - margin, cols - 1),
        x1: clamp(Math.ceil(u1 * cols - 1) + margin, cols - 1),
        y0: clamp(Math.floor(v0 * rows) - margin, rows - 1),
        y1: clamp(Math.ceil(v1 * rows - 1) + margin, rows - 1),
      },
    };
  }

  render(gl: WebGL2RenderingContext, options: CustomRenderMethodInput): void {
    const slot = this.slot;
    if (!this.program || !slot || !this.map) return;

    const { store, box } = slot;
    const { detail, rect } = this.viewport();
    const pos = store.positionAt(this.timeMs);
    let i = Math.floor(pos);
    let f = pos - i;

    store.prefetch(i - 1, i + 3, detail, rect);

    let texA = store.frameTex(i, detail, rect);
    let texB = store.frameTex(i + 1, detail, rect);
    let flow: WebGLTexture | null = null;

    // While the exact neighbouring frames are still loading, show the nearest
    // available one without interpolation — a slightly stale picture beats a hole in
    // the map while scrubbing.
    if (!texA) {
      const near = store.nearestLoaded(i);
      if (near < 0) return;
      texA = store.frameTex(near, detail, rect);
      texB = texA;
      f = 0;
    } else if (!texB) {
      texB = texA;
      f = 0;
    } else {
      flow = store.flowTex(i);
    }

    gl.useProgram(this.program);
    gl.bindVertexArray(this.vao);
    gl.disable(gl.DEPTH_TEST);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);

    gl.uniformMatrix4fv(
      this.uni('u_matrix'),
      false,
      options.defaultProjectionData.mainMatrix as Float32Array,
    );
    gl.uniform1f(this.uni('u_opacity'), this.opacity);
    gl.uniform4f(this.uni('u_box'), box[0], box[1], box[2], box[3]);
    gl.uniform1f(this.uni('u_mix'), f);
    gl.uniform1f(this.uni('u_useFlow'), flow ? 1 : 0);
    if (flow) {
      // Motion fields are expressed in layer pixels; the shader works in uv.
      gl.uniform2f(
        this.uni('u_flowUV'),
        store.layer.flowMaxPixels / store.layer.width,
        store.layer.flowMaxPixels / store.layer.height,
      );
    }

    const bind = (unit: number, tex: WebGLTexture | null, name: string) => {
      gl.activeTexture(gl.TEXTURE0 + unit);
      gl.bindTexture(gl.TEXTURE_2D, tex ?? this.blank);
      gl.uniform1i(this.uni(name), unit);
    };
    bind(0, this.ramp, 'u_ramp');
    bind(1, texA, 'u_a');
    bind(2, texB, 'u_b');
    bind(3, flow, 'u_flow');

    gl.drawElements(gl.TRIANGLES, this.indexCount, gl.UNSIGNED_SHORT, 0);
    gl.bindVertexArray(null);
  }
}

/** Layer extent as (west, north, east, south) in mercator 0..1. */
export function mercBox(layer: Layer): [number, number, number, number] {
  const nw = MercatorCoordinate.fromLngLat({ lng: layer.bounds.west, lat: layer.bounds.north });
  const se = MercatorCoordinate.fromLngLat({ lng: layer.bounds.east, lat: layer.bounds.south });
  return [nw.x, nw.y, se.x, se.y];
}
