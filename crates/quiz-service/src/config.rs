//! Environment driven configuration with performance-oriented defaults.

use std::path::PathBuf;

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

#[derive(Clone, Debug)]
pub struct Config {
    /// HTTP listen address.
    pub listen_addr: String,
    /// Path of the SQLite database file (parent dirs are created).
    pub database_path: PathBuf,
    /// Secret used to sign JWTs. Falls back to a dev secret with a warning.
    pub jwt_secret: String,
    /// JWT lifetime in hours.
    pub jwt_ttl_hours: i64,
    /// Maximum pool connections (SQLite handles concurrent BEGIN CONCURRENT
    /// writers, so this can exceed the classic single-writer sizing).
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
    /// Per-connection page cache in KiB (negative pragma cache_size).
    pub cache_size_kib: i64,
    /// Memory-mapped I/O read limit in bytes.
    pub mmap_size: i64,
    /// OpenTelemetry OTLP gRPC endpoint (enables OTel tracing when set).
    pub otel_endpoint: Option<String>,
    /// Log format: `json` (default) or `pretty`.
    pub log_format: String,
    /// Seed a demo user + quiz on startup (idempotent).
    pub seed_demo: bool,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

impl Config {
    pub fn from_env() -> Self {
        let database_path = std::env::var("QUIZ_DATABASE")
            .ok()
            .filter(|v| !v.is_empty())
            .map(strip_sqlite_scheme)
            .unwrap_or_else(|| PathBuf::from("quiz.db"));

        let jwt_secret = std::env::var("QUIZ_JWT_SECRET")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                tracing::warn!("QUIZ_JWT_SECRET not set - using an insecure development secret");
                "quiz-service-dev-secret-do-not-use-in-production".to_string()
            });

        let otel_endpoint = std::env::var("QUIZ_OTEL_ENDPOINT")
            .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT"))
            .ok()
            .filter(|v| !v.is_empty());

        Config {
            listen_addr: env_or("QUIZ_LISTEN_ADDR", "0.0.0.0:8080"),
            database_path,
            jwt_secret,
            jwt_ttl_hours: env_or("QUIZ_JWT_TTL_HOURS", "24").parse().unwrap_or(24),
            pool_max: env_or("QUIZ_POOL_MAX", "8").parse().unwrap_or(8),
            pool_min: env_or("QUIZ_POOL_MIN", "2").parse().unwrap_or(2),
            busy_timeout_ms: env_or("QUIZ_BUSY_TIMEOUT_MS", "5000")
                .parse()
                .unwrap_or(5000),
            tx_mode: TxMode::parse(&env_or("QUIZ_TX_MODE", "concurrent"))
                .unwrap_or(TxMode::Concurrent),
            tx_max_retries: env_or("QUIZ_TX_MAX_RETRIES", "64").parse().unwrap_or(64),
            commit_mutex: env_or("QUIZ_COMMIT_MUTEX", "1") != "0",
            vfs: env_or("QUIZ_SQLITE_VFS", "unix-excl"),
            cache_size_kib: env_or("QUIZ_CACHE_SIZE_KIB", "32768")
                .parse()
                .unwrap_or(32768),
            mmap_size: env_or("QUIZ_MMAP_SIZE", "268435456")
                .parse()
                .unwrap_or(268_435_456),
            otel_endpoint,
            log_format: env_or("QUIZ_LOG_FORMAT", "json"),
            seed_demo: env_or("QUIZ_SEED_DEMO", "0") == "1",
        }
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
}
