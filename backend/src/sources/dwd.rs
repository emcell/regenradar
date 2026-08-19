//! DWD RV nowcast ingest — the app's only data source.
//!
//! `DE1200_RV_LATEST.tar.bz2` is republished every five minutes and contains 25
//! frames: the current radar analysis plus 24 nowcast steps reaching two hours
//! ahead, on a 1 km grid. That is the same product the commercial rain-radar apps
//! render, and it is open data under GeoNutzV.
//!
//! Coverage is Germany plus roughly 100–250 km beyond its borders — the reach of the
//! seventeen German radars. The DE1200 grid it is delivered on is considerably
//! larger than that, so each run is cropped to where it actually has data.

use std::{
    io::Read,
    sync::{Arc, OnceLock, RwLock},
};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use tracing::{debug, info, warn};

use crate::{
    motion,
    proj::{self, MercBox, RAD_NX, RAD_NY},
    radolan::{self, RadolanFrame},
    raster::{self, ResampleMap},
    state::{AppState, FrameOrigin, Geometry, RenderedFrame, SourcePart},
};

pub const ATTRIBUTION: &str = "Datenbasis: Deutscher Wetterdienst";

/// Roughly how large a delivered tile should be, in raster pixels.
///
/// Smaller tiles mean a zoomed-in viewport pulls less it cannot see, at the cost of
/// some compression efficiency — each tile carries its own header and starts its
/// entropy model from scratch.
pub const TILE_TARGET: u32 = 160;

/// A rectangle of the source grid, in stored index order: columns west to east,
/// rows south to north.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Crop {
    c0: usize,
    c1: usize,
    j0: usize,
    j1: usize,
}

impl Crop {
    fn full() -> Self {
        Self {
            c0: 0,
            c1: RAD_NX - 1,
            j0: 0,
            j1: RAD_NY - 1,
        }
    }

    /// Smallest rectangle containing both.
    ///
    /// The union across frames matters: the nowcast advects the whole field, so the
    /// data edge travels with the wind and a later frame reaches where the analysis
    /// did not. Cropping to the analysis alone would clip up to a hundred kilometres
    /// off the forecast.
    fn union(self, other: Crop) -> Crop {
        Crop {
            c0: self.c0.min(other.c0),
            c1: self.c1.max(other.c1),
            j0: self.j0.min(other.j0),
            j1: self.j1.max(other.j1),
        }
    }

    fn cells(&self) -> usize {
        (self.c1 - self.c0 + 1) * (self.j1 - self.j0 + 1)
    }

    /// Grow outward to multiples of `snap`, clamped to the grid.
    ///
    /// Without snapping the crop would shift by a pixel or two on every run — the
    /// nowcast advects the composite's data edge along with the rain — and every
    /// shift would mean rebuilding the reprojection table and changing the layer's
    /// geometry for no visible benefit.
    fn snapped(self, snap: usize) -> Self {
        let snap = snap.max(1);
        let lo = |v: usize| (v / snap) * snap;
        let hi = |v: usize, max: usize| (((v / snap) + 1) * snap - 1).min(max);
        Self {
            c0: lo(self.c0),
            c1: hi(self.c1, RAD_NX - 1),
            j0: lo(self.j0),
            j1: hi(self.j1, RAD_NY - 1),
        }
    }

    /// Web-Mercator envelope, found by walking the border.
    ///
    /// The RADOLAN grid is polar stereographic and rotated against north, so its
    /// edges are straight lines there but curves in Mercator; sampling the border
    /// rather than the four corners is what keeps the bulge inside the box.
    fn merc_box(&self) -> MercBox {
        let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
        let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        let mut visit = |col: f64, j: f64| {
            // Half a pixel outward so the box covers cell edges, not centres.
            let row_from_north = (RAD_NY - 1) as f64 - j;
            let (lon, lat) = proj::radolan_to_lonlat(col, row_from_north);
            let (x, y) = (proj::merc_x(lon), proj::merc_y(lat));
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        };
        let (cl, cr) = (self.c0 as f64 - 0.5, self.c1 as f64 + 0.5);
        let (jb, jt) = (self.j0 as f64 - 0.5, self.j1 as f64 + 0.5);
        for i in self.c0..=self.c1 + 1 {
            let col = (i as f64 - 0.5).clamp(cl, cr);
            visit(col, jb);
            visit(col, jt);
        }
        for j in self.j0..=self.j1 + 1 {
            let row = (j as f64 - 0.5).clamp(jb, jt);
            visit(cl, row);
            visit(cr, row);
        }
        MercBox { x0, y0, x1, y1 }
    }
}

/// Smallest rectangle containing every valid cell of one frame.
fn frame_bounds(frame: &RadolanFrame) -> Option<Crop> {
    let (mut c0, mut c1) = (usize::MAX, 0usize);
    let (mut j0, mut j1) = (usize::MAX, 0usize);
    let mut any = false;

    for j in 0..RAD_NY {
        let row = &frame.mm_per_hour[j * RAD_NX..(j + 1) * RAD_NX];
        let Some(first) = row.iter().position(|v| !v.is_nan()) else {
            continue;
        };
        let last = row.iter().rposition(|v| !v.is_nan()).unwrap_or(first);
        any = true;
        c0 = c0.min(first);
        c1 = c1.max(last);
        j0 = j0.min(j);
        j1 = j1.max(j);
    }
    any.then_some(Crop { c0, c1, j0, j1 })
}

/// Cached reprojection table, keyed by the crop and the resolution it was built for.
type MapCache = OnceLock<RwLock<Option<(Crop, u64, Arc<ResampleMap>)>>>;

/// The reprojection table, rebuilt only when the cropped geometry actually changes.
fn resample_map(crop: Crop, resolution_m: f64) -> Arc<ResampleMap> {
    static CACHE: MapCache = OnceLock::new();
    let cache = CACHE.get_or_init(|| RwLock::new(None));
    let res_key = resolution_m.to_bits();

    if let Some((c, r, map)) = cache.read().unwrap().as_ref() {
        if *c == crop && *r == res_key {
            return map.clone();
        }
    }

    let merc = crop.merc_box();
    let width = (merc.width() / resolution_m).round().max(64.0) as u32;
    let height = (merc.height() / resolution_m).round().max(64.0) as u32;
    info!(
        width,
        height,
        crop = format!("{}..{} x {}..{}", crop.c0, crop.c1, crop.j0, crop.j1),
        "building DWD reprojection map ({:.0} x {:.0} km, {:.0}% of the grid)",
        merc.width() / 1000.0,
        merc.height() / 1000.0,
        100.0 * crop.cells() as f64 / Crop::full().cells() as f64,
    );

    let map = Arc::new(ResampleMap::build(
        merc,
        width,
        height,
        RAD_NX,
        RAD_NY,
        |lon, lat| {
            let (col, row_from_north) = proj::lonlat_to_radolan(lon, lat);
            // The binary stores rows south → north.
            (col, (RAD_NY - 1) as f64 - row_from_north)
        },
    ));
    *cache.write().unwrap() = Some((crop, res_key, map.clone()));
    map
}

pub async fn run(state: Arc<AppState>) {
    let mut last_modified: Option<String> = None;
    loop {
        match poll_once(&state, &mut last_modified).await {
            Ok(true) => {}
            Ok(false) => debug!("DWD RV unchanged"),
            Err(e) => warn!("DWD RV ingest failed: {e:#}"),
        }
        tokio::time::sleep(state.cfg.dwd_poll).await;
    }
}

async fn poll_once(state: &AppState, last_modified: &mut Option<String>) -> Result<bool> {
    let url = &state.cfg.dwd_url;

    // Cheap change check first — the archive is ~2 MB and only turns over every
    // five minutes, so most polls should cost nothing but a HEAD.
    if last_modified.is_some() {
        if let Ok(head) = state.http.head(url).send().await {
            let current = head
                .headers()
                .get(reqwest::header::LAST_MODIFIED)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            if current.is_some() && current == *last_modified {
                return Ok(false);
            }
        }
    }

    let resp = state.http.get(url).send().await?.error_for_status()?;
    let stamp = resp
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = resp.bytes().await?;

    let resolution_m = state.cfg.dwd_resolution_m;
    let snap = state.cfg.dwd_crop_snap;
    let part = tokio::task::spawn_blocking(move || build_part(&body, resolution_m, snap))
        .await
        .context("DWD ingest task panicked")??;

    let size_kb: usize = part
        .frames
        .iter()
        .map(|f| f.overview.len() + f.tiles.iter().map(|t| t.len()).sum::<usize>())
        .sum::<usize>()
        / 1024;
    info!(
        run = %part.run,
        frames = part.frames.len(),
        raster = format!("{}x{}", part.geometry.width, part.geometry.height),
        size_kb,
        "DWD RV run ingested"
    );
    state.set_radar(Arc::new(part));
    *last_modified = stamp;
    Ok(true)
}

/// Walk the archive, handing each decoded frame to `sink`.
///
/// One frame is alive at a time. The alternative — collecting all 25 up front — costs
/// 125 MB of `f32` for a product whose rendered output is three megabytes, and the
/// crop has to be known before any frame can be resampled anyway.
fn for_each_frame(archive: &[u8], mut sink: impl FnMut(RadolanFrame) -> Result<()>) -> Result<()> {
    let now = Utc::now();
    let decoder = bzip2::read::BzDecoder::new(std::io::Cursor::new(archive));
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries().context("reading tar entries")? {
        let mut entry = entry?;
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf)?;
        if buf.is_empty() {
            continue;
        }
        match radolan::parse(&buf, now) {
            Ok(f) => sink(f)?,
            // One malformed member should not throw away a whole run.
            Err(e) => warn!("skipping RADOLAN member: {e:#}"),
        }
    }
    Ok(())
}

/// Decode the archive and render the radar part of the timeline. Pure CPU — callers
/// run this off the async runtime.
///
/// Two passes over the archive: the first establishes the crop, which depends on
/// every frame, and the second resamples against it. Decompressing twice costs about
/// a second every five minutes and saves well over a hundred megabytes of residency.
fn build_part(archive: &[u8], resolution_m: f64, snap: usize) -> Result<SourcePart> {
    // --- pass one: the crop, the frame order, and a grid sanity check ---
    let mut bounds: Option<Crop> = None;
    let mut order: Vec<(i64, chrono::DateTime<Utc>, chrono::DateTime<Utc>)> = Vec::new();
    let mut grid: Option<(usize, usize)> = None;

    for_each_frame(archive, |f| {
        match grid {
            None => grid = Some((f.nx, f.ny)),
            Some(g) if g != (f.nx, f.ny) => bail!(
                "archive mixes grid sizes: {}x{} and {}x{}",
                g.0,
                g.1,
                f.nx,
                f.ny
            ),
            _ => {}
        }
        if let Some(c) = frame_bounds(&f) {
            bounds = Some(match bounds {
                None => c,
                Some(b) => b.union(c),
            });
        }
        order.push((f.forecast_minutes, f.time, f.run_time));
        Ok(())
    })?;

    if order.is_empty() {
        bail!("archive contained no readable RADOLAN frames");
    }
    let crop = bounds.unwrap_or_else(Crop::full).snapped(snap);
    let map = resample_map(crop, resolution_m);
    let (width, height) = (map.width, map.height);

    // Position of each frame, in tar order, once sorted by lead time.
    let mut sorted: Vec<usize> = (0..order.len()).collect();
    sorted.sort_by_key(|&i| order[i].0);
    let mut slot = vec![0usize; order.len()];
    for (pos, &i) in sorted.iter().enumerate() {
        slot[i] = pos;
    }

    // --- pass two: resample and encode, still one frame at a time ---
    let tiling = raster::Tiling::choose(width, height, TILE_TARGET);
    let mut encoded: Vec<Vec<u8>> = vec![Vec::new(); order.len()];
    let mut rendered: Vec<Option<RenderedFrame>> = (0..order.len()).map(|_| None).collect();
    let mut mask: Option<Vec<u8>> = None;
    let mut seen = 0usize;
    // One scratch raster for the whole run; the geometry never changes between frames.
    let mut field: Vec<f32> = Vec::new();

    for_each_frame(archive, |f| {
        let pos = *slot.get(seen).unwrap_or(&0);
        seen += 1;
        map.apply_into(&f.mm_per_hour, &mut field);
        // Where the radar can see, taken from the analysis: that is the honest answer
        // to "does a radar reach my location", independent of where the nowcast
        // drifts to.
        if f.forecast_minutes == 0 {
            mask = Some(raster::coverage_mask(&field));
        }
        // `encoded` outlives the loop for motion estimation, so this one is moved
        // rather than reused.
        let bytes = raster::encode_field(&field);
        let (overview, tiles) = raster::split(&bytes, width, height, &tiling)?;
        rendered[pos] = Some(RenderedFrame {
            overview: Arc::new(overview),
            tiles: tiles.into_iter().map(Arc::new).collect(),
        });
        encoded[pos] = bytes;
        Ok(())
    })?;
    drop(field);

    let mut flow = Vec::new();
    for i in 0..encoded.len().saturating_sub(1) {
        let field = motion::estimate(&encoded[i], &encoded[i + 1], width, height);
        flow.push(Arc::new(field.encode()?));
    }
    drop(encoded);

    let mask = mask.context("archive had no analysis frame")?;
    let (_, mask_tiles) = raster::split(&mask, width, height, &tiling)?;
    let times = sorted.iter().map(|&i| order[i].1).collect();
    let run_time = order[sorted[0]].2;

    Ok(SourcePart {
        run: run_time.format("%Y%m%dT%H%M").to_string(),
        origin: FrameOrigin::Radar,
        geometry: Geometry {
            merc: map.merc,
            width,
            height,
            resolution_m,
        },
        times,
        frames: rendered
            .into_iter()
            .map(|f| f.expect("every slot filled"))
            .collect(),
        tiling,
        flow,
        mask: Some(mask_tiles.into_iter().map(Arc::new).collect()),
        updated_at: Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_with(cells: &[(usize, usize)]) -> RadolanFrame {
        let mut mm = vec![f32::NAN; RAD_NX * RAD_NY];
        for &(c, j) in cells {
            mm[j * RAD_NX + c] = 1.0;
        }
        RadolanFrame {
            time: Utc::now(),
            run_time: Utc::now(),
            forecast_minutes: 0,
            nx: RAD_NX,
            ny: RAD_NY,
            mm_per_hour: mm,
        }
    }

    #[test]
    fn bounds_span_the_union_of_all_frames() {
        // The nowcast drifts east: a later frame reaches columns the analysis did not.
        let a = frame_bounds(&frame_with(&[(100, 200), (500, 800)])).unwrap();
        let b = frame_bounds(&frame_with(&[(300, 150), (900, 900)])).unwrap();
        let c = a.union(b);
        assert_eq!((c.c0, c.c1), (100, 900));
        assert_eq!((c.j0, c.j1), (150, 900));
    }

    #[test]
    fn bounds_are_none_for_an_empty_composite() {
        let empty = RadolanFrame {
            time: Utc::now(),
            run_time: Utc::now(),
            forecast_minutes: 0,
            nx: RAD_NX,
            ny: RAD_NY,
            mm_per_hour: vec![f32::NAN; RAD_NX * RAD_NY],
        };
        assert!(frame_bounds(&empty).is_none());
    }

    /// Build the same run repeatedly and watch resident memory.
    ///
    /// Opt-in because it needs a real archive:
    /// `REGENRADAR_TEST_RV=/path/to/DE1200_RV_LATEST.tar.bz2 cargo test -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn repeated_builds_do_not_grow() {
        let path = std::env::var("REGENRADAR_TEST_RV").expect("set REGENRADAR_TEST_RV");
        let archive = std::fs::read(&path).unwrap();

        let rss = || -> i64 {
            let out = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &std::process::id().to_string()])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .unwrap_or(0)
        };

        let mut readings = Vec::new();
        for i in 0..6 {
            let part = build_part(&archive, 1430.0, 64).unwrap();
            assert_eq!(part.frames.len(), 25);
            drop(part);
            let kb = rss();
            readings.push(kb);
            println!("build {i}: RSS {} MB", kb / 1024);
        }

        // The first pass legitimately allocates the reprojection table; anything
        // after that should reuse it and settle.
        let settled = readings[readings.len() - 1] - readings[2];
        println!("growth after warm-up: {} MB", settled / 1024);
        assert!(
            settled < 20 * 1024,
            "resident memory grew {} MB over four builds",
            settled / 1024
        );
    }

    #[test]
    fn snapping_only_ever_grows_and_stays_on_the_grid() {
        let c = Crop {
            c0: 121,
            c1: 1071,
            j0: 64,
            j1: 1123,
        }
        .snapped(64);
        assert!(c.c0 <= 121 && c.c1 >= 1071 && c.j0 <= 64 && c.j1 >= 1123);
        assert_eq!(c.c0 % 64, 0);
        assert_eq!((c.c1 + 1) % 64, 0);
        assert!(c.c1 < RAD_NX && c.j1 < RAD_NY);
    }

    #[test]
    fn snapping_absorbs_drift_within_a_cell() {
        // Consecutive runs differing by a few pixels must land on the same crop, or
        // the reprojection table would be rebuilt every five minutes.
        let a = Crop {
            c0: 121,
            c1: 1071,
            j0: 70,
            j1: 1123,
        }
        .snapped(64);
        let b = Crop {
            c0: 118,
            c1: 1075,
            j0: 66,
            j1: 1120,
        }
        .snapped(64);
        assert_eq!(a, b);
    }

    #[test]
    fn snapping_is_idempotent() {
        let once = Crop {
            c0: 121,
            c1: 1071,
            j0: 64,
            j1: 1123,
        }
        .snapped(64);
        assert_eq!(once.snapped(64), once);
    }

    #[test]
    fn snapping_the_full_grid_is_a_no_op() {
        assert_eq!(Crop::full().snapped(64), Crop::full());
    }

    #[test]
    fn cropped_box_sits_inside_the_full_grid_box() {
        let full = Crop::full().merc_box();
        let crop = Crop {
            c0: 64,
            c1: 1087,
            j0: 64,
            j1: 1151,
        }
        .merc_box();
        assert!(crop.x0 >= full.x0 && crop.x1 <= full.x1);
        assert!(crop.y0 >= full.y0 && crop.y1 <= full.y1);
        assert!(crop.width() < full.width() && crop.height() < full.height());
    }

    #[test]
    fn full_crop_spans_the_documented_grid_extent() {
        // The DE1200 envelope, cross-checked against the DWD WMS capabilities.
        let b = Crop::full().merc_box();
        let [west, south, east, north] = b.lonlat();
        assert!((west - 1.4633).abs() < 0.02, "west = {west}");
        assert!((east - 18.7317).abs() < 0.02, "east = {east}");
        assert!((south - 45.6848).abs() < 0.02, "south = {south}");
        assert!((north - 56.2196).abs() < 0.02, "north = {north}");
    }
}
