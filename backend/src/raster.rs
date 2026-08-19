//! Resampling source grids into Web Mercator rasters, and encoding them as PNG.
//!
//! Both ingest paths resample the *same* geometry for every timestep of a run, so
//! the expensive part — the per-output-pixel inverse projection — is computed once
//! into a [`ResampleMap`] and then reused as a plain gather. That turns a 25-frame
//! run from tens of seconds of trigonometry into a few hundred milliseconds.

use anyhow::Result;

use crate::proj::{self, MercBox};

/// Rain rate that maps to encoded byte 1. Below this we emit 0 (fully transparent).
pub const INTENSITY_MIN_MMH: f32 = 0.08;
/// Rain rate that saturates the scale at byte 255.
pub const INTENSITY_MAX_MMH: f32 = 120.0;

/// Encode a rain rate onto the shared logarithmic 0..255 scale.
///
/// Logarithmic because rain rate spans four orders of magnitude and because the
/// client interpolates these bytes directly — in log space a linear blend between
/// two frames stays perceptually sensible, and a fade from "nothing" to "drizzle"
/// ramps smoothly instead of jumping.
#[inline]
pub fn encode_intensity(mm_h: f32) -> u8 {
    if !mm_h.is_finite() || mm_h < INTENSITY_MIN_MMH {
        return 0;
    }
    let lo = INTENSITY_MIN_MMH.ln();
    let hi = INTENSITY_MAX_MMH.ln();
    let t = ((mm_h.ln() - lo) / (hi - lo)).clamp(0.0, 1.0);
    // 1..=255, so that byte 0 stays reserved for "no precipitation".
    (1.0 + t * 254.0).round().clamp(1.0, 255.0) as u8
}

/// A precomputed bilinear gather from a source grid into a Mercator raster.
pub struct ResampleMap {
    pub width: u32,
    pub height: u32,
    pub merc: MercBox,
    /// Source index of the top-left contributor, or `u32::MAX` when the output pixel
    /// falls outside the source grid entirely.
    base: Vec<u32>,
    /// Fractional position inside the source cell, `(fx, fy)`.
    frac: Vec<(f32, f32)>,
    src_w: usize,
    src_h: usize,
}

impl ResampleMap {
    /// Build a map from a source grid addressed by a caller-supplied
    /// `lonlat -> (col, row)` function.
    ///
    /// `row` must count in the same direction the source data is stored in.
    pub fn build<F>(
        merc: MercBox,
        width: u32,
        height: u32,
        src_w: usize,
        src_h: usize,
        to_src: F,
    ) -> Self
    where
        F: Fn(f64, f64) -> (f64, f64),
    {
        let n = (width as usize) * (height as usize);
        let mut base = vec![u32::MAX; n];
        let mut frac = vec![(0.0f32, 0.0f32); n];

        let px = merc.width() / width as f64;
        let py = merc.height() / height as f64;

        for row in 0..height as usize {
            // Row 0 is the north edge; Mercator y grows northwards.
            let my = merc.y1 - (row as f64 + 0.5) * py;
            let lat = proj::merc_lat(my);
            for col in 0..width as usize {
                let mx = merc.x0 + (col as f64 + 0.5) * px;
                let lon = proj::merc_lon(mx);
                let (sc, sr) = to_src(lon, lat);

                let c0 = sc.floor();
                let r0 = sr.floor();
                if c0 < -1.0 || r0 < -1.0 || c0 >= src_w as f64 || r0 >= src_h as f64 {
                    continue;
                }
                let idx = row * width as usize + col;
                // Clamp the anchor into range; out-of-range corners are dropped by
                // the weight normalisation in `apply`.
                let ci = c0.max(0.0) as usize;
                let ri = r0.max(0.0) as usize;
                base[idx] = (ri * src_w + ci) as u32;
                frac[idx] = ((sc - c0) as f32, (sr - r0) as f32);
            }
        }

        Self {
            width,
            height,
            merc,
            base,
            frac,
            src_w,
            src_h,
        }
    }

    /// Gather one source field into a freshly allocated Mercator raster.
    #[cfg(test)]
    pub fn apply(&self, src: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        self.apply_into(src, &mut out);
        out
    }

    /// Gather into a caller-owned buffer.
    ///
    /// Every timestep of a run resamples the same geometry, so reusing one buffer
    /// across all of them keeps a 25-frame ingest from cycling six megabytes of
    /// allocation twenty-five times over.
    ///
    /// Bilinear over the valid neighbours only, with the weights renormalised — so a
    /// pixel next to the edge of radar coverage keeps its value instead of being
    /// dragged towards zero by a `NaN` neighbour.
    pub fn apply_into(&self, src: &[f32], out: &mut Vec<f32>) {
        let n = (self.width as usize) * (self.height as usize);
        out.clear();
        out.resize(n, f32::NAN);
        for (i, slot) in out.iter_mut().enumerate() {
            let b = self.base[i];
            if b == u32::MAX {
                continue;
            }
            let b = b as usize;
            let ci = b % self.src_w;
            let ri = b / self.src_w;
            let (fx, fy) = self.frac[i];

            let mut acc = 0.0f32;
            let mut wsum = 0.0f32;
            for (dr, wr) in [(0usize, 1.0 - fy), (1, fy)] {
                let r = ri + dr;
                if r >= self.src_h || wr <= 0.0 {
                    continue;
                }
                for (dc, wc) in [(0usize, 1.0 - fx), (1, fx)] {
                    let c = ci + dc;
                    if c >= self.src_w || wc <= 0.0 {
                        continue;
                    }
                    let v = src[r * self.src_w + c];
                    if v.is_nan() {
                        continue;
                    }
                    let w = wr * wc;
                    acc += v * w;
                    wsum += w;
                }
            }
            if wsum > 0.0 {
                *slot = acc / wsum;
            }
        }
    }
}

/// Turn a Mercator rain-rate raster into the encoded 0..255 intensity bytes.
/// `NaN` (no data) encodes as 0, same as "no rain" — the coverage mask is what
/// distinguishes them on the client.
pub fn encode_field(field: &[f32]) -> Vec<u8> {
    field.iter().map(|&v| encode_intensity(v)).collect()
}

/// Coverage mask: 255 where the source had data, 0 where it did not.
///
/// Only the point readout consumes this. The map itself needs no mask: outside
/// coverage the encoded intensity is zero, and the colour ramp is transparent there.
pub fn coverage_mask(field: &[f32]) -> Vec<u8> {
    field
        .iter()
        .map(|v| if v.is_nan() { 0 } else { 255 })
        .collect()
}

/// How a frame is cut up for delivery.
///
/// The map almost never shows the whole composite: zoomed in on a town, a client
/// that fetched all of Germany would be throwing away 98 % of what it downloaded.
/// So each frame ships as a grid of tiles plus one downscaled overview, and the
/// client takes only what its viewport actually covers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tiling {
    pub cols: u32,
    pub rows: u32,
    /// Nominal tile size; the last column and row may come out smaller.
    pub tile_w: u32,
    pub tile_h: u32,
    pub overview_w: u32,
    pub overview_h: u32,
}

/// How much the overview is reduced. Independent of the tile grid on purpose: the
/// grid should be fine enough that a zoomed-in viewport touches few tiles, while the
/// overview has to stay sharp enough for a zoomed-*out* map — tying the two together
/// means one of them is always wrong.
const OVERVIEW_DIV: u32 = 4;

impl Tiling {
    /// Choose a grid whose tiles are close to `target` pixels a side.
    pub fn choose(width: u32, height: u32, target: u32) -> Self {
        let target = target.max(64);
        let cols = width.div_ceil(target).max(1);
        let rows = height.div_ceil(target).max(1);
        Self {
            cols,
            rows,
            tile_w: width.div_ceil(cols),
            tile_h: height.div_ceil(rows),
            overview_w: width.div_ceil(OVERVIEW_DIV).max(1),
            overview_h: height.div_ceil(OVERVIEW_DIV).max(1),
        }
    }

    /// Extent of one tile, clamped to the raster.
    pub fn tile_rect(&self, width: u32, height: u32, tx: u32, ty: u32) -> (u32, u32, u32, u32) {
        let x0 = tx * self.tile_w;
        let y0 = ty * self.tile_h;
        let w = self.tile_w.min(width.saturating_sub(x0));
        let h = self.tile_h.min(height.saturating_sub(y0));
        (x0, y0, w, h)
    }
}

/// Copy one tile out of a raster.
pub fn crop(data: &[u8], width: u32, x0: u32, y0: u32, w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h) as usize);
    for row in 0..h {
        let start = ((y0 + row) * width + x0) as usize;
        out.extend_from_slice(&data[start..start + w as usize]);
    }
    out
}

/// Box-average a raster down to `dst_w` x `dst_h`.
///
/// Averaging the log-encoded bytes rather than the rain rates is deliberate: it is
/// exactly what the GPU does when it minifies the full-resolution texture, so the
/// overview and the detailed view agree instead of one looking heavier.
pub fn downscale(data: &[u8], width: u32, height: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (dst_w * dst_h) as usize];
    for dy in 0..dst_h {
        let y0 = (dy as u64 * height as u64 / dst_h as u64) as u32;
        let y1 = (((dy + 1) as u64 * height as u64).div_ceil(dst_h as u64) as u32)
            .max(y0 + 1)
            .min(height);
        for dx in 0..dst_w {
            let x0 = (dx as u64 * width as u64 / dst_w as u64) as u32;
            let x1 = (((dx + 1) as u64 * width as u64).div_ceil(dst_w as u64) as u32)
                .max(x0 + 1)
                .min(width);
            let mut sum = 0u32;
            let mut n = 0u32;
            for y in y0..y1 {
                let row = (y * width) as usize;
                for x in x0..x1 {
                    sum += data[row + x as usize] as u32;
                    n += 1;
                }
            }
            out[(dy * dst_w + dx) as usize] = if n > 0 { (sum / n) as u8 } else { 0 };
        }
    }
    out
}

/// Cut an encoded raster into its tiles and overview, ready to publish.
pub fn split(data: &[u8], width: u32, height: u32, t: &Tiling) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
    let overview = encode_gray(
        &downscale(data, width, height, t.overview_w, t.overview_h),
        t.overview_w,
        t.overview_h,
    )?;
    let mut tiles = Vec::with_capacity((t.cols * t.rows) as usize);
    for ty in 0..t.rows {
        for tx in 0..t.cols {
            let (x0, y0, w, h) = t.tile_rect(width, height, tx, ty);
            tiles.push(encode_gray(&crop(data, width, x0, y0, w, h), w, h)?);
        }
    }
    Ok((overview, tiles))
}

/// Encode a single-channel raster losslessly.
///
/// WebP rather than PNG: measured across a full 41-frame run it is 26 % smaller for
/// bit-identical output, because its spatial predictors suit a rain field better
/// than PNG's row filters. Lossy encoding was rejected — at quality 92 individual
/// pixels drifted by up to 16 % in mm/h, and the point readout decodes these bytes
/// as data, not as a picture.
///
/// Single channel on purpose: an alpha channel would make the browser build a
/// premultiplied texture, silently corrupting values that are not colour.
pub fn encode_gray(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    image_webp::WebPEncoder::new(&mut out)
        .encode(data, width, height, image_webp::ColorType::L8)
        .map_err(|e| anyhow::anyhow!("WebP encode failed: {e}"))?;
    Ok(out)
}

/// Encode an interleaved RGB raster losslessly. Used for motion fields, again
/// avoiding an alpha channel so no premultiplication can touch the values.
pub fn encode_rgb(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    image_webp::WebPEncoder::new(&mut out)
        .encode(data, width, height, image_webp::ColorType::Rgb8)
        .map_err(|e| anyhow::anyhow!("WebP encode failed: {e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intensity_scale_is_monotonic_and_bounded() {
        assert_eq!(encode_intensity(0.0), 0);
        assert_eq!(encode_intensity(f32::NAN), 0);
        assert_eq!(encode_intensity(INTENSITY_MIN_MMH / 2.0), 0);
        assert_eq!(encode_intensity(INTENSITY_MIN_MMH), 1);
        assert_eq!(encode_intensity(INTENSITY_MAX_MMH), 255);
        assert_eq!(encode_intensity(1e6), 255);
        let mut prev = 0u8;
        for i in 0..500 {
            let v = 0.05 * 1.02f32.powi(i);
            let b = encode_intensity(v);
            assert!(b >= prev, "not monotonic at {v}");
            prev = b;
        }
    }

    #[test]
    fn resample_identity_grid() {
        // A source grid whose "projection" is the identity on a degree box.
        let merc = MercBox::from_lonlat(0.0, 0.0, 4.0, 4.0);
        let map = ResampleMap::build(merc, 8, 8, 4, 4, |lon, lat| (lon, 4.0 - lat));
        let src: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let out = map.apply(&src);
        assert_eq!(out.len(), 64);
        assert!(out.iter().all(|v| !v.is_nan()));
        // North-west corner draws from the low indices, south-east from the high ones.
        assert!(out[0] < out[63]);
    }

    #[test]
    fn resample_skips_nan_neighbours() {
        let merc = MercBox::from_lonlat(0.0, 0.0, 2.0, 2.0);
        let map = ResampleMap::build(merc, 4, 4, 2, 2, |lon, lat| (lon, 2.0 - lat));
        let src = vec![5.0f32, f32::NAN, f32::NAN, f32::NAN];
        let out = map.apply(&src);
        // Every pixel that sees the one valid cell takes its value undiluted.
        for v in out.iter().filter(|v| !v.is_nan()) {
            assert!((v - 5.0).abs() < 1e-5, "got {v}");
        }
    }

    /// Decode a WebP back to its luminance bytes.
    fn decode_gray(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut dec = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes)).unwrap();
        let (w, h) = dec.dimensions();
        let mut buf = vec![0u8; dec.output_buffer_size().unwrap()];
        dec.read_image(&mut buf).unwrap();
        // WebP always decodes to RGB(A); the luminance sits in every channel.
        let step = buf.len() / (w as usize * h as usize);
        (w, h, buf.iter().step_by(step).copied().collect())
    }

    #[test]
    fn tiling_covers_the_raster_exactly_once() {
        let (w, h) = (1251u32, 1250u32);
        let t = Tiling::choose(w, h, 320);
        assert_eq!((t.cols, t.rows), (4, 4));

        let mut covered = vec![0u8; (w * h) as usize];
        for ty in 0..t.rows {
            for tx in 0..t.cols {
                let (x0, y0, tw, th) = t.tile_rect(w, h, tx, ty);
                assert!(
                    x0 + tw <= w && y0 + th <= h,
                    "tile spills out of the raster"
                );
                for y in y0..y0 + th {
                    for x in x0..x0 + tw {
                        covered[(y * w + x) as usize] += 1;
                    }
                }
            }
        }
        assert!(
            covered.iter().all(|&c| c == 1),
            "tiles must tile, without gaps or overlap"
        );
    }

    #[test]
    fn edge_tiles_are_clipped() {
        // 100 divides into two even halves, so nothing is clipped.
        let t = Tiling::choose(100, 100, 64);
        assert_eq!((t.cols, t.rows), (2, 2));
        assert_eq!(t.tile_rect(100, 100, 1, 1), (50, 50, 50, 50));

        // 130 needs three columns of 44, which overshoots — the last one is cut.
        let t = Tiling::choose(130, 130, 64);
        assert_eq!((t.cols, t.tile_w), (3, 44));
        assert_eq!(t.tile_rect(130, 130, 1, 1), (44, 44, 44, 44));
        assert_eq!(t.tile_rect(130, 130, 2, 2), (88, 88, 42, 42));
    }

    #[test]
    fn crop_extracts_the_right_pixels() {
        // 4x3 raster of increasing values.
        let data: Vec<u8> = (0..12).collect();
        assert_eq!(crop(&data, 4, 1, 1, 2, 2), vec![5, 6, 9, 10]);
        assert_eq!(crop(&data, 4, 0, 0, 4, 1), vec![0, 1, 2, 3]);
    }

    #[test]
    fn downscale_averages_and_preserves_bounds() {
        let data = vec![0u8, 100, 100, 200, 0, 100, 100, 200];
        let out = downscale(&data, 4, 2, 2, 1);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], 50);
        assert_eq!(out[1], 150);

        // A uniform field stays uniform at any target size.
        let flat = vec![77u8; 37 * 41];
        for (w, h) in [(9u32, 10u32), (1, 1), (37, 41)] {
            let small = downscale(&flat, 37, 41, w, h);
            assert_eq!(small.len(), (w * h) as usize);
            assert!(small.iter().all(|&v| v == 77), "{w}x{h} drifted");
        }
    }

    #[test]
    fn encoding_is_bit_exact() {
        // These bytes are data, not a picture: the point readout decodes them back
        // into rain rates, so a single changed byte is a wrong forecast.
        let mut data = vec![0u8; 64 * 48];
        for (i, v) in data.iter_mut().enumerate() {
            *v = ((i * 7) % 256) as u8;
        }
        let bytes = encode_gray(&data, 64, 48).unwrap();
        let (w, h, back) = decode_gray(&bytes);
        assert_eq!((w, h), (64, 48));
        assert_eq!(back, data, "lossless encoding must round-trip exactly");
    }

    #[test]
    fn encodes_the_requested_dimensions() {
        let data = vec![7u8; 16 * 9];
        let (w, h, _) = decode_gray(&encode_gray(&data, 16, 9).unwrap());
        assert_eq!((w, h), (16, 9));
    }

    #[test]
    fn rgb_encoding_round_trips() {
        let data: Vec<u8> = (0..16 * 9 * 3).map(|i| (i % 251) as u8).collect();
        let bytes = encode_rgb(&data, 16, 9).unwrap();
        let mut dec = image_webp::WebPDecoder::new(std::io::Cursor::new(&bytes)).unwrap();
        let mut buf = vec![0u8; dec.output_buffer_size().unwrap()];
        dec.read_image(&mut buf).unwrap();
        assert_eq!(buf.len(), data.len());
        assert_eq!(buf, data, "motion fields must survive intact");
    }
}
