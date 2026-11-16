//! Regenradar backend.
//!
//! Ingests the DWD RV radar composite every five minutes, reprojects it into a Web
//! Mercator raster, estimates the motion between consecutive frames, and serves the
//! result as GraphQL metadata plus immutable PNGs.

mod config;
mod gql;
mod grib2;
mod motion;
mod odim;
mod proj;
mod radolan;
mod raster;
mod sources;
mod state;

use std::sync::Arc;

/// The ingest cycles a handful of multi-megabyte rasters dozens of times per run.
/// The platform allocator handles that pattern by growing its high-water mark until
/// it settles a few hundred megabytes above what is actually live; mimalloc reuses
/// the blocks instead.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use axum::{
    extract::{Path, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response},
    routing::{any, get},
    Router,
};
use tower_http::{
    compression::CompressionLayer, cors::CorsLayer, services::ServeDir, trace::TraceLayer,
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::{config::Config, state::AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn")),
        )
        .init();

    let cfg = Config::from_env();

    // Container health probe: ask the running instance whether it has data, then
    // exit with the answer. Doing it in-process keeps curl out of the runtime image.
    if std::env::args().any(|a| a == "--healthcheck") {
        std::process::exit(match healthcheck(&cfg).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("unhealthy: {e:#}");
                1
            }
        });
    }

    info!(?cfg.bind, "starting regenradar");

    let http = reqwest::Client::builder()
        .user_agent(concat!(
            "regenradar/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/; open data client)"
        ))
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let state = Arc::new(AppState::new(cfg.clone(), http));

    // The motion field used where radar hands over to the model: a declared "no
    // displacement" rather than a match between two unrelated forecasts.
    state.set_still(Arc::new(motion::MotionField::zero().encode()?));

    tokio::spawn(sources::dwd::run(state.clone()));
    tokio::spawn(sources::icon::run(state.clone()));

    let schema = gql::schema(state.clone());

    // Request tracing covers the real endpoints only. A readiness probe answering
    // 503 while the first run is still downloading is expected, and logging that as
    // an error every few seconds makes a healthy startup look broken.
    let api = Router::new()
        .route("/data/{layer}/{run}/{file}", get(frame))
        .route("/graphiql", get(graphiql))
        .route_service("/graphql", async_graphql_axum::GraphQL::new(schema))
        .with_state(state.clone())
        .layer(TraceLayer::new_for_http());

    let probes = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .with_state(state.clone());

    let mut app = api.merge(probes);

    // Serve the built PWA from the same origin when it is present, so the whole
    // thing runs as a single process with no CORS and no separate web server.
    if cfg.web_dir.is_dir() {
        info!(dir = %cfg.web_dir.display(), "serving frontend");
        // Nested in its own Router so the cache policy applies to the static bundle
        // only. Applied app-wide it would also rewrite the frame tiles, whose
        // run-stamped URLs are the whole reason they can be cached forever.
        let statics = Router::new()
            .fallback_service(
                ServeDir::new(&cfg.web_dir).fallback(any(spa_shell).with_state(state.clone())),
            )
            .layer(middleware::from_fn(static_cache_headers));
        app = app.fallback_service(statics);
    } else {
        warn!(
            dir = %cfg.web_dir.display(),
            "frontend build not found — API only (run `npm run build` in web/)"
        );
    }

    // Layers go on last: in axum they only wrap routes registered before the call,
    // so applying them earlier would leave the static file service uncompressed.
    let app = app
        .layer(CorsLayer::permissive())
        // The JS bundle is ~975 kB of text; served raw that is four times the
        // download it needs to be. The default predicate leaves already-compressed
        // responses — every frame and every tile — untouched.
        .layer(CompressionLayer::new());

    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    info!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Single-page-app fallback for unmatched paths.
///
/// Crucially it refuses to answer anything that looks like a file. Serving the HTML
/// shell for a missing `/assets/index-<hash>.js` returns 200 with a body the browser
/// then tries to parse as JavaScript — the app dies with a syntax error and a blank
/// map, which is a far worse failure than an honest 404. This happens for real
/// whenever a client holds a cached `index.html` from a previous build.
async fn spa_shell(State(state): State<Arc<AppState>>, uri: Uri) -> Response {
    if std::path::Path::new(uri.path()).extension().is_some() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match tokio::fs::read(state.cfg.web_dir.join("index.html")).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "index.html missing").into_response(),
    }
}

/// Cache policy for the static bundle.
///
/// Asset file names carry a content hash, so they can be cached forever. Everything
/// that points *at* them — the HTML shell, the service worker, the manifest — must
/// be revalidated, or a new deployment stays invisible behind a stale entry point.
async fn static_cache_headers(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let mut res = next.run(req).await;
    let policy = if path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else if path.ends_with(".js")
        || path.ends_with(".html")
        || path.ends_with(".webmanifest")
        || path == "/"
    {
        "no-cache"
    } else {
        "public, max-age=3600"
    };
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(policy));
    res
}

/// Probe a locally running instance's readiness endpoint.
async fn healthcheck(cfg: &Config) -> anyhow::Result<()> {
    let port = cfg.bind.port();
    let url = format!("http://127.0.0.1:{port}/readyz");
    let res = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(4))
        .build()?
        .get(&url)
        .send()
        .await?;
    if res.status().is_success() {
        Ok(())
    } else {
        anyhow::bail!("{url} answered {}", res.status())
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutting down");
}

async fn healthz(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let layer = state.layer();
    let part = |p: Option<std::sync::Arc<crate::state::SourcePart>>| {
        p.map(|p| {
            serde_json::json!({
                "run": p.run,
                "frames": p.frames.len(),
                "first": p.times.first(),
                "last": p.times.last(),
                "updatedAt": p.updated_at,
            })
        })
    };
    let body = Json(serde_json::json!({
        "ok": layer.is_some(),
        "startedAt": state.started_at,
        "layer": layer.map(|b| serde_json::json!({
            "run": b.run,
            "frames": b.frames.len(),
            "raster": format!("{}x{}", b.width, b.height),
            "segments": b.segments.iter().map(|s| serde_json::json!({
                "origin": s.origin.label(),
                "start": s.start,
                "end": s.end,
            })).collect::<Vec<_>>(),
            "sizeKb": b.bytes() / 1024,
        })),
        "radar": part(state.radar()),
        "model": part(state.model()),
    }));
    // Still 200 while warming up: the process is healthy, it just has no data yet.
    (StatusCode::OK, no_store(), body)
}

/// Probe answers must not be cached anywhere: a stale "ready" routes traffic at an
/// instance that has nothing to serve.
fn no_store() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h
}

/// Readiness, as distinct from liveness.
///
/// `/healthz` answers 200 whenever the process is up, so a scheduler never restarts
/// a perfectly healthy instance because the DWD had a bad minute. This one reports
/// whether there is actually a forecast to serve, which is what a load balancer
/// should route on.
async fn readyz(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.layer() {
        Some(b) => (
            StatusCode::OK,
            no_store(),
            format!("ready: run {}\n", b.run),
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            no_store(),
            "no data yet\n".to_string(),
        ),
    }
}

/// Prometheus text exposition, for alerting on stale data.
///
/// The process stays healthy when the DWD changes a URL or a format — it just keeps
/// serving the last forecast — so liveness probes never notice. What does notice is
/// the age of the newest data: alert on `time() - regenradar_data_timestamp_seconds`.
async fn metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "# HELP regenradar_data_timestamp_seconds Valid time of the first (analysis) frame of the current run, per source.");
    let _ = writeln!(out, "# TYPE regenradar_data_timestamp_seconds gauge");
    let sources = [("radar", state.radar()), ("model", state.model())];
    for (name, part) in &sources {
        if let Some(t) = part.as_ref().and_then(|p| p.times.first()) {
            let _ = writeln!(
                out,
                "regenradar_data_timestamp_seconds{{source=\"{name}\"}} {}",
                t.timestamp()
            );
        }
    }
    let _ = writeln!(out, "# HELP regenradar_ingest_timestamp_seconds When the current run of each source was ingested.");
    let _ = writeln!(out, "# TYPE regenradar_ingest_timestamp_seconds gauge");
    for (name, part) in &sources {
        if let Some(p) = part {
            let _ = writeln!(
                out,
                "regenradar_ingest_timestamp_seconds{{source=\"{name}\"}} {}",
                p.updated_at.timestamp()
            );
        }
    }
    let _ = writeln!(
        out,
        "# HELP regenradar_started_timestamp_seconds Process start time."
    );
    let _ = writeln!(out, "# TYPE regenradar_started_timestamp_seconds gauge");
    let _ = writeln!(
        out,
        "regenradar_started_timestamp_seconds {}",
        state.started_at.timestamp()
    );
    let mut headers = no_store();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    (StatusCode::OK, headers, out)
}

async fn graphiql() -> impl IntoResponse {
    Html(
        async_graphql::http::GraphiQLSource::build()
            .endpoint("/graphql")
            .title("Regenradar API")
            .finish(),
    )
}

/// Serve one rendered PNG.
///
/// The run id is part of the path, so a given URL's bytes never change and the
/// response can be cached forever. When a URL's run is no longer current the answer
/// is 404, which is the client's cue to re-query the schema.
async fn frame(
    State(state): State<Arc<AppState>>,
    Path((layer, run, file)): Path<(String, String, String)>,
) -> impl IntoResponse {
    match state.lookup(&layer, &run, &file) {
        Some(bytes) => {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/webp"));
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
            (StatusCode::OK, headers, bytes.to_vec()).into_response()
        }
        None => (StatusCode::NOT_FOUND, "unknown frame").into_response(),
    }
}
