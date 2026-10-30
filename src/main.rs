//! Read-only dashboard adapter for the supplied duco-pool-rs API.
mod temperature;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio::sync::RwLock;
use tower_http::compression::CompressionLayer;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_SNAPSHOT_AGE_MS: u64 = 20_000;
const HISTORY_AGE_MS: u64 = 86_400_000;
const MAX_HISTORY_POINTS: usize = 20_000;
// rewriting the whole history every poll wears SD cards; at most a minute is lost on a crash
const HISTORY_SAVE_EVERY: u32 = 12;
const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
const POOL_PROCESS: &str = "duco-pool-rs";

const SCRIPT: &str = include_str!("../assets/script.js");
const STYLES: &str = include_str!("../assets/styles.css");
const CHART_JS: &str = include_str!("../assets/vendor/chart.umd.min.js");
// asset URLs carry a hash of their contents; browsers can cache them indefinitely
// and still pick up a rebuilt dashboard
static INDEX: LazyLock<String> = LazyLock::new(|| {
    let mut hasher = DefaultHasher::new();
    (SCRIPT, STYLES, CHART_JS).hash(&mut hasher);
    include_str!("../assets/index.html")
        .replace("{{ASSET_VERSION}}", &format!("{:016x}", hasher.finish()))
});
fn asset(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    (
        [
            ("content-type", content_type),
            ("cache-control", "public, max-age=31536000, immutable"),
        ],
        body,
    )
}

async fn pool_data(client: &reqwest::Client, config: &Value) -> Option<Value> {
    // Do not log reqwest errors: request URLs contain the pool access key.
    let mut response = client
        .get(format!(
            "http://127.0.0.1:{}/data",
            config["dashboard_port"]
        ))
        .query(&[("key", config["dashboard_key"].as_str()?)])
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    if response
        .content_length()
        .is_some_and(|len| len > MAX_JSON_BYTES as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > MAX_JSON_BYTES.saturating_sub(bytes.len()) {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(|v| v["miner_stats"].is_object() && v["connections"].is_u64())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn number(v: &Value, key: &str) -> f64 {
    v[key].as_f64().filter(|x| x.is_finite()).unwrap_or(0.)
}
fn public_workers(raw: &Value) -> Vec<Value> {
    raw["miner_stats"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .map(|w| {
            json!({
                "username":w["u"], "hashrate":number(w,"h"), "sharetime":w["s"],
                "accepted":number(w,"a"), "rejected":number(w,"r"), "difficulty":w["d"],
                "miner":w["sft"], "rig":w["id"], "lastSeen":number(w,"t")*1000.,
                "ping":w["pg"]
            })
        })
        .collect()
}

/// one chart sample; the field names are what the frontend reads
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Point {
    t: u64,
    hashrate: f64,
    connections: u64,
    workers: usize,
    cpu: f32,
    ram: f64,
    cpu_temp: Option<f64>,
}
/// drops points older than a day, then the oldest beyond the point cap;
/// points are kept in time order, so both are trimmed from the front
fn trim_history(history: &mut VecDeque<Point>, now: u64) {
    let cutoff = now.saturating_sub(HISTORY_AGE_MS);
    while history.front().is_some_and(|p| p.t < cutoff) {
        history.pop_front();
    }
    let extra = history.len().saturating_sub(MAX_HISTORY_POINTS);
    history.drain(..extra);
}
/// a missing file starts empty history; a corrupt or oversized one is set aside as
/// `.bad` rather than failing startup, so a bad write cannot keep the dashboard down
fn load_history(path: &Path) -> VecDeque<Point> {
    if !path.exists() {
        return VecDeque::new();
    }
    let parsed = std::fs::metadata(path)
        .map_err(anyhow::Error::from)
        .and_then(|meta| {
            anyhow::ensure!(meta.len() <= MAX_JSON_BYTES as u64, "file is too large");
            Ok(serde_json::from_slice::<Vec<Point>>(&std::fs::read(path)?)?)
        });
    let mut points = match parsed {
        Ok(points) => points,
        Err(err) => {
            let bad = path.with_extension("json.bad");
            match std::fs::rename(path, &bad) {
                Ok(()) => eprintln!(
                    "Ignoring unreadable dashboard history ({err}); moved it to {}",
                    bad.display()
                ),
                Err(_) => eprintln!("Ignoring unreadable dashboard history ({err})"),
            }
            return VecDeque::new();
        }
    };
    // hand-edited files or clock changes can leave points out of order
    points.sort_by_key(|p| p.t);
    points.dedup_by_key(|p| p.t);
    let mut history = VecDeque::from(points);
    trim_history(&mut history, now());
    history
}

#[derive(Default)]
struct Snapshot {
    stats: Value,
    workers: Vec<Value>,
    rewards: Value,
    updated: u64,
}
struct App {
    snapshot: RwLock<Snapshot>,
    history: RwLock<VecDeque<Point>>,
}
type Shared = Arc<App>;
type ApiResult = Result<Json<Value>, (StatusCode, &'static str)>;
/// `?since=<ms>` returns only history points newer than `since`; polling clients
/// fetch the full day once and then just the new points
async fn statistics(
    State(app): State<Shared>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult {
    let s = app.snapshot.read().await;
    if now().saturating_sub(s.updated) > MAX_SNAPSHOT_AGE_MS {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Pool data is unavailable or stale",
        ));
    }
    let since = params
        .get("since")
        .and_then(|x| x.parse::<u64>().ok())
        .unwrap_or(0);
    let history = app.history.read().await;
    let start = history.partition_point(|p| p.t <= since);
    let mut stats = s.stats.clone();
    stats["history"] = json!(history.range(start..).collect::<Vec<_>>());
    Ok(Json(stats))
}
async fn save_history(app: &App, path: &Path) {
    // serialized under the lock, written without it; readers are not blocked on disk I/O
    let Ok(bytes) = serde_json::to_vec(&*app.history.read().await) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if tokio::fs::write(&tmp, bytes).await.is_ok() {
        if tokio::fs::rename(&tmp, path).await.is_err() {
            eprintln!("Could not replace dashboard history file");
        }
    } else {
        eprintln!("Could not write dashboard history file");
    }
}
async fn workers(State(app): State<Shared>) -> ApiResult {
    let s = app.snapshot.read().await;
    if now().saturating_sub(s.updated) > MAX_SNAPSHOT_AGE_MS {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Pool worker data is unavailable or stale",
        ));
    }
    Ok(Json(json!(s.workers)))
}
async fn rewards(State(app): State<Shared>) -> ApiResult {
    let s = app.snapshot.read().await;
    if now().saturating_sub(s.updated) > MAX_SNAPSHOT_AGE_MS {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Pool reward data is unavailable or stale",
        ));
    }
    Ok(Json(s.rewards.clone()))
}

struct HostMetrics {
    cpu: f32,
    ram: f64,
    temp: Option<f64>,
    uptime: Option<u64>,
}
/// samples the dashboard host; the pool process is looked up by name once and then
/// refreshed by PID, the full process table is only rescanned when that PID goes away
struct HostMonitor {
    system: System,
    pool_pid: Option<Pid>,
}
impl HostMonitor {
    fn new() -> Self {
        let mut system = System::new();
        // CPU usage is a delta between refreshes; the first reading is taken now
        system.refresh_cpu_all();
        Self {
            system,
            pool_pid: None,
        }
    }
    fn sample(&mut self) -> HostMetrics {
        self.system.refresh_cpu_all();
        self.system.refresh_memory();
        if let Some(pid) = self.pool_pid {
            self.system
                .refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
            // the PID may have been reused by another process after a pool restart
            if !self
                .system
                .process(pid)
                .is_some_and(|p| p.name() == POOL_PROCESS)
            {
                self.pool_pid = None;
            }
        }
        if self.pool_pid.is_none() {
            self.system.refresh_processes(ProcessesToUpdate::All, true);
            self.pool_pid = self
                .system
                .processes()
                .values()
                .find(|p| p.name() == POOL_PROCESS)
                .map(|p| p.pid());
        }
        HostMetrics {
            cpu: self.system.global_cpu_usage(),
            ram: 100. * self.system.used_memory() as f64 / self.system.total_memory().max(1) as f64,
            temp: temperature::cpu_temperature(),
            uptime: self
                .pool_pid
                .and_then(|pid| self.system.process(pid))
                .map(|p| p.run_time()),
        }
    }
}

/// adds a point unless the clock went backwards, which would break time ordering
fn record_point(history: &mut VecDeque<Point>, point: Point) {
    if history.back().is_some_and(|last| point.t <= last.t) {
        return;
    }
    let t = point.t;
    history.push_back(point);
    trim_history(history, t);
}

async fn poll(app: Shared, config: Value, history_path: PathBuf) {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(4))
        .build()
        .expect("HTTP client");
    let mut host = HostMonitor::new();
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    let mut polls_since_save = 0;
    loop {
        interval.tick().await;
        let Some(raw) = pool_data(&client, &config).await else {
            eprintln!("Pool API poll failed; retaining the last snapshot");
            continue;
        };
        let rows = public_workers(&raw);
        let metrics = host.sample();
        // pools without these fields predate them; shown as unavailable
        let rewards = raw
            .get("balances")
            .filter(|b| b.is_object())
            .cloned()
            .unwrap_or_else(|| json!({}));
        // unix secs, 0 until the pool's first accepted sync
        let last_sync_time = raw["last_sync_time"].as_u64().filter(|&t| t > 0);
        let accepted: f64 = rows.iter().map(|w| number(w, "accepted")).sum();
        let rejected: f64 = rows.iter().map(|w| number(w, "rejected")).sum();
        let users: HashSet<_> = rows.iter().filter_map(|w| w["username"].as_str()).collect();
        let hashrate: f64 = rows.iter().map(|w| number(w, "hashrate")).sum();
        let connections = raw["connections"].as_u64().unwrap_or_default();
        let stamp = now();
        let stats = json!({
            "poolName": config["poolName"],
            "motd": config["motd"],
            "poolPort": config["port"],
            "runtime": "duco-pool-rs",
            "connections": connections,
            "workers": rows.len(),
            "users": users.len(),
            "hashrate": hashrate,
            "acceptedShares": accepted,
            "rejectedShares": rejected,
            "acceptRate": (accepted + rejected > 0.).then(|| 100. * accepted / (accepted + rejected)),
            "cpu": metrics.cpu,
            "ram": metrics.ram,
            "cpuTemp": metrics.temp,
            "uptime": metrics.uptime,
            "syncCount": last_sync_time.and(raw["last_sync_count"].as_u64()),
            "lastSyncAt": last_sync_time.map(|t| t * 1000),
        });
        record_point(
            &mut *app.history.write().await,
            Point {
                t: stamp,
                hashrate,
                connections,
                workers: rows.len(),
                cpu: metrics.cpu,
                ram: metrics.ram,
                cpu_temp: metrics.temp,
            },
        );
        *app.snapshot.write().await = Snapshot {
            stats,
            workers: rows,
            rewards,
            updated: stamp,
        };
        polls_since_save += 1;
        if polls_since_save >= HISTORY_SAVE_EVERY {
            polls_since_save = 0;
            save_history(&app, &history_path).await;
        }
    }
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config_path = std::env::var_os("POOL_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| "config/config.json".into());
    anyhow::ensure!(
        std::fs::metadata(&config_path)?.len() <= MAX_JSON_BYTES as u64,
        "Pool configuration is too large"
    );
    let config: Value = serde_json::from_slice(&std::fs::read(&config_path)?)?;
    anyhow::ensure!(
        config["dashboard_key"]
            .as_str()
            .is_some_and(|k| k.len() > 20),
        "dashboard_key must exceed 20 characters"
    );
    anyhow::ensure!(
        config["dashboard_port"]
            .as_u64()
            .is_some_and(|p| (1..=65535).contains(&p)),
        "dashboard_port must be a valid TCP port"
    );
    let bind = std::env::var("DASHBOARD_BIND").unwrap_or_else(|_| "127.0.0.1:6031".into());
    // next to the pool config by default; does not depend on the working directory
    let history_path = std::env::var_os("DASHBOARD_HISTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| config_path.with_file_name("dashboard-history.json"));
    let app = Arc::new(App {
        snapshot: RwLock::new(Snapshot::default()),
        history: RwLock::new(load_history(&history_path)),
    });
    let poller = tokio::spawn(poll(app.clone(), config, history_path.clone()));
    let router = Router::new()
        .route("/statistics", get(statistics))
        .route("/workers", get(workers))
        .route("/rewards", get(rewards))
        .route(
            "/ping",
            get(|| async { Json(json!({"success":true,"result":"Pong!"})) }),
        )
        .route(
            "/",
            get(|| async { ([("cache-control", "no-cache")], Html(INDEX.as_str())) }),
        )
        .route("/styles.css", get(|| async { asset("text/css", STYLES) }))
        .route(
            "/script.js",
            get(|| async { asset("text/javascript", SCRIPT) }),
        )
        .route(
            "/vendor/chart.umd.min.js",
            get(|| async { asset("text/javascript", CHART_JS) }),
        )
        .layer(CompressionLayer::new())
        .with_state(app.clone());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("Dashboard listening on {bind}");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    // the poller is stopped first; its periodic save cannot interleave with the final one
    poller.abort();
    let _ = poller.await;
    save_history(&app, &history_path).await;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn point(t: u64) -> Point {
        Point {
            t,
            hashrate: 0.,
            connections: 0,
            workers: 0,
            cpu: 0.,
            ram: 0.,
            cpu_temp: None,
        }
    }
    #[test]
    fn worker_adapter_whitelists_fields_and_converts_timestamps() {
        let raw = json!({"miner_stats":{"one":{"u":"alice","h":1234,"a":12,"r":1,"t":100,"ip":"private","k":"secret","pw":"secret"}}});
        let workers = public_workers(&raw);
        assert_eq!(workers[0]["username"], "alice");
        assert_eq!(workers[0]["lastSeen"], 100000.);
        for name in ["ip", "k", "pw"] {
            assert!(workers[0].get(name).is_none());
        }
        assert!(public_workers(&json!({})).is_empty());
    }
    #[tokio::test]
    async fn statistics_returns_only_history_after_since() {
        let app = Arc::new(App {
            snapshot: RwLock::new(Snapshot {
                stats: json!({}),
                updated: now(),
                ..Snapshot::default()
            }),
            history: RwLock::new((1..=4).map(|t| point(t * 1000)).collect()),
        });
        let query = |since: &str| Query(HashMap::from([("since".into(), since.into())]));
        let times = |stats: Value| -> Vec<u64> {
            stats["history"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["t"].as_u64().unwrap())
                .collect()
        };
        let Json(all) = statistics(State(app.clone()), Query(HashMap::new()))
            .await
            .unwrap();
        assert_eq!(all["history"][0]["cpuTemp"], Value::Null);
        assert_eq!(times(all), [1000, 2000, 3000, 4000]);
        let Json(newer) = statistics(State(app.clone()), query("2000")).await.unwrap();
        assert_eq!(times(newer), [3000, 4000]);
        let Json(none) = statistics(State(app.clone()), query("4000")).await.unwrap();
        assert!(times(none).is_empty());
        let Json(invalid) = statistics(State(app), query("junk")).await.unwrap();
        assert_eq!(times(invalid).len(), 4);
    }
    #[test]
    fn history_skips_clock_jumps_and_trims_old_and_excess_points() {
        let day = HISTORY_AGE_MS;
        let mut history = VecDeque::new();
        record_point(&mut history, point(day));
        record_point(&mut history, point(day + 5));
        // clock stepped backwards; ignored until it passes the last point again
        record_point(&mut history, point(day + 1));
        record_point(&mut history, point(day + 5));
        assert_eq!(
            history.iter().map(|p| p.t).collect::<Vec<_>>(),
            [day, day + 5]
        );
        record_point(&mut history, point(2 * day + 1));
        assert_eq!(
            history.iter().map(|p| p.t).collect::<Vec<_>>(),
            [day + 5, 2 * day + 1]
        );
        let mut full: VecDeque<_> = (1..=MAX_HISTORY_POINTS as u64 + 3).map(point).collect();
        trim_history(&mut full, 0);
        assert_eq!(full.len(), MAX_HISTORY_POINTS);
        assert_eq!(full.front().unwrap().t, 4);
    }
    #[test]
    fn corrupt_history_is_set_aside_and_valid_history_is_ordered() {
        let dir = std::env::temp_dir().join(format!("duino-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dashboard-history.json");
        assert!(load_history(&path).is_empty());

        std::fs::write(&path, "[{\"t\":").unwrap();
        assert!(load_history(&path).is_empty());
        assert!(!path.exists());
        assert!(path.with_extension("json.bad").exists());

        let t = now();
        let points = [
            point(t),
            point(t - 2000),
            point(t - 1000),
            point(t),
            point(1),
        ];
        std::fs::write(&path, serde_json::to_vec(&points).unwrap()).unwrap();
        let loaded: Vec<_> = load_history(&path).iter().map(|p| p.t).collect();
        assert_eq!(loaded, [t - 2000, t - 1000, t]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[tokio::test]
    async fn public_endpoints_reject_missing_and_stale_snapshots() {
        let app = Arc::new(App {
            snapshot: RwLock::new(Snapshot::default()),
            history: RwLock::new(VecDeque::new()),
        });
        assert_eq!(
            statistics(State(app.clone()), Query(HashMap::new()))
                .await
                .unwrap_err()
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        *app.snapshot.write().await = Snapshot {
            stats: json!({"workers":1}),
            workers: vec![json!({"username":"alice"})],
            updated: now(),
            ..Snapshot::default()
        };
        assert_eq!(
            workers(State(app.clone())).await.unwrap().0[0]["username"],
            "alice"
        );
        app.snapshot.write().await.updated = now() - MAX_SNAPSHOT_AGE_MS - 1;
        assert_eq!(
            workers(State(app)).await.unwrap_err().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
