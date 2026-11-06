//! Environment driven configuration with performance-oriented defaults.

use std::path::PathBuf;
use std::time::Duration;

/// Shortest accepted ticket / internal key, in bytes.
pub const MIN_KEY_LEN: usize = 32;

/// SQLite write-transaction strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxMode {
    /// `BEGIN CONCURRENT` - optimistic page-level locking, the point of the
    /// begin-concurrent SQLite branch. Conflicts surface as
    /// SQLITE_BUSY_SNAPSHOT at COMMIT and are retried.
    Concurrent,
    /// `BEGIN DEFERRED` - classic snapshot isolation, for baseline comparison.
    Deferred,
    /// `BEGIN IMMEDIATE` - grab the write lock up front, serializing writers.
    Immediate,
}

impl TxMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            TxMode::Concurrent => "concurrent",
            TxMode::Deferred => "deferred",
            TxMode::Immediate => "immediate",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "concurrent" => Some(TxMode::Concurrent),
            "deferred" => Some(TxMode::Deferred),
            "immediate" => Some(TxMode::Immediate),
            _ => None,
        }
    }

    pub fn begin_stmt(&self) -> &'static str {
        match self {
            TxMode::Concurrent => "BEGIN CONCURRENT",
            TxMode::Deferred => "BEGIN DEFERRED",
            TxMode::Immediate => "BEGIN IMMEDIATE",
        }
    }
}

/// A secret that never shows up in logs or `Debug` output.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Secret(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Public listener: the bank under `/v1/*` and the game socket `/ws` only.
    pub listen_addr: String,
    /// Internal listener: health, readiness, metrics and `/internal/*`.
    pub internal_addr: String,
    /// Path of the SQLite database file (parent dirs are created).
    pub database_path: PathBuf,
    /// HS256 key the Go server signs tickets with.
    pub ticket_key: Secret,
    /// Key the Go server sends in `x-internal-key` on internal calls.
    pub internal_key: Secret,
    /// Directory of the consistent snapshot; `None` turns snapshots off.
    pub snapshot_dir: Option<PathBuf>,
    /// Time between snapshot attempts.
    pub snapshot_interval: Duration,
    /// Origins of the site's pages: the only ones CORS answers and a
    /// websocket upgrade accepts.
    pub allowed_origins: Vec<String>,
    /// The Go server, where lesson-time spans are pushed; `None` turns
    /// pushing off.
    pub lms_url: Option<String>,
    /// Tokio worker threads: the node's SFU must keep its cores.
    pub worker_threads: usize,
    /// Maximum pool connections. Each holds its own page cache, so the worst
    /// case memory is `pool_max x cache_size_kib` plus the mapped file.
    pub pool_max: u32,
    /// Minimum pool connections kept warm.
    pub pool_min: u32,
    /// SQLite busy timeout per connection.
    pub busy_timeout_ms: u64,
    /// Write-transaction strategy.
    pub tx_mode: TxMode,
    /// Max retries of a write transaction on SQLITE_BUSY / SQLITE_BUSY_SNAPSHOT.
    pub tx_max_retries: u32,
    /// Serialize COMMITs with an in-process mutex, as recommended by the
    /// begin-concurrent docs for single-process deployments.
    pub commit_mutex: bool,
    /// SQLite VFS. Default `unix-excl` (faster locking for single-process
    /// use, as recommended by the branch docs); set to `unix` to allow
    /// external processes (e.g. the sqlite3 CLI) to access the live file.
    pub vfs: String,
    /// Per-connection page cache in KiB (negative pragma cache_size). A write
    /// transaction must fit in it (concurrent transactions cannot spill): the
    /// biggest one, a class's answers at the close of a question, touches well
    /// under 1 MiB.
    pub cache_size_kib: i64,
    /// Memory-mapped I/O read limit in bytes (file-backed and shared by all
    /// connections: resident only as far as the database is that big).
    pub mmap_size: i64,
    /// OpenTelemetry OTLP gRPC endpoint (enables OTel tracing when set).
    pub otel_endpoint: Option<String>,
    /// Log format: `json` (default) or `pretty`.
    pub log_format: String,
}

impl Config {
    /// Read the configuration from the process environment.
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Read the configuration through `lookup` (the environment in
    /// production, a map in tests).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let get = |name: &str| lookup(name).filter(|v| !v.is_empty());
        let or = |name: &str, default: &str| get(name).unwrap_or_else(|| default.to_string());
        let num = |name: &str, default: &str| -> Result<u64, String> {
            or(name, default)
                .parse()
                .map_err(|_| format!("{name} must be a number"))
        };
        let key = |name: &str| -> Result<Secret, String> {
            let value = get(name).ok_or_else(|| {
                format!("{name} is required (at least {MIN_KEY_LEN} bytes, shared with the server)")
            })?;
            if value.len() < MIN_KEY_LEN {
                return Err(format!(
                    "{name} is too short: {} bytes, at least {MIN_KEY_LEN} are required",
                    value.len()
                ));
            }
            Ok(Secret(value))
        };

        let database_path = get("QUIZ_DATABASE")
            .map(strip_sqlite_scheme)
            .unwrap_or_else(|| PathBuf::from("quiz.db"));

        // An explicitly empty QUIZ_SNAPSHOT_DIR turns snapshots off; unset
        // means the default directory.
        let snapshot_dir = match lookup("QUIZ_SNAPSHOT_DIR") {
            None => Some(PathBuf::from("/data/snapshots")),
            Some(v) if v.is_empty() => None,
            Some(v) => Some(PathBuf::from(v)),
        };
        let snapshot_secs = num("QUIZ_SNAPSHOT_INTERVAL_SECS", "900")?;
        if snapshot_secs == 0 {
            return Err("QUIZ_SNAPSHOT_INTERVAL_SECS must be at least 1".into());
        }

        let allowed_origins: Vec<String> = get("QUIZ_ALLOWED_ORIGINS")
            .ok_or("QUIZ_ALLOWED_ORIGINS is required (comma separated origins of the site)")?
            .split(',')
            .map(|o| o.trim().trim_end_matches('/').to_string())
            .filter(|o| !o.is_empty())
            .collect();
        if allowed_origins.is_empty()
            || allowed_origins
                .iter()
                .any(|o| o == "*" || !o.contains("://"))
        {
            return Err("QUIZ_ALLOWED_ORIGINS must list origins like https://example.org".into());
        }

        // Unset means the in-cluster Go server; an empty value turns span
        // pushing off.
        let lms_url = match lookup("QUIZ_LMS_URL") {
            None => Some("http://lms-server.lms.svc.cluster.local:8065".to_string()),
            Some(v) if v.is_empty() => None,
            Some(v) => Some(v.trim_end_matches('/').to_string()),
        };
        if let Some(url) = &lms_url
            && !url.starts_with("http://")
        {
            return Err(
                "QUIZ_LMS_URL must be a plain http:// URL (it is an in-cluster call)".into(),
            );
        }
        let pool_max = num("QUIZ_POOL_MAX", "4")? as u32;
        let pool_min = num("QUIZ_POOL_MIN", "2")? as u32;
        if pool_max == 0 || pool_min > pool_max {
            return Err(
                "QUIZ_POOL_MIN must not exceed QUIZ_POOL_MAX, which must be at least 1".into(),
            );
        }
        let worker_threads = num("QUIZ_WORKER_THREADS", "2")? as usize;
        if worker_threads == 0 {
            return Err("QUIZ_WORKER_THREADS must be at least 1".into());
        }

        let otel_endpoint =
            get("QUIZ_OTEL_ENDPOINT").or_else(|| get("OTEL_EXPORTER_OTLP_ENDPOINT"));

        let tx_mode = match get("QUIZ_TX_MODE") {
            None => TxMode::Concurrent,
            Some(v) => TxMode::parse(&v).ok_or_else(|| {
                "QUIZ_TX_MODE must be concurrent, deferred or immediate".to_string()
            })?,
        };

        Ok(Config {
            listen_addr: or("QUIZ_LISTEN_ADDR", "0.0.0.0:8080"),
            internal_addr: or("QUIZ_INTERNAL_ADDR", "0.0.0.0:8081"),
            database_path,
            ticket_key: key("QUIZ_TICKET_KEY")?,
            internal_key: key("QUIZ_INTERNAL_KEY")?,
            snapshot_dir,
            snapshot_interval: Duration::from_secs(snapshot_secs),
            allowed_origins,
            lms_url,
            worker_threads,
            pool_max,
            pool_min,
            busy_timeout_ms: num("QUIZ_BUSY_TIMEOUT_MS", "5000")?,
            tx_mode,
            tx_max_retries: num("QUIZ_TX_MAX_RETRIES", "64")? as u32,
            commit_mutex: or("QUIZ_COMMIT_MUTEX", "1") != "0",
            vfs: or("QUIZ_SQLITE_VFS", "unix-excl"),
            cache_size_kib: num("QUIZ_CACHE_SIZE_KIB", "4096")? as i64,
            mmap_size: num("QUIZ_MMAP_SIZE", "67108864")? as i64,
            otel_endpoint,
            log_format: or("QUIZ_LOG_FORMAT", "json"),
        })
    }
}

/// Accept `sqlite://path`, `sqlite:path` or a bare path.
fn strip_sqlite_scheme(raw: String) -> PathBuf {
    let mut s = raw.as_str();
    if let Some(rest) = s.strip_prefix("sqlite://") {
        s = rest;
    } else if let Some(rest) = s.strip_prefix("sqlite:") {
        s = rest;
    }
    // Drop URI query fragments like `?mode=rwc`.
    if let Some(idx) = s.find('?') {
        s = &s[..idx];
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn lookup(extra: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let mut map: HashMap<String, String> = [
            ("QUIZ_TICKET_KEY", KEY),
            ("QUIZ_ALLOWED_ORIGINS", "https://site.example"),
            ("QUIZ_INTERNAL_KEY", "fedcba9876543210fedcba9876543210"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in extra {
            map.insert(k.to_string(), v.to_string());
        }
        move |name| map.get(name).cloned()
    }

    #[test]
    fn parses_database_urls() {
        assert_eq!(
            strip_sqlite_scheme("sqlite://data/x.db".into()),
            PathBuf::from("data/x.db")
        );
        assert_eq!(
            strip_sqlite_scheme("sqlite:data/x.db?mode=rwc".into()),
            PathBuf::from("data/x.db")
        );
        assert_eq!(
            strip_sqlite_scheme("/tmp/q.db".into()),
            PathBuf::from("/tmp/q.db")
        );
    }

    #[test]
    fn defaults_follow_the_contract() {
        let cfg = Config::from_lookup(lookup(&[])).unwrap();
        assert_eq!(cfg.listen_addr, "0.0.0.0:8080");
        assert_eq!(cfg.internal_addr, "0.0.0.0:8081");
        assert_eq!(cfg.snapshot_dir, Some(PathBuf::from("/data/snapshots")));
        assert_eq!(cfg.snapshot_interval, Duration::from_secs(900));
        assert_eq!(cfg.worker_threads, 2);
        assert_eq!(
            cfg.allowed_origins,
            vec!["https://site.example".to_string()]
        );
        assert!(cfg.lms_url.unwrap().starts_with("http://lms-server"));
    }

    #[test]
    fn keys_are_required_and_long_enough() {
        let missing = Config::from_lookup(|n| match n {
            "QUIZ_TICKET_KEY" => None,
            "QUIZ_ALLOWED_ORIGINS" => Some("https://site.example".to_string()),
            "QUIZ_INTERNAL_KEY" => Some(KEY.to_string()),
            _ => None,
        });
        assert!(missing.unwrap_err().contains("QUIZ_TICKET_KEY is required"));

        let short = Config::from_lookup(lookup(&[("QUIZ_INTERNAL_KEY", "short")]));
        assert!(
            short
                .unwrap_err()
                .contains("QUIZ_INTERNAL_KEY is too short")
        );
    }

    #[test]
    fn origins_are_required_and_must_be_origins() {
        let none = Config::from_lookup(|n| match n {
            "QUIZ_TICKET_KEY" | "QUIZ_INTERNAL_KEY" => Some(KEY.into()),
            _ => None,
        });
        assert!(
            none.unwrap_err()
                .contains("QUIZ_ALLOWED_ORIGINS is required")
        );
        assert!(Config::from_lookup(lookup(&[("QUIZ_ALLOWED_ORIGINS", "*")])).is_err());
        let two = Config::from_lookup(lookup(&[(
            "QUIZ_ALLOWED_ORIGINS",
            "https://a.org/, http://localhost:5173",
        )]))
        .unwrap();
        assert_eq!(
            two.allowed_origins,
            vec!["https://a.org", "http://localhost:5173"]
        );
    }

    #[test]
    fn empty_lms_url_turns_span_pushing_off() {
        assert_eq!(
            Config::from_lookup(lookup(&[("QUIZ_LMS_URL", "")]))
                .unwrap()
                .lms_url,
            None
        );
        assert!(Config::from_lookup(lookup(&[("QUIZ_LMS_URL", "https://x")])).is_err());
    }

    #[test]
    fn memory_defaults_fit_a_256_mib_pod_with_room_to_spare() {
        let cfg = Config::from_lookup(lookup(&[])).unwrap();
        // every connection's page cache, plus the largest mapping
        let worst_case_mib =
            (i64::from(cfg.pool_max) * cfg.cache_size_kib) / 1024 + cfg.mmap_size / (1024 * 1024);
        assert_eq!(
            (cfg.pool_max, cfg.cache_size_kib, cfg.mmap_size),
            (4, 4096, 64 * 1024 * 1024)
        );
        assert!(worst_case_mib <= 80, "{worst_case_mib} MiB");
        // overridable, but a pool must make sense
        let big = Config::from_lookup(lookup(&[
            ("QUIZ_POOL_MAX", "8"),
            ("QUIZ_CACHE_SIZE_KIB", "32768"),
        ]))
        .unwrap();
        assert_eq!((big.pool_max, big.cache_size_kib), (8, 32768));
        assert!(
            Config::from_lookup(lookup(&[("QUIZ_POOL_MAX", "1"), ("QUIZ_POOL_MIN", "2")])).is_err()
        );
    }

    #[test]
    fn empty_snapshot_dir_turns_snapshots_off() {
        let cfg = Config::from_lookup(lookup(&[("QUIZ_SNAPSHOT_DIR", "")])).unwrap();
        assert_eq!(cfg.snapshot_dir, None);
    }

    #[test]
    fn debug_output_hides_keys() {
        let cfg = Config::from_lookup(lookup(&[])).unwrap();
        assert!(!format!("{cfg:?}").contains(KEY));
    }
}
