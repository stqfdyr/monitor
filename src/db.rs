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
  cpu REAL NOT NULL, load1 REAL NOT NULL,
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
const SCHEMA_VERSION: i64 = 1;

/// Adds a column that older databases lack.
///
/// A column already present is the one failure this is allowed to shrug at —
/// it is what "the migration has nothing left to do" looks like. Everything
/// else is real, and the `let _ =` this replaces hid a full disk and a corrupt
/// page just as quietly as it hid a second run.
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
    // The column used to hold a sha256 of the token. It holds the token itself
    // now, so the panel can show an install command without minting a new one
    // to do it. Databases from before the change keep their old digests, which
    // no agent can present: those nodes need a new token issued from the panel
    // and their agent reinstalled, once.
    if schema_mentions(conn, "node", "token_hash")? {
        conn.execute("ALTER TABLE node RENAME COLUMN token_hash TO token", [])?;
        info!("renamed node.token_hash to node.token; existing nodes need a fresh token");
    }
    // A key can only be reordered by rebuilding the table, and CREATE TABLE IF
    // NOT EXISTS leaves an existing one exactly as it was. The old order put
    // task_id between the node and the timestamp, so a chart request -- which
    // needs no credentials, and holds the connection the agents report through
    // -- read every probe result the node had ever kept to answer for one hour
    // of it: 0.8 ms against 42 ms at a month of retention.
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

/// Takes the database away from everyone but its owner.
///
/// This file is the credential store: node tokens are in it in the clear, next
/// to the GitHub client secret and the password hash. SQLite creates it with
/// whatever the umask allows, which on a default 022 is world-readable, and the
/// WAL and shared-memory files beside it hold the same rows.
///
/// Best effort on purpose: a database on a filesystem with no Unix modes still
/// works, and refusing to start over it would be worse than the exposure.
fn restrict(path: &str) {
    #[cfg(unix)]
    for file in [path.to_owned(), format!("{path}-wal"), format!("{path}-shm")] {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
    }
}

/// The chart's probe query: per bucket, the median round trip, the range it
/// moved through, and how much of it was lost.
///
/// The median rather than the mean, the way Smokeping draws it -- one SYN
/// retransmit is tens of milliseconds and drags a mean, and it is the reading
/// that is wrong rather than the link. It comes out of a window function
/// because SQLite has no `median()`: rank each bucket's answers by latency and
/// average the middle one, or the middle two when there is an even number.
/// Ranking successes and timeouts in separate partitions is what keeps -1 out
/// of the ordering without a second pass over the table.
///
/// Named because the query plan is asserted on it in
/// `rekeying_ping_record_keeps_the_rows_and_lets_the_window_query_seek`, and a
/// second copy over there is a copy that stops matching what actually runs.
const PING_WINDOW: &str = "WITH s AS (
       SELECT task_id, ts/?3 AS b, latency,
              ROW_NUMBER() OVER (PARTITION BY task_id, ts/?3, latency>=0 ORDER BY latency) AS r,
              COUNT(*)     OVER (PARTITION BY task_id, ts/?3, latency>=0)                  AS n
       FROM ping_record WHERE node_id=?1 AND ts>=?2
     )
     SELECT task_id, b*?3,
            CAST(AVG(CASE WHEN latency>=0 AND r IN ((n+1)/2, (n+2)/2) THEN latency END) AS INTEGER),
            MIN(CASE WHEN latency>=0 THEN latency END),
            MAX(CASE WHEN latency>=0 THEN latency END),
            (100*SUM(latency<0) + COUNT(*) - 1)/COUNT(*)
     FROM s GROUP BY task_id, b ORDER BY b";

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        // Asked before CREATE TABLE runs: a file with no tables is a database
        // that has never existed, and it gets today's schema outright rather
        // than the history of how the schema arrived at it.
        let fresh = conn
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table'", [], |r| r.get::<_, i64>(0))?
            == 0;
        conn.execute_batch(SCHEMA)?;
        restrict(path);

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if !fresh && version < 1 {
            migrate_to_1(&conn)?;
        }
        // Stamped once the database matches this build. Before there was a
        // version every migration below re-ran on every start, and each one
        // decided for itself whether it had already happened.
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
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
    /// Both rows or neither: a node whose `traffic` row went missing cannot
    /// report at all, because `accumulate` reads that row on every report and
    /// a failure there drops the whole message, live metrics included.
    pub fn create_node(&self, n: &Node, token: &str) -> Result<i64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            // A new node belongs at the end of the list. `sort` comes from the
            // caller as 0, which would otherwise tie it with whatever the last
            // manual reorder put first.
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

    /// Records that the node reported. Called on the same beat as the metric
    /// row, so it costs one row update a minute rather than one a report.
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

    pub fn traffic(&self, node_id: i64) -> Traffic {
        self.conn()
            .query_row(
                "SELECT total_rx, total_tx, month_rx, month_tx, month_start, day_rx, day_tx
                     FROM traffic WHERE node_id=?1",
                [node_id],
                |r| {
                    Ok(Traffic {
                        total_rx: r.get(0)?,
                        total_tx: r.get(1)?,
                        month_rx: r.get(2)?,
                        month_tx: r.get(3)?,
                        month_start: r.get(4)?,
                        day_rx: r.get(5)?,
                        day_tx: r.get(6)?,
                    })
                },
            )
            .unwrap_or_default()
    }

    /// Every node's counters in one query. The node list renders a row per node
    /// and used to fetch each one separately: on a hub with a few hundred nodes
    /// that was a few hundred round trips through the single connection, with
    /// the agents' writes queued behind them.
    pub fn all_traffic(&self) -> HashMap<i64, Traffic> {
        let conn = self.conn();
        let Ok(mut stmt) = conn.prepare_cached(
            "SELECT node_id, total_rx, total_tx, month_rx, month_tx, month_start, day_rx, day_tx
                 FROM traffic",
        ) else {
            return HashMap::new();
        };
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Traffic {
                    total_rx: r.get(1)?,
                    total_tx: r.get(2)?,
                    month_rx: r.get(3)?,
                    month_tx: r.get(4)?,
                    month_start: r.get(5)?,
                    day_rx: r.get(6)?,
                    day_tx: r.get(7)?,
                },
            ))
        });
        rows.map(|r| r.flatten().collect()).unwrap_or_default()
    }

    /// Folds one report's raw kernel counters into the node's running totals.
    ///
    /// A changed boot_id, or a counter that moved backwards, means the kernel
    /// started counting from zero again. This is what keeps the total from
    /// collapsing every time a box reboots, which is what showing the kernel's
    /// own counter would do.
    ///
    /// The billing reset day is read here rather than passed in: it lives one
    /// join away from the row this already reads, and fetching it separately
    /// cost every report a second turn at the single write connection.
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
        // subtract from, and a bare reading is not a delta -- it is the whole
        // machine's history, which is why booking one is never safe.
        //
        // That covers all three ways a baseline goes missing. A node's first
        // report has none yet. A reading that shrank under the *same* boot lost
        // one: an interface the sum counted has gone (wg0 down, a tunnel
        // stopped), so the reading is the rest of the machine's history and
        // booking it would count that history twice. And a changed boot_id
        // means the counters restarted -- or, indistinguishably from here, that
        // a second machine is reporting under the same token, alternating
        // boot_ids several times a second and adding its entire lifetime
        // counter each time. Re-aligning costs the seconds between a reboot and
        // the first report after it; guessing wrong the other way costs
        // hundreds of gigabytes against a total that only ever climbs.
        let (d_rx, d_tx) = if prev_boot.is_empty() || prev_boot != boot_id {
            // Worth a line either way: on a healthy node this is a real reboot,
            // and a node "rebooting" every few seconds is two machines sharing
            // one token, which nothing else here would ever make visible.
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

        // Both boundaries are human dates — the day a provider resets an
        // allowance, and the day a person means by "today" — so both follow the
        // hub's own timezone. On UTC the billing month turned over eight hours
        // into the reset day for a hub in CST, while "today" beside it turned
        // at midnight: two counters on the same row disagreeing about the date.
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
        let load1 = m.get("load").and_then(|v| v.get(0)).and_then(|v| v.as_f64()).unwrap_or(0.0);
        self.conn()
            .prepare_cached(
                "INSERT OR REPLACE INTO metric
               (node_id, ts, cpu, load1, mem_used, swap_used, disk_used, net_rx, net_tx, tcp, udp, procs)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            )?
            .execute(params![
                node_id,
                ts,
                f("cpu"),
                load1,
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
    /// Without the thinning a month-wide window is tens of thousands of rows
    /// built into as many `serde_json` maps before any of it is written -- on a
    /// path anyone on the internet can ask for, against the connection the
    /// agents report through.
    ///
    /// Bucketed rather than filtered on a multiple of `step`: rows normally
    /// land on the minute, but nothing enforces it, and a filter would answer a
    /// stamp that sits between the grid lines with silence.
    ///
    /// Averaged over the bucket, not sampled from it. `MAX(ts)` with bare
    /// columns beside it answers with one arbitrary row, which is the 1/60
    /// sampling decisions.md already threw out at the write side -- put back one
    /// layer up, and worse: the seven-day window kept one minute in fourteen.
    /// Measured on cc, that window integrated to 53.69 GB of traffic against
    /// the 27.52 GB the minutes actually hold. Averaged it is 28.02 GB, so the
    /// chart's integral matches the accumulator again.
    ///
    /// The stamp is the bucket's own start rather than a row inside it, so
    /// every series lands on the same grid -- which is what lets the probe
    /// lines below share rows instead of each carrying its own timestamps.
    pub fn metrics(&self, node_id: i64, since: i64, step: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT (MIN(ts)/?3)*?3, AVG(cpu), AVG(load1), CAST(AVG(mem_used) AS INTEGER),
                    CAST(AVG(swap_used) AS INTEGER), CAST(AVG(disk_used) AS INTEGER),
                    CAST(AVG(net_rx) AS INTEGER), CAST(AVG(net_tx) AS INTEGER)
             FROM metric WHERE node_id=?1 AND ts>=?2 GROUP BY ts/?3 ORDER BY ts",
        )?;
        let rows = stmt.query_map(params![node_id, since, step], |r| {
            Ok(serde_json::json!({
                "ts": r.get::<_, i64>(0)?, "cpu": r.get::<_, f64>(1)?, "load1": r.get::<_, f64>(2)?,
                "mem_used": r.get::<_, i64>(3)?, "swap_used": r.get::<_, i64>(4)?,
                "disk_used": r.get::<_, i64>(5)?, "net_rx": r.get::<_, i64>(6)?,
                "net_tx": r.get::<_, i64>(7)?,
            }))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Drops history past the retention window. Traffic totals are unaffected:
    /// they live in their own table precisely so history can be pruned freely.
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
    /// failing between the delete and the inserts would silently unassign every
    /// node from a probe that still lists them in the panel.
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
    /// what a probe is called says nothing a visitor could act on, while its
    /// target and node assignments stay in the panel.
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
    /// the bucket's mean round trip, and how much of it was lost.
    ///
    /// These stamps are whenever the probe finished rather than on a minute, so
    /// the thinning buckets them instead of matching a multiple. Same reason as
    /// `metrics` above -- and this is the larger half of that response, because
    /// a probe reports far more often than once a minute.
    ///
    /// A timeout is stored as -1, so it has to be kept out of the median and
    /// counted instead. Keeping the newest row of the bucket was worse than
    /// coarse: on cc's 移动v4 it dropped 389 of the 788 timeouts in a day and
    /// then drew the survivors as an unbroken line, which is a probe losing
    /// half its packets rendered as a healthy one. `latency` is null when the
    /// whole bucket timed out; `loss` is the percentage that did, and rides
    /// along only when there is any -- a healthy day is 2 880 rows, and
    /// `"loss":0` on each of them is 29 kB of nothing.
    ///
    /// **Rounded up, so that no `loss` key means no timeout and nothing else.**
    /// Truncating folded the reader's two cases into one answer: a bucket of
    /// 180 samples that lost one of them is 100/180, which integer division
    /// calls 0, which drops the key, which reads as a clean bucket. A window
    /// wide enough to fill a bucket that far is `hours=2160`, and this endpoint
    /// accepts it. Rounding up costs under a percentage point and errs towards
    /// saying a probe lost something, which is the safe direction.
    pub fn ping_records(&self, node_id: i64, since: i64, step: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(PING_WINDOW)?;
        let rows = stmt.query_map(params![node_id, since, step], |r| {
            let mut row = serde_json::json!({
                "task_id": r.get::<_, i64>(0)?, "ts": r.get::<_, i64>(1)?,
                "latency": r.get::<_, Option<i64>>(2)?
            });
            // Only when the bucket actually moved. Every bucket holds one
            // sample at the hour and six-hour windows, where a band would be a
            // zero-height ribbon under every line and pure payload.
            if let (Some(lo), Some(hi)) = (r.get::<_, Option<i64>>(3)?, r.get::<_, Option<i64>>(4)?) {
                if hi > lo {
                    row["band"] = serde_json::json!([lo, hi]);
                }
            }
            if let loss @ 1.. = r.get::<_, i64>(5)? {
                row["loss"] = loss.into();
            }
            Ok(row)
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
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

    /// PRAGMA settings are per connection, so a value read back through any
    /// other handle proves nothing about the one the hub actually writes on.
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

    fn node(db: &Db, reset_day: u32) -> i64 {
        let token = format!("token-{}", rand::random::<u32>());
        db.create_node(&Node { name: "n".into(), traffic_reset_day: reset_day, ..Default::default() }, &token)
            .unwrap()
    }

    #[test]
    fn traffic_survives_a_reboot_instead_of_resetting() {
        let db = db();
        let id = node(&db, 1);

        // First report only establishes the baseline: the box's lifetime
        // counters are not booked as traffic seen by this hub.
        let t = db.accumulate(id, "boot-a", 5_000, 3_000).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (0, 0));

        let t = db.accumulate(id, "boot-a", 9_000, 6_000).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (4_000, 3_000));

        // Reboot: new boot_id, counters restart near zero. What this guards is
        // that the total does not fall back to the fresh counter value, which
        // is the collapse this hub exists to not have. The 700 bytes the box
        // moved before its first report are not booked -- there is no baseline
        // to have measured them against.
        let t = db.accumulate(id, "boot-b", 700, 400).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (4_000, 3_000), "a reboot must not reset the total");

        // And counting resumes from the new baseline.
        let t = db.accumulate(id, "boot-b", 1_700, 900).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (5_000, 3_500));
        assert_eq!((t.month_rx, t.month_tx), (5_000, 3_500));
    }

    /// One install command pasted onto a second machine. Both agents answer to
    /// the same node, evict each other from `App.agents` and reconnect, so the
    /// hub sees two boot_ids alternating a few times a second -- each carrying
    /// its own machine's lifetime counter. Booking those readings added ~180 GB
    /// per swap to a total that only ever climbs and cannot be walked back.
    #[test]
    fn two_machines_sharing_one_token_cannot_inflate_the_total() {
        let db = db();
        let id = node(&db, 1);
        let (a, b) = (100_000_000_000, 80_000_000_000); // two lifetime counters

        db.accumulate(id, "boot-a", a, a).unwrap();
        let t = db.accumulate(id, "boot-a", a + 1_000, a + 1_000).unwrap();
        assert_eq!(t.total_rx, 1_000, "the real machine's own traffic still counts");

        // Now they take turns. Every swap is a boot_id the hub has no baseline
        // for, so every swap books nothing.
        for round in 0..3 {
            db.accumulate(id, "boot-b", b + round, b + round).unwrap();
            db.accumulate(id, "boot-a", a + 1_000 + round, a + 1_000 + round).unwrap();
        }
        let t = db.traffic(id);
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
        // this reading is the rest of the machine's history rather than fresh
        // bytes. Booking it would land 2_500 here -- and tens of gigabytes on a
        // box that has been up a month.
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
    /// that never does. They hang off separate stored dates, so each rollover
    /// has to leave the other one alone -- which only shows up if both happen
    /// to the same node, one after the other.
    #[test]
    fn day_and_month_restart_independently_while_the_total_keeps_climbing() {
        let db = db();
        let id = node(&db, 1);
        db.accumulate(id, "boot-a", 0, 0).unwrap();
        let t = db.accumulate(id, "boot-a", 8_000, 4_000).unwrap();
        assert_eq!((t.day_rx, t.day_tx), (8_000, 4_000));
        assert_eq!((t.month_rx, t.month_tx), (8_000, 4_000));

        // Midnight passes. Forced through the stored date, which is what the
        // rollover actually reads.
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
        assert_eq!(db.traffic(id).total_rx, 0);
    }

    #[test]
    fn a_token_is_readable_and_rotation_retires_the_old_one() {
        let db = db();
        let id = db.create_node(&Node { name: "n".into(), ..Default::default() }, "first-token").unwrap();

        // Readable, which is the whole point: the panel shows the install
        // command without having to issue a new token to be able to.
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

        // Every rejected shape leaves the order it found. The partial list is
        // the one that matters: the panel sends the ids it has, and a stale tab
        // that never saw a node would otherwise renumber the list around it.
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
        assert_eq!(db.traffic(id).total_rx, 800);
    }

    /// The rekeying in `open()`: rows have to survive it, and the chart's query
    /// has to come out able to seek. Both halves matter -- a migration that
    /// keeps every row on the old key is silent, and stays silent while the
    /// query it exists for goes on reading the node's whole history to answer
    /// for an hour of it.
    #[test]
    fn rekeying_ping_record_keeps_the_rows_and_lets_the_window_query_seek() {
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

        // What the rebuild was for. Without the timestamp second in the key the
        // plan stops at `node_id=?` and scans everything under it.
        let plan: String = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {PING_WINDOW}"))
            .unwrap()
            .query_map(params![1, 0, 60], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join(" | ");
        assert!(plan.contains("node_id=? AND ts>?"), "the window has to be a seek, not a scan: {plan}");

        // Opening again must not rebuild a table that is already right.
        drop(conn);
        drop(db);
        assert!(Db::open(path).is_ok());
        let _ = std::fs::remove_file(&file);
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
