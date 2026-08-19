//! Projections.
//!
//! Two coordinate systems matter here:
//!
//! * **Web Mercator (EPSG:3857)** — what MapLibre draws in, and therefore what we
//!   resample every raster into so the client can place it as an axis-aligned quad.
//! * **The DWD RADOLAN `DE1200` grid** — polar stereographic on the WGS84
//!   ellipsoid, true at 60°N, centred on 10°E. It is rotated against north
//!   everywhere except along 10°E, so a naive lat/lon rectangle placement drifts by
//!   kilometres at the corners.
//!
//! The `DE1200` constants below were verified against Bright Sky's `latlon_position`
//! for five cities spread across the grid (Berlin, Munich, Cologne, Zurich, Kiel);
//! the implied grid origin agreed to within 0.7 m in x and 0.9 m in y.

use std::sync::LazyLock;

pub const EARTH_R: f64 = 6_378_137.0;

#[inline]
pub fn merc_x(lon_deg: f64) -> f64 {
    EARTH_R * lon_deg.to_radians()
}

#[inline]
pub fn merc_y(lat_deg: f64) -> f64 {
    let phi = lat_deg.to_radians();
    EARTH_R * (std::f64::consts::FRAC_PI_4 + phi / 2.0).tan().ln()
}

#[inline]
pub fn merc_lon(x: f64) -> f64 {
    (x / EARTH_R).to_degrees()
}

#[inline]
pub fn merc_lat(y: f64) -> f64 {
    (2.0 * (y / EARTH_R).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees()
}

// ---------------------------------------------------------------------------
// RADOLAN DE1200
// ---------------------------------------------------------------------------

/// Columns (west → east).
pub const RAD_NX: usize = 1100;
/// Rows (the binary stores them south → north).
pub const RAD_NY: usize = 1200;
/// Grid spacing in metres.
pub const RAD_RES: f64 = 1000.0;

const RAD_A: f64 = 6_378_137.0;
const RAD_B: f64 = 6_356_752.314_245_179;
/// x of the centre of column 0, in pole-centred metres.
const RAD_X0: f64 = -543_196.835_217_76;
/// y of the centre of the northern-most row, in pole-centred metres.
const RAD_Y0_NORTH: f64 = -3_622_588.860_222_72;

struct StereoConst {
    e: f64,
    /// `a * m_ts / t_ts` — the radius scale factor.
    k: f64,
    lon0: f64,
}

static STEREO: LazyLock<StereoConst> = LazyLock::new(|| {
    let e = (1.0 - (RAD_B * RAD_B) / (RAD_A * RAD_A)).sqrt();
    let lat_ts = 60.0_f64.to_radians();
    let s = lat_ts.sin();
    let m_ts = lat_ts.cos() / (1.0 - e * e * s * s).sqrt();
    let t_ts = (std::f64::consts::FRAC_PI_4 - lat_ts / 2.0).tan()
        / (((1.0 - e * s) / (1.0 + e * s)).powf(e / 2.0));
    StereoConst {
        e,
        k: RAD_A * m_ts / t_ts,
        lon0: 10.0_f64.to_radians(),
    }
});

/// lon/lat (degrees) → fractional RADOLAN grid position.
///
/// Returns `(col, row_from_north)`, both in pixel units where an integer value is a
/// pixel *centre*. Note the row counts from the north edge, i.e. the opposite of the
/// order the binary stores rows in.
#[inline]
pub fn lonlat_to_radolan(lon: f64, lat: f64) -> (f64, f64) {
    let c = &*STEREO;
    let lam = lon.to_radians();
    let phi = lat.to_radians();
    let s = phi.sin();
    let t = (std::f64::consts::FRAC_PI_4 - phi / 2.0).tan()
        / (((1.0 - c.e * s) / (1.0 + c.e * s)).powf(c.e / 2.0));
    let rho = c.k * t;
    let d = lam - c.lon0;
    let x = rho * d.sin();
    let y = -rho * d.cos();
    ((x - RAD_X0) / RAD_RES, (RAD_Y0_NORTH - y) / RAD_RES)
}

/// Fractional RADOLAN grid position → lon/lat (degrees). Inverse of
/// [`lonlat_to_radolan`].
pub fn radolan_to_lonlat(col: f64, row_from_north: f64) -> (f64, f64) {
    let c = &*STEREO;
    let x = RAD_X0 + col * RAD_RES;
    let y = RAD_Y0_NORTH - row_from_north * RAD_RES;
    let rho = x.hypot(y);
    let t = rho / c.k;
    // Snyder's iterative inverse for the conformal latitude.
    let mut lat = std::f64::consts::FRAC_PI_2 - 2.0 * t.atan();
    for _ in 0..8 {
        let s = lat.sin();
        lat = std::f64::consts::FRAC_PI_2
            - 2.0 * (t * (((1.0 - c.e * s) / (1.0 + c.e * s)).powf(c.e / 2.0))).atan();
    }
    let lon = c.lon0 + x.atan2(-y);
    (lon.to_degrees(), lat.to_degrees())
}

/// Web Mercator bounding box in metres, `min` inclusive / `max` exclusive at the
/// pixel-edge level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MercBox {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl MercBox {
    /// Build a box from geographic corners. Test fixtures only — production geometry
    /// comes from walking a source grid's border, which a lat/lon rectangle would
    /// misrepresent for a rotated grid.
    #[cfg(test)]
    pub fn from_lonlat(west: f64, south: f64, east: f64, north: f64) -> Self {
        Self {
            x0: merc_x(west),
            y0: merc_y(south),
            x1: merc_x(east),
            y1: merc_y(north),
        }
    }

    pub fn width(&self) -> f64 {
        self.x1 - self.x0
    }

    pub fn height(&self) -> f64 {
        self.y1 - self.y0
    }

    /// `[west, south, east, north]` in degrees. The Mercator box is axis aligned, so
    /// this is exact rather than an approximation.
    pub fn lonlat(&self) -> [f64; 4] {
        [
            merc_lon(self.x0),
            merc_lat(self.y0),
            merc_lon(self.x1),
            merc_lat(self.y1),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ground truth from Bright Sky's `latlon_position` (absolute pixel indices on
    /// the DE1200 grid, row counted from north).
    const FIXTURES: [(f64, f64, f64, f64); 5] = [
        (52.520, 13.405, 783.502, 416.260), // Berlin
        (48.137, 11.575, 668.533, 935.781), // Munich
        (50.938, 6.960, 318.868, 601.433),  // Cologne
        (47.376, 8.541, 424.780, 1026.707), // Zurich
        (54.323, 10.135, 552.241, 215.728), // Kiel
    ];

    #[test]
    fn radolan_forward_matches_brightsky() {
        for (lat, lon, col, row) in FIXTURES {
            let (c, r) = lonlat_to_radolan(lon, lat);
            assert!(
                (c - col).abs() < 0.01 && (r - row).abs() < 0.01,
                "lon={lon} lat={lat}: got ({c:.3}, {r:.3}), want ({col:.3}, {row:.3})"
            );
        }
    }

    #[test]
    fn radolan_roundtrips() {
        for (lat, lon, _, _) in FIXTURES {
            let (c, r) = lonlat_to_radolan(lon, lat);
            let (lon2, lat2) = radolan_to_lonlat(c, r);
            assert!((lon - lon2).abs() < 1e-9 && (lat - lat2).abs() < 1e-9);
        }
    }

    #[test]
    fn mercator_roundtrips() {
        for lat in [-70.0, -12.5, 0.0, 33.0, 51.7, 71.0] {
            assert!((merc_lat(merc_y(lat)) - lat).abs() < 1e-9);
        }
        for lon in [-179.0, -13.0, 0.0, 10.0, 36.0, 179.0] {
            assert!((merc_lon(merc_x(lon)) - lon).abs() < 1e-9);
        }
    }
}
