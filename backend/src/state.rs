//! The in-memory snapshot the API serves from.
//!
//! Two sources feed one timeline:
//!
//! * **DWD RV radar**, every five minutes, covering now to +2 h at 1 km. This is an
//!   extrapolation of what the radars actually see.
//! * **DWD ICON-D2**, every three hours, covering the rest of the horizon at 2.2 km
//!   in quarter-hour steps. This is a weather model, a different kind of forecast.
//!
//! They are *not* blended. The radar is more accurate while it lasts, so it simply
//! wins up to its last frame and the model takes over from the next step onwards.
//! The seam is exposed through the API instead of hidden, so the client can show
//! which part of the slider is which.
//!
//! Both render into the same raster geometry, which makes the published layer a
//! single continuous frame stack — the client never learns there are two sources.
//! The geometry comes from the radar run, so whenever the radar's crop shifts the
//! model part is re-rendered against the new one.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use chrono::{DateTime, Duration, Utc};

use crate::{config::Config, proj::MercBox};

/// Where a frame's data came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOrigin {
    Radar,
    Model,
}

impl FrameOrigin {
    pub fn label(self) -> &'static str {
        match self {
            FrameOrigin::Radar => "Radar",
            FrameOrigin::Model => "Modell",
        }
    }
}

/// The raster both sources are resampled into.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub merc: MercBox,
    pub width: u32,
    pub height: u32,
    pub resolution_m: f64,
}

/// One rendered timestep: an overview plus the full-resolution tiles.
#[derive(Debug)]
pub struct RenderedFrame {
    pub overview: Arc<Vec<u8>>,
    /// Row-major, `tiling.rows * tiling.cols` entries.
    pub tiles: Vec<Arc<Vec<u8>>>,
}

/// One source's contribution, already rendered.
#[derive(Debug)]
pub struct SourcePart {
    /// Identity of the upstream run, for change detection and the published run id.
    pub run: String,
    pub origin: FrameOrigin,
    pub geometry: Geometry,
    pub times: Vec<DateTime<Utc>>,
    pub frames: Vec<RenderedFrame>,
    pub tiling: crate::raster::Tiling,
    /// Motion field between consecutive frames; `frames.len() - 1` entries.
    pub flow: Vec<Arc<Vec<u8>>>,
    /// Radar coverage mask, tiled like the frames so the point readout fetches only
    /// the tile it needs.
    pub mask: Option<Vec<Arc<Vec<u8>>>>,
    pub updated_at: DateTime<Utc>,
}

/// One timestep of the published layer.
#[derive(Debug, Clone)]
pub struct FrameMeta {
    pub time: DateTime<Utc>,
    /// False only for the radar analysis step.
    pub forecast: bool,
    pub origin: FrameOrigin,
    /// Index within the published stack; every tile URL derives from it.
    pub index: usize,
}

/// A contiguous stretch of the timeline fed by one source.
#[derive(Debug, Clone)]
pub struct Segment {
    pub origin: FrameOrigin,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct LayerBundle {
    pub id: String,
    pub label: String,
    pub attribution: String,
    /// Changes whenever new data lands; every URL below embeds it.
    pub run: String,
    pub width: u32,
    pub height: u32,
    pub merc: MercBox,
    pub resolution_m: f64,
    pub frames: Vec<FrameMeta>,
    pub flow: Vec<String>,
    pub flow_max_px: f64,
    pub tiling: crate::raster::Tiling,
    /// True when a tiled coverage mask is published.
    pub has_mask: bool,
    pub segments: Vec<Segment>,
    pub updated_at: DateTime<Utc>,
    pub files: HashMap<String, Arc<Vec<u8>>>,
}

#[cfg(test)]
impl SourcePart {
    fn clone_for_test(&self) -> SourcePart {
        SourcePart {
            run: self.run.clone(),
            origin: self.origin,
            geometry: self.geometry,
            times: self.times.clone(),
            frames: self
                .frames
                .iter()
                .map(|f| RenderedFrame {
                    overview: f.overview.clone(),
                    tiles: f.tiles.clone(),
                })
                .collect(),
            tiling: self.tiling,
            flow: self.flow.clone(),
            mask: self.mask.clone(),
            updated_at: self.updated_at,
        }
    }
}

impl LayerBundle {
    pub fn url(&self, file: &str) -> String {
        format!("/data/{}/{}/{}", self.id, self.run, file)
    }

    pub fn file(&self, name: &str) -> Option<Arc<Vec<u8>>> {
        self.files.get(name).cloned()
    }

    pub fn bytes(&self) -> usize {
        self.files.values().map(|v| v.len()).sum()
    }
}

pub const LAYER_ID: &str = "dwd";

pub struct AppState {
    pub cfg: Config,
    pub http: reqwest::Client,
    radar: RwLock<Option<Arc<SourcePart>>>,
    model: RwLock<Option<Arc<SourcePart>>>,
    published: RwLock<Option<Arc<LayerBundle>>>,
    /// Zero-motion field, inserted where the two sources meet.
    still: RwLock<Option<Arc<Vec<u8>>>>,
    pub started_at: DateTime<Utc>,
}

impl AppState {
    pub fn new(cfg: Config, http: reqwest::Client) -> Self {
        Self {
            cfg,
            http,
            radar: RwLock::new(None),
            model: RwLock::new(None),
            published: RwLock::new(None),
            still: RwLock::new(None),
            started_at: Utc::now(),
        }
    }

    pub fn radar(&self) -> Option<Arc<SourcePart>> {
        self.radar.read().unwrap().clone()
    }

    pub fn model(&self) -> Option<Arc<SourcePart>> {
        self.model.read().unwrap().clone()
    }

    pub fn layer(&self) -> Option<Arc<LayerBundle>> {
        self.published.read().unwrap().clone()
    }

    pub fn set_radar(&self, part: Arc<SourcePart>) {
        *self.radar.write().unwrap() = Some(part);
        self.republish();
    }

    pub fn set_model(&self, part: Arc<SourcePart>) {
        *self.model.write().unwrap() = Some(part);
        self.republish();
    }

    pub fn set_still(&self, png: Arc<Vec<u8>>) {
        *self.still.write().unwrap() = Some(png);
    }

    /// Rebuild the published layer from whatever the sources currently hold.
    pub fn republish(&self) {
        let Some(radar) = self.radar() else { return };
        let still = self.still.read().unwrap().clone();
        let bundle = merge(
            &radar,
            self.model().as_deref(),
            still,
            Duration::hours(self.cfg.model_horizon_hours),
        );
        *self.published.write().unwrap() = Some(Arc::new(bundle));
    }

    pub fn lookup(&self, layer: &str, run: &str, file: &str) -> Option<Arc<Vec<u8>>> {
        if layer != LAYER_ID {
            return None;
        }
        self.layer().filter(|b| b.run == run)?.file(file)
    }
}

/// Stitch the radar and model parts into one frame stack.
///
/// The radar always wins where it exists; the model contributes only the steps
/// strictly after the radar's last frame, and only while its geometry still matches.
fn merge(
    radar: &SourcePart,
    model: Option<&SourcePart>,
    still: Option<Arc<Vec<u8>>>,
    horizon: Duration,
) -> LayerBundle {
    let mut files: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
    let mut frames: Vec<FrameMeta> = Vec::new();
    let mut flow: Vec<String> = Vec::new();
    let mut segments: Vec<Segment> = Vec::new();

    let push_frame = |files: &mut HashMap<String, Arc<Vec<u8>>>,
                      frames: &mut Vec<FrameMeta>,
                      time: DateTime<Utc>,
                      origin: FrameOrigin,
                      forecast: bool,
                      rendered: &RenderedFrame,
                      tiling: &crate::raster::Tiling| {
        let index = frames.len();
        files.insert(overview_name(index), rendered.overview.clone());
        for (i, tile) in rendered.tiles.iter().enumerate() {
            let (tx, ty) = (i as u32 % tiling.cols, i as u32 / tiling.cols);
            files.insert(tile_name(index, tx, ty), tile.clone());
        }
        frames.push(FrameMeta {
            time,
            forecast,
            origin,
            index,
        });
    };

    for (i, (time, rendered)) in radar.times.iter().zip(&radar.frames).enumerate() {
        push_frame(
            &mut files,
            &mut frames,
            *time,
            radar.origin,
            i > 0,
            rendered,
            &radar.tiling,
        );
        if i > 0 {
            if let Some(f) = radar.flow.get(i - 1) {
                let name = format!("m{:02}.webp", flow.len());
                files.insert(name.clone(), f.clone());
                flow.push(name);
            }
        }
    }
    if let (Some(first), Some(last)) = (radar.times.first(), radar.times.last()) {
        segments.push(Segment {
            origin: radar.origin,
            start: *first,
            end: *last,
            source: radar.run.clone(),
        });
    }

    let radar_end = radar.times.last().copied();
    let cutoff = Utc::now() + horizon;

    if let (Some(model), Some(radar_end)) = (model, radar_end) {
        // The model is only stitched in while it matches the radar's geometry *and*
        // its tiling: the client addresses both sources through one tile grid.
        if model.geometry == radar.geometry && model.tiling == radar.tiling {
            let usable: Vec<usize> = model
                .times
                .iter()
                .enumerate()
                .filter(|(_, t)| **t > radar_end && **t <= cutoff)
                .map(|(i, _)| i)
                .collect();

            let mut first_model = None;
            for (n, &i) in usable.iter().enumerate() {
                // The step across the seam gets no motion field: the two sources are
                // independent forecasts, and matching one against the other would
                // invent a displacement that means nothing.
                let motion = if n == 0 {
                    still.clone()
                } else {
                    model.flow.get(i - 1).cloned()
                };
                if let Some(png) = motion {
                    let name = format!("m{:02}.webp", flow.len());
                    files.insert(name.clone(), png);
                    flow.push(name);
                }
                push_frame(
                    &mut files,
                    &mut frames,
                    model.times[i],
                    model.origin,
                    true,
                    &model.frames[i],
                    &model.tiling,
                );
                if first_model.is_none() {
                    first_model = Some(model.times[i]);
                }
            }
            if let (Some(start), Some(&last)) = (first_model, usable.last()) {
                segments.push(Segment {
                    origin: model.origin,
                    start,
                    end: model.times[last],
                    source: model.run.clone(),
                });
            }
        }
    }

    let has_mask = radar.mask.is_some();
    if let Some(mask) = &radar.mask {
        for (i, tile) in mask.iter().enumerate() {
            let (tx, ty) = (i as u32 % radar.tiling.cols, i as u32 / radar.tiling.cols);
            files.insert(mask_name(tx, ty), tile.clone());
        }
    }

    let run = match model {
        Some(m) if segments.len() > 1 => format!("{}-{}", radar.run, m.run),
        _ => radar.run.clone(),
    };

    LayerBundle {
        id: LAYER_ID.into(),
        label: "Niederschlag".into(),
        attribution: crate::sources::dwd::ATTRIBUTION.into(),
        run,
        width: radar.geometry.width,
        height: radar.geometry.height,
        merc: radar.geometry.merc,
        resolution_m: radar.geometry.resolution_m,
        frames,
        flow,
        flow_max_px: crate::motion::FLOW_MAX_PX,
        tiling: radar.tiling,
        has_mask,
        segments,
        updated_at: Utc::now(),
        files,
    }
}

/// File names. Kept as functions so the client's URL templates and the server agree
/// in one place.
pub fn overview_name(frame: usize) -> String {
    format!("o{frame:02}.webp")
}

pub fn tile_name(frame: usize, x: u32, y: u32) -> String {
    format!("t{frame:02}_{x}_{y}.webp")
}

pub fn mask_name(x: u32, y: u32) -> String {
    format!("k{x}_{y}.webp")
}

#[derive(Debug, Clone, Copy)]
pub struct Timeline {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub step_seconds: i64,
}

/// The slider's time axis.
///
/// Starts at "now" — the brief is a forecast, so anything already past is not worth
/// a tick — and ends where the last frame does.
pub fn timeline_for(layer: &LayerBundle) -> Option<Timeline> {
    let first = layer.frames.first()?.time;
    let last = layer.frames.last()?.time;
    let now = Utc::now();

    // A run that has gone stale would otherwise produce an empty axis; show what is
    // left instead.
    let start = if now > first && now < last {
        now
    } else {
        first
    };
    Some(Timeline {
        start,
        end: last,
        step_seconds: 300,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> Geometry {
        Geometry {
            merc: MercBox {
                x0: 0.0,
                y0: 0.0,
                x1: 1000.0,
                y1: 1000.0,
            },
            width: 4,
            height: 4,
            resolution_m: 1430.0,
        }
    }

    fn png(tag: u8) -> Arc<Vec<u8>> {
        Arc::new(vec![tag])
    }

    fn tiling() -> crate::raster::Tiling {
        // Realistic enough to produce a 2x2 grid; `choose` clamps tiny targets.
        crate::raster::Tiling::choose(128, 128, 64)
    }

    fn part(
        origin: FrameOrigin,
        run: &str,
        start: DateTime<Utc>,
        n: usize,
        step_min: i64,
        geo: Geometry,
    ) -> Arc<SourcePart> {
        let t = tiling();
        let tiles = (t.cols * t.rows) as usize;
        Arc::new(SourcePart {
            run: run.into(),
            origin,
            geometry: geo,
            times: (0..n)
                .map(|i| start + Duration::minutes(i as i64 * step_min))
                .collect(),
            frames: (0..n)
                .map(|i| RenderedFrame {
                    overview: png(i as u8),
                    tiles: (0..tiles).map(|k| png((i * 10 + k) as u8)).collect(),
                })
                .collect(),
            tiling: t,
            flow: (0..n.saturating_sub(1))
                .map(|i| png(100 + i as u8))
                .collect(),
            mask: (origin == FrameOrigin::Radar)
                .then(|| (0..tiles).map(|k| png(200 + k as u8)).collect()),
            updated_at: Utc::now(),
        })
    }

    fn state() -> AppState {
        AppState::new(Config::default(), reqwest::Client::new())
    }

    #[test]
    fn radar_alone_publishes_a_radar_only_timeline() {
        let s = state();
        let now = Utc::now();
        s.set_radar(part(FrameOrigin::Radar, "r1", now, 25, 5, geometry()));
        let l = s.layer().unwrap();
        assert_eq!(l.frames.len(), 25);
        assert_eq!(l.segments.len(), 1);
        assert_eq!(l.segments[0].origin, FrameOrigin::Radar);
        assert_eq!(l.run, "r1");
        assert!(l.has_mask);
        // Every frame publishes an overview and a full set of tiles.
        let t = tiling();
        for f in &l.frames {
            assert!(l.files.contains_key(&overview_name(f.index)));
            for y in 0..t.rows {
                for x in 0..t.cols {
                    assert!(
                        l.files.contains_key(&tile_name(f.index, x, y)),
                        "missing tile {x},{y} of frame {}",
                        f.index
                    );
                }
            }
        }
        assert!(l.files.contains_key(&mask_name(0, 0)));
    }

    #[test]
    fn model_extends_the_stack_only_beyond_the_radar() {
        let s = state();
        let now = Utc::now();
        // Radar: now .. now+120 in 5-minute steps.
        s.set_radar(part(FrameOrigin::Radar, "r1", now, 25, 5, geometry()));
        // Model: starts at now, quarter-hourly, out past the radar.
        s.set_model(part(FrameOrigin::Model, "m1", now, 40, 15, geometry()));

        let l = s.layer().unwrap();
        let radar_end = now + Duration::minutes(120);

        assert_eq!(l.segments.len(), 2);
        assert_eq!(l.segments[1].origin, FrameOrigin::Model);
        assert!(
            l.segments[1].start > radar_end,
            "model must not overlap the radar"
        );
        assert_eq!(l.run, "r1-m1");

        // Times strictly increase across the seam.
        for w in l.frames.windows(2) {
            assert!(w[1].time > w[0].time, "frames must be ordered");
        }
        // Everything up to the radar's end is radar; everything after is model.
        for f in &l.frames {
            let want = if f.time <= radar_end {
                FrameOrigin::Radar
            } else {
                FrameOrigin::Model
            };
            assert_eq!(f.origin, want, "wrong origin at {}", f.time);
        }
    }

    #[test]
    fn the_horizon_caps_the_model_part() {
        let cfg = Config {
            model_horizon_hours: 6,
            ..Default::default()
        };
        let s = AppState::new(cfg, reqwest::Client::new());
        let now = Utc::now();
        s.set_radar(part(FrameOrigin::Radar, "r1", now, 25, 5, geometry()));
        // 48 hours of model data offered; only six may be used.
        s.set_model(part(FrameOrigin::Model, "m1", now, 192, 15, geometry()));

        let l = s.layer().unwrap();
        let last = l.frames.last().unwrap().time;
        assert!(
            last <= now + Duration::hours(6) + Duration::minutes(1),
            "last frame {last}"
        );
        assert!(
            last > now + Duration::hours(5),
            "horizon should be nearly filled"
        );
    }

    #[test]
    fn a_model_tiled_differently_is_ignored() {
        // Both sources are addressed through one tile grid, so a mismatch has to be
        // dropped rather than stitched in.
        let s = state();
        let now = Utc::now();
        s.set_radar(part(FrameOrigin::Radar, "r1", now, 25, 5, geometry()));
        let mut odd = (*part(FrameOrigin::Model, "m1", now, 40, 15, geometry())).clone_for_test();
        odd.tiling = crate::raster::Tiling::choose(512, 512, 64);
        s.set_model(Arc::new(odd));

        let l = s.layer().unwrap();
        assert_eq!(
            l.segments.len(),
            1,
            "mismatched tiling must not be stitched in"
        );
    }

    #[test]
    fn a_model_rendered_for_another_geometry_is_ignored() {
        let s = state();
        let now = Utc::now();
        s.set_radar(part(FrameOrigin::Radar, "r1", now, 25, 5, geometry()));
        let mut other = geometry();
        other.width = 8;
        s.set_model(part(FrameOrigin::Model, "m1", now, 40, 15, other));

        let l = s.layer().unwrap();
        assert_eq!(
            l.segments.len(),
            1,
            "stale geometry must not be stitched in"
        );
        assert_eq!(l.frames.len(), 25);
    }

    #[test]
    fn the_seam_carries_no_motion_field() {
        let s = state();
        let now = Utc::now();
        s.set_still(png(42));
        s.set_radar(part(FrameOrigin::Radar, "r1", now, 25, 5, geometry()));
        s.set_model(part(FrameOrigin::Model, "m1", now, 40, 15, geometry()));

        let l = s.layer().unwrap();
        assert_eq!(l.flow.len(), l.frames.len() - 1, "one motion field per gap");
        // The bridging entry is the still field, not a matched one.
        let seam = l
            .frames
            .iter()
            .position(|f| f.origin == FrameOrigin::Model)
            .unwrap();
        let bridge = l.files.get(&l.flow[seam - 1]).unwrap();
        assert_eq!(**bridge, vec![42]);
    }

    #[test]
    fn timeline_starts_at_now_not_at_the_run() {
        let s = state();
        let run = Utc::now() - Duration::minutes(8);
        s.set_radar(part(FrameOrigin::Radar, "r1", run, 25, 5, geometry()));
        let l = s.layer().unwrap();
        let t = timeline_for(&l).unwrap();
        assert!(t.start >= Utc::now() - Duration::seconds(2));
        assert_eq!(t.end, run + Duration::minutes(120));
    }

    #[test]
    fn stale_run_still_yields_an_axis() {
        let s = state();
        let run = Utc::now() - Duration::hours(6);
        s.set_radar(part(FrameOrigin::Radar, "r1", run, 25, 5, geometry()));
        let l = s.layer().unwrap();
        let t = timeline_for(&l).unwrap();
        assert!(t.start < t.end);
        assert_eq!(t.start, run);
    }

    #[test]
    fn lookup_rejects_a_stale_run() {
        let s = state();
        s.set_radar(part(FrameOrigin::Radar, "r1", Utc::now(), 2, 5, geometry()));
        assert!(s.lookup(LAYER_ID, "r1", &overview_name(0)).is_some());
        assert!(s.lookup(LAYER_ID, "r1", &tile_name(0, 1, 1)).is_some());
        assert!(s.lookup(LAYER_ID, "old", &overview_name(0)).is_none());
        assert!(s.lookup("nope", "r1", &overview_name(0)).is_none());
        assert!(s.lookup(LAYER_ID, "r1", "missing.webp").is_none());
    }

    #[test]
    fn state_starts_empty() {
        let s = state();
        assert!(s.layer().is_none());
        assert!(s.radar().is_none());
    }
}
