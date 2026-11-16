//! Decoder for the DWD RV nowcast in ODIM_H5 (HDF5), as published since autumn 2026
//! (`composite_rv_LATEST.tar`, one `composite_rv_<run>_<lead>-hd5` member per frame).
//!
//! Same product and same grid as the old RADOLAN binary (`DE1200`, 1100 × 1200 at
//! 1 km, polar stereographic on WGS84 — the file's `projdef` matches the constants
//! in [`crate::proj`]), only the container changed:
//!
//! * `/what` `date` + `time`: the run (radar measurement) time.
//! * `/dataset1/what` `startdate`/`starttime` .. `enddate`/`endtime`: the accumulation
//!   window of this frame; its end is the valid time.
//! * `/dataset1/data1/data`: `u32`, quantity `ACRR` (mm accumulated over the window),
//!   physical value = `raw * gain + offset`; `nodata` = outside radar coverage,
//!   `undetect` = no rain.
//! * Rows are stored **north → south** (ODIM convention); [`RadolanFrame`] keeps the
//!   RADOLAN order south → north, so rows are flipped here.

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use hdf5_reader::Hdf5File;

use crate::proj::{RAD_NX, RAD_NY};
use crate::radolan::RadolanFrame;

/// HDF5 superblock signature.
const MAGIC: &[u8] = b"\x89HDF\r\n\x1a\n";

pub fn is_hdf5(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

pub fn parse(bytes: Vec<u8>) -> Result<RadolanFrame> {
    let file = Hdf5File::from_vec(bytes).map_err(|e| anyhow!("opening HDF5: {e}"))?;

    let attr_str = |group: &str, name: &str| -> Result<String> {
        file.group(group)
            .and_then(|g| g.attribute(name))
            .and_then(|a| a.read_string())
            .map(|s| s.trim_end_matches('\0').trim().to_owned())
            .map_err(|e| anyhow!("{group}@{name}: {e}"))
    };
    let attr_f64 = |group: &str, name: &str| -> Result<f64> {
        file.group(group)
            .and_then(|g| g.attribute(name))
            .and_then(|a| a.read_as_f64())
            .map_err(|e| anyhow!("{group}@{name}: {e}"))
    };

    let (nx, ny) = (
        attr_f64("/where", "xsize")? as usize,
        attr_f64("/where", "ysize")? as usize,
    );
    if nx != RAD_NX || ny != RAD_NY {
        bail!("unexpected grid {nx}x{ny}, this build only handles {RAD_NX}x{RAD_NY} (DE1200)");
    }

    let quantity = attr_str("/dataset1/data1/what", "quantity")?;
    if quantity != "ACRR" {
        bail!("unexpected quantity {quantity:?}, expected ACRR");
    }

    let run_time = timestamp(&attr_str("/what", "date")?, &attr_str("/what", "time")?)?;
    let start = timestamp(
        &attr_str("/dataset1/what", "startdate")?,
        &attr_str("/dataset1/what", "starttime")?,
    )?;
    let time = timestamp(
        &attr_str("/dataset1/what", "enddate")?,
        &attr_str("/dataset1/what", "endtime")?,
    )?;
    let interval_min = (time - start).num_minutes();
    if interval_min <= 0 {
        bail!("empty accumulation window {start} .. {time}");
    }
    let forecast_minutes = (time - run_time).num_minutes();

    let gain = attr_f64("/dataset1/data1/what", "gain")?;
    let offset = attr_f64("/dataset1/data1/what", "offset")?;
    let nodata = attr_f64("/dataset1/data1/what", "nodata")?;
    let undetect = attr_f64("/dataset1/data1/what", "undetect")?;

    let data = file
        .dataset("/dataset1/data1/data")
        .map_err(|e| anyhow!("/dataset1/data1/data: {e}"))?;
    if data.shape() != [ny as u64, nx as u64] {
        bail!("dataset shape {:?} does not match {ny}x{nx}", data.shape());
    }
    let raw = data
        .read_array::<u32>()
        .map_err(|e| anyhow!("reading radar field: {e}"))?;
    let raw = raw.as_slice().context("radar field is not contiguous")?;

    Ok(RadolanFrame {
        time,
        run_time,
        forecast_minutes,
        nx,
        ny,
        mm_per_hour: decode(raw, nx, ny, gain, offset, nodata, undetect, interval_min),
    })
}

/// Raw ODIM values (rows north → south) → mm/h (rows south → north).
#[allow(clippy::too_many_arguments)]
fn decode(
    raw: &[u32],
    nx: usize,
    ny: usize,
    gain: f64,
    offset: f64,
    nodata: f64,
    undetect: f64,
    interval_min: i64,
) -> Vec<f32> {
    // The map speaks mm/h; the file holds mm per `interval_min`.
    let per_hour = 60.0 / interval_min as f64;
    let (nodata, undetect) = (nodata as u32, undetect as u32);
    let mut out = vec![0.0f32; nx * ny];
    for (r, src) in raw.chunks_exact(nx).enumerate() {
        let dst = &mut out[(ny - 1 - r) * nx..(ny - r) * nx];
        for (o, &v) in dst.iter_mut().zip(src) {
            *o = if v == nodata {
                f32::NAN
            } else if v == undetect {
                0.0
            } else {
                ((v as f64 * gain + offset).max(0.0) * per_hour) as f32
            };
        }
    }
    out
}

/// ODIM `YYYYMMDD` + `HHMMSS`, always UTC.
fn timestamp(date: &str, time: &str) -> Result<DateTime<Utc>> {
    let naive = NaiveDateTime::parse_from_str(&format!("{date}{time}"), "%Y%m%d%H%M%S")
        .with_context(|| format!("invalid ODIM timestamp {date:?} {time:?}"))?;
    Ok(Utc.from_utc_datetime(&naive))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proj;

    #[test]
    fn decode_flips_rows_and_scales() {
        let (nx, ny) = (2, 3);
        // north row first: [1001, nodata], [0, 0], [0, 2]
        let raw = [1001, u32::MAX, 0, 0, 0, 2];
        let out = decode(&raw, nx, ny, 0.001, -0.001, u32::MAX as f64, 0.0, 5);
        // 1001 → 1.000 mm / 5 min → 12 mm/h, now in the last (northern-most) row
        assert!((out[4] - 12.0).abs() < 1e-4);
        assert!(out[5].is_nan());
        // raw 2 → 0.001 mm / 5 min → 0.012 mm/h, now in the first (southern) row
        assert!((out[1] - 0.012).abs() < 1e-6);
        assert_eq!(out[0], 0.0);
    }

    #[test]
    fn parses_odim_timestamps() {
        let t = timestamp("20261116", "121000").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-11-16T12:10:00+00:00");
        assert!(timestamp("20261316", "121000").is_err());
    }

    /// The grid in `proj` is the grid the ODIM file describes: its corner
    /// coordinates (`/where` UL/LR, copied from a 2026-11-16 file) are the outer
    /// corners of the first and last cells, to within a few metres.
    #[test]
    fn proj_grid_matches_odim_corners() {
        let close = |(lon, lat): (f64, f64), (elon, elat): (f64, f64)| {
            let dx = (lon - elon) * 111_320.0 * lat.to_radians().cos();
            let dy = (lat - elat) * 110_540.0;
            (dx * dx + dy * dy).sqrt()
        };
        let ul = proj::radolan_to_lonlat(-0.5, -0.5);
        let lr = proj::radolan_to_lonlat(RAD_NX as f64 - 0.5, RAD_NY as f64 - 0.5);
        let d_ul = close(ul, (1.463301510256666, 55.862087108249824));
        let d_lr = close(lr, (16.580869348598274, 45.68460578137082));
        assert!(
            d_ul < 50.0 && d_lr < 50.0,
            "UL off by {d_ul:.0} m, LR by {d_lr:.0} m"
        );
    }

    /// Opt-in, needs a real member:
    /// `REGENRADAR_TEST_ODIM=/path/to/composite_rv_..._000-hd5 cargo test -- --ignored`
    #[test]
    #[ignore]
    fn parses_a_real_member() {
        let path = std::env::var("REGENRADAR_TEST_ODIM").expect("set REGENRADAR_TEST_ODIM");
        let f = parse(std::fs::read(path).unwrap()).unwrap();
        assert_eq!((f.nx, f.ny), (RAD_NX, RAD_NY));
        let valid = f.mm_per_hour.iter().filter(|v| !v.is_nan()).count();
        println!(
            "run {} lead {} min, {valid} cells with data",
            f.run_time, f.forecast_minutes
        );
        assert!(valid > RAD_NX * RAD_NY / 4);
    }
}
