//! Motion estimation between consecutive precipitation frames.
//!
//! The source products only give us a value every 5 minutes (DWD) or 15 minutes
//! (Open-Meteo). Cross-fading between them makes rain appear to dissolve in one
//! place and condense in another. What we want is for a shower to *travel*.
//!
//! So for each pair of neighbouring frames we estimate a coarse displacement field
//! by block matching, ship it alongside the frames, and let the client's shader warp
//! both frames towards the requested in-between time before blending them. That is
//! the same idea as optical-flow frame interpolation in video, applied to rain.
//!
//! The field is deliberately coarse (a vector every ~32 output pixels): rain fields
//! advect coherently at these scales, and a smooth field avoids the tearing a noisy
//! per-pixel estimate would produce.

use anyhow::Result;

use crate::raster::encode_rgb;

/// Downsampling factor applied before matching.
const DOWN: usize = 4;
/// Half-width of the correlation window, in downsampled pixels.
const WIN_RADIUS: i32 = 6;
/// Spacing of output vectors, in downsampled pixels.
const STRIDE: usize = 8;
/// Maximum searched displacement, in downsampled pixels.
const SEARCH: i32 = 5;

/// The largest displacement representable in the encoded field, in full-resolution
/// output pixels. The client needs this to decode.
pub const FLOW_MAX_PX: f64 = (SEARCH as usize * DOWN) as f64;

pub struct MotionField {
    pub width: u32,
    pub height: u32,
    /// Interleaved RGB: R = x displacement, G = y displacement, both centred on 128.
    pub rgb: Vec<u8>,
}

impl MotionField {
    pub fn encode(&self) -> Result<Vec<u8>> {
        encode_rgb(&self.rgb, self.width, self.height)
    }

    /// A field with no motion anywhere — used when a pair cannot be matched.
    pub fn zero() -> Self {
        Self {
            width: 1,
            height: 1,
            rgb: vec![128, 128, 0],
        }
    }
}

/// Estimate the displacement taking frame `a` to frame `b`.
///
/// Both inputs are encoded intensity bytes (see [`crate::raster::encode_intensity`])
/// of the same `width` x `height` raster.
pub fn estimate(a: &[u8], b: &[u8], width: u32, height: u32) -> MotionField {
    let (cw, ch) = (width as usize / DOWN, height as usize / DOWN);
    if cw < 4 || ch < 4 {
        return MotionField::zero();
    }
    let ca = downsample(a, width as usize, height as usize, cw, ch);
    let cb = downsample(b, width as usize, height as usize, cw, ch);

    let nbx = (cw / STRIDE).max(1);
    let nby = (ch / STRIDE).max(1);
    let mut vx = vec![0.0f32; nbx * nby];
    let mut vy = vec![0.0f32; nbx * nby];
    let mut valid = vec![false; nbx * nby];

    for by in 0..nby {
        for bx in 0..nbx {
            let cx = (bx * STRIDE + STRIDE / 2) as i32;
            let cy = (by * STRIDE + STRIDE / 2) as i32;
            if let Some((dx, dy)) = match_block(&ca, &cb, cw, ch, cx, cy) {
                let i = by * nbx + bx;
                vx[i] = dx;
                vy[i] = dy;
                valid[i] = true;
            }
        }
    }

    // Blocks without enough signal inherit the field's dominant motion rather than
    // sitting at zero, which would otherwise shear the interpolation at the edge of
    // a shower.
    let (gx, gy) = median_vector(&vx, &vy, &valid);
    for i in 0..vx.len() {
        if !valid[i] {
            vx[i] = gx;
            vy[i] = gy;
        }
    }

    smooth(&mut vx, nbx, nby);
    smooth(&mut vy, nbx, nby);

    let scale = DOWN as f32; // downsampled pixels -> output pixels
    let mut rgb = Vec::with_capacity(nbx * nby * 3);
    for i in 0..nbx * nby {
        rgb.push(encode_component(vx[i] * scale));
        rgb.push(encode_component(vy[i] * scale));
        rgb.push(0);
    }

    MotionField {
        width: nbx as u32,
        height: nby as u32,
        rgb,
    }
}

#[inline]
fn encode_component(px: f32) -> u8 {
    let t = (px as f64 / FLOW_MAX_PX).clamp(-1.0, 1.0);
    (128.0 + t * 127.0).round().clamp(0.0, 255.0) as u8
}

fn downsample(src: &[u8], w: usize, h: usize, cw: usize, ch: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; cw * ch];
    for y in 0..ch {
        for x in 0..cw {
            let mut sum = 0u32;
            for dy in 0..DOWN {
                let sy = y * DOWN + dy;
                if sy >= h {
                    continue;
                }
                let row = sy * w;
                for dx in 0..DOWN {
                    let sx = x * DOWN + dx;
                    if sx >= w {
                        continue;
                    }
                    sum += src[row + sx] as u32;
                }
            }
            out[y * cw + x] = sum as f32 / (DOWN * DOWN) as f32;
        }
    }
    out
}

/// Sum of absolute differences over a window, minimised over the search range.
/// Returns `None` when the window carries too little precipitation to match on.
fn match_block(a: &[f32], b: &[f32], w: usize, h: usize, cx: i32, cy: i32) -> Option<(f32, f32)> {
    let x0 = (cx - WIN_RADIUS).max(0);
    let x1 = (cx + WIN_RADIUS).min(w as i32 - 1);
    let y0 = (cy - WIN_RADIUS).max(0);
    let y1 = (cy + WIN_RADIUS).min(h as i32 - 1);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    let mut energy = 0.0f32;
    for y in y0..=y1 {
        for x in x0..=x1 {
            energy += a[y as usize * w + x as usize];
        }
    }
    // Roughly "the window is more than faintly wet somewhere".
    let n = ((x1 - x0 + 1) * (y1 - y0 + 1)) as f32;
    if energy < 2.0 * n {
        return None;
    }

    let mut best = f32::INFINITY;
    let mut best_d = (0i32, 0i32);
    // Keep the SAD around the optimum so the result can be refined to sub-pixel.
    let mut sads = vec![f32::INFINITY; ((2 * SEARCH + 1) * (2 * SEARCH + 1)) as usize];

    for dy in -SEARCH..=SEARCH {
        for dx in -SEARCH..=SEARCH {
            let mut sad = 0.0f32;
            let mut count = 0.0f32;
            for y in y0..=y1 {
                let sy = y + dy;
                if sy < 0 || sy >= h as i32 {
                    continue;
                }
                for x in x0..=x1 {
                    let sx = x + dx;
                    if sx < 0 || sx >= w as i32 {
                        continue;
                    }
                    sad +=
                        (a[y as usize * w + x as usize] - b[sy as usize * w + sx as usize]).abs();
                    count += 1.0;
                }
            }
            if count < n * 0.5 {
                continue;
            }
            let sad = sad / count;
            sads[((dy + SEARCH) * (2 * SEARCH + 1) + (dx + SEARCH)) as usize] = sad;
            if sad < best {
                best = sad;
                best_d = (dx, dy);
            }
        }
    }
    if !best.is_finite() {
        return None;
    }

    let at = |dx: i32, dy: i32| -> f32 {
        if dx.abs() > SEARCH || dy.abs() > SEARCH {
            return f32::INFINITY;
        }
        sads[((dy + SEARCH) * (2 * SEARCH + 1) + (dx + SEARCH)) as usize]
    };
    let (dx, dy) = best_d;
    Some((
        dx as f32 + parabolic(at(dx - 1, dy), best, at(dx + 1, dy)),
        dy as f32 + parabolic(at(dx, dy - 1), best, at(dx, dy + 1)),
    ))
}

/// Sub-pixel offset of the minimum of a parabola through three samples.
#[inline]
fn parabolic(left: f32, centre: f32, right: f32) -> f32 {
    if !left.is_finite() || !right.is_finite() {
        return 0.0;
    }
    let denom = left - 2.0 * centre + right;
    if denom.abs() < 1e-6 {
        return 0.0;
    }
    (0.5 * (left - right) / denom).clamp(-0.5, 0.5)
}

fn median_vector(vx: &[f32], vy: &[f32], valid: &[bool]) -> (f32, f32) {
    let mut xs: Vec<f32> = vx
        .iter()
        .zip(valid)
        .filter(|(_, v)| **v)
        .map(|(a, _)| *a)
        .collect();
    let mut ys: Vec<f32> = vy
        .iter()
        .zip(valid)
        .filter(|(_, v)| **v)
        .map(|(a, _)| *a)
        .collect();
    if xs.is_empty() {
        return (0.0, 0.0);
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (xs[xs.len() / 2], ys[ys.len() / 2])
}

fn smooth(v: &mut [f32], w: usize, h: usize) {
    if w < 3 || h < 3 {
        return;
    }
    let src = v.to_vec();
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0.0;
            let mut n = 0.0;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let (xx, yy) = (x as i32 + dx, y as i32 + dy);
                    if xx < 0 || yy < 0 || xx >= w as i32 || yy >= h as i32 {
                        continue;
                    }
                    sum += src[yy as usize * w + xx as usize];
                    n += 1.0;
                }
            }
            v[y * w + x] = sum / n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paint a blob of "rain" centred on `(cx, cy)`.
    fn blob(w: usize, h: usize, cx: f32, cy: f32, r: f32) -> Vec<u8> {
        let mut v = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
                v[y * w + x] = if d < r {
                    (200.0 * (1.0 - d / r)).max(40.0) as u8
                } else {
                    0
                };
            }
        }
        v
    }

    fn decode(m: &MotionField, bx: usize, by: usize) -> (f32, f32) {
        let i = (by * m.width as usize + bx) * 3;
        (
            (m.rgb[i] as f64 - 128.0) as f32 / 127.0 * FLOW_MAX_PX as f32,
            (m.rgb[i + 1] as f64 - 128.0) as f32 / 127.0 * FLOW_MAX_PX as f32,
        )
    }

    #[test]
    fn recovers_a_known_translation() {
        let (w, h) = (256usize, 256usize);
        let a = blob(w, h, 100.0, 128.0, 40.0);
        // Shift 12 px east, 8 px south.
        let b = blob(w, h, 112.0, 136.0, 40.0);
        let m = estimate(&a, &b, w as u32, h as u32);

        // Sample near the blob centre.
        let bx = (100 / DOWN) / STRIDE;
        let by = (128 / DOWN) / STRIDE;
        let (dx, dy) = decode(&m, bx, by);
        assert!((dx - 12.0).abs() < 4.0, "dx = {dx}, expected ~12");
        assert!((dy - 8.0).abs() < 4.0, "dy = {dy}, expected ~8");
    }

    #[test]
    fn empty_frames_give_no_motion() {
        let (w, h) = (128u32, 128u32);
        let a = vec![0u8; (w * h) as usize];
        let m = estimate(&a, &a, w, h);
        assert!(m.rgb.chunks(3).all(|c| c[0] == 128 && c[1] == 128));
    }

    #[test]
    fn tiny_rasters_degrade_gracefully() {
        let a = vec![10u8; 4];
        let m = estimate(&a, &a, 2, 2);
        assert_eq!((m.width, m.height), (1, 1));
    }
}
