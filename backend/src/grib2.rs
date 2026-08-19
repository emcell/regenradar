//! A minimal GRIB2 reader — just enough for DWD's ICON-D2 precipitation files.
//!
//! Only two encodings are handled, and deliberately so:
//!
//! * **Grid definition template 3.0** — a plain regular latitude/longitude grid.
//! * **Data representation template 5.0** — "simple packing", where every value is
//!   an n-bit integer restored with `Y = (R + X · 2^E) / 10^D`.
//!
//! That covers `icon-d2_germany_regular-lat-lon_*`. It deliberately does *not* cover
//! template 5.42 (CCSDS/AEC), which DWD uses for the icosahedral ICON-D2 files and
//! for ICON-EU — decoding those would mean pulling in eccodes or writing an adaptive
//! entropy decoder, and the regular-lat-lon product makes both unnecessary.
//!
//! A bitmap (section 6) is common here: the model's rotated-pole domain does not
//! fill the lat/lon rectangle it is interpolated onto, so the corners are marked
//! missing and are not packed at all.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, TimeZone, Utc};

/// A regular latitude/longitude grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatLonGrid {
    pub ni: usize,
    pub nj: usize,
    /// Latitude/longitude of the first grid point, in degrees.
    pub lat1: f64,
    pub lon1: f64,
    /// Increments in degrees, always positive.
    pub di: f64,
    pub dj: f64,
    /// True when rows are stored south to north.
    pub south_to_north: bool,
}

impl LatLonGrid {
    pub fn len(&self) -> usize {
        self.ni * self.nj
    }

    /// Fractional grid position of a coordinate, with the row counted in storage
    /// order so it can index straight into [`Grib2Message::values`].
    #[inline]
    pub fn locate(&self, lon: f64, lat: f64) -> (f64, f64) {
        // The grid may start east of the antimeridian's 0..360 form.
        let mut dlon = lon - self.lon1;
        if dlon < -180.0 {
            dlon += 360.0;
        } else if dlon > 180.0 {
            dlon -= 360.0;
        }
        let col = dlon / self.di;
        let row = if self.south_to_north {
            (lat - self.lat1) / self.dj
        } else {
            (self.lat1 - lat) / self.dj
        };
        (col, row)
    }
}

#[derive(Debug, Clone)]
pub struct Grib2Message {
    /// Parameter category and number from section 4 (1/52 = total precipitation).
    pub category: u8,
    pub number: u8,
    /// End of the accumulation period, for statistically processed products
    /// (template 4.8). This — not the forecast hour — is what a run's fifteen-minute
    /// series is keyed on.
    pub valid_time: Option<DateTime<Utc>>,
    pub grid: LatLonGrid,
    /// Row-major in storage order, `NaN` where the bitmap marks a point missing.
    pub values: Vec<f32>,
}

#[inline]
fn u16be(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

#[inline]
fn u32be(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// GRIB2 stores signed integers sign-and-magnitude, not two's complement.
#[inline]
fn i16_grib(b: &[u8], o: usize) -> i32 {
    let raw = u16be(b, o);
    let mag = (raw & 0x7fff) as i32;
    if raw & 0x8000 != 0 {
        -mag
    } else {
        mag
    }
}

#[inline]
fn i32_grib(b: &[u8], o: usize) -> i64 {
    let raw = u32be(b, o);
    let mag = (raw & 0x7fff_ffff) as i64;
    if raw & 0x8000_0000 != 0 {
        -mag
    } else {
        mag
    }
}

/// Decode every message in a GRIB2 stream.
///
/// A DWD "hourly" precipitation file actually holds four messages — the quarter
/// hours — so callers should expect several.
pub fn parse(bytes: &[u8]) -> Result<Vec<Grib2Message>> {
    let mut out = Vec::new();
    let mut off = 0usize;

    while off + 16 <= bytes.len() {
        if &bytes[off..off + 4] != b"GRIB" {
            // Tolerate padding between messages rather than giving up on the file.
            off += 1;
            continue;
        }
        if bytes[off + 7] != 2 {
            bail!("GRIB edition {} is not supported", bytes[off + 7]);
        }
        let total = u64::from_be_bytes(bytes[off + 8..off + 16].try_into()?) as usize;
        if total == 0 || off + total > bytes.len() {
            bail!(
                "message claims {total} bytes but the stream has {}",
                bytes.len() - off
            );
        }
        out.push(parse_message(&bytes[off..off + total]).context("decoding GRIB2 message")?);
        off += total;
    }

    if out.is_empty() {
        bail!("no GRIB2 messages found");
    }
    Ok(out)
}

fn parse_message(msg: &[u8]) -> Result<Grib2Message> {
    let mut grid: Option<LatLonGrid> = None;
    let mut category = 0u8;
    let mut number = 0u8;
    let mut valid_time = None;
    let mut packing: Option<(f32, i32, i32, u8, usize)> = None;
    let mut bitmap: Option<Vec<u8>> = None;
    let mut data: Option<&[u8]> = None;

    let mut p = 16usize;
    while p + 5 <= msg.len() {
        if &msg[p..p + 4] == b"7777" {
            break;
        }
        let len = u32be(msg, p) as usize;
        if len < 5 || p + len > msg.len() {
            bail!("malformed section at offset {p}");
        }
        let s = &msg[p..p + len];
        match s[4] {
            3 => grid = Some(parse_grid(s)?),
            4 => {
                if len < 11 {
                    bail!("product section too short");
                }
                let template = u16be(s, 7);
                category = s[9];
                number = s[10];
                // Template 4.8 appends the end of the overall accumulation period.
                if template == 8 && len >= 41 {
                    let year = u16be(s, 34) as i32;
                    valid_time = Utc
                        .with_ymd_and_hms(
                            year,
                            s[36] as u32,
                            s[37] as u32,
                            s[38] as u32,
                            s[39] as u32,
                            s[40] as u32,
                        )
                        .single();
                }
            }
            5 => {
                if len < 21 {
                    bail!("data representation section too short");
                }
                let count = u32be(s, 5) as usize;
                let template = u16be(s, 9);
                if template != 0 {
                    bail!(
                        "data representation template {template} is not supported \
                         (only 0, simple packing)"
                    );
                }
                let r = f32::from_be_bytes([s[11], s[12], s[13], s[14]]);
                packing = Some((r, i16_grib(s, 15), i16_grib(s, 17), s[19], count));
            }
            6 => {
                if len < 6 {
                    bail!("bitmap section too short");
                }
                // 255 means "no bitmap"; anything else than 0 would be a predefined
                // map we have no way to resolve.
                match s[5] {
                    255 => {}
                    0 => bitmap = Some(s[6..].to_vec()),
                    other => bail!("predefined bitmap {other} is not supported"),
                }
            }
            7 => data = Some(&s[5..]),
            _ => {}
        }
        p += len;
    }

    let grid = grid.context("no grid definition (section 3)")?;
    let (r, e, d, bits, packed_count) = packing.context("no data representation (section 5)")?;
    let data = data.context("no data section (section 7)")?;

    let values = unpack(&grid, data, bitmap.as_deref(), r, e, d, bits, packed_count)?;
    Ok(Grib2Message {
        category,
        number,
        valid_time,
        grid,
        values,
    })
}

fn parse_grid(s: &[u8]) -> Result<LatLonGrid> {
    if s.len() < 72 {
        bail!("grid section too short");
    }
    let template = u16be(s, 12);
    if template != 0 {
        bail!("grid definition template {template} is not supported (only 0, regular lat/lon)");
    }
    let ni = u32be(s, 30) as usize;
    let nj = u32be(s, 34) as usize;
    if ni == 0 || nj == 0 {
        bail!("degenerate grid {ni}x{nj}");
    }
    let micro = 1e-6;
    let lat1 = i32_grib(s, 46) as f64 * micro;
    let mut lon1 = i32_grib(s, 50) as f64 * micro;
    if lon1 > 180.0 {
        lon1 -= 360.0;
    }
    let di = (i32_grib(s, 63) as f64 * micro).abs();
    let dj = (i32_grib(s, 67) as f64 * micro).abs();
    if di <= 0.0 || dj <= 0.0 {
        bail!("grid has no increment");
    }
    // Scanning mode bit 2: set means rows run south to north.
    let south_to_north = s[71] & 0x40 != 0;
    Ok(LatLonGrid {
        ni,
        nj,
        lat1,
        lon1,
        di,
        dj,
        south_to_north,
    })
}

#[allow(clippy::too_many_arguments)]
fn unpack(
    grid: &LatLonGrid,
    data: &[u8],
    bitmap: Option<&[u8]>,
    r: f32,
    e: i32,
    d: i32,
    bits: u8,
    packed_count: usize,
) -> Result<Vec<f32>> {
    let n = grid.len();
    let scale = (2f64).powi(e) / (10f64).powi(d);
    let offset = r as f64 / (10f64).powi(d);

    // Constant field: a zero bit width means every present point equals R.
    if bits == 0 {
        let v = offset as f32;
        return Ok((0..n)
            .map(|i| if present(bitmap, i) { v } else { f32::NAN })
            .collect());
    }
    if bits > 32 {
        bail!("{bits} bits per value is out of range");
    }

    let needed = (packed_count * bits as usize).div_ceil(8);
    if data.len() < needed {
        bail!("data section holds {} bytes, need {needed}", data.len());
    }

    let mut out = Vec::with_capacity(n);
    let mut taken = 0usize;
    for i in 0..n {
        if !present(bitmap, i) {
            out.push(f32::NAN);
            continue;
        }
        if taken >= packed_count {
            out.push(f32::NAN);
            continue;
        }
        let bit = taken * bits as usize;
        out.push((offset + read_bits(data, bit, bits) as f64 * scale) as f32);
        taken += 1;
    }
    Ok(out)
}

#[inline]
fn present(bitmap: Option<&[u8]>, i: usize) -> bool {
    match bitmap {
        None => true,
        Some(bm) => {
            let byte = i / 8;
            byte < bm.len() && bm[byte] & (0x80 >> (i % 8)) != 0
        }
    }
}

/// Read `bits` bits starting at bit offset `start`, most significant bit first.
#[inline]
fn read_bits(data: &[u8], start: usize, bits: u8) -> u32 {
    let mut acc: u64 = 0;
    let mut got = 0u32;
    let mut byte = start / 8;
    let skip = (start % 8) as u32;
    while got < bits as u32 + skip {
        acc = (acc << 8) | *data.get(byte).unwrap_or(&0) as u64;
        got += 8;
        byte += 1;
    }
    let extra = got - bits as u32 - skip;
    ((acc >> extra) & ((1u64 << bits) - 1)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble a one-message GRIB2 stream around a 4x2 grid.
    fn synth(values: &[u16], bits: u8, bitmap: Option<&[u8]>, r: f32, e: i32, d: i32) -> Vec<u8> {
        let mut sec3 = vec![0u8; 72];
        sec3[4] = 3;
        sec3[12..14].copy_from_slice(&0u16.to_be_bytes()); // template 0
        sec3[30..34].copy_from_slice(&4u32.to_be_bytes()); // Ni
        sec3[34..38].copy_from_slice(&2u32.to_be_bytes()); // Nj
        sec3[46..50].copy_from_slice(&(43_180_000u32).to_be_bytes()); // La1
        sec3[50..54].copy_from_slice(&(356_060_000u32).to_be_bytes()); // Lo1 in 0..360
        sec3[63..67].copy_from_slice(&(20_000u32).to_be_bytes()); // Di = 0.02
        sec3[67..71].copy_from_slice(&(20_000u32).to_be_bytes()); // Dj
        sec3[71] = 0x40; // south to north
        let len = sec3.len() as u32;
        sec3[0..4].copy_from_slice(&len.to_be_bytes());

        let mut sec4 = vec![0u8; 58];
        sec4[4] = 4;
        sec4[7..9].copy_from_slice(&8u16.to_be_bytes()); // template 8
        sec4[9] = 1; // category
        sec4[10] = 52; // total precipitation
        sec4[34..36].copy_from_slice(&2026u16.to_be_bytes());
        sec4[36] = 8;
        sec4[37] = 17;
        sec4[38] = 3;
        sec4[39] = 15;
        sec4[40] = 0;
        let len = sec4.len() as u32;
        sec4[0..4].copy_from_slice(&len.to_be_bytes());

        let present_count = values.len();
        let mut sec5 = vec![0u8; 21];
        sec5[4] = 5;
        sec5[5..9].copy_from_slice(&(present_count as u32).to_be_bytes());
        sec5[9..11].copy_from_slice(&0u16.to_be_bytes()); // simple packing
        sec5[11..15].copy_from_slice(&r.to_be_bytes());
        let enc = |v: i32| -> [u8; 2] {
            let m = (v.unsigned_abs() as u16) & 0x7fff;
            (if v < 0 { m | 0x8000 } else { m }).to_be_bytes()
        };
        sec5[15..17].copy_from_slice(&enc(e));
        sec5[17..19].copy_from_slice(&enc(d));
        sec5[19] = bits;
        let len = sec5.len() as u32;
        sec5[0..4].copy_from_slice(&len.to_be_bytes());

        let mut sec6 = vec![0u8; 6];
        sec6[4] = 6;
        match bitmap {
            None => sec6[5] = 255,
            Some(bm) => {
                sec6[5] = 0;
                sec6.extend_from_slice(bm);
            }
        }
        let l6 = sec6.len() as u32;
        sec6[0..4].copy_from_slice(&l6.to_be_bytes());

        let mut packed = Vec::new();
        let mut acc: u64 = 0;
        let mut nbits = 0u32;
        for v in values {
            acc = (acc << bits) | *v as u64;
            nbits += bits as u32;
            while nbits >= 8 {
                nbits -= 8;
                packed.push(((acc >> nbits) & 0xff) as u8);
            }
        }
        if nbits > 0 {
            packed.push(((acc << (8 - nbits)) & 0xff) as u8);
        }
        let mut sec7 = vec![0u8; 5];
        sec7[4] = 7;
        sec7.extend_from_slice(&packed);
        let l7 = sec7.len() as u32;
        sec7[0..4].copy_from_slice(&l7.to_be_bytes());

        let mut sec1 = vec![0u8; 21];
        sec1[4] = 1;
        sec1[0..4].copy_from_slice(&21u32.to_be_bytes());

        let body_len =
            16 + sec1.len() + sec3.len() + sec4.len() + sec5.len() + sec6.len() + sec7.len() + 4;
        let mut msg = Vec::with_capacity(body_len);
        msg.extend_from_slice(b"GRIB");
        msg.extend_from_slice(&[0, 0, 0, 2]);
        msg.extend_from_slice(&(body_len as u64).to_be_bytes());
        msg.extend_from_slice(&sec1);
        msg.extend_from_slice(&sec3);
        msg.extend_from_slice(&sec4);
        msg.extend_from_slice(&sec5);
        msg.extend_from_slice(&sec6);
        msg.extend_from_slice(&sec7);
        msg.extend_from_slice(b"7777");
        msg
    }

    #[test]
    fn decodes_simple_packing() {
        // Y = (R + X * 2^E) / 10^D  ->  with R=0, E=0, D=1 that is X/10.
        let raw = synth(&[0, 10, 25, 100, 5, 0, 3, 7], 16, None, 0.0, 0, 1);
        let msgs = parse(&raw).unwrap();
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        assert_eq!((m.category, m.number), (1, 52));
        assert_eq!(m.values.len(), 8);
        assert!((m.values[1] - 1.0).abs() < 1e-5);
        assert!((m.values[3] - 10.0).abs() < 1e-5);
    }

    #[test]
    fn honours_the_binary_scale_factor() {
        // E = -1 halves each step.
        let raw = synth(&[0, 2, 4, 8, 0, 0, 0, 0], 8, None, 0.0, -1, 0);
        let m = &parse(&raw).unwrap()[0];
        assert!((m.values[1] - 1.0).abs() < 1e-5);
        assert!((m.values[3] - 4.0).abs() < 1e-5);
    }

    #[test]
    fn applies_the_reference_value() {
        let raw = synth(&[0, 1, 2, 3, 0, 0, 0, 0], 8, None, 5.0, 0, 0);
        let m = &parse(&raw).unwrap()[0];
        assert!((m.values[0] - 5.0).abs() < 1e-5);
        assert!((m.values[3] - 8.0).abs() < 1e-5);
    }

    #[test]
    fn skips_points_the_bitmap_marks_missing() {
        // Points 0 and 7 absent: 0b01111110.
        let raw = synth(
            &[10, 20, 30, 40, 50, 60],
            8,
            Some(&[0b0111_1110]),
            0.0,
            0,
            0,
        );
        let m = &parse(&raw).unwrap()[0];
        assert_eq!(m.values.len(), 8);
        assert!(m.values[0].is_nan(), "masked point must be missing");
        assert!(m.values[7].is_nan());
        assert!(
            (m.values[1] - 10.0).abs() < 1e-5,
            "packing resumes at the first present point"
        );
        assert!((m.values[6] - 60.0).abs() < 1e-5);
    }

    #[test]
    fn reads_the_accumulation_end_time() {
        let raw = synth(&[0; 8], 8, None, 0.0, 0, 0);
        let m = &parse(&raw).unwrap()[0];
        assert_eq!(
            m.valid_time.unwrap().to_rfc3339(),
            "2026-08-17T03:15:00+00:00"
        );
    }

    #[test]
    fn normalises_longitude_and_scan_order() {
        let raw = synth(&[0; 8], 8, None, 0.0, 0, 0);
        let g = parse(&raw).unwrap()[0].grid;
        assert!(
            (g.lon1 - (-3.94)).abs() < 1e-6,
            "0..360 form must become -180..180"
        );
        assert!((g.lat1 - 43.18).abs() < 1e-6);
        assert!(g.south_to_north);

        // Row 0 is the southern edge, so a point one increment north is row 1.
        let (col, row) = g.locate(-3.94 + 0.02, 43.18 + 0.02);
        assert!((col - 1.0).abs() < 1e-6 && (row - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_zero_bit_field_is_constant() {
        let raw = synth(&[], 0, None, 2.5, 0, 0);
        let m = &parse(&raw).unwrap()[0];
        assert!(m.values.iter().all(|v| (v - 2.5).abs() < 1e-5));
    }

    #[test]
    fn rejects_unsupported_compression() {
        let mut raw = synth(&[0; 8], 8, None, 0.0, 0, 0);
        // Rewrite the data representation template to 42 (CCSDS).
        let pos = raw
            .windows(5)
            .position(|w| w[4] == 5 && u32be(w, 0) == 21)
            .unwrap();
        raw[pos + 9..pos + 11].copy_from_slice(&42u16.to_be_bytes());
        let err = parse(&raw).unwrap_err().to_string();
        assert!(format!("{err:#}").contains("GRIB2") || !err.is_empty());
    }

    /// Decode a real DWD file. Opt-in because it needs one on disk:
    /// `REGENRADAR_TEST_GRIB=/path/to/file.grib2 cargo test -- --ignored`
    #[test]
    #[ignore]
    fn decodes_a_real_icon_d2_file() {
        let path = std::env::var("REGENRADAR_TEST_GRIB").expect("set REGENRADAR_TEST_GRIB");
        let bytes = std::fs::read(&path).unwrap();
        let msgs = parse(&bytes).unwrap();
        println!("{} messages", msgs.len());
        assert_eq!(msgs.len(), 4, "a DWD hourly file holds four quarter hours");

        for m in &msgs {
            let valid: Vec<f32> = m.values.iter().copied().filter(|v| !v.is_nan()).collect();
            let max = valid.iter().copied().fold(f32::MIN, f32::max);
            println!(
                "  {} | {}x{} | {} present | max {:.2} mm",
                m.valid_time.unwrap(),
                m.grid.ni,
                m.grid.nj,
                valid.len(),
                max
            );
            assert_eq!((m.category, m.number), (1, 52));
            assert_eq!(m.grid.len(), 1215 * 746);
            assert!(
                valid.len() > 700_000,
                "most of the domain should be present"
            );
            assert!(
                valid.iter().all(|v| *v >= -0.001),
                "accumulations cannot be negative"
            );
            assert!(max < 500.0, "implausible accumulation {max}");
        }

        // Quarter-hourly, and genuinely accumulating: differencing consecutive
        // messages is how a rate is derived, so they must not be copies of each
        // other and must never decrease.
        for w in msgs.windows(2) {
            let dt = w[1].valid_time.unwrap() - w[0].valid_time.unwrap();
            assert_eq!(dt.num_minutes(), 15);

            let mut grew = 0usize;
            for (a, b) in w[0].values.iter().zip(&w[1].values) {
                if a.is_nan() || b.is_nan() {
                    continue;
                }
                assert!(b - a >= -0.01, "accumulation went backwards: {a} -> {b}");
                if b - a > 0.01 {
                    grew += 1;
                }
            }
            println!("  {} points gained rain", grew);
            assert!(grew > 0, "consecutive messages are identical");
        }
    }

    #[test]
    fn rejects_a_truncated_stream() {
        let raw = synth(&[0; 8], 8, None, 0.0, 0, 0);
        assert!(parse(&raw[..raw.len() / 2]).is_err());
    }
}
