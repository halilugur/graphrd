//! Process-wide counters and histograms, written in the Prometheus text
//! exposition format by `GET /metrics`, plus the slow query log.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Upper bounds (seconds) of the histogram buckets.
const BOUNDS: [f64; 14] = [0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

/// A latency histogram with fixed buckets; lock-free.
pub struct Histogram {
    buckets: [AtomicU64; BOUNDS.len()],
    count: AtomicU64,
    sum_micros: AtomicU64,
}

impl Histogram {
/// A histogram at zero.
    pub const fn new() -> Histogram {
        Histogram { buckets: [const { AtomicU64::new(0) }; BOUNDS.len()], count: AtomicU64::new(0), sum_micros: AtomicU64::new(0) }
    }

/// Record one observation.
    pub fn observe(&self, d: Duration) {
        let secs = d.as_secs_f64();
        if let Some(i) = BOUNDS.iter().position(|b| secs <= *b) {
            self.buckets[i].fetch_add(1, Relaxed);
        }
        self.count.fetch_add(1, Relaxed);
        self.sum_micros.fetch_add(d.as_micros() as u64, Relaxed);
    }

/// How many were observed.
    pub fn count(&self) -> u64 {
        self.count.load(Relaxed)
    }

    fn write(&self, out: &mut String, name: &str, help: &str) {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} histogram");
        let mut cum = 0;
        for (b, n) in BOUNDS.iter().zip(&self.buckets) {
            cum += n.load(Relaxed);
            let _ = writeln!(out, "{name}_bucket{{le=\"{b}\"}} {cum}");
        }
        let count = self.count();
        let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {count}");
        let _ = writeln!(out, "{name}_sum {}", self.sum_micros.load(Relaxed) as f64 / 1e6);
        let _ = writeln!(out, "{name}_count {count}");
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram::new()
    }
}

/// SPARQL query evaluation times.
pub static QUERIES: Histogram = Histogram::new();
/// SPARQL update times.
pub static UPDATES: Histogram = Histogram::new();
/// Write-ahead log fsync times (group commit: one may cover several commits).
pub static WAL_FSYNC: Histogram = Histogram::new();
/// Queries that failed.
pub static QUERY_ERRORS: AtomicU64 = AtomicU64::new(0);
/// Updates that failed.
pub static UPDATE_ERRORS: AtomicU64 = AtomicU64::new(0);
/// Queries and updates over the --slow-query threshold.
pub static SLOW_QUERIES: AtomicU64 = AtomicU64::new(0);
/// Index block reads served from the block cache.
pub static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
/// Index block reads that went to the disk.
pub static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
/// Statements added through HTTP uploads and imports.
pub static STATEMENTS_LOADED: AtomicU64 = AtomicU64::new(0);

/// HTTP responses by status class (1xx..5xx).
static RESPONSES: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// Queries and updates slower than this many milliseconds go to the slow query log
/// (0 = off; `serve --slow-query <ms>`).
pub static SLOW_MS: AtomicU64 = AtomicU64::new(0);

static STARTED: Mutex<Option<Instant>> = Mutex::new(None);
static SLOW_LOG: Mutex<VecDeque<SlowEntry>> = Mutex::new(VecDeque::new());
const SLOW_KEEP: usize = 100;

#[derive(Clone)]
/// One entry of the slow query log.
pub struct SlowEntry {
/// When it ran, Unix milliseconds.
    pub at_millis: i64,
/// Which repository.
    pub repository: String,
/// `query` or `update`.
    pub kind: &'static str,
/// How long it took.
    pub millis: u64,
/// The query or update text.
    pub text: String,
}

/// Note the server's start (the uptime gauge).
pub fn start() {
    let mut s = STARTED.lock().unwrap_or_else(|e| e.into_inner());
    s.get_or_insert_with(Instant::now);
}

/// HTTP responses so far.
pub fn responses_total() -> u64 {
    RESPONSES.iter().map(|c| c.load(Relaxed)).sum()
}

/// Count a response into its status class.
pub fn response(status: u16) {
    if let Some(c) = RESPONSES.get((status / 100).saturating_sub(1) as usize) {
        c.fetch_add(1, Relaxed);
    }
}

/// Record a finished query or update; slow ones are logged to standard error and kept
/// for `GET /rest/monitor/slow-queries`.
pub fn finished(kind: &'static str, repository: &str, text: &str, took: Duration, ok: bool) {
    let (hist, errors) = if kind == "update" { (&UPDATES, &UPDATE_ERRORS) } else { (&QUERIES, &QUERY_ERRORS) };
    hist.observe(took);
    if !ok {
        errors.fetch_add(1, Relaxed);
    }
    let limit = SLOW_MS.load(Relaxed);
    let millis = took.as_millis() as u64;
    if limit == 0 || millis < limit {
        return;
    }
    SLOW_QUERIES.fetch_add(1, Relaxed);
    let mut short: String = text.chars().take(4000).collect();
    if short.len() < text.len() {
        short.push('…');
    }
    let at_millis = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let line = crate::json::Json::obj()
        .with("slow", kind)
        .with("repository", repository)
        .with("ms", millis as usize)
        .with("text", short.as_str());
    eprintln!("{line}");
    let mut log = SLOW_LOG.lock().unwrap_or_else(|e| e.into_inner());
    if log.len() == SLOW_KEEP {
        log.pop_front();
    }
    log.push_back(SlowEntry { at_millis, repository: repository.into(), kind, millis, text: short });
}

/// The slow query log, newest first.
pub fn slow_queries() -> Vec<SlowEntry> {
    SLOW_LOG.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().cloned().collect()
}

/// Resident set size of this process, where the platform makes it cheap to read.
fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

fn counter(out: &mut String, name: &str, help: &str, v: u64) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}");
}

/// A label value as a Prometheus quoted string.
pub fn escape_label(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Everything except the per-repository gauges, which the server adds.
pub fn render(out: &mut String) {
    let up = STARTED.lock().unwrap_or_else(|e| e.into_inner()).map_or(0.0, |s| s.elapsed().as_secs_f64());
    let _ = writeln!(out, "# HELP graphrd_info Build information.\n# TYPE graphrd_info gauge\ngraphrd_info{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(out, "# HELP graphrd_uptime_seconds Seconds since the server started.\n# TYPE graphrd_uptime_seconds gauge\ngraphrd_uptime_seconds {up}");
    let _ = writeln!(out, "# HELP graphrd_http_responses_total HTTP responses by status class.\n# TYPE graphrd_http_responses_total counter");
    for (i, c) in RESPONSES.iter().enumerate() {
        let _ = writeln!(out, "graphrd_http_responses_total{{code=\"{}xx\"}} {}", i + 1, c.load(Relaxed));
    }
    QUERIES.write(out, "graphrd_query_duration_seconds", "Time to evaluate a SPARQL query.");
    UPDATES.write(out, "graphrd_update_duration_seconds", "Time to run a SPARQL update.");
    WAL_FSYNC.write(out, "graphrd_wal_fsync_duration_seconds", "Time of one write-ahead log fsync (one covers every commit waiting for it).");
    counter(out, "graphrd_query_errors_total", "Queries that failed.", QUERY_ERRORS.load(Relaxed));
    counter(out, "graphrd_update_errors_total", "Updates that failed.", UPDATE_ERRORS.load(Relaxed));
    counter(out, "graphrd_slow_queries_total", "Queries and updates slower than --slow-query.", SLOW_QUERIES.load(Relaxed));
    counter(out, "graphrd_block_cache_hits_total", "Index block reads served from the block cache.", CACHE_HITS.load(Relaxed));
    counter(out, "graphrd_block_cache_misses_total", "Index block reads that went to disk.", CACHE_MISSES.load(Relaxed));
    counter(out, "graphrd_statements_loaded_total", "Statements added through HTTP uploads and imports.", STATEMENTS_LOADED.load(Relaxed));
    counter(out, "graphrd_request_log_lines_dropped_total", "Access log lines dropped because the queue was full.", crate::log::dropped());
    if let Some(rss) = resident_bytes() {
        let _ = writeln!(out, "# HELP graphrd_resident_memory_bytes Resident memory of the process.\n# TYPE graphrd_resident_memory_bytes gauge\ngraphrd_resident_memory_bytes {rss}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets_are_cumulative() {
        let h = Histogram::new();
        h.observe(Duration::from_micros(300));
        h.observe(Duration::from_millis(3));
        h.observe(Duration::from_secs(20));
        let mut out = String::new();
        h.write(&mut out, "t", "test");
        assert!(out.contains("t_bucket{le=\"0.0005\"} 1\n"));
        assert!(out.contains("t_bucket{le=\"0.005\"} 2\n"));
        assert!(out.contains("t_bucket{le=\"10\"} 2\n"));
        assert!(out.contains("t_bucket{le=\"+Inf\"} 3\n"));
        assert!(out.contains("t_count 3\n"));
    }
}
