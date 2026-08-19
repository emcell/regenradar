//! GraphQL schema.
//!
//! The API deliberately carries *metadata only*. Pixels travel as plain PNG over
//! HTTP under an immutable, run-stamped URL, because that is what browsers and CDNs
//! already cache well and what `texImage2D` wants anyway. GraphQL's job is to tell
//! the client which frames exist, when they are valid, where they belong on the map,
//! and how to decode and colour them — so the colour scale and intensity encoding
//! live in exactly one place.

use std::sync::Arc;

use async_graphql::{Context, Enum, Object, SimpleObject};
use chrono::{DateTime, Utc};

use crate::{
    raster::{INTENSITY_MAX_MMH, INTENSITY_MIN_MMH},
    state::{self, AppState, FrameOrigin, LayerBundle},
};

/// Which kind of forecast a frame is.
///
/// The two are never blended: radar extrapolation is more accurate while it lasts,
/// so it simply wins until its last frame and the model takes over from the next
/// step. Clients should make the difference visible rather than hide it.
#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum Origin {
    /// Extrapolated radar observation. 1 km, five-minute steps, out to +2 h.
    Radar,
    /// Numerical weather model (ICON-D2). 2.2 km, quarter-hour steps.
    Model,
}

impl From<FrameOrigin> for Origin {
    fn from(o: FrameOrigin) -> Self {
        match o {
            FrameOrigin::Radar => Origin::Radar,
            FrameOrigin::Model => Origin::Model,
        }
    }
}

#[derive(SimpleObject)]
/// A contiguous stretch of the timeline fed by one source.
pub struct Segment {
    pub origin: Origin,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Human-readable name of the source, for a legend or tooltip.
    pub label: String,
    /// Identity of the underlying run.
    pub run: String,
}

#[derive(SimpleObject)]
/// Geographic and Web-Mercator extent of a layer's raster.
pub struct Bounds {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
    /// EPSG:3857 metres. The raster is axis-aligned in this system, which is why the
    /// client can draw it as a plain quad with no reprojection of its own.
    pub merc_x0: f64,
    pub merc_y0: f64,
    pub merc_x1: f64,
    pub merc_y1: f64,
}

#[derive(SimpleObject)]
pub struct Frame {
    /// Position in the stack. Substituted into the layer's URL templates.
    pub index: i32,
    pub time: DateTime<Utc>,
    /// False only for the radar analysis step.
    pub forecast: bool,
    pub origin: Origin,
}

#[derive(SimpleObject)]
pub struct Layer {
    pub id: String,
    pub label: String,
    pub attribution: String,
    /// Changes whenever new data lands; every URL below embeds it.
    pub run: String,
    pub width: i32,
    pub height: i32,
    /// Raster resolution in Mercator metres per pixel.
    pub resolution_meters: f64,
    pub bounds: Bounds,
    pub frames: Vec<Frame>,
    /// Motion fields between consecutive frames; `frames.length - 1` entries.
    pub flow_urls: Vec<String>,
    /// Displacement encoded by a full-scale motion-field component, in layer pixels.
    pub flow_max_pixels: f64,
    /// How frames are cut up for delivery.
    pub tiling: TilingInfo,
    pub updated_at: DateTime<Utc>,
}

#[derive(SimpleObject)]
/// The delivery grid.
///
/// Every frame ships as `cols * rows` full-resolution tiles plus one downscaled
/// overview. A client zoomed into a town fetches the handful of tiles its viewport
/// covers instead of the whole composite; a client looking at the country fetches
/// the overview, which is all its screen can resolve anyway.
///
/// URLs are built from templates rather than enumerated: at 41 frames times 16 tiles
/// a full list would be a bigger payload than the metadata it belongs to.
/// `{frame}` is the zero-padded two-digit frame index, `{x}` and `{y}` are tile
/// coordinates.
pub struct TilingInfo {
    pub cols: i32,
    pub rows: i32,
    /// Nominal tile size in raster pixels; the last column and row may be smaller.
    pub tile_width: i32,
    pub tile_height: i32,
    pub overview_width: i32,
    pub overview_height: i32,
    pub tile_url_template: String,
    pub overview_url_template: String,
    /// Coverage mask, tiled the same way. Absent when the layer covers its whole box.
    pub mask_url_template: Option<String>,
}

#[derive(SimpleObject)]
/// The slider's extent. Starts at the present — this is a forecast tool.
pub struct Timeline {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Native spacing of the finest source; useful for tick marks, not a constraint
    /// on the slider, which is continuous.
    pub step_seconds: i32,
    /// Which source covers which stretch, in order. This is what lets a client
    /// colour the slider by provenance instead of pretending the forecast is
    /// uniform.
    pub segments: Vec<Segment>,
}

#[derive(SimpleObject)]
/// How to turn an encoded pixel byte back into a rain rate.
///
/// `byte == 0` means no precipitation. Otherwise:
/// `mmh = minMmh * (maxMmh / minMmh) ^ ((byte - 1) / 254)`.
pub struct IntensityScale {
    pub min_mmh: f64,
    pub max_mmh: f64,
}

#[derive(SimpleObject)]
/// One stop of the shared colour ramp, positioned on the normalised 0..1 intensity
/// scale rather than on mm/h, so client and legend cannot drift apart.
pub struct ColorStop {
    pub at: f64,
    pub r: i32,
    pub g: i32,
    pub b: i32,
    pub a: f64,
}

#[derive(SimpleObject)]
pub struct RadarState {
    pub timeline: Option<Timeline>,
    pub layers: Vec<Layer>,
    pub intensity: IntensityScale,
    pub color_scale: Vec<ColorStop>,
    pub attribution: Vec<String>,
    /// True once at least one layer has data.
    pub ready: bool,
    pub server_time: DateTime<Utc>,
}

/// The colour ramp. Chosen to stay legible on a dark basemap: drizzle appears as a
/// faint cool wash rather than a solid block, and the warm end is reserved for rates
/// that actually matter.
fn color_scale() -> Vec<ColorStop> {
    const STOPS: [(f64, i32, i32, i32, f64); 9] = [
        (0.000, 70, 150, 255, 0.00),
        (0.045, 70, 165, 255, 0.40),
        (0.160, 45, 205, 225, 0.72),
        (0.300, 40, 210, 130, 0.88),
        (0.450, 180, 225, 65, 0.95),
        (0.580, 255, 210, 55, 1.00),
        (0.700, 255, 145, 45, 1.00),
        (0.830, 238, 55, 60, 1.00),
        (1.000, 226, 60, 205, 1.00),
    ];
    STOPS
        .iter()
        .map(|&(at, r, g, b, a)| ColorStop { at, r, g, b, a })
        .collect()
}

fn to_layer(b: &Arc<LayerBundle>) -> Layer {
    let [west, south, east, north] = b.merc.lonlat();
    Layer {
        id: b.id.clone(),
        label: b.label.clone(),
        attribution: b.attribution.clone(),
        run: b.run.clone(),
        width: b.width as i32,
        height: b.height as i32,
        resolution_meters: b.resolution_m,
        bounds: Bounds {
            west,
            south,
            east,
            north,
            merc_x0: b.merc.x0,
            merc_y0: b.merc.y0,
            merc_x1: b.merc.x1,
            merc_y1: b.merc.y1,
        },
        frames: b
            .frames
            .iter()
            .map(|f| Frame {
                // The stack index, which is also what the URL templates substitute.
                index: f.index as i32,
                time: f.time,
                forecast: f.forecast,
                origin: f.origin.into(),
            })
            .collect(),
        flow_urls: b.flow.iter().map(|f| b.url(f)).collect(),
        flow_max_pixels: b.flow_max_px,
        tiling: TilingInfo {
            cols: b.tiling.cols as i32,
            rows: b.tiling.rows as i32,
            tile_width: b.tiling.tile_w as i32,
            tile_height: b.tiling.tile_h as i32,
            overview_width: b.tiling.overview_w as i32,
            overview_height: b.tiling.overview_h as i32,
            tile_url_template: b.url("t{frame}_{x}_{y}.webp"),
            overview_url_template: b.url("o{frame}.webp"),
            mask_url_template: b.has_mask.then(|| b.url("k{x}_{y}.webp")),
        },
        updated_at: b.updated_at,
    }
}

pub struct Query;

#[Object]
impl Query {
    /// Everything the map needs in one round trip.
    async fn radar<'a>(&self, ctx: &Context<'a>) -> async_graphql::Result<RadarState> {
        let state = ctx.data::<Arc<AppState>>()?;
        let layer = state.layer();

        Ok(RadarState {
            timeline: layer
                .as_ref()
                .and_then(|b| state::timeline_for(b))
                .zip(layer.as_ref())
                .map(|(t, b)| Timeline {
                    start: t.start,
                    end: t.end,
                    step_seconds: t.step_seconds as i32,
                    segments: b
                        .segments
                        .iter()
                        .map(|s| Segment {
                            origin: s.origin.into(),
                            start: s.start,
                            end: s.end,
                            label: s.origin.label().to_string(),
                            run: s.source.clone(),
                        })
                        .collect(),
                }),
            ready: layer.is_some(),
            attribution: layer.iter().map(|b| b.attribution.clone()).collect(),
            layers: layer.iter().map(to_layer).collect(),
            intensity: IntensityScale {
                min_mmh: INTENSITY_MIN_MMH as f64,
                max_mmh: INTENSITY_MAX_MMH as f64,
            },
            color_scale: color_scale(),
            server_time: Utc::now(),
        })
    }
}

pub type RadarSchema =
    async_graphql::Schema<Query, async_graphql::EmptyMutation, async_graphql::EmptySubscription>;

pub fn schema(state: Arc<AppState>) -> RadarSchema {
    async_graphql::Schema::build(
        Query,
        async_graphql::EmptyMutation,
        async_graphql::EmptySubscription,
    )
    .data(state)
    .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_scale_is_ordered_and_normalised() {
        let stops = color_scale();
        assert_eq!(stops.first().unwrap().at, 0.0);
        assert_eq!(stops.last().unwrap().at, 1.0);
        for w in stops.windows(2) {
            assert!(w[1].at > w[0].at, "stops must increase");
        }
        for s in &stops {
            assert!((0.0..=1.0).contains(&s.a));
            assert!(
                (0..=255).contains(&s.r) && (0..=255).contains(&s.g) && (0..=255).contains(&s.b)
            );
        }
    }

    #[tokio::test]
    async fn query_works_before_any_data_arrives() {
        let state = Arc::new(AppState::new(
            crate::config::Config::default(),
            reqwest::Client::new(),
        ));
        let schema = schema(state);
        let res = schema
            .execute("{ radar { ready timeline { start } layers { id } intensity { minMmh maxMmh } colorScale { at } } }")
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        assert_eq!(data["radar"]["ready"], false);
        assert!(data["radar"]["timeline"].is_null());
        assert_eq!(data["radar"]["layers"].as_array().unwrap().len(), 0);
        assert_eq!(data["radar"]["colorScale"].as_array().unwrap().len(), 9);
    }
}
