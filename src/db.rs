//! SQLite storage. One writer connection behind a mutex: at a handful of nodes
//! reporting every couple of seconds every statement here is sub-millisecond.
// ponytail: single global connection; move to a read pool if the dashboard ever
// blocks behind ingest.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anyhow::Result;
use chrono::{Datelike, Local, NaiveDate, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tracing::info;

pub struct Db(Mutex<Connection>);

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
-- 8 MiB of page cache. The whole working set of a few hundred nodes fits, so
-- the read paths stop going back to the filesystem.
PRAGMA cache_size = -8192;
-- Without these the WAL grows to whatever the busiest minute needed and never
-- gives the space back: a hub is a long-running process on a small VPS.
PRAGMA wal_autocheckpoint = 256;
PRAGMA journal_size_limit = 1048576;

CREATE TABLE IF NOT EXISTS setting (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS node (
  id            INTEGER PRIMARY KEY,
  name          TEXT    NOT NULL,
  -- The agent's credential, in the clear: the panel shows a node's install
  -- command whenever it is asked, so it has to be able to read it back.
  token         TEXT    NOT NULL UNIQUE,
  sort          INTEGER NOT NULL DEFAULT 0,
  public        INTEGER NOT NULL DEFAULT 1,
  price         REAL    NOT NULL DEFAULT 0,
  currency      TEXT    NOT NULL DEFAULT 'USD',
  billing_cycle TEXT    NOT NULL DEFAULT 'monthly',
  expires_at    TEXT,
  remark        TEXT    NOT NULL DEFAULT '',
  traffic_limit INTEGER NOT NULL DEFAULT 0,
  traffic_mode  TEXT    NOT NULL DEFAULT 'sum',
  traffic_reset_day INTEGER NOT NULL DEFAULT 1,
  hostname TEXT NOT NULL DEFAULT '', os TEXT NOT NULL DEFAULT '',
  kernel   TEXT NOT NULL DEFAULT '', arch TEXT NOT NULL DEFAULT '',
  virt     TEXT NOT NULL DEFAULT '', cpu_name TEXT NOT NULL DEFAULT '',
  cpu_cores INTEGER NOT NULL DEFAULT 0, mem_total INTEGER NOT NULL DEFAULT 0,
  swap_total INTEGER NOT NULL DEFAULT 0, disk_total INTEGER NOT NULL DEFAULT 0,
  agent_version TEXT NOT NULL DEFAULT '', ip TEXT NOT NULL DEFAULT '',
  ipv4 TEXT NOT NULL DEFAULT '', ipv6 TEXT NOT NULL DEFAULT '',
  -- Survives the disconnection it describes, unlike the in-memory live entry:
  -- an offline node's page is exactly where "since when" is worth reading.
  last_seen INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);

-- Monotonic byte counters that survive both agent reboots and hub restarts.
CREATE TABLE IF NOT EXISTS traffic (
  node_id  INTEGER PRIMARY KEY REFERENCES node(id) ON DELETE CASCADE,
  boot_id  TEXT    NOT NULL DEFAULT '',
  last_rx  INTEGER NOT NULL DEFAULT 0,
  last_tx  INTEGER NOT NULL DEFAULT 0,
  total_rx INTEGER NOT NULL DEFAULT 0,
  total_tx INTEGER NOT NULL DEFAULT 0,
  month_rx INTEGER NOT NULL DEFAULT 0,
  month_tx INTEGER NOT NULL DEFAULT 0,
  month_start TEXT NOT NULL DEFAULT '',
  day_rx INTEGER NOT NULL DEFAULT 0,
  day_tx INTEGER NOT NULL DEFAULT 0,
  day_start TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS metric (
  node_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
  ts      INTEGER NOT NULL,
  cpu REAL NOT NULL,
  mem_used INTEGER NOT NULL, swap_used INTEGER NOT NULL, disk_used INTEGER NOT NULL,
  net_rx INTEGER NOT NULL, net_tx INTEGER NOT NULL,
  tcp INTEGER NOT NULL, udp INTEGER NOT NULL, procs INTEGER NOT NULL,
  PRIMARY KEY (node_id, ts)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS ping_task (
  id       INTEGER PRIMARY KEY,
  name     TEXT    NOT NULL,
  target   TEXT    NOT NULL,
  interval INTEGER NOT NULL DEFAULT 60
);

CREATE TABLE IF NOT EXISTS ping_node (
  task_id INTEGER NOT NULL REFERENCES ping_task(id) ON DELETE CASCADE,
  node_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
  PRIMARY KEY (task_id, node_id)
);

-- Key order follows the only query there is: one node, one time window,
-- every probe. With task_id ahead of ts SQLite can seek to the node and no
-- further, then scans every record it ever kept -- see the migration in open().
CREATE TABLE IF NOT EXISTS ping_record (
  node_id INTEGER NOT NULL, task_id INTEGER NOT NULL,
  ts INTEGER NOT NULL, latency INTEGER NOT NULL,
  PRIMARY KEY (node_id, ts, task_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS session (
  token_hash TEXT    PRIMARY KEY,
  expires_at INTEGER NOT NULL
);
"#;

/// Schema revision this build expects, stamped into `PRAGMA user_version`.
/// Bump it and add a `migrate_to_N` when the schema changes under a database
/// that is already in service.
const SCHEMA_VERSION: i64 = 2;

/// Adds a column that older databases lack. A duplicate column means the
/// migration has already run; every other error is real and must propagate.
fn add_column(conn: &Connection, table: &str, column: &str) -> Result<()> {
    match conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column}"), []) {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("duplicate column name") => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// True when `table`'s stored DDL contains `needle` — how a migration asks
/// what shape the database it inherited is actually in.
fn schema_mentions(conn: &Connection, table: &str, needle: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name=?1 AND sql LIKE ?2",
        params![table, format!("%{needle}%")],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

/// Everything that had accumulated before there was a version to record it
/// under. Runs once, on a database that predates the stamp.
fn migrate_to_1(conn: &Connection) -> Result<()> {
    for column in [
        "day_rx INTEGER NOT NULL DEFAULT 0",
        "day_tx INTEGER NOT NULL DEFAULT 0",
        "day_start TEXT NOT NULL DEFAULT ''",
    ] {
        add_column(conn, "traffic", column)?;
    }
    for column in [
        "ipv4 TEXT NOT NULL DEFAULT ''",
        "ipv6 TEXT NOT NULL DEFAULT ''",
        "last_seen INTEGER NOT NULL DEFAULT 0",
    ] {
        add_column(conn, "node", column)?;
    }
    // The column held a sha256 of the token and now holds the token itself.
    // Databases from before the change keep digests no agent can present:
    // those nodes need a new token issued from the panel.
    if schema_mentions(conn, "node", "token_hash")? {
        conn.execute("ALTER TABLE node RENAME COLUMN token_hash TO token", [])?;
        info!("renamed node.token_hash to node.token; existing nodes need a fresh token");
    }
    // Reordering a key means rebuilding the table; CREATE TABLE IF NOT EXISTS
    // leaves an existing one alone. The old order put task_id between the node
    // and the timestamp, so the chart query scanned the node's whole history to
    // answer for one hour of it: 42 ms against 0.8 ms at a month of retention.
    if schema_mentions(conn, "ping_record", "(node_id, task_id, ts)")? {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE ping_record_rekeyed (
               node_id INTEGER NOT NULL, task_id INTEGER NOT NULL,
               ts INTEGER NOT NULL, latency INTEGER NOT NULL,
               PRIMARY KEY (node_id, ts, task_id)
             ) WITHOUT ROWID;
             INSERT INTO ping_record_rekeyed SELECT * FROM ping_record;
             DROP TABLE ping_record;
             ALTER TABLE ping_record_rekeyed RENAME TO ping_record;
             COMMIT;",
        )?;
        info!("rebuilt ping_record on a key the latency chart can seek");
    }
    Ok(())
}

/// `metric.load1` was written on every history row and read by nothing: the
/// card draws the live `load` array off the report, and no chart draws load
/// from history. Dropping it is 21% of what the five unread columns cost, and
/// the only one of them the hub can lose without also losing a number the UI
/// shows.
///
/// The column is `NOT NULL` with no default, so this is not optional: skip it
/// and every metric insert this build makes fails the constraint.
fn migrate_to_2(conn: &Connection) -> Result<()> {
    if schema_mentions(conn, "metric", "load1")? {
        conn.execute("ALTER TABLE metric DROP COLUMN load1", [])?;
        info!("dropped metric.load1; nothing read it");
    }
    Ok(())
}

/// Brings a database that is already in service up to `SCHEMA_VERSION` and
/// stamps it. `from` is the version it is at now, so a fresh file passes
/// `SCHEMA_VERSION` and only gets the stamp.
///
/// Restoring a backup lands here too: the copy brings its own version with it,
/// and it needs the same migrations a restart would have run.
fn migrate(conn: &Connection, from: i64) -> Result<()> {
    if from < 1 {
        migrate_to_1(conn)?;
    }
    if from < 2 {
        migrate_to_2(conn)?;
    }
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    Ok(())
}

/// Every table a backup has to carry before this build will restore it.
const TABLES: [&str; 8] =
    ["setting", "node", "traffic", "metric", "ping_task", "ping_node", "ping_record", "session"];

/// One node's stored configuration and last known facts.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Node {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    #[serde(default = "yes")]
    pub public: bool,
    #[serde(default)]
    pub sort: i64,
    #[serde(default)]
    pub price: f64,
    #[serde(default = "usd")]
    pub currency: String,
    #[serde(default = "monthly")]
    pub billing_cycle: String,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub remark: String,
    /// Monthly allowance in bytes; 0 means unmetered.
    #[serde(default)]
    pub traffic_limit: i64,
    /// How the allowance is counted: sum, max, up or down.
    #[serde(default = "sum")]
    pub traffic_mode: String,
    #[serde(default = "one")]
    pub traffic_reset_day: u32,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub kernel: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub virt: String,
    #[serde(default)]
    pub cpu_name: String,
    #[serde(default)]
    pub cpu_cores: i64,
    #[serde(default)]
    pub mem_total: i64,
    #[serde(default)]
    pub swap_total: i64,
    #[serde(default)]
    pub disk_total: i64,
    #[serde(default)]
    pub agent_version: String,
    #[serde(default)]
    pub ip: String,
    /// Reported by the agent from its own interfaces, unlike `ip`, which is
    /// merely whichever address the agent's connection happened to come from.
    #[serde(default)]
    pub ipv4: String,
    #[serde(default)]
    pub ipv6: String,
    /// Unix seconds of the node's last report, written once a minute alongside
    /// the metric row. Zero for a node that has never reported.
    #[serde(default)]
    pub last_seen: i64,
    /// What the agent authenticates with. Readable so the panel can show an
    /// install command on demand; never leaves the admin view.
    #[serde(default)]
    pub token: String,
}

fn yes() -> bool {
    true
}
fn usd() -> String {
    "USD".into()
}
fn monthly() -> String {
    "monthly".into()
}
fn sum() -> String {
    "sum".into()
}
fn one() -> u32 {
    1
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct Traffic {
    pub total_rx: i64,
    pub total_tx: i64,
    pub month_rx: i64,
    pub month_tx: i64,
    pub month_start: String,
    pub day_rx: i64,
    pub day_tx: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PingTask {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    pub target: String,
    #[serde(default)]
    pub interval: i64,
    #[serde(default)]
    pub nodes: Vec<i64>,
}

/// Restricts the database to its owner.
///
/// It is the credential store: node tokens in the clear, the GitHub client
/// secret, the password hash. SQLite creates it under the umask, which on a
/// default 022 is world-readable, and the WAL and shm files hold the same rows.
///
/// Best effort: a filesystem with no Unix modes still works.
fn restrict(path: &str) {
    for file in [path.to_owned(), format!("{path}-wal"), format!("{path}-shm")] {
        own_only(&file);
    }
}

/// One file, owner-only. Also used on the backup copy `VACUUM INTO` writes,
/// which is the whole credential store in one portable file and is created
/// under the umask like any other.
fn own_only(file: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600));
    }
}

/// The `main` database's path as SQLite itself reports it, empty for
/// `:memory:`. Asked rather than remembered so there is one answer to what
/// file is open.
fn main_file(conn: &Connection) -> String {
    conn.query_row("PRAGMA database_list", [], |r| r.get(2)).unwrap_or_default()
}

fn bytes_of(file: &str) -> i64 {
    std::fs::metadata(file).map(|m| m.len() as i64).unwrap_or(0)
}

/// Bytes the database occupies. The WAL counts: committed rows sit there
/// until a checkpoint folds them into the main file, so the two together are
/// what an operator sees on disk.
fn on_disk(file: &str) -> i64 {
    bytes_of(file) + bytes_of(&format!("{file}-wal"))
}

/// The rows behind the latency chart: one node's probe results over a window,
/// bucketed and in time order. Everything the chart draws is folded out of
/// them in [`close_bucket`].
///
/// The key is `(node_id, ts, task_id)`, so this is a seek and the rows come out
/// sorted with no sorter behind them -- which is what lets the fold hold one
/// bucket at a time. Asking SQLite for the summary instead cost three sorts of
/// the whole window -- two window passes and a GROUP BY -- against this one
/// scan: measured on a week of four probes, 284 ms against 54 ms, all of it
/// holding the single connection the agents write through.
///
/// A constant because the query plan is asserted on it in
/// `rekeying_ping_record_keeps_the_rows_and_lets_the_chart_query_seek`.
const PING_ROWS: &str = "SELECT ts/?3, task_id, latency FROM ping_record
     WHERE node_id=?1 AND ts>=?2
           AND task_id IN (SELECT task_id FROM ping_node WHERE node_id=?1)
     ORDER BY ts";

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        // Asked before CREATE TABLE runs: a file with no tables gets today's
        // schema outright rather than the history of how it was arrived at.
        let fresh = conn
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table'", [], |r| r.get::<_, i64>(0))?
            == 0;
        conn.execute_batch(SCHEMA)?;
        restrict(path);

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        migrate(&conn, if fresh { SCHEMA_VERSION } else { version })?;
        Ok(Self(Mutex::new(conn)))
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- settings ----

    pub fn get(&self, key: &str) -> Option<String> {
        self.conn()
            .query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO setting (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- nodes ----

    pub fn nodes(&self) -> Result<Vec<Node>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT * FROM node ORDER BY sort, id")?;
        let rows = stmt.query_map([], |r| Ok(row_to_node(r)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn node(&self, id: i64) -> Result<Option<Node>> {
        Ok(self
            .conn()
            .query_row("SELECT * FROM node WHERE id = ?1", [id], |r| Ok(row_to_node(r)))
            .optional()?)
    }

    /// Creates a node and returns its id.
    ///
    /// Both rows or neither: `accumulate` reads the `traffic` row on every
    /// report, so a node missing one cannot report at all.
    pub fn create_node(&self, n: &Node, token: &str) -> Result<i64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            // A new node belongs at the end. The caller sends sort 0, which
            // would tie with whatever the last reorder put first.
            "INSERT INTO node (name, token, sort, public, price, currency, billing_cycle,
                               expires_at, remark, traffic_limit, traffic_mode, traffic_reset_day, created_at)
             VALUES (?1,?2,(SELECT COALESCE(MAX(sort),-1)+1 FROM node),?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                n.name,
                token,
                n.public,
                n.price,
                n.currency,
                n.billing_cycle,
                n.expires_at,
                n.remark,
                n.traffic_limit,
                n.traffic_mode,
                n.traffic_reset_day,
                Utc::now().timestamp()
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute("INSERT INTO traffic (node_id) VALUES (?1)", [id])?;
        tx.commit()?;
        Ok(id)
    }

    /// Records that the node reported. Written on the same beat as the metric
    /// row, so it costs one update a minute rather than one a report.
    pub fn touch_seen(&self, id: i64, ts: i64) -> Result<()> {
        self.conn().execute("UPDATE node SET last_seen=?2 WHERE id=?1", params![id, ts])?;
        Ok(())
    }

    pub fn update_node(&self, id: i64, n: &Node) -> Result<()> {
        self.conn().execute(
            "UPDATE node SET name=?2, sort=?3, public=?4, price=?5, currency=?6, billing_cycle=?7,
                             expires_at=?8, remark=?9, traffic_limit=?10, traffic_mode=?11,
                             traffic_reset_day=?12
             WHERE id=?1",
            params![
                id,
                n.name,
                n.sort,
                n.public,
                n.price,
                n.currency,
                n.billing_cycle,
                n.expires_at,
                n.remark,
                n.traffic_limit,
                n.traffic_mode,
                n.traffic_reset_day
            ],
        )?;
        Ok(())
    }

    pub fn set_expiry(&self, id: i64, date: &str) -> Result<()> {
        self.conn().execute("UPDATE node SET expires_at=?2 WHERE id=?1", params![id, date])?;
        Ok(())
    }

    pub fn reorder_nodes(&self, ids: &[i64]) -> Result<()> {
        let unique: HashSet<_> = ids.iter().collect();
        if unique.len() != ids.len() {
            anyhow::bail!("node order contains duplicates");
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM node", [], |r| r.get(0))?;
        if count as usize != ids.len() {
            anyhow::bail!("node order must include every node");
        }
        for (sort, id) in ids.iter().enumerate() {
            if tx.execute("UPDATE node SET sort=?2 WHERE id=?1", params![id, sort as i64])? != 1 {
                anyhow::bail!("node order contains an unknown node");
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_node(&self, id: i64) -> Result<()> {
        self.conn().execute("DELETE FROM node WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Replaces a node's token, which immediately locks out the old one.
    pub fn reset_token(&self, id: i64, token: &str) -> Result<()> {
        self.conn().execute("UPDATE node SET token=?2 WHERE id=?1", params![id, token])?;
        Ok(())
    }

    pub fn node_by_token(&self, token: &str) -> Result<Option<i64>> {
        Ok(self.conn().query_row("SELECT id FROM node WHERE token = ?1", [token], |r| r.get(0)).optional()?)
    }

    /// Stores the slow-changing facts an agent sends when it connects.
    pub fn save_facts(&self, id: i64, f: &serde_json::Value, ip: &str) -> Result<()> {
        let s = |k: &str| f.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
        let n = |k: &str| f.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        self.conn().execute(
            "UPDATE node SET hostname=?2, os=?3, kernel=?4, arch=?5, virt=?6, cpu_name=?7,
                             cpu_cores=?8, mem_total=?9, swap_total=?10, disk_total=?11,
                             agent_version=?12, ip=?13, ipv4=?14, ipv6=?15
             WHERE id=?1",
            params![
                id,
                s("hostname"),
                s("os"),
                s("kernel"),
                s("arch"),
                s("virt"),
                s("cpu_name"),
                n("cpu_cores"),
                n("mem_total"),
                n("swap_total"),
                n("disk_total"),
                s("agent_version"),
                ip,
                s("ipv4"),
                s("ipv6")
            ],
        )?;
        Ok(())
    }

    // ---- traffic ----

    /// Every node's counters in one query, because the node list renders a row
    /// per node and a query per node queues the agents' writes behind it.
    ///
    /// The period counters are gated on the period they were written for. They
    /// restart lazily in `accumulate`, on the node's next report, so a node
    /// offline since before a boundary still holds the previous period's bytes
    /// on disk. This is the only reader, so the rule lives in one place.
    pub fn all_traffic(&self) -> HashMap<i64, Traffic> {
        let conn = self.conn();
        let Ok(mut stmt) = conn.prepare_cached(
            "SELECT t.node_id, t.total_rx, t.total_tx, t.month_rx, t.month_tx, t.month_start,
                    t.day_rx, t.day_tx, t.day_start, n.traffic_reset_day
                 FROM traffic t JOIN node n ON n.id = t.node_id",
        ) else {
            return HashMap::new();
        };
        let today = Local::now().date_naive();
        let day = today.to_string();
        let rows = stmt.query_map([], |r| {
            // Zero rather than absent: a theme drawing a meter needs a
            // number.
            let current = |stored: String, now: &str, rx: i64, tx: i64| {
                if stored == now {
                    (rx, tx)
                } else {
                    (0, 0)
                }
            };
            let period = period_start(today, r.get(9)?).to_string();
            let (month_rx, month_tx) = current(r.get(5)?, &period, r.get(3)?, r.get(4)?);
            let (day_rx, day_tx) = current(r.get(8)?, &day, r.get(6)?, r.get(7)?);
            Ok((
                r.get::<_, i64>(0)?,
                Traffic {
                    total_rx: r.get(1)?,
                    total_tx: r.get(2)?,
                    month_rx,
                    month_tx,
                    month_start: period,
                    day_rx,
                    day_tx,
                },
            ))
        });
        rows.map(|r| r.flatten().collect()).unwrap_or_default()
    }

    /// Folds one report's raw kernel counters into the node's running totals.
    ///
    /// A changed boot_id, or a counter that moved backwards, means the kernel
    /// restarted its counting; the total must not follow it down.
    ///
    /// The billing reset day is read here rather than passed in: it is one join
    /// from a row this already reads, and fetching it separately cost every
    /// report a second turn at the single write connection.
    pub fn accumulate(&self, node_id: i64, boot_id: &str, rx: i64, tx: i64) -> Result<Traffic> {
        let conn = self.conn();
        let (
            prev_boot,
            last_rx,
            last_tx,
            mut total_rx,
            mut total_tx,
            mut month_rx,
            mut month_tx,
            month_start,
            mut day_rx,
            mut day_tx,
            day_start,
            reset_day,
        ) = conn
            .prepare_cached(
                "SELECT t.boot_id, t.last_rx, t.last_tx, t.total_rx, t.total_tx, t.month_rx, t.month_tx,
                    t.month_start, t.day_rx, t.day_tx, t.day_start, n.traffic_reset_day
                 FROM traffic t JOIN node n ON n.id = t.node_id WHERE t.node_id=?1",
            )?
            .query_row([node_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, String>(10)?,
                    r.get::<_, u32>(11)?,
                ))
            })?;

        // One rule: only bytes this hub watched a counter climb through are
        // booked. Without a baseline under this exact boot there is nothing to
        // subtract from, and a bare reading is the machine's whole history.
        //
        // Three ways the baseline goes missing, all handled the same way. A
        // first report has none. A reading that shrank under the same boot lost
        // one -- an interface the sum counted has gone -- so the reading is the
        // rest of that history and booking it would count it twice. A changed
        // boot_id means the counters restarted, or, indistinguishably from
        // here, that a second machine shares the token. Re-aligning costs the
        // seconds since the reboot; the other way costs hundreds of gigabytes
        // against a total that only ever climbs.
        let (d_rx, d_tx) = if prev_boot.is_empty() || prev_boot != boot_id {
            // Worth a line either way: on a healthy node this is a reboot, and
            // one every few seconds is two machines sharing a token.
            if !prev_boot.is_empty() {
                info!("node {node_id} reports a new boot; re-aligning to {rx} rx / {tx} tx");
            }
            (0, 0)
        } else {
            ((rx - last_rx).max(0), (tx - last_tx).max(0))
        };
        total_rx += d_rx;
        total_tx += d_tx;
        month_rx += d_rx;
        month_tx += d_tx;
        day_rx += d_rx;
        day_tx += d_tx;

        // Both boundaries are human dates -- the day a provider resets an
        // allowance, the day a person means by "today" -- so both follow the
        // hub's timezone rather than UTC.
        let period = period_start(Local::now().date_naive(), reset_day).to_string();
        if month_start != period {
            // New billing period: the month counter restarts, the total does not.
            month_rx = d_rx;
            month_tx = d_tx;
        }
        let today = Local::now().date_naive().to_string();
        if day_start != today {
            day_rx = d_rx;
            day_tx = d_tx;
        }

        conn.prepare_cached(
            "UPDATE traffic SET boot_id=?2, last_rx=?3, last_tx=?4, total_rx=?5, total_tx=?6,
                                month_rx=?7, month_tx=?8, month_start=?9, day_rx=?10, day_tx=?11,
                                day_start=?12 WHERE node_id=?1",
        )?
        .execute(params![
            node_id, boot_id, rx, tx, total_rx, total_tx, month_rx, month_tx, period, day_rx, day_tx, today
        ])?;
        Ok(Traffic { total_rx, total_tx, month_rx, month_tx, month_start: period, day_rx, day_tx })
    }

    /// Lets the panel correct a total, e.g. after moving a node to new hardware.
    pub fn set_traffic(
        &self,
        node_id: i64,
        total_rx: i64,
        total_tx: i64,
        month_rx: i64,
        month_tx: i64,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE traffic SET total_rx=?2, total_tx=?3, month_rx=?4, month_tx=?5 WHERE node_id=?1",
            params![node_id, total_rx, total_tx, month_rx, month_tx],
        )?;
        Ok(())
    }

    // ---- metrics ----

    pub fn insert_metric(&self, node_id: i64, ts: i64, m: &serde_json::Value) -> Result<()> {
        let f = |k: &str| m.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
        let n = |k: &str| m.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        self.conn()
            .prepare_cached(
                "INSERT OR REPLACE INTO metric
               (node_id, ts, cpu, mem_used, swap_used, disk_used, net_rx, net_tx, tcp, udp, procs)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            )?
            .execute(params![
                node_id,
                ts,
                f("cpu"),
                n("mem_used"),
                n("swap_used"),
                n("disk_used"),
                n("net_rx"),
                n("net_tx"),
                n("tcp"),
                n("udp"),
                n("procs")
            ])?;
        Ok(())
    }

    /// History for one node, thinned to one sample every `step` seconds.
    ///
    /// Bucketed rather than filtered on a multiple of `step`: rows normally
    /// land on the minute, but nothing enforces it, and a filter would answer a
    /// stamp between the grid lines with silence.
    ///
    /// Averaged over the bucket, not sampled from it. Keeping one row per
    /// bucket is the 1/60 sampling the write side already rejected, applied a
    /// layer up: the seven-day window integrated to 53.69 GB against the
    /// 27.52 GB the minutes hold. Averaged it is 28.02 GB, matching the
    /// accumulator.
    ///
    /// `swap_used`, `tcp`, `udp` and `procs` are stored but not answered with
    /// -- nothing draws them from history. The columns stay by the user's
    /// call; `load1` was the fifth and is gone, see `migrate_to_2`.
    ///
    /// The stamp is the bucket's start rather than a row inside it, so every
    /// series lands on one grid and the probe rows below can be shared.
    pub fn metrics(&self, node_id: i64, since: i64, step: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT (MIN(ts)/?3)*?3, AVG(cpu), CAST(AVG(mem_used) AS INTEGER),
                    CAST(AVG(disk_used) AS INTEGER),
                    CAST(AVG(net_rx) AS INTEGER), CAST(AVG(net_tx) AS INTEGER)
             FROM metric WHERE node_id=?1 AND ts>=?2 GROUP BY ts/?3 ORDER BY ts/?3",
        )?;
        let rows = stmt.query_map(params![node_id, since, step], |r| {
            Ok(serde_json::json!({
                "ts": r.get::<_, i64>(0)?, "cpu": r.get::<_, f64>(1)?,
                "mem_used": r.get::<_, i64>(2)?, "disk_used": r.get::<_, i64>(3)?,
                "net_rx": r.get::<_, i64>(4)?, "net_tx": r.get::<_, i64>(5)?,
            }))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Drops history past the retention window. Traffic totals live in their
    /// own table precisely so history can be pruned freely.
    pub fn prune(&self, keep_days: i64) -> Result<usize> {
        let cutoff = Utc::now().timestamp() - keep_days * 86_400;
        let conn = self.conn();
        let a = conn.execute("DELETE FROM metric WHERE ts < ?1", [cutoff])?;
        let b = conn.execute("DELETE FROM ping_record WHERE ts < ?1", [cutoff])?;
        Ok(a + b)
    }

    // ---- ping ----

    pub fn ping_tasks(&self) -> Result<Vec<PingTask>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, name, target, interval FROM ping_task ORDER BY id")?;
        let tasks: Vec<PingTask> = stmt
            .query_map([], |r| {
                Ok(PingTask {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    target: r.get(2)?,
                    interval: r.get(3)?,
                    nodes: Vec::new(),
                })
            })?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        let mut stmt = conn.prepare("SELECT node_id FROM ping_node WHERE task_id=?1")?;
        tasks
            .into_iter()
            .map(|mut t| {
                t.nodes = stmt.query_map([t.id], |r| r.get(0))?.collect::<Result<_, _>>()?;
                Ok(t)
            })
            .collect()
    }

    /// The assignments are replaced wholesale, so they go in one transaction:
    /// failing between the delete and the inserts would unassign every node
    /// from a probe the panel still lists them under.
    pub fn save_ping_task(&self, t: &PingTask) -> Result<i64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let id = if t.id > 0 {
            tx.execute(
                "UPDATE ping_task SET name=?2, target=?3, interval=?4 WHERE id=?1",
                params![t.id, t.name, t.target, t.interval],
            )?;
            t.id
        } else {
            tx.execute(
                "INSERT INTO ping_task (name, target, interval) VALUES (?1,?2,?3)",
                params![t.name, t.target, t.interval],
            )?;
            tx.last_insert_rowid()
        };
        tx.execute("DELETE FROM ping_node WHERE task_id=?1", [id])?;
        for node in &t.nodes {
            tx.execute("INSERT INTO ping_node (task_id, node_id) VALUES (?1,?2)", params![id, node])?;
        }
        tx.commit()?;
        Ok(id)
    }

    pub fn delete_ping_task(&self, id: i64) -> Result<()> {
        self.conn().execute("DELETE FROM ping_task WHERE id=?1", [id])?;
        Ok(())
    }

    /// The task list to push to one agent.
    pub fn ping_tasks_for(&self, node_id: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT t.id, t.target, t.interval FROM ping_task t
             JOIN ping_node n ON n.task_id = t.id WHERE n.node_id = ?1",
        )?;
        let rows = stmt.query_map([node_id], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?, "target": r.get::<_, String>(1)?,
                "interval": r.get::<_, i64>(2)?
            }))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Probe names keyed by id, for labelling a latency chart. Names only:
    /// targets and node assignments stay behind `Admin`.
    pub fn ping_task_names(&self) -> Result<serde_json::Value> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, name FROM ping_task")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut names = serde_json::Map::new();
        for row in rows {
            let (id, name) = row?;
            names.insert(id.to_string(), serde_json::json!(name));
        }
        Ok(serde_json::Value::Object(names))
    }

    pub fn insert_ping(&self, node_id: i64, task_id: i64, ts: i64, latency: i64) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO ping_record (node_id, task_id, ts, latency) VALUES (?1,?2,?3,?4)",
            params![node_id, task_id, ts, latency],
        )?;
        Ok(())
    }

    /// Probe results for one node, one sample per probe per `step` seconds:
    /// the bucket's median round trip, its range, and the share that was lost.
    ///
    /// These stamps fall wherever the probe finished rather than on a minute,
    /// so the thinning buckets them instead of matching a multiple, as in
    /// `metrics` above. This is the larger half of that response: a probe
    /// reports far more often than once a minute.
    ///
    /// [`PING_ROWS`] answers in time order, so a bucket is finished the moment
    /// the next one opens and only that one is ever held -- at most the probes
    /// assigned to the node times the results one bucket spans.
    pub fn ping_records(&self, node_id: i64, since: i64, step: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(PING_ROWS)?;
        let mut rows = stmt.query(params![node_id, since, step])?;
        let mut out = Vec::new();
        // Per probe in the bucket being filled: what answered, and how many
        // did not.
        let mut open: Vec<(i64, Vec<i64>, i64)> = Vec::new();
        let mut bucket = 0;
        while let Some(row) = rows.next()? {
            let (b, task, latency) = (row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?);
            if b != bucket {
                close_bucket(&mut out, &mut open, bucket * step);
                bucket = b;
            }
            let probe = match open.iter().position(|(id, ..)| *id == task) {
                Some(at) => &mut open[at],
                None => {
                    open.push((task, Vec::new(), 0));
                    open.last_mut().expect("just pushed")
                }
            };
            // A timeout is stored as -1: kept out of the median and counted
            // instead.
            if latency < 0 {
                probe.2 += 1;
            } else {
                probe.1.push(latency);
            }
        }
        close_bucket(&mut out, &mut open, bucket * step);
        Ok(out)
    }

    // ---- the database file itself ----

    /// The file this connection is open on, empty for `:memory:`.
    pub fn file(&self) -> String {
        main_file(&self.conn())
    }

    /// The retention window `prune` and the data page both work from. Stored
    /// as text by the settings form, so a missing or unparsable value is the
    /// default rather than an error.
    pub fn retention_days(&self) -> i64 {
        self.get("retention_days").and_then(|v| v.parse::<i64>().ok()).unwrap_or(30).clamp(1, 3_650)
    }

    /// What the panel's data page reads: how much room the file takes, how
    /// much of it is free pages waiting for a `VACUUM`, and how far back the
    /// history behind that actually reaches.
    ///
    /// `oldest` against `retention` is the one pair here that can be wrong:
    /// history older than the window means `prune` has not been running.
    pub fn stats(&self) -> Result<serde_json::Value> {
        // Before the connection: `conn()` hands out a guard on a plain Mutex,
        // and `retention_days` reaches for the same one.
        let retention = self.retention_days();
        let conn = self.conn();
        let file = main_file(&conn);
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let free_pages: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        // Both pruned at the same cutoff, so the earlier of the two is where
        // history starts. A full scan of each -- the same one the counts below
        // already pay for.
        let oldest: Option<i64> = conn.query_row(
            "SELECT MIN(ts) FROM (SELECT MIN(ts) AS ts FROM metric UNION ALL SELECT MIN(ts) FROM ping_record)",
            [],
            |r| r.get(0),
        )?;
        let mut rows = serde_json::Map::new();
        for table in TABLES {
            let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
            rows.insert(table.to_owned(), serde_json::json!(n));
        }
        Ok(serde_json::json!({
            "path": file,
            "size": bytes_of(&file),
            "wal": bytes_of(&format!("{file}-wal")),
            "free": free_pages * page_size,
            "oldest": oldest,
            "retention": retention,
            "rows": rows,
        }))
    }

    /// Writes a consistent copy of the live database to `dest`, which must not
    /// exist yet.
    ///
    /// `VACUUM INTO` is SQLite's own answer for this: one statement, a single
    /// read transaction, and the copy comes out compacted with the free pages
    /// already dropped. It reads the whole file, so the caller runs it off the
    /// runtime -- every other statement here is sub-millisecond, this one is
    /// not.
    pub fn backup_into(&self, dest: &str) -> Result<()> {
        // A second connection to the same file. `VACUUM INTO` only reads, and
        // WAL lets it read a consistent snapshot while the agents keep writing
        // through the first one -- exporting is the one heavy operation here
        // that does not have to stop them, and it is the one people press.
        // A fresh connection inherits none of the PRAGMAs in SCHEMA, so the
        // busy timeout has to be set again or a checkpoint racing this read
        // returns SQLITE_BUSY immediately.
        let reader = Connection::open(self.file())?;
        reader.busy_timeout(std::time::Duration::from_secs(5))?;
        reader.execute("VACUUM INTO ?1", [dest])?;
        // The copy is the credential store in one portable file: node tokens
        // in the clear, the GitHub secret, the password hash. SQLite creates
        // it under the umask, which on the usual 022 is world-readable.
        own_only(dest);
        Ok(())
    }

    /// Rebuilds the file, giving back the pages that deleted history left
    /// behind. Returns the bytes recovered.
    ///
    /// SQLite's rules for `VACUUM`, and why they hold here: it cannot run
    /// inside a transaction or with a live statement on the connection (one
    /// connection, and this call owns it); it needs about as much free disk as
    /// the database itself, and a failure rolls back leaving the original
    /// untouched; and it can renumber rowids, which nothing here keys on --
    /// `metric` and `ping_record` are WITHOUT ROWID and every other table
    /// declares its own primary key.
    ///
    /// In WAL mode the rewrite lands in the WAL first, so without the
    /// checkpoint the file on disk grows instead of shrinking.
    pub fn vacuum(&self) -> Result<i64> {
        let conn = self.conn();
        let file = main_file(&conn);
        let before = on_disk(&file);
        conn.execute_batch("VACUUM")?;
        // Best effort: the space is already reclaimed inside the database, and
        // a checkpoint that cannot run right now is not a failed vacuum.
        let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        Ok((before - on_disk(&file)).max(0))
    }

    /// What a file has to be before a single page of it is copied over the
    /// live database. Restore is the one button here that destroys data, and
    /// the file behind it came from a disk this hub knows nothing about.
    pub fn check_backup(&self, src: &str) -> Result<()> {
        // Read-write, not read-only: a plain copy of a running hub's database
        // is in WAL mode, and SQLite cannot open one of those read-only
        // without its -shm file.
        let candidate = Connection::open(src)?;
        let health: String = candidate
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(|e| anyhow::anyhow!("not a readable SQLite database: {e}"))?;
        if health != "ok" {
            anyhow::bail!("the file is a damaged database: {health}");
        }
        // Pages are copied verbatim, so whatever schema the file carries is
        // the schema this hub then runs its own statements against. A view or
        // a trigger where a table belongs turns every later write into
        // someone else's code.
        let plotted: i64 = candidate.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('view', 'trigger')",
            [],
            |r| r.get(0),
        )?;
        if plotted > 0 {
            anyhow::bail!("the file carries views or triggers, which a hub backup never does");
        }
        for table in TABLES {
            let found: i64 = candidate.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )?;
            if found == 0 {
                anyhow::bail!("the file is not a hub backup: no {table} table");
            }
        }
        let version: i64 = candidate.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "the backup is from a newer hub (schema {version}, this one reads {SCHEMA_VERSION}); upgrade first"
            );
        }
        // The online backup API refuses a page size change while the
        // destination is in WAL mode. Saying so beats SQLITE_READONLY.
        let theirs: i64 = candidate.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let ours: i64 = self.conn().query_row("PRAGMA page_size", [], |r| r.get(0))?;
        if theirs != ours {
            anyhow::bail!("the backup uses a {theirs}-byte page, this database uses {ours}");
        }
        Ok(())
    }

    /// Copies a checked backup over the live database, page by page, through
    /// SQLite's online backup API -- the destination keeps its file, its
    /// permissions and its journal mode, and a failure part way through rolls
    /// back rather than leaving half a database behind.
    ///
    /// Call [`Db::check_backup`] first. Like the other two, this reads and
    /// writes the whole file, so it belongs off the runtime.
    pub fn restore_from(&self, src: &str) -> Result<()> {
        let mut conn = self.conn();
        conn.restore(rusqlite::MAIN_DB, src, None::<fn(rusqlite::backup::Progress)>)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        migrate(&conn, version)?;
        Ok(())
    }

    // ---- sessions ----

    pub fn create_session(&self, token_hash: &str, expires_at: i64) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO session (token_hash, expires_at) VALUES (?1, ?2)",
            params![token_hash, expires_at],
        )?;
        Ok(())
    }

    pub fn session_valid(&self, token_hash: &str) -> bool {
        self.conn()
            .query_row(
                "SELECT 1 FROM session WHERE token_hash=?1 AND expires_at > ?2",
                params![token_hash, Utc::now().timestamp()],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    /// Live sessions, newest first. Expired rows are filtered here rather than
    /// left to `expire_sessions`, which only sweeps once an hour.
    pub fn sessions(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT token_hash, expires_at FROM session WHERE expires_at > ?1 ORDER BY expires_at DESC",
        )?;
        let rows = stmt
            .query_map([Utc::now().timestamp()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn drop_session(&self, token_hash: &str) -> Result<()> {
        self.conn().execute("DELETE FROM session WHERE token_hash=?1", [token_hash])?;
        Ok(())
    }

    /// Invalidates every login. Used when the admin password changes.
    pub fn drop_all_sessions(&self) -> Result<()> {
        self.conn().execute("DELETE FROM session", [])?;
        Ok(())
    }

    pub fn expire_sessions(&self) -> Result<()> {
        self.conn().execute("DELETE FROM session WHERE expires_at <= ?1", [Utc::now().timestamp()])?;
        Ok(())
    }
}

/// Turns one finished bucket into a row per probe, stamped with the bucket's
/// start so every series lands on the same grid.
///
/// Median rather than mean, as Smokeping draws it: one SYN retransmit is tens
/// of milliseconds and drags a mean, and it is the reading that is wrong rather
/// than the link.
///
/// `latency` is null when a whole bucket timed out. `loss` is the percentage
/// that did and rides along only when there is any -- a healthy day is 2 880
/// rows, and `"loss":0` on each is 29 kB of nothing. Rounded up, so that no
/// `loss` key means no timeout and nothing else: truncating reports a bucket
/// that lost 1 of 180 as clean.
fn close_bucket(out: &mut Vec<serde_json::Value>, open: &mut Vec<(i64, Vec<i64>, i64)>, ts: i64) {
    // By probe, not by whichever answered first in this bucket: the chart
    // shades its lines by the order they arrive in.
    open.sort_unstable_by_key(|(task, ..)| *task);
    for (task, mut answered, lost) in open.drain(..) {
        answered.sort_unstable();
        let middle = match answered.len() {
            0 => None,
            n if n % 2 == 1 => Some(answered[n / 2]),
            n => Some((answered[n / 2 - 1] + answered[n / 2]) / 2),
        };
        let mut row = serde_json::json!({"task_id": task, "ts": ts, "latency": middle});
        // Only when the bucket actually moved. At the hour and six-hour
        // windows a bucket holds one sample, and a band would be a
        // zero-height ribbon under every line.
        if let (Some(lo), Some(hi)) = (answered.first(), answered.last()) {
            if hi > lo {
                row["band"] = serde_json::json!([lo, hi]);
            }
        }
        if lost > 0 {
            let total = answered.len() as i64 + lost;
            row["loss"] = ((100 * lost + total - 1) / total).into();
        }
        out.push(row);
    }
}

fn row_to_node(r: &rusqlite::Row<'_>) -> Node {
    let s = |i: &str| r.get::<_, String>(i).unwrap_or_default();
    let n = |i: &str| r.get::<_, i64>(i).unwrap_or(0);
    Node {
        id: n("id"),
        name: s("name"),
        public: r.get::<_, bool>("public").unwrap_or(true),
        sort: n("sort"),
        price: r.get::<_, f64>("price").unwrap_or(0.0),
        currency: s("currency"),
        billing_cycle: s("billing_cycle"),
        expires_at: r.get::<_, Option<String>>("expires_at").unwrap_or(None),
        remark: s("remark"),
        traffic_limit: n("traffic_limit"),
        traffic_mode: s("traffic_mode"),
        traffic_reset_day: n("traffic_reset_day") as u32,
        hostname: s("hostname"),
        os: s("os"),
        kernel: s("kernel"),
        arch: s("arch"),
        virt: s("virt"),
        cpu_name: s("cpu_name"),
        cpu_cores: n("cpu_cores"),
        mem_total: n("mem_total"),
        swap_total: n("swap_total"),
        disk_total: n("disk_total"),
        agent_version: s("agent_version"),
        ip: s("ip"),
        ipv4: s("ipv4"),
        ipv6: s("ipv6"),
        last_seen: n("last_seen"),
        token: s("token"),
    }
}

/// Start of the billing period containing `today`, given a reset day of month.
/// A reset day past the end of a short month lands on that month's last day.
pub fn period_start(today: NaiveDate, reset_day: u32) -> NaiveDate {
    let day = reset_day.clamp(1, 31);
    let clamped = |y: i32, m: u32| {
        let last =
            NaiveDate::from_ymd_opt(if m == 12 { y + 1 } else { y }, if m == 12 { 1 } else { m + 1 }, 1)
                .unwrap()
                .pred_opt()
                .unwrap()
                .day();
        NaiveDate::from_ymd_opt(y, m, day.min(last)).unwrap()
    };
    let this = clamped(today.year(), today.month());
    if today >= this {
        this
    } else if today.month() == 1 {
        clamped(today.year() - 1, 12)
    } else {
        clamped(today.year(), today.month() - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open(":memory:").unwrap()
    }

    /// PRAGMA settings are per connection, so a value read through any other
    /// handle proves nothing about the one the hub writes on.
    #[test]
    fn the_tuning_pragmas_reach_the_connection_the_hub_uses() {
        let db = db();
        let conn = db.conn();
        let read = |p: &str| conn.query_row(&format!("PRAGMA {p}"), [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(read("cache_size"), -8192, "8 MiB of page cache");
        assert_eq!(read("wal_autocheckpoint"), 256);
        assert_eq!(read("journal_size_limit"), 1_048_576);
        assert_eq!(read("busy_timeout"), 5_000);
    }

    /// A real file, because the whole point of these three is what happens
    /// to one. Cleaned up by the test that made it.
    struct Scratch(String);

    impl Scratch {
        fn new() -> Self {
            Self(
                std::env::temp_dir()
                    .join(format!("monitor-test-{}.db", rand::random::<u64>()))
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm", ".copy"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0));
            }
        }
    }

    /// Backup and restore are the two buttons that can lose every row in the
    /// database, so this walks the whole path: take a copy, change the live
    /// database, put the copy back, and see the change gone.
    #[test]
    fn a_backup_restores_the_database_it_was_taken_from() {
        let scratch = Scratch::new();
        let copy = format!("{}.copy", scratch.0);
        let db = Db::open(&scratch.0).unwrap();
        let kept =
            db.create_node(&Node { name: "backed-up".into(), ..Default::default() }, "token-kept").unwrap();
        db.backup_into(&copy).unwrap();

        // Everything after the copy has to disappear when it is restored --
        // including a node that took the deleted one's id back.
        db.delete_node(kept).unwrap();
        db.create_node(&Node { name: "after".into(), ..Default::default() }, "token-after").unwrap();

        db.check_backup(&copy).unwrap();
        db.restore_from(&copy).unwrap();
        let back = db.nodes().unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!((back[0].name.as_str(), back[0].token.as_str()), ("backed-up", "token-kept"));
        assert!(db.node_by_token("token-after").unwrap().is_none(), "the row made after the copy is gone");

        // The connection is still the hub's: it can write, it is still on the
        // schema this build expects, and it is still journalling the way the
        // hub was opened -- the copy `VACUUM INTO` wrote is not in WAL mode.
        node(&db, 1);
        let conn = db.conn();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(),
            SCHEMA_VERSION
        );
        assert_eq!(conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0)).unwrap(), "wal");
        drop(conn);
        let _ = std::fs::remove_file(&copy);
    }

    /// The upload behind restore is a file from somewhere else. Each of these
    /// is a way for it to not be a hub backup, and every one of them has to be
    /// caught before a single page is copied over live data.
    #[test]
    fn restore_refuses_anything_that_is_not_a_backup_of_this_hub() {
        let scratch = Scratch::new();
        let db = Db::open(&scratch.0).unwrap();
        let bad = format!("{}.copy", scratch.0);

        std::fs::write(&bad, b"this is not a database at all").unwrap();
        assert!(db.check_backup(&bad).is_err(), "not SQLite");

        let _ = std::fs::remove_file(&bad);
        let empty = Connection::open(&bad).unwrap();
        empty.execute_batch("CREATE TABLE unrelated (a)").unwrap();
        assert!(db.check_backup(&bad).is_err(), "SQLite, but not this schema");

        // A file carrying its own code where a table belongs: the restore
        // copies pages, so that schema would be the one the hub then runs
        // every statement against.
        empty.execute_batch(&SCHEMA.replace("PRAGMA journal_mode = WAL;", "")).unwrap();
        empty
            .execute_batch(
                "DROP TABLE session; CREATE VIEW session AS SELECT 1 AS token_hash, 2 AS expires_at",
            )
            .unwrap();
        assert!(db.check_backup(&bad).is_err(), "a view where a table belongs");

        // From a hub that knows a schema this build has never seen.
        let _ = std::fs::remove_file(&bad);
        let newer = Connection::open(&bad).unwrap();
        newer.execute_batch(SCHEMA).unwrap();
        newer.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1)).unwrap();
        assert!(db.check_backup(&bad).is_err(), "from a newer hub");

        newer.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}")).unwrap();
        db.check_backup(&bad).unwrap();
    }

    /// `oldest` is what the data page holds against the retention window, so it
    /// has to reach across both pruned tables, not just whichever has rows.
    #[test]
    fn stats_report_the_earliest_history_row_and_the_window_it_is_kept_for() {
        let scratch = Scratch::new();
        let db = Db::open(&scratch.0).unwrap();
        let id = node(&db, 1);
        let now = Utc::now().timestamp();

        assert_eq!(db.stats().unwrap()["oldest"], serde_json::Value::Null, "no history, no start");
        assert_eq!(db.stats().unwrap()["retention"], 30, "an unset window is the default");

        db.insert_metric(id, now - 3 * 86_400, &serde_json::json!({"cpu": 1.0})).unwrap();
        assert_eq!(db.stats().unwrap()["oldest"], now - 3 * 86_400);

        // Older, and in the other table: the earlier of the two wins.
        db.insert_ping(id, 1, now - 9 * 86_400, 12).unwrap();
        assert_eq!(db.stats().unwrap()["oldest"], now - 9 * 86_400);

        db.set("retention_days", "9999").unwrap();
        assert_eq!(db.stats().unwrap()["retention"], 3_650, "a stored window is still clamped");
    }

    /// Deleted rows leave free pages behind; only a rebuild gives them back to
    /// the filesystem, and in WAL mode only after the checkpoint.
    #[test]
    fn vacuum_gives_the_deleted_pages_back_to_the_filesystem() {
        let scratch = Scratch::new();
        let db = Db::open(&scratch.0).unwrap();
        let id = node(&db, 1);
        let now = Utc::now().timestamp();
        let sample = serde_json::json!({"cpu": 1.0, "mem_used": 1, "swap_used": 1, "disk_used": 1,
            "net_rx": 1, "net_tx": 1, "tcp": 1, "udp": 1, "procs": 1});
        for i in 0..20_000 {
            db.insert_metric(id, now - i, &sample).unwrap();
        }
        let _ = db.conn().query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        let fat = on_disk(&scratch.0);
        db.prune(0).unwrap();

        let freed = db.vacuum().unwrap();
        assert!(freed > 0, "a vacuum after deleting 20 000 rows has to return space");
        assert!(on_disk(&scratch.0) < fat);
        assert_eq!(db.stats().unwrap()["rows"]["metric"], 0);
        assert_eq!(db.nodes().unwrap().len(), 1, "vacuum keeps the rows that are left");
    }

    fn node(db: &Db, reset_day: u32) -> i64 {
        let token = format!("token-{}", rand::random::<u32>());
        db.create_node(&Node { name: "n".into(), traffic_reset_day: reset_day, ..Default::default() }, &token)
            .unwrap()
    }

    #[test]
    fn traffic_survives_a_reboot_instead_of_resetting() {
        let db = db();
        let id = node(&db, 1);

        // The first report only establishes the baseline.
        let t = db.accumulate(id, "boot-a", 5_000, 3_000).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (0, 0));

        let t = db.accumulate(id, "boot-a", 9_000, 6_000).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (4_000, 3_000));

        // Reboot: new boot_id, counters restart near zero. The total must not
        // fall back to the fresh value, and the 700 bytes moved before the
        // first report are not booked -- nothing measured them.
        let t = db.accumulate(id, "boot-b", 700, 400).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (4_000, 3_000), "a reboot must not reset the total");

        // And counting resumes from the new baseline.
        let t = db.accumulate(id, "boot-b", 1_700, 900).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (5_000, 3_500));
        assert_eq!((t.month_rx, t.month_tx), (5_000, 3_500));
    }

    /// One install command pasted onto a second machine: both agents answer to
    /// the same node and evict each other, so the hub sees two boot_ids
    /// alternating, each carrying its own lifetime counter. Booking those adds
    /// ~180 GB per swap to a total that only ever climbs.
    #[test]
    fn two_machines_sharing_one_token_cannot_inflate_the_total() {
        let db = db();
        let id = node(&db, 1);
        let (a, b) = (100_000_000_000, 80_000_000_000); // two lifetime counters

        db.accumulate(id, "boot-a", a, a).unwrap();
        let t = db.accumulate(id, "boot-a", a + 1_000, a + 1_000).unwrap();
        assert_eq!(t.total_rx, 1_000, "the real machine's own traffic still counts");

        // Every swap is a boot_id with no baseline, so every swap books
        // nothing.
        for round in 0..3 {
            db.accumulate(id, "boot-b", b + round, b + round).unwrap();
            db.accumulate(id, "boot-a", a + 1_000 + round, a + 1_000 + round).unwrap();
        }
        let t = db.all_traffic()[&id].clone();
        assert!(t.total_rx < 10_000, "six swaps booked {} bytes, not a lifetime counter", t.total_rx);
    }

    #[test]
    fn a_shrinking_reading_re_aligns_instead_of_re_counting_history() {
        let db = db();
        let id = node(&db, 1);
        db.accumulate(id, "boot-a", 10_000, 10_000).unwrap();
        let t = db.accumulate(id, "boot-a", 12_000, 12_000).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (2_000, 2_000));

        // Same boot, reading dropped: an interface the sum counted is gone, so
        // this is the rest of the machine's history, not fresh bytes.
        let t = db.accumulate(id, "boot-a", 500, 500).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (2_000, 2_000));

        // Aligned to the smaller baseline, counting picks up from there.
        let t = db.accumulate(id, "boot-a", 900, 900).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (2_400, 2_400));

        // A new boot re-aligns the same way, for the same reason: there is no
        // baseline under it either.
        let t = db.accumulate(id, "boot-b", 300, 300).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (2_400, 2_400));

        // One direction shrinking does not cost the other its increment.
        let t = db.accumulate(id, "boot-b", 100, 900).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (2_400, 3_000));
    }

    /// The two counters that restart on their own schedule, against a total
    /// that never does. Each hangs off its own stored date, so a rollover has
    /// to leave the other alone.
    #[test]
    fn day_and_month_restart_independently_while_the_total_keeps_climbing() {
        let db = db();
        let id = node(&db, 1);
        db.accumulate(id, "boot-a", 0, 0).unwrap();
        let t = db.accumulate(id, "boot-a", 8_000, 4_000).unwrap();
        assert_eq!((t.day_rx, t.day_tx), (8_000, 4_000));
        assert_eq!((t.month_rx, t.month_tx), (8_000, 4_000));

        // Midnight passes, forced through the stored date the rollover reads.
        db.conn().execute("UPDATE traffic SET day_start='1999-01-01' WHERE node_id=?1", [id]).unwrap();
        let t = db.accumulate(id, "boot-a", 9_500, 4_600).unwrap();
        assert_eq!((t.day_rx, t.day_tx), (1_500, 600), "a new day counts only this report's delta");
        assert_eq!(t.month_rx, 9_500, "the month is not a day");
        assert_eq!(t.total_rx, 9_500, "and the total is neither");

        // Then the billing period turns over, part-way through that same day.
        db.conn().execute("UPDATE traffic SET month_start='1999-01-01' WHERE node_id=?1", [id]).unwrap();
        let t = db.accumulate(id, "boot-a", 10_000, 4_700).unwrap();
        assert_eq!((t.month_rx, t.month_tx), (500, 100), "a new period counts only this report's delta");
        assert_eq!((t.day_rx, t.day_tx), (2_000, 700), "the day carries on across a billing rollover");
        assert_eq!(t.total_rx, 10_000, "lifetime total is untouched by either rollover");
    }

    /// The other half of the rollover: the counters restart on the node's next
    /// report, so a node quiet since before a boundary still holds the previous
    /// period's bytes on disk. The read side must not answer with those.
    #[test]
    fn a_node_that_went_quiet_before_a_boundary_reads_as_zero_this_period() {
        let db = db();
        let id = node(&db, 1);
        db.accumulate(id, "boot-a", 0, 0).unwrap();
        db.accumulate(id, "boot-a", 8_000, 4_000).unwrap();
        assert_eq!(db.all_traffic()[&id].day_rx, 8_000, "still today, so it still counts");

        // Offline across both boundaries, with no report to restart either.
        db.conn()
            .execute(
                "UPDATE traffic SET day_start='1999-01-01', month_start='1999-01-01' WHERE node_id=?1",
                [id],
            )
            .unwrap();
        let t = db.all_traffic()[&id].clone();
        assert_eq!((t.day_rx, t.day_tx), (0, 0), "yesterday's bytes are not today's");
        assert_eq!((t.month_rx, t.month_tx), (0, 0), "last period's bytes are not this period's");
        assert_eq!(t.month_start, period_start(Local::now().date_naive(), 1).to_string());
        assert_eq!((t.total_rx, t.total_tx), (8_000, 4_000), "the lifetime total never resets");
    }

    #[test]
    fn period_start_handles_short_months_and_wraparound() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        // Reset on the 15th, today is the 20th: this month.
        assert_eq!(period_start(d(2026, 3, 20), 15), d(2026, 3, 15));
        // Same day counts as the start of the new period.
        assert_eq!(period_start(d(2026, 3, 15), 15), d(2026, 3, 15));
        // Before the reset day: the period began last month.
        assert_eq!(period_start(d(2026, 3, 10), 15), d(2026, 2, 15));
        // January rolls back into the previous year.
        assert_eq!(period_start(d(2026, 1, 10), 15), d(2025, 12, 15));
        // Day 31 in February clamps to the 28th, and 2028 is a leap year.
        assert_eq!(period_start(d(2026, 2, 28), 31), d(2026, 2, 28));
        assert_eq!(period_start(d(2028, 2, 29), 31), d(2028, 2, 29));
    }

    #[test]
    fn deleting_a_node_takes_its_data_with_it() {
        let db = db();
        let id = node(&db, 1);
        db.accumulate(id, "b", 10, 10).unwrap();
        db.insert_metric(id, 1, &serde_json::json!({"cpu": 1.0})).unwrap();
        db.delete_node(id).unwrap();
        assert!(db.node(id).unwrap().is_none());
        assert_eq!(db.metrics(id, 0, 60).unwrap().len(), 0);
        assert!(!db.all_traffic().contains_key(&id));
    }

    #[test]
    fn a_token_is_readable_and_rotation_retires_the_old_one() {
        let db = db();
        let id = db.create_node(&Node { name: "n".into(), ..Default::default() }, "first-token").unwrap();

        // Readable, so the panel can show the install command without issuing
        // a new token to do it.
        assert_eq!(db.node(id).unwrap().unwrap().token, "first-token");
        assert_eq!(db.node_by_token("first-token").unwrap(), Some(id));

        db.reset_token(id, "second-token").unwrap();
        assert_eq!(db.node(id).unwrap().unwrap().token, "second-token");
        assert_eq!(db.node_by_token("second-token").unwrap(), Some(id));
        assert_eq!(db.node_by_token("first-token").unwrap(), None, "the old token stops working");
    }

    #[test]
    fn nodes_can_be_reordered_atomically() {
        let db = db();
        let (a, b, c) = (node(&db, 1), node(&db, 1), node(&db, 1));
        let order = || db.nodes().unwrap().iter().map(|n| n.id).collect::<Vec<_>>();
        db.reorder_nodes(&[c, a, b]).unwrap();
        assert_eq!(order(), vec![c, a, b]);

        // Every rejected shape leaves the order it found. The partial list
        // matters most: a stale tab would otherwise renumber around a node it
        // never saw.
        assert!(db.reorder_nodes(&[a, a, c]).is_err(), "duplicates");
        assert!(db.reorder_nodes(&[a, b]).is_err(), "a node left out");
        assert!(db.reorder_nodes(&[a, b, 9999]).is_err(), "an id that is not a node");
        assert_eq!(order(), vec![c, a, b]);
        // A node added afterwards goes to the end, not to wherever sort 0 puts it.
        let d = node(&db, 1);
        assert_eq!(db.nodes().unwrap().iter().map(|n| n.id).collect::<Vec<_>>(), vec![c, a, b, d]);
    }

    #[test]
    fn prune_drops_history_but_never_traffic_totals() {
        let db = db();
        let id = node(&db, 1);
        db.accumulate(id, "b", 100, 100).unwrap();
        db.accumulate(id, "b", 900, 900).unwrap();
        let old = Utc::now().timestamp() - 40 * 86_400;
        db.insert_metric(id, old, &serde_json::json!({"cpu": 1.0})).unwrap();
        db.insert_metric(id, Utc::now().timestamp(), &serde_json::json!({"cpu": 2.0})).unwrap();

        db.prune(30).unwrap();
        assert_eq!(db.metrics(id, 0, 60).unwrap().len(), 1);
        assert_eq!(db.all_traffic()[&id].total_rx, 800);
    }

    /// The rekeying in `open()`: rows have to survive it, and the chart's
    /// query has to come out able to seek. A migration that keeps every row on
    /// the old key is silent, and stays silent while the query it exists for
    /// scans the node's whole history.
    #[test]
    fn rekeying_ping_record_keeps_the_rows_and_lets_the_chart_query_seek() {
        let file = std::env::temp_dir().join(format!("monitor-rekey-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let path = file.to_str().unwrap();

        // A database as an older hub left it.
        let old = Connection::open(path).unwrap();
        old.execute_batch(
            "CREATE TABLE ping_record (
               node_id INTEGER NOT NULL, task_id INTEGER NOT NULL,
               ts INTEGER NOT NULL, latency INTEGER NOT NULL,
               PRIMARY KEY (node_id, task_id, ts)
             ) WITHOUT ROWID;
             INSERT INTO ping_record VALUES (1,7,100,12),(1,8,100,34),(1,7,200,56),(2,7,100,78);",
        )
        .unwrap();
        drop(old);

        let db = Db::open(path).unwrap();
        let conn = db.conn();
        let rows: Vec<(i64, i64, i64, i64)> = conn
            .prepare("SELECT node_id, task_id, ts, latency FROM ping_record ORDER BY node_id, ts, task_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![(1, 7, 100, 12), (1, 8, 100, 34), (1, 7, 200, 56), (2, 7, 100, 78)]);

        // Without the timestamp second in the key the plan stops at
        // `node_id=?` and scans everything under it -- and the fold in
        // `ping_records` needs the rows in time order, which only the seek
        // gives without a sorter.
        let plan: String = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {PING_ROWS}"))
            .unwrap()
            .query_map(params![1, 0, 60], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join(" | ");
        assert!(plan.contains("node_id=? AND ts>?"), "the window has to be a seek, not a scan: {plan}");
        assert!(!plan.contains("ORDER BY"), "the time order has to come off the key, not a sorter: {plan}");

        // Opening again must not rebuild a table that is already right.
        drop(conn);
        drop(db);
        assert!(Db::open(path).is_ok());
        let _ = std::fs::remove_file(&file);
    }

    /// Dropping `metric.load1` under a database in service. The column is
    /// `NOT NULL` with no default, so a migration that quietly did not run
    /// does not leave a stale column -- it stops every history row from being
    /// written, for as long as nobody looks at a chart.
    #[test]
    fn dropping_load1_keeps_the_history_and_lets_new_rows_in() {
        let file = std::env::temp_dir().join(format!("monitor-load1-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let path = file.to_str().unwrap();

        // A database as a hub before this build left it: one metric row with
        // a load average in it, stamped at the schema version of the day.
        let old = Connection::open(path).unwrap();
        old.execute_batch(
            "CREATE TABLE metric (
               node_id INTEGER NOT NULL, ts INTEGER NOT NULL,
               cpu REAL NOT NULL, load1 REAL NOT NULL,
               mem_used INTEGER NOT NULL, swap_used INTEGER NOT NULL, disk_used INTEGER NOT NULL,
               net_rx INTEGER NOT NULL, net_tx INTEGER NOT NULL,
               tcp INTEGER NOT NULL, udp INTEGER NOT NULL, procs INTEGER NOT NULL,
               PRIMARY KEY (node_id, ts)
             ) WITHOUT ROWID;
             INSERT INTO metric VALUES (1,60,12.5,0.75,100,0,0,0,0,0,0,0);
             PRAGMA user_version = 1;",
        )
        .unwrap();
        drop(old);

        let db = Db::open(path).unwrap();
        assert!(!schema_mentions(&db.conn(), "metric", "load1").unwrap(), "the column has to be gone");
        // The row it was written on stays, and so does everything else on it.
        let kept = &db.metrics(1, 0, 60).unwrap()[0];
        assert_eq!((kept["ts"].as_i64(), kept["cpu"].as_f64()), (Some(60), Some(12.5)));
        // And the shape this build inserts now fits the table.
        db.insert_metric(1, 120, &serde_json::json!({"cpu": 2.0, "load": [0.5, 0.4, 0.3]})).unwrap();
        assert_eq!(db.metrics(1, 0, 60).unwrap().len(), 2);

        // Opening again must not try to drop a column that is already gone.
        drop(db);
        assert!(Db::open(path).is_ok());
        let _ = std::fs::remove_file(&file);
    }

    /// Removing a node from a probe has to take the probe off that node's
    /// chart. `ping_record` carries no foreign key to the assignment that
    /// produced it, so the rows outlive it until retention -- the window query
    /// is what has to stop drawing them, and it has to do so at once rather
    /// than an hour later when the sweep next runs.
    #[test]
    fn a_probe_taken_off_a_node_stops_appearing_in_its_history() {
        let db = db();
        let id = node(&db, 1);
        let probe = |nodes: Vec<i64>, task| {
            db.save_ping_task(&PingTask {
                id: task,
                name: "cm".into(),
                target: "1.1.1.1:443".into(),
                interval: 60,
                nodes,
            })
            .unwrap()
        };
        let task = probe(vec![id], 0);
        db.insert_ping(id, task, 100, 42).unwrap();
        assert_eq!(db.ping_records(id, 0, 60).unwrap().len(), 1, "an assigned probe draws");

        probe(vec![], task);
        assert!(db.ping_records(id, 0, 60).unwrap().is_empty(), "an unassigned one does not");

        // The rows are still there: reassigning brings the history back rather
        // than starting over.
        probe(vec![id], task);
        assert_eq!(db.ping_records(id, 0, 60).unwrap().len(), 1, "and it comes back with its history");
    }

    #[test]
    fn ping_tasks_round_trip_with_their_node_assignments() {
        let db = db();
        let (a, b) = (node(&db, 1), node(&db, 1));
        let id = db
            .save_ping_task(&PingTask {
                id: 0,
                name: "cf".into(),
                target: "1.1.1.1:443".into(),
                interval: 60,
                nodes: vec![a, b],
            })
            .unwrap();
        assert_eq!(db.ping_tasks_for(a).unwrap().len(), 1);

        // Reassigning to one node must drop the other's copy.
        db.save_ping_task(&PingTask {
            id,
            name: "cf".into(),
            target: "1.1.1.1:443".into(),
            interval: 30,
            nodes: vec![a],
        })
        .unwrap();
        assert_eq!(db.ping_tasks_for(b).unwrap().len(), 0);
        assert_eq!(db.ping_tasks().unwrap()[0].interval, 30);
    }
}
