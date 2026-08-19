//! Decoder for the DWD RADOLAN binary composite format, as used by the `RV`
//! nowcast product (`DE1200_RV*.tar.bz2`).
//!
//! Layout: an ASCII header terminated by `ETX` (0x03), then `nx * ny` little-endian
//! `u16` values. Rows are stored **south → north**, columns west → east — verified
//! by diffing a raw file against Bright Sky's decoding of the same timestamp.
//!
//! Header, e.g.:
//! ```text
//! RV170750100000826BY   2640189VS 5SW  P42001HPR E-02INT   5GP1200x1100VV 000MF 00000008MS 97<deasb,...>
//!   │└─┬──┘└─┬─┘└┬─┘                            └─┬──┘└─┬─┘└────┬────┘└─┬─┘
//!   │ DDhhmm  WMO MMYY                        precision interval  rows x cols  forecast minutes
//!   product
//! ```

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, TimeZone, Utc};

use crate::proj::{RAD_NX, RAD_NY};

/// Bit 13 marks "no data" (outside radar coverage). The canonical fill is 0x29C4.
const FLAG_NODATA: u16 = 0x2000;
/// Bit 12 marks a value that was interpolated / flagged as secondary.
const FLAG_SECONDARY: u16 = 0x1000;
const VALUE_MASK: u16 = 0x0FFF;

#[derive(Debug, Clone)]
pub struct RadolanFrame {
    /// Valid time of this frame (measurement time + forecast offset).
    pub time: DateTime<Utc>,
    /// Time of the underlying radar measurement the nowcast was launched from.
    pub run_time: DateTime<Utc>,
    /// Minutes ahead of `run_time`. 0 means it is an analysis, not a forecast.
    pub forecast_minutes: i64,
    pub nx: usize,
    pub ny: usize,
    /// Rain rate in mm/h, `f32::NAN` where the composite has no data.
    /// Row-major, **south → north**, matching the on-disk order.
    pub mm_per_hour: Vec<f32>,
}

/// Parse one RADOLAN file.
///
/// `assumed_now` disambiguates the header's day-of-month-only timestamp across
/// month boundaries; pass the current UTC time.
pub fn parse(bytes: &[u8], assumed_now: DateTime<Utc>) -> Result<RadolanFrame> {
    let etx = bytes
        .iter()
        .position(|&b| b == 0x03)
        .ok_or_else(|| anyhow!("no ETX terminator: not a RADOLAN file"))?;
    let header = String::from_utf8_lossy(&bytes[..etx]).into_owned();
    let payload = &bytes[etx + 1..];

    // The `MS` section carries a free-text radar-site list that can contain anything,
    // so every keyword lookup is restricted to the part in front of it.
    let scan_end = header.find("MS").unwrap_or(header.len());
    let scan = &header[..scan_end];

    let (ny, nx) = parse_dimensions(scan).context("parsing GP grid dimensions")?;
    if nx != RAD_NX || ny != RAD_NY {
        bail!("unexpected grid {nx}x{ny}, this build only handles {RAD_NX}x{RAD_NY} (DE1200)");
    }

    let expected = nx * ny * 2;
    if payload.len() < expected {
        bail!(
            "payload too short: {} bytes, need {expected} for {nx}x{ny}",
            payload.len()
        );
    }

    let precision = parse_precision(scan).context("parsing PR precision")?;
    let interval_min = parse_keyword_int(scan, "INT").unwrap_or(5).max(1) as f32;
    let forecast_minutes = parse_keyword_int(scan, "VV").unwrap_or(0);
    let run_time = parse_timestamp(&header, assumed_now)?;
    let time = run_time + chrono::Duration::minutes(forecast_minutes);

    // Values accumulate over `interval_min`; the map speaks mm/h.
    let to_mm_h = precision * (60.0 / interval_min);

    let mut mm_per_hour = vec![0.0f32; nx * ny];
    for (out, chunk) in mm_per_hour
        .iter_mut()
        .zip(payload[..expected].chunks_exact(2))
    {
        let raw = u16::from_le_bytes([chunk[0], chunk[1]]);
        *out = if raw & FLAG_NODATA != 0 {
            f32::NAN
        } else {
            // The secondary flag says how the value was derived, not that it is
            // invalid, so the magnitude still counts.
            let _ = FLAG_SECONDARY;
            (raw & VALUE_MASK) as f32 * to_mm_h
        };
    }

    Ok(RadolanFrame {
        time,
        run_time,
        forecast_minutes,
        nx,
        ny,
        mm_per_hour,
    })
}

/// `GP1200x1100` → `(1200, 1100)` i.e. `(rows, cols)`.
fn parse_dimensions(scan: &str) -> Result<(usize, usize)> {
    let idx = scan.find("GP").ok_or_else(|| anyhow!("no GP keyword"))?;
    let rest = &scan[idx + 2..];
    let x = rest
        .find('x')
        .ok_or_else(|| anyhow!("malformed GP value"))?;
    let rows: usize = rest[..x].trim().parse().context("GP rows")?;
    let cols_str: String = rest[x + 1..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || c.is_whitespace())
        .collect();
    let cols: usize = cols_str.trim().parse().context("GP cols")?;
    Ok((rows, cols))
}

/// `PR E-02` → `0.01`, the value of one raw unit in millimetres.
fn parse_precision(scan: &str) -> Result<f32> {
    let idx = scan.find("PR").ok_or_else(|| anyhow!("no PR keyword"))?;
    let rest = scan[idx + 2..].trim_start();
    let mut chars = rest.chars();
    if chars.next() != Some('E') {
        bail!("unsupported PR format: {:?}", &rest[..rest.len().min(8)]);
    }
    let digits: String = chars
        .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '+')
        .collect();
    let exp: i32 = digits.trim().parse().context("PR exponent")?;
    Ok(10f32.powi(exp))
}

/// Read the integer that follows a keyword, e.g. `INT   5` or `VV 000`.
fn parse_keyword_int(scan: &str, key: &str) -> Option<i64> {
    let idx = scan.find(key)?;
    let rest = scan[idx + key.len()..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// The header opens with `PP DDhhmm WWWWW MMYY`. Only a two-digit year is given, and
/// the day comes before the month, so the pieces have to be reassembled carefully.
fn parse_timestamp(header: &str, assumed_now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let b = header.as_bytes();
    if b.len() < 17 {
        bail!("header too short for a timestamp");
    }
    let num = |range: std::ops::Range<usize>| -> Result<u32> {
        std::str::from_utf8(&b[range])?
            .trim()
            .parse::<u32>()
            .map_err(Into::into)
    };
    let day = num(2..4)?;
    let hour = num(4..6)?;
    let minute = num(6..8)?;
    let month = num(13..15)?;
    let year = 2000 + num(15..17)? as i32;

    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .ok_or_else(|| {
            anyhow!("invalid header timestamp {year}-{month:02}-{day:02} {hour:02}:{minute:02} (now {assumed_now})")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_header() -> Vec<u8> {
        let mut v =
            b"RV170750100000826BY   2640189VS 5SW  P42001HPR E-02INT   5GP1200x1100VV 015MF 00000008MS 97<deasb,deboo>"
                .to_vec();
        v.push(0x03);
        v
    }

    #[test]
    fn parses_header_fields() {
        let mut file = synth_header();
        file.extend(std::iter::repeat_n(0u8, RAD_NX * RAD_NY * 2));
        let f = parse(&file, Utc::now()).unwrap();
        assert_eq!(f.run_time.to_rfc3339(), "2026-08-17T07:50:00+00:00");
        assert_eq!(f.forecast_minutes, 15);
        assert_eq!(f.time.to_rfc3339(), "2026-08-17T08:05:00+00:00");
        assert_eq!((f.nx, f.ny), (RAD_NX, RAD_NY));
    }

    #[test]
    fn decodes_values_and_nodata() {
        let mut file = synth_header();
        let mut data = vec![0u8; RAD_NX * RAD_NY * 2];
        // 100 raw units = 1.00 mm per 5 min = 12 mm/h
        data[0..2].copy_from_slice(&100u16.to_le_bytes());
        // canonical nodata fill
        data[2..4].copy_from_slice(&0x29C4u16.to_le_bytes());
        file.extend(data);
        let f = parse(&file, Utc::now()).unwrap();
        assert!((f.mm_per_hour[0] - 12.0).abs() < 1e-4);
        assert!(f.mm_per_hour[1].is_nan());
    }

    #[test]
    fn rejects_truncated_payload() {
        let mut file = synth_header();
        file.extend(vec![0u8; 16]);
        assert!(parse(&file, Utc::now()).is_err());
    }
}
