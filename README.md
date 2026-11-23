# Pool dashboard

An optional Rust companion for the pool's authenticated `/data` endpoint. It serves
a self-contained dashboard with hashrate, active connections, worker search and
sorting, a user leaderboard, host metrics and 24 hours of chart history. It runs
separately from the mining process and needs no Node runtime or external CDN.

## Run

In the pool's `config/config.json`, set `dashboard_key` to a random secret longer
than 20 bytes. The pool disables `/data` for shorter keys. Generate a key locally,
for example with `openssl rand -hex 32`, and restart the pool after editing its
configuration. Never paste the key into frontend code or a public URL.

From the repository root:

```sh
cargo build --release --locked
POOL_CONFIG=/path/to/pool/config/config.json ./target/release/duco-pool-dashboard
```

Open <http://127.0.0.1:6031>. The dashboard polls the pool's configured
`dashboard_port` on loopback every five seconds. The dashboard and the pool API
must listen on different ports. The pool API must accept loopback connections.
Static assets are embedded in the dashboard executable and cached by browsers
until the dashboard is rebuilt; responses are gzip-compressed when the browser
accepts it.

Settings use environment variables:

| Variable | Default | Purpose |
| --- | --- | --- |
| `POOL_CONFIG` | `config/config.json` | Pool configuration containing the API port and secret |
| `DASHBOARD_BIND` | `127.0.0.1:6031` | Dashboard listen address; use `0.0.0.0:6030` for LAN access on port 6030 |
| `DASHBOARD_HISTORY` | `dashboard-history.json` next to `POOL_CONFIG` | History file; its parent directory must exist and be writable |

For example, to preserve an existing dashboard on port 6030, change the pool's
`dashboard_port` to 6032 and restart the pool, then run:

```sh
POOL_CONFIG=/path/to/pool/config/config.json \
DASHBOARD_BIND=0.0.0.0:6030 \
DASHBOARD_HISTORY=/path/to/state/history.json \
./target/release/duco-pool-dashboard
```

[duco-dashboard.service](duco-dashboard.service) is an example systemd unit. Edit
its paths and user before installing it. The dashboard does not change the pool's
bind address or firewall rules. Keep the original keyed API protected: it exposes
more worker information than this dashboard. The dashboard's `/statistics`,
`/workers` and `/rewards` endpoints are intentionally public and read-only; they
omit worker IPs, wallet IDs, chip IDs, IoT readings and mining passwords.

## Data and limitations

- Share counters and acceptance rate describe currently connected workers; they
  can decrease when workers reconnect. Empty share totals show an unavailable rate.
- Pending rewards come from the pool's `balances`: DUCO earned per user that has
  not been accepted by a server sync yet. `/rewards` returns them as-is, including
  users who are no longer connected; the leaderboard displays each user's pending
  DUCO and the summary shows the pool-wide total. Pools that do not publish
  `balances` return HTTP 503 from `/rewards`, shown as unavailable.
- CPU, RAM and temperature are sampled on the dashboard host. Uptime is read from
  a local process named `duco-pool-rs`; run one pool instance per host for this
  lookup. The process is found once and then tracked by PID; the process list is
  only rescanned when that PID exits. Missing process/sensor readings show as
  unavailable.
- Sync status shows the time of the pool's last accepted sync, from
  `last_sync_time`. It is unavailable until the first one, and with pool
  versions that do not publish this field.
- `/statistics`, `/workers` and `/rewards` return HTTP 503 when the last
  successful pool poll is more than 20 seconds old. Pool API response bodies and
  on-disk JSON inputs are capped at 16 MiB. History retains at most 20,000 points
  from the last day.
- History survives dashboard restarts. It is saved once a minute and on a clean
  shutdown, so a crash loses at most the last minute. An unreadable, corrupt or
  oversized history file is renamed to `*.json.bad` and the dashboard starts with
  empty history. Failed history writes are logged while live polling continues.
- If the system clock steps backwards, new history points are skipped until it
  passes the last recorded point, so the history stays in time order.
- `/statistics?since=<ms>` returns only history points newer than `since`; the
  page fetches the full day once, then just new points. Without `since` the
  full history is returned.

## Checks

```sh
typos
cargo fmt --check
cargo test --locked
cargo clippy --release --locked --all-targets -- -D warnings
```

Chart.js 4.4.1 is bundled under its [MIT license](assets/vendor/LICENSE.chartjs.md).
The dashboard companion is licensed under the [AGPL-3.0](LICENSE). Courtesy of [ZL1LAC](https://github.com/ZL1LAC)
