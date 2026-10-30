//! Read-only dashboard adapter for the supplied duco-pool-rs API.
mod temperature;
use axum::{extract::State, http::StatusCode, response::Html, routing::get, Json, Router};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_SNAPSHOT_AGE_MS: u64 = 20_000;
const HISTORY_AGE_MS: u64 = 86_400_000;
const MAX_HISTORY_POINTS: usize = 20_000;
const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;

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
                "ping":w["pg"], "reward":number(w,"rw")
            })
        })
        .collect()
}
#[derive(Default)]
struct Snapshot {
    stats: Value,
    workers: Vec<Value>,
    updated: u64,
}
struct App {
    snapshot: RwLock<Snapshot>,
    history: RwLock<Vec<Value>>,
}
type Shared = Arc<App>;
type ApiResult = Result<Json<Value>, (StatusCode, &'static str)>;
async fn statistics(State(app): State<Shared>) -> ApiResult {
    let s = app.snapshot.read().await;
    if now().saturating_sub(s.updated) > MAX_SNAPSHOT_AGE_MS {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Pool data is unavailable or stale",
        ));
    }
    let mut stats = s.stats.clone();
    stats["history"] = json!(*app.history.read().await);
    Ok(Json(stats))
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
    let mut totals = std::collections::HashMap::<String, f64>::new();
    for worker in &s.workers {
        if let Some(username) = worker["username"].as_str() {
            *totals.entry(username.to_owned()).or_default() += number(worker, "reward");
        }
    }
    Ok(Json(json!(totals)))
}
async fn poll(app: Shared, config: Value, history_path: PathBuf, service: Option<String>) {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(4))
        .build()
        .expect("HTTP client");
    let mut system = sysinfo::System::new_all();
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    loop {
        interval.tick().await;
        let Some(raw) = pool_data(&client, &config).await else {
            eprintln!("Pool API poll failed; retaining the last snapshot");
            continue;
        };
        let rows = public_workers(&raw);
        system.refresh_cpu_all();
        system.refresh_memory();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let process = system
            .processes()
            .values()
            .find(|p| p.name() == "duco-pool-rs");
        let uptime = process.map(|p| p.run_time());
        let pid = process.map(|p| p.pid().to_string());
        let cpu = system.global_cpu_usage();
        let ram = 100. * system.used_memory() as f64 / system.total_memory().max(1) as f64;
        let temp = temperature::cpu_temperature();
        let accepted: f64 = rows.iter().map(|w| number(w, "accepted")).sum();
        let rejected: f64 = rows.iter().map(|w| number(w, "rejected")).sum();
        let users: HashSet<_> = rows.iter().filter_map(|w| w["username"].as_str()).collect();
        let hashrate: f64 = rows.iter().map(|w| number(w, "hashrate")).sum();
        let mut sync_count = None;
        let mut sync_at = None;
        if let (Some(pid), Some(service)) = (pid, service.as_deref()) {
            if let Ok(out) = tokio::process::Command::new("journalctl")
                .args([
                    "-u",
                    service,
                    "-n",
                    "40",
                    "--no-pager",
                    "-o",
                    "json",
                    &format!("_PID={pid}"),
                ])
                .output()
                .await
            {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if let Ok(entry) = serde_json::from_str::<Value>(line) {
                        if let Some(message) = entry["MESSAGE"].as_str() {
                            if let Some(n) = message
                                .rsplit("Successfully synced ")
                                .next()
                                .filter(|_| message.contains("Successfully synced "))
                            {
                                sync_count = n.trim().parse::<u64>().ok();
                                sync_at = entry["__REALTIME_TIMESTAMP"]
                                    .as_str()
                                    .and_then(|x| x.parse::<u64>().ok())
                                    .map(|x| x / 1000);
                            }
                        }
                    }
                }
            }
        }
        let stamp = now();
        let stats = json!({"poolName":config["poolName"],"motd":config["motd"],"poolPort":config["port"],
            "runtime":"duco-pool-rs", "connections":raw["connections"],"workers":rows.len(),"users":users.len(),
            "hashrate":hashrate,"acceptedShares":accepted,"rejectedShares":rejected,
            "acceptRate":if accepted+rejected>0. {Some(100.*accepted/(accepted+rejected))}else{None},
            "cpu":cpu,"ram":ram,"cpuTemp":temp,"uptime":uptime,"syncCount":sync_count,"lastSyncAt":sync_at});
        let mut history = app.history.write().await;
        history.push(json!({"t":stamp,"hashrate":hashrate,"connections":raw["connections"],"workers":rows.len(),"cpu":cpu,"ram":ram,"cpuTemp":temp}));
        history.retain(|p| {
            p["t"]
                .as_u64()
                .is_some_and(|t| t >= stamp.saturating_sub(HISTORY_AGE_MS))
        });
        if history.len() > MAX_HISTORY_POINTS {
            let extra = history.len() - MAX_HISTORY_POINTS;
            history.drain(..extra);
        }
        let tmp = history_path.with_extension("json.tmp");
        if let Ok(bytes) = serde_json::to_vec(&*history) {
            if tokio::fs::write(&tmp, bytes).await.is_ok() {
                if tokio::fs::rename(&tmp, &history_path).await.is_err() {
                    eprintln!("Could not replace dashboard history file");
                }
            } else {
                eprintln!("Could not write dashboard history file");
            }
        }
        drop(history);
        *app.snapshot.write().await = Snapshot {
            stats,
            workers: rows,
            updated: stamp,
        };
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
    let config: Value = serde_json::from_slice(&std::fs::read(config_path)?)?;
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
    let history_path = std::env::var_os("DASHBOARD_HISTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| "dashboard-rs/history.json".into());
    let history = if history_path.exists() {
        anyhow::ensure!(
            std::fs::metadata(&history_path)?.len() <= MAX_JSON_BYTES as u64,
            "History file is too large"
        );
        let mut history: Vec<Value> = serde_json::from_slice(&std::fs::read(&history_path)?)?;
        history.retain(|p| {
            p["t"]
                .as_u64()
                .is_some_and(|t| t >= now().saturating_sub(HISTORY_AGE_MS))
        });
        if history.len() > MAX_HISTORY_POINTS {
            history.drain(..history.len() - MAX_HISTORY_POINTS);
        }
        history
    } else {
        Vec::new()
    };
    let app = Arc::new(App {
        snapshot: RwLock::new(Snapshot::default()),
        history: RwLock::new(history),
    });
    tokio::spawn(poll(
        app.clone(),
        config,
        history_path,
        std::env::var("POOL_SERVICE").ok(),
    ));
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
            get(|| async { Html(include_str!("../assets/index.html")) }),
        )
        .route(
            "/styles.css",
            get(|| async {
                (
                    [("content-type", "text/css")],
                    include_str!("../assets/styles.css"),
                )
            }),
        )
        .route(
            "/script.js",
            get(|| async {
                (
                    [("content-type", "text/javascript")],
                    include_str!("../assets/script.js"),
                )
            }),
        )
        .route(
            "/vendor/chart.umd.min.js",
            get(|| async {
                (
                    [("content-type", "text/javascript")],
                    include_str!("../assets/vendor/chart.umd.min.js"),
                )
            }),
        )
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("Dashboard listening on {bind}");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn pending_rewards_sum_workers_by_username_and_reject_stale_data() {
        let raw = json!({"miner_stats":{
            "one":{"u":"alice","rw":1.25},
            "two":{"u":"alice","rw":2.5},
            "three":{"u":"bob","rw":0.5},
            "four":{"u":"carol"}
        }});
        let app = Arc::new(App {
            snapshot: RwLock::new(Snapshot {
                workers: public_workers(&raw),
                updated: now(),
                ..Snapshot::default()
            }),
            history: RwLock::new(Vec::new()),
        });
        let Json(totals) = rewards(State(app.clone())).await.unwrap();
        assert_eq!(totals, json!({"alice":3.75,"bob":0.5,"carol":0.0}));
        app.snapshot.write().await.updated = 0;
        assert_eq!(
            rewards(State(app)).await.unwrap_err().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
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
    async fn public_endpoints_reject_missing_and_stale_snapshots() {
        let app = Arc::new(App {
            snapshot: RwLock::new(Snapshot::default()),
            history: RwLock::new(Vec::new()),
        });
        assert_eq!(
            statistics(State(app.clone())).await.unwrap_err().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        *app.snapshot.write().await = Snapshot {
            stats: json!({"workers":1}),
            workers: vec![json!({"username":"alice"})],
            updated: now(),
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
