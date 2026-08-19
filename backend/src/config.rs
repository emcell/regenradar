//! Runtime configuration. Everything is env-overridable so the same binary can run
//! on a laptop and on a small VPS without a config file.

use std::{env, net::SocketAddr, path::PathBuf, time::Duration};

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    /// Directory of the built frontend. Served at `/` when it exists.
    pub web_dir: PathBuf,

    // --- DWD RV radar composite, the only data source ---
    pub dwd_url: String,
    /// How often to check whether a newer 5-minute run has been published.
    pub dwd_poll: Duration,
    /// Raster resolution in Web-Mercator metres per pixel.
    ///
    /// The default is the point below which nothing is gained. The source grid is
    /// 1 km on the ground, and Mercator stretches by `1/cos(latitude)`, so one
    /// ground kilometre spans 1431 Mercator metres at the composite's southern edge
    /// (45.68°N) and more further north. Sampling finer than that only interpolates.
    pub dwd_resolution_m: f64,
    /// Crop the raster to where the run actually has data, snapped to this many
    /// source pixels so the geometry stays stable between runs.
    pub dwd_crop_snap: usize,

    // --- ICON-D2, the forecast past the radar's two hours ---
    pub model_enabled: bool,
    pub model_url_base: String,
    /// How far ahead of now the model part reaches, in hours.
    pub model_horizon_hours: i64,
    /// How often to reconsider which run and which steps are needed. The download
    /// itself only happens when that choice actually changes.
    pub model_poll: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // 8080 collides with too much on a dev machine.
            bind: "0.0.0.0:8787".parse().unwrap(),
            web_dir: PathBuf::from("../web/dist"),

            dwd_url: "https://opendata.dwd.de/weather/radar/composite/rv/DE1200_RV_LATEST.tar.bz2"
                .into(),
            dwd_poll: Duration::from_secs(60),
            dwd_resolution_m: 1430.0,
            dwd_crop_snap: 64,

            model_enabled: true,
            model_url_base: "https://opendata.dwd.de/weather/nwp/icon-d2/grib".into(),
            model_horizon_hours: 6,
            model_poll: Duration::from_secs(600),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            bind: var("REGENRADAR_BIND")
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.bind),
            web_dir: var("REGENRADAR_WEB_DIR")
                .map(PathBuf::from)
                .unwrap_or(d.web_dir),

            dwd_url: var("REGENRADAR_DWD_URL").unwrap_or(d.dwd_url),
            dwd_poll: secs("REGENRADAR_DWD_POLL_SECS").unwrap_or(d.dwd_poll),
            dwd_resolution_m: num("REGENRADAR_DWD_RESOLUTION_M").unwrap_or(d.dwd_resolution_m),
            dwd_crop_snap: num::<f64>("REGENRADAR_DWD_CROP_SNAP")
                .map(|v| (v as usize).clamp(1, 512))
                .unwrap_or(d.dwd_crop_snap),

            model_enabled: var("REGENRADAR_MODEL_ENABLED")
                .map(|v| !matches!(v.as_str(), "0" | "false" | "no"))
                .unwrap_or(d.model_enabled),
            model_url_base: var("REGENRADAR_MODEL_URL").unwrap_or(d.model_url_base),
            model_horizon_hours: num::<f64>("REGENRADAR_MODEL_HORIZON_HOURS")
                .map(|v| (v as i64).clamp(0, 48))
                .unwrap_or(d.model_horizon_hours),
            model_poll: secs("REGENRADAR_MODEL_POLL_SECS").unwrap_or(d.model_poll),
        }
    }
}

fn var(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn num<T: std::str::FromStr>(key: &str) -> Option<T> {
    var(key).and_then(|v| v.trim().parse().ok())
}

fn secs(key: &str) -> Option<Duration> {
    num::<u64>(key).map(Duration::from_secs)
}
