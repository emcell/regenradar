//! DWD ICON-D2 ingest — the forecast beyond the radar's two hours.
//!
//! Where the radar nowcast extrapolates observed motion and runs out of skill after
//! about two hours, ICON-D2 computes atmospheric physics and reaches 48 hours. It is
//! a different kind of forecast, and this app treats it as one: no blending, a hard
//! handover at the radar's last frame, and the seam declared through the API.
//!
//! # Which files
//!
//! `icon-d2_germany_regular-lat-lon_single-level_<run>_<step>_2d_tot_prec.grib2.bz2`.
//! The regular lat/lon variant matters: it is packed with GRIB2 data representation
//! template 0 (simple packing), which [`crate::grib2`] handles in a few dozen lines.
//! The icosahedral variant — and all of ICON-EU — uses template 42 (CCSDS), which
//! would mean eccodes or an adaptive entropy decoder.
//!
//! Each hourly file holds **four** messages, the quarter hours, so the model part has
//! 15-minute resolution rather than the hourly one the file names suggest.
//!
//! # Accumulations, not rates
//!
//! `tot_prec` is the total since the run started. The rain in one quarter hour is the
//! difference between consecutive messages, which is why one extra message before the
//! wanted window is always fetched.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Timelike, Utc};
use tracing::{debug, info, warn};

use crate::{
    grib2::{self, LatLonGrid},
    motion,
    raster::{self, ResampleMap},
    state::{AppState, FrameOrigin, Geometry, RenderedFrame, SourcePart},
};

/// Model runs start every three hours.
const RUN_INTERVAL_HOURS: i64 = 3;
/// How far back to look for a run that is already published.
const MAX_RUN_AGE_HOURS: i64 = 12;

/// What a refresh intends to fetch. Re-fetching is skipped when this is unchanged.
#[derive(Debug, Clone, PartialEq)]
struct Plan {
    run: DateTime<Utc>,
    first_step: i64,
    last_step: i64,
    geometry: Geometry,
}

impl Plan {
    fn run_id(&self) -> String {
        self.run.format("%Y%m%d%H").to_string()
    }
}

/// What one pass accomplished, which decides how soon to try again.
enum Outcome {
    Updated,
    Unchanged,
    /// No radar run yet, so there is no geometry and no handover point.
    Waiting,
}

pub async fn run(state: Arc<AppState>) {
    if !state.cfg.model_enabled {
        info!("ICON-D2 layer disabled");
        return;
    }
    let mut current: Option<Plan> = None;
    loop {
        let wait = match refresh(&state, &mut current).await {
            Ok(Outcome::Updated) | Ok(Outcome::Unchanged) => state.cfg.model_poll,
            // The first pass usually lands before the radar has finished ingesting;
            // sitting out a full poll interval then would leave the map two hours
            // short for ten minutes.
            Ok(Outcome::Waiting) => std::time::Duration::from_secs(10),
            Err(e) => {
                warn!("ICON-D2 ingest failed: {e:#}");
                std::time::Duration::from_secs(60)
            }
        };
        tokio::time::sleep(wait).await;
    }
}

async fn refresh(state: &Arc<AppState>, current: &mut Option<Plan>) -> Result<Outcome> {
    // The radar defines both the geometry and where the model's turn begins.
    let Some(radar) = state.radar() else {
        debug!("waiting for a radar run before fetching the model");
        return Ok(Outcome::Waiting);
    };
    let Some(&radar_end) = radar.times.last() else {
        return Ok(Outcome::Waiting);
    };

    let horizon = Utc::now() + Duration::hours(state.cfg.model_horizon_hours);
    // A margin so the plan survives the clock advancing between refreshes.
    let wanted_end = horizon + Duration::minutes(45);
    let wanted_start = next_quarter_after(radar_end);
    if wanted_start >= wanted_end {
        return Ok(Outcome::Unchanged);
    }

    let Some(plan) = choose_run(state, wanted_start, wanted_end, radar.geometry).await? else {
        bail!("no ICON-D2 run covers {wanted_start} .. {wanted_end}");
    };
    if current.as_ref() == Some(&plan) {
        return Ok(Outcome::Unchanged);
    }

    let files = fetch(state, &plan).await?;
    let geometry = plan.geometry;
    let run_id = plan.run_id();
    let part = tokio::task::spawn_blocking(move || render(files, run_id, geometry, wanted_start))
        .await
        .context("ICON-D2 render task panicked")??;

    info!(
        run = %part.run,
        frames = part.frames.len(),
        steps = format!("{}..{}", plan.first_step, plan.last_step),
        first = %part.times.first().map(|t| t.to_rfc3339()).unwrap_or_default(),
        last = %part.times.last().map(|t| t.to_rfc3339()).unwrap_or_default(),
        "ICON-D2 run ingested"
    );
    state.set_model(Arc::new(part));
    *current = Some(plan);
    Ok(Outcome::Updated)
}

/// First quarter-hour boundary strictly after `t`.
fn next_quarter_after(t: DateTime<Utc>) -> DateTime<Utc> {
    let floored = t
        .with_minute(t.minute() / 15 * 15)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(t);
    floored + Duration::minutes(15)
}

/// Newest run that already has the steps we need on the server.
///
/// Runs appear roughly 80 minutes after their nominal time, so the most recent
/// nominal run is often incomplete and the one before it has to do.
async fn choose_run(
    state: &Arc<AppState>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    geometry: Geometry,
) -> Result<Option<Plan>> {
    let now = Utc::now();
    let latest = now
        .with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now);
    let latest = latest - Duration::hours(latest.hour() as i64 % RUN_INTERVAL_HOURS);

    for back in 0..=(MAX_RUN_AGE_HOURS / RUN_INTERVAL_HOURS) {
        let run = latest - Duration::hours(back * RUN_INTERVAL_HOURS);
        // One message before the window, so the first rate can be differenced.
        let first_step = step_of(run, start - Duration::minutes(15));
        let last_step = step_of(run, end);
        if first_step < 0 {
            continue;
        }
        let plan = Plan {
            run,
            first_step,
            last_step: last_step.max(first_step),
            geometry,
        };
        // Probing the last step is enough: DWD publishes a run's steps in order.
        if head_ok(state, &plan, plan.last_step).await {
            return Ok(Some(plan));
        }
        debug!(run = %plan.run_id(), step = plan.last_step, "ICON-D2 run not ready");
    }
    Ok(None)
}

/// Which hourly file holds the message valid at `t`.
fn step_of(run: DateTime<Utc>, t: DateTime<Utc>) -> i64 {
    (t - run).num_minutes().div_euclid(60)
}

fn url_for(state: &AppState, plan: &Plan, step: i64) -> String {
    format!(
        "{}/{:02}/tot_prec/icon-d2_germany_regular-lat-lon_single-level_{}_{:03}_2d_tot_prec.grib2.bz2",
        state.cfg.model_url_base.trim_end_matches('/'),
        plan.run.hour(),
        plan.run_id(),
        step
    )
}

async fn head_ok(state: &Arc<AppState>, plan: &Plan, step: i64) -> bool {
    matches!(
        state.http.head(url_for(state, plan, step)).send().await,
        Ok(r) if r.status().is_success()
    )
}

/// The compressed files of one plan, in step order.
///
/// Downloading first and decoding later keeps peak memory to a couple of grids: the
/// bz2 payloads are about a megabyte each, while the decoded `f32` grids are 3.6 MB
/// apiece and there are four per file.
async fn fetch(state: &Arc<AppState>, plan: &Plan) -> Result<Vec<bytes::Bytes>> {
    let mut files = Vec::new();
    for step in plan.first_step..=plan.last_step {
        let url = url_for(state, plan, step);
        match state.http.get(&url).send().await {
            Ok(r) if r.status().is_success() => files.push(r.bytes().await?),
            // A missing tail just shortens the horizon; stop and use what we have.
            Ok(r) => {
                warn!(status = %r.status(), step, "ICON-D2 step unavailable, stopping here");
                break;
            }
            Err(e) => {
                warn!("ICON-D2 step {step} failed: {e}");
                break;
            }
        }
    }
    if files.is_empty() {
        bail!("no ICON-D2 steps could be downloaded");
    }
    Ok(files)
}

/// Difference the accumulations, resample, and encode. Pure CPU.
///
/// Streaming on purpose: only the previous message's accumulation is kept, because
/// `tot_prec` is cumulative and a rate needs exactly two neighbours. Holding the
/// whole series would be some eighty megabytes of `f32` for a few megabytes of output.
fn render(
    files: Vec<bytes::Bytes>,
    run: String,
    geometry: Geometry,
    from: DateTime<Utc>,
) -> Result<SourcePart> {
    let (width, height) = (geometry.width, geometry.height);
    let mut map: Option<Arc<ResampleMap>> = None;
    let mut grid: Option<LatLonGrid> = None;

    let mut prev: Option<(DateTime<Utc>, Vec<f32>)> = None;
    // Scratch buffers reused across every quarter hour of the run.
    let mut field: Vec<f32> = Vec::new();
    let mut raster_buf: Vec<f32> = Vec::new();
    let mut times = Vec::new();
    let mut encoded: Vec<Vec<u8>> = Vec::new();
    let mut frames: Vec<RenderedFrame> = Vec::new();
    // The same grid the radar uses, so the merged stack has one tile scheme.
    let tiling = raster::Tiling::choose(width, height, super::dwd::TILE_TARGET);

    for body in files {
        let mut raw = Vec::new();
        std::io::copy(
            &mut bzip2::read::BzDecoder::new(std::io::Cursor::new(&body[..])),
            &mut raw,
        )?;
        let mut messages = grib2::parse(&raw)?;
        drop(raw);

        // 1/52 is total precipitation. Within a file the quarter hours may be in any
        // order; across files the steps already are.
        messages.retain(|m| (m.category, m.number) == (1, 52) && m.valid_time.is_some());
        messages.sort_by_key(|m| m.valid_time);

        for m in messages {
            let time = m.valid_time.expect("filtered above");
            match grid {
                None => {
                    grid = Some(m.grid);
                    map = Some(resample_map(&m.grid, geometry));
                }
                Some(g) if g != m.grid => bail!("grid changed between steps"),
                _ => {}
            }

            if let Some((t0, ref acc)) = prev {
                let minutes = (time - t0).num_minutes();
                if minutes > 0 && time >= from {
                    // mm accumulated in the window -> mm/h. The window is labelled
                    // with its end, matching how the radar frames are stamped.
                    let per_hour = 60.0 / minutes as f32;
                    field.clear();
                    field.extend(acc.iter().zip(&m.values).map(|(a, b)| {
                        if a.is_nan() || b.is_nan() {
                            f32::NAN
                        } else {
                            (b - a).max(0.0) * per_hour
                        }
                    }));

                    map.as_ref()
                        .expect("set with the grid")
                        .apply_into(&field, &mut raster_buf);
                    let bytes = raster::encode_field(&raster_buf);
                    let (overview, tiles) = raster::split(&bytes, width, height, &tiling)?;
                    frames.push(RenderedFrame {
                        overview: Arc::new(overview),
                        tiles: tiles.into_iter().map(Arc::new).collect(),
                    });
                    encoded.push(bytes);
                    times.push(time);
                }
            }
            prev = Some((time, m.values));
        }
    }

    if frames.is_empty() {
        bail!("no model steps left after the radar's horizon");
    }

    let mut flow = Vec::new();
    for i in 0..encoded.len().saturating_sub(1) {
        let field = motion::estimate(&encoded[i], &encoded[i + 1], width, height);
        flow.push(Arc::new(field.encode()?));
    }
    drop(encoded);

    Ok(SourcePart {
        run,
        origin: FrameOrigin::Model,
        geometry,
        times,
        frames,
        tiling,
        flow,
        mask: None,
        updated_at: Utc::now(),
    })
}

/// Reprojection table from the output raster to the ICON grid, cached because it is
/// only invalidated when the radar's crop moves.
fn resample_map(grid: &LatLonGrid, geometry: Geometry) -> Arc<ResampleMap> {
    use std::sync::{OnceLock, RwLock};
    /// Keyed by the source grid and the output geometry, both of which change rarely.
    type MapCache = OnceLock<RwLock<Option<(LatLonGrid, Geometry, Arc<ResampleMap>)>>>;
    static CACHE: MapCache = OnceLock::new();
    let cache = CACHE.get_or_init(|| RwLock::new(None));

    if let Some((g, geo, map)) = cache.read().unwrap().as_ref() {
        if g == grid && *geo == geometry {
            return map.clone();
        }
    }

    info!(
        raster = format!("{}x{}", geometry.width, geometry.height),
        grid = format!("{}x{}", grid.ni, grid.nj),
        "building ICON-D2 reprojection map"
    );
    let g = *grid;
    let map = Arc::new(ResampleMap::build(
        geometry.merc,
        geometry.width,
        geometry.height,
        grid.ni,
        grid.nj,
        move |lon, lat| g.locate(lon, lat),
    ));
    *cache.write().unwrap() = Some((*grid, geometry, map.clone()));
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 17, h, m, 0).unwrap()
    }

    #[test]
    fn the_model_starts_on_the_quarter_after_the_radar() {
        assert_eq!(next_quarter_after(at(13, 25)), at(13, 30));
        assert_eq!(next_quarter_after(at(13, 30)), at(13, 45));
        assert_eq!(next_quarter_after(at(13, 46)), at(14, 0));
        assert_eq!(next_quarter_after(at(13, 59)), at(14, 0));
    }

    #[test]
    fn steps_map_onto_hourly_files() {
        let run = at(12, 0);
        assert_eq!(step_of(run, at(12, 0)), 0);
        assert_eq!(step_of(run, at(12, 45)), 0, "quarter hours share one file");
        assert_eq!(step_of(run, at(15, 0)), 3);
        assert_eq!(step_of(run, at(15, 59)), 3);
        assert_eq!(step_of(run, at(16, 0)), 4);
    }

    #[test]
    fn a_time_before_the_run_has_no_step() {
        assert!(step_of(at(12, 0), at(11, 30)) < 0);
    }

    #[test]
    fn url_follows_the_dwd_layout() {
        let state = AppState::new(crate::config::Config::default(), reqwest::Client::new());
        let plan = Plan {
            run: at(12, 0),
            first_step: 2,
            last_step: 9,
            geometry: Geometry {
                merc: crate::proj::MercBox {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 1.0,
                    y1: 1.0,
                },
                width: 4,
                height: 4,
                resolution_m: 1430.0,
            },
        };
        let url = url_for(&state, &plan, 3);
        assert!(url.ends_with(
            "/12/tot_prec/icon-d2_germany_regular-lat-lon_single-level_2026081712_003_2d_tot_prec.grib2.bz2"
        ), "got {url}");
    }
}
