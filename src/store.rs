//! Disk-backed quad store: a term dictionary plus four LSM indexes (SPOG,
//! POSG, OSPG, GSPO). Changes are logged to a write-ahead log and folded
//! into immutable run files at checkpoints, so the data set is not limited
//! by memory. Inferred statements live in a reserved pseudo-graph and are
//! persisted together with the explicit ones.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::fts;
use crate::lsm::{BlockCache, Index, Key};
use crate::rdf::{Quad, Term, turtle};
use crate::reason::{Facts, Reasoner, Triple};

pub use crate::dict::{Dict, Id};

/// Graph id of the default graph.
pub const DEFAULT_GRAPH: Id = 0;
/// Pseudo-graph holding inferred (implicit) statements. Never a dictionary
/// id; reads treat it as part of the default graph.
pub const INFERRED: Id = Id::MAX - 1;

/// Checkpoint when the memtables hold this many entries in total.
/// When set, every commit is fsynced before it is acknowledged, so it survives power loss and
/// not only a crash of the process. `graphrd serve --sync off` clears it.
pub static SYNC_COMMITS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

const MEM_LIMIT: usize = 2_000_000;
/// The vector index's file in a store directory.
const VECTORS: &str = "vectors.bin";
const WAL_LIMIT: u64 = 256 * 1024 * 1024;
/// The log is zero-filled this far ahead of its end, so a commit's fsync writes only data:
/// appending would also change the file's size, and syncing that metadata costs about as
/// much again on most file systems. Replay stops at the first zero byte.
const WAL_PREALLOC: u64 = 1024 * 1024;
const CACHE_BLOCKS: usize = 16_384; // ~70 MB of decoded blocks
const REASON_CHUNK: usize = 200_000;

const S: usize = 0;
const P: usize = 1;
const O: usize = 2;
const G: usize = 3;

/// Index key orders: SPOG, POSG, OSPG, GSPO.
const ORDERS: [[usize; 4]; 4] = [[S, P, O, G], [P, O, S, G], [O, S, P, G], [G, S, P, O]];
const NAMES: [&str; 4] = ["spog", "posg", "ospg", "gspo"];

fn permute(q: [Id; 4], order: &[usize; 4]) -> Key {
    [q[order[0]], q[order[1]], q[order[2]], q[order[3]]]
}

fn unpermute(k: Key, order: &[usize; 4]) -> [Id; 4] {
    let mut q = [0; 4];
    for (i, &pos) in order.iter().enumerate() {
        q[pos] = k[i];
    }
    q
}

/// Group commit for the write-ahead log. A writer appends its transaction while holding the
/// store lock, releases the lock, then waits here until the log is fsynced up to its
/// transaction; one fsync covers every transaction appended meanwhile.
#[derive(Default)]
pub struct WalSync {
    file: std::sync::Mutex<Option<File>>,
    /// Transactions appended to the log (and flushed to the OS).
    logged: std::sync::atomic::AtomicU64,
    /// Transactions known to be on disk.
    synced: std::sync::atomic::AtomicU64,
}

impl WalSync {
    /// Wait until the log is fsynced up to `seq` (one fsync may cover many commits).
    pub fn wait(&self, seq: u64) -> io::Result<()> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.synced.load(SeqCst) >= seq {
            return Ok(());
        }
        let file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if self.synced.load(SeqCst) >= seq {
            return Ok(()); // another writer's fsync covered this transaction
        }
        let target = self.logged.load(SeqCst);
        if let Some(f) = file.as_ref() {
            let t0 = std::time::Instant::now();
            f.sync_data()?;
            crate::metrics::WAL_FSYNC.observe(t0.elapsed());
        }
        self.synced.fetch_max(target, SeqCst);
        Ok(())
    }
}

/// A full-text query prepared by `Store::fts_prepare`.
pub struct FtsPrepared {
    /// The query and its typo-tolerant fallbacks (see `FtsQuery::forms`), each with its
    /// document frequencies once known.
    forms: Vec<(fts::FtsQuery, std::cell::OnceCell<Vec<f64>>)>,
    corpus: Option<fts::Corpus>,
}

impl FtsPrepared {
    /// Whether nothing is indexed yet.
    pub fn is_empty(&self) -> bool {
        self.forms[0].0.is_empty()
    }

    /// The first form of the query that matches `doc` (for highlighting).
    pub fn matching(&self, doc: &fts::Doc) -> &fts::FtsQuery {
        self.forms
            .iter()
            .map(|(q, _)| q)
            .find(|q| q.score(doc, None, &[]).is_some())
            .unwrap_or(&self.forms[0].0)
    }
}

/// An indexed quad store: dictionary, four LSM indexes, WAL, full-text, vectors.
pub struct Store {
    /// Shared with snapshots (append-only).
    pub dict: Arc<Dict>,
    indexes: [Index; 4],
    dir: Option<PathBuf>,
    wal: Option<BufWriter<File>>,
    wal_bytes: u64,
    /// How far the log file is zero-filled (see `WAL_PREALLOC`).
    wal_alloc: u64,
    reasoner: Option<Arc<Reasoner>>,
    /// The ruleset's reasoner before `Reasoner::compile` added the rules of the data's
    /// OWL list axioms (`reasoner` is the compiled one).
    reasoner_base: Option<Arc<Reasoner>>,
    ruleset: (String, bool),
    explicit_count: u64,
    inferred_count: u64,
    /// Full-text postings `[key, literal id, 0, 0]` and `[fts::STATS, literal id, words, 1]`
    /// (see `fts`), when enabled.
    fts: Index,
    fts_enabled: bool,
    /// Indexed literals and their total words (for BM25), counted on first use; shared with
    /// snapshots.
    fts_stats: Arc<std::sync::Mutex<Option<(u64, u64)>>>,
    /// The stem keys of words whose vocabulary trigrams this process has written (writing
    /// them again is harmless, so this only saves work; it is cleared when it grows large).
    fts_vocab: HashMap<String, [u32; 2]>,
    /// Spatial index `[z high, z low, level, literal id]` over geometry literals (see
    /// `geo::index_cells`); always kept.
    geo: Index,
    /// Nearest-neighbour index over `g:vector` literals (see `vector`), shared with snapshots:
    /// searches check results against the snapshot's statements.
    vectors: Arc<std::sync::RwLock<crate::vector::Hnsw>>,
    /// Search connectors by name (see `connector`), shared with snapshots.
    pub connectors: Arc<std::sync::RwLock<HashMap<String, Arc<crate::connector::Running>>>>,
    /// Subjects whose statements changed since the connectors last looked (only kept while
    /// there are connectors).
    pub feed: Option<Arc<std::sync::Mutex<HashSet<Id>>>>,
    /// When set, every applied change is recorded: (removed, inserted).
    capture: Option<(Vec<Quad>, Vec<Quad>)>,
    /// RDF Rank scores (0..1) and whether data changed since computing them.
    rank: Arc<std::collections::HashMap<Id, f32>>,
    rank_outdated: bool,
    /// Incremented on every change (cache invalidation for derived indexes).
    version: u64,
    wal_sync: std::sync::Arc<WalSync>,
    /// Queries without a dataset see the union of all graphs as their default graph (true),
    /// or only the default graph (false).
    pub union_default_graph: bool,
    /// Sequence number of a logged transaction the current writer still has to wait for.
    unsynced: Option<u64>,
    /// Commit time (Unix milliseconds) of the last logged transaction; commit times only grow.
    last_commit: i64,
    /// The change history kept for time travel, when enabled.
    history: Option<History>,
    /// While a batch is open (`begin_batch`), the log lines of its changes: they reach the log
    /// as one transaction when it ends, so a crash keeps all of them or none.
    batch: Option<String>,
}

/// `history.log`: every committed transaction as in the write-ahead log (`+ ` / `- ` N-Quads
/// lines, then `c <commit time>`), after a first line `s <time>` from which on the history is
/// complete. Unlike the log it is not emptied by checkpoints, only pruned past the retention.
struct History {
    file: BufWriter<File>,
    /// Kept for this many milliseconds (0: for ever).
    retention: i64,
    /// Commit time of the oldest transaction in the file.
    oldest: Option<i64>,
}

const HISTORY: &str = "history.log";

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The text of a log file (write-ahead log or history) up to its zero-filled tail. A crash can
/// tear the last write anywhere, even inside a multi-byte character, so invalid UTF-8 is
/// replaced instead of failing: it can only be in the torn transaction, which has no commit
/// marker and is dropped.
fn read_log(path: &Path) -> io::Result<String> {
    let mut bytes = fs::read(path)?;
    if let Some(end) = bytes.iter().position(|&b| b == 0) {
        bytes.truncate(end);
    }
    Ok(match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    })
}

/// The commit time in a log line `c <time>` (None for other lines; Some(None) for a bare `c`).
fn commit_line(line: &str) -> Option<Option<i64>> {
    match line.strip_prefix('c') {
        Some("") => Some(None),
        Some(rest) if rest.starts_with(' ') => Some(rest.trim().parse().ok()),
        _ => None,
    }
}

struct Manifest {
    dict: Id,
    explicit: u64,
    inferred: u64,
    ruleset: String,
    same_as: bool,
    runs: [Vec<String>; 4],
    fts: bool,
    /// Whether the full-text postings have the current layout (stems, trigrams, statistics).
    fts2: bool,
    fts_runs: Vec<String>,
    /// Whether the spatial index is complete (stores written before it existed have none).
    geo: bool,
    geo_runs: Vec<String>,
    /// Whether the vector index covers every vector literal (stores written before it existed
    /// do not).
    vec: bool,
}

impl Manifest {
    fn read(path: &Path) -> Option<Manifest> {
        let text = fs::read_to_string(path).ok()?;
        let mut m = Manifest::empty();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("dict") => m.dict = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0),
                Some("explicit") => {
                    m.explicit = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0)
                }
                Some("inferred") => {
                    m.inferred = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0)
                }
                Some("ruleset") => {
                    m.ruleset = parts.next().unwrap_or("empty").to_string();
                    m.same_as = parts.next() != Some("0");
                }
                Some("fts") => {
                    let v = parts.next();
                    m.fts = matches!(v, Some("1" | "2"));
                    m.fts2 = v == Some("2");
                }
                Some("geo") => m.geo = parts.next() == Some("1"),
                Some("vec") => m.vec = parts.next() == Some("1"),
                Some("index") => {
                    let name = parts.next().unwrap_or("");
                    if name == "fts" {
                        m.fts_runs = parts.map(String::from).collect();
                    } else if name == "geo" {
                        m.geo_runs = parts.map(String::from).collect();
                    } else if let Some(i) = NAMES.iter().position(|n| *n == name) {
                        m.runs[i] = parts.map(String::from).collect();
                    }
                }
                _ => {}
            }
        }
        Some(m)
    }

    fn empty() -> Manifest {
        Manifest {
            dict: 0,
            explicit: 0,
            inferred: 0,
            ruleset: "empty".into(),
            same_as: true,
            runs: Default::default(),
            fts: false,
            fts2: false,
            fts_runs: Vec::new(),
            geo: false,
            geo_runs: Vec::new(),
            vec: false,
        }
    }

    fn write(&self, path: &Path) -> io::Result<()> {
        let mut text = format!(
            "graphrd-manifest 1\ndict {}\nexplicit {}\ninferred {}\nruleset {} {}\nfts {}\ngeo {}\nvec {}\n",
            self.dict,
            self.explicit,
            self.inferred,
            self.ruleset,
            self.same_as as u8,
            if self.fts { 2 } else { 0 },
            self.geo as u8,
            self.vec as u8
        );
        for (i, runs) in self.runs.iter().enumerate() {
            text.push_str(&format!("index {} {}\n", NAMES[i], runs.join(" ")));
        }
        text.push_str(&format!("index fts {}\n", self.fts_runs.join(" ")));
        text.push_str(&format!("index geo {}\n", self.geo_runs.join(" ")));
        let tmp = path.with_extension("tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)
    }
}

/// Zero-fill `len` bytes of `f` from `at`, without moving its write position.
fn preallocate(file: &File, at: u64, len: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        let zeros = vec![0u8; 1 << 20];
        let mut off = 0;
        while off < len {
            let n = (len - off).min(zeros.len() as u64) as usize;
            file.write_all_at(&zeros[..n], at + off)?;
            off += n as u64;
        }
    }
    #[cfg(windows)]
    {
        // seek + write, then restore the position the caller relies on
        use std::io::Seek;
        let mut f = file;
        let pos = f.stream_position()?;
        f.seek(std::io::SeekFrom::Start(at))?;
        let zeros = vec![0u8; 1 << 20];
        let mut left = len;
        while left > 0 {
            let n = (left as usize).min(zeros.len());
            f.write_all(&zeros[..n])?;
            left -= n as u64;
        }
        f.seek(std::io::SeekFrom::Start(pos))?;
    }
    #[cfg(not(any(unix, windows)))]
    let _ = (file, at, len);
    Ok(())
}

impl Store {
    /// A store that lives in memory only (`serve --memory`, tests).
    pub fn memory() -> Self {
        let cache = BlockCache::new(16);
        Store {
            dict: Arc::new(Dict::memory()),
            indexes: NAMES.map(|n| Index::memory(n, cache.clone())),
            dir: None,
            wal: None,
            wal_bytes: 0,
            wal_alloc: 0,
            reasoner: None,
            reasoner_base: None,
            ruleset: ("empty".into(), true),
            explicit_count: 0,
            inferred_count: 0,
            fts: Index::memory("fts", cache.clone()),
            fts_enabled: false,
            fts_stats: Default::default(),
            fts_vocab: HashMap::new(),
            geo: Index::memory("geo", cache),
            vectors: Default::default(),
            connectors: Default::default(),
            feed: None,
            capture: None,
            rank: Default::default(),
            rank_outdated: false,
            version: 0,
            wal_sync: Default::default(),
            union_default_graph: true,
            unsynced: None,
            last_commit: 0,
            history: None,
            batch: None,
        }
    }

    /// A read-only view of the store as it is now, for readers: it shares the dictionary,
    /// the frozen memtable levels and the run files, so taking one costs a few `Arc` clones,
    /// and later writes to `self` do not show in it. It has no log and cannot be written.
    pub fn snapshot(&mut self) -> Store {
        Store {
            dict: self.dict.clone(),
            indexes: self.indexes.each_mut().map(|i| i.snapshot()),
            dir: self.dir.clone(),
            wal: None,
            wal_bytes: self.wal_bytes,
            wal_alloc: 0,
            reasoner: self.reasoner.clone(),
            reasoner_base: self.reasoner_base.clone(),
            ruleset: self.ruleset.clone(),
            explicit_count: self.explicit_count,
            inferred_count: self.inferred_count,
            fts: self.fts.snapshot(),
            fts_enabled: self.fts_enabled,
            fts_stats: self.fts_stats.clone(),
            fts_vocab: HashMap::new(),
            geo: self.geo.snapshot(),
            vectors: self.vectors.clone(),
            connectors: self.connectors.clone(),
            feed: self.feed.clone(),
            capture: None,
            rank: self.rank.clone(),
            rank_outdated: self.rank_outdated,
            version: self.version,
            wal_sync: self.wal_sync.clone(),
            union_default_graph: self.union_default_graph,
            unsynced: None,
            last_commit: self.last_commit,
            history: None,
            batch: None,
        }
    }

    /// Open (or create) a persistent store in `dir`.
    pub fn open(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let cache = BlockCache::new(CACHE_BLOCKS);
        let manifest = Manifest::read(&dir.join("MANIFEST"));
        let fresh = manifest.is_none();
        let m = manifest.unwrap_or_else(Manifest::empty);
        let mut indexes = Vec::new();
        for (i, name) in NAMES.iter().enumerate() {
            indexes.push(Index::open(dir, name, &m.runs[i], cache.clone())?);
        }
        let mut st = Store {
            dict: Arc::new(Dict::open(dir, m.dict)?),
            indexes: indexes.try_into().ok().expect("four indexes"),
            dir: Some(dir.to_path_buf()),
            wal: None,
            wal_bytes: 0,
            wal_alloc: 0,
            reasoner: None,
            reasoner_base: None,
            ruleset: (m.ruleset.clone(), m.same_as),
            explicit_count: m.explicit,
            inferred_count: m.inferred,
            fts: Index::open(dir, "fts", &m.fts_runs, cache.clone())?,
            fts_enabled: m.fts,
            fts_stats: Default::default(),
            fts_vocab: HashMap::new(),
            geo: Index::open(dir, "geo", &m.geo_runs, cache.clone())?,
            vectors: Arc::new(std::sync::RwLock::new(
                match crate::vector::Hnsw::load(&dir.join(VECTORS)) {
                    Ok(h) => h,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Default::default(),
                    Err(e) => {
                        eprintln!("graphrd: {e}; the vector index is rebuilt");
                        Default::default()
                    }
                },
            )),
            connectors: Default::default(),
            feed: None,
            capture: None,
            rank: Default::default(),
            rank_outdated: false,
            version: 0,
            wal_sync: Default::default(),
            union_default_graph: true,
            unsynced: None,
            last_commit: 0,
            history: None,
            batch: None,
        };
        st.load_rank();
        st.reasoner_base = Reasoner::new(&m.ruleset, m.same_as, &st.dict).map(Arc::new);
        st.reasoner = st.reasoner_base.clone();
        st.compile_rules();
        for idx in &st.indexes {
            idx.sweep()?;
        }
        st.fts.sweep()?;
        st.geo.sweep()?;
        // Migrate stores written by the earlier snapshot-only format.
        let legacy = dir.join("snapshot.nq");
        if fresh && legacy.exists() {
            let text = fs::read_to_string(&legacy)?;
            let quads = turtle::parse_nquads_raw(&text).map_err(io::Error::other)?;
            st.apply_unlogged(&[], &quads);
            st.checkpoint()?;
            fs::rename(&legacy, dir.join("snapshot.nq.migrated"))?;
        }
        // Replay changes made after the last checkpoint, in order.
        let wal_path = dir.join("wal.log");
        if wal_path.exists() {
            let text = read_log(&wal_path)?;
            // Every transaction ends with a "c" line; a transaction whose marker is missing was
            // torn by a crash and is dropped whole. (Logs written before markers existed have
            // none at all; their lines are taken one by one.)
            let marked = text.lines().any(|l| commit_line(l).is_some());
            let mut txn: Vec<(bool, Vec<Quad>)> = Vec::new();
            // Group runs of consecutive additions / removals so that
            // inference is redone once per run, not once per statement.
            let (mut adds, mut dels): (Vec<Quad>, Vec<Quad>) = (Vec::new(), Vec::new());
            let mut commit = |txn: &mut Vec<(bool, Vec<Quad>)>, st: &mut Store| {
                for (add, qs) in txn.drain(..) {
                    if add {
                        if !dels.is_empty() {
                            st.apply_unlogged(&std::mem::take(&mut dels), &[]);
                        }
                        adds.extend(qs);
                    } else {
                        if !adds.is_empty() {
                            st.apply_unlogged(&[], &std::mem::take(&mut adds));
                        }
                        dels.extend(qs);
                    }
                }
            };
            for line in text.lines() {
                if let Some(at) = commit_line(line) {
                    st.last_commit = st.last_commit.max(at.unwrap_or(0));
                    commit(&mut txn, &mut st);
                    continue;
                }
                let (op, rest) = line.split_at(line.len().min(2));
                // A torn final line from a crash is ignored.
                let Ok(qs) = turtle::parse_nquads_raw(rest) else {
                    continue;
                };
                match op {
                    "+ " => txn.push((true, qs)),
                    "- " => txn.push((false, qs)),
                    _ => continue,
                }
                if !marked {
                    commit(&mut txn, &mut st);
                }
            }
            st.apply_unlogged(&dels, &[]);
            st.apply_unlogged(&[], &adds);
        }
        if !fresh && !m.geo {
            st.index_all_geometries();
        }
        let vectors_missing = !dir.join(VECTORS).exists();
        if !fresh && (!m.vec || vectors_missing) {
            st.index_all_vectors();
        }
        if m.fts && !m.fts2 {
            // full-text postings of an earlier layout: index again
            st.fts_enabled = false;
            st.set_fts(true);
        }
        if dir.join(HISTORY).exists() {
            st.open_history(0)?;
            // transactions logged but not yet in the history when the process stopped
            st.history_catch_up(&read_log(&wal_path).unwrap_or_default())?;
        }
        st.checkpoint()?;
        Ok(st)
    }

    fn manifest(&self) -> Manifest {
        Manifest {
            dict: self.dict.count(),
            explicit: self.explicit_count,
            inferred: self.inferred_count,
            ruleset: self.ruleset.0.clone(),
            same_as: self.ruleset.1,
            runs: [0, 1, 2, 3].map(|i| self.indexes[i].run_names()),
            fts: self.fts_enabled,
            fts2: self.fts_enabled,
            fts_runs: self.fts.run_names(),
            geo: true,
            vec: true,
            geo_runs: self.geo.run_names(),
        }
    }

    /// Fold the memtables into run files, persist the dictionary, and
    /// truncate the write-ahead log.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        let Some(dir) = self.dir.clone() else {
            return Ok(());
        };
        if self.batch.is_some() {
            // the batch's changes are in the memtables but not yet in the log: folding them
            // into run files now would make part of the transaction durable
            return Ok(());
        }
        if let Some(w) = &mut self.wal {
            w.flush()?;
        }
        // the history must hold every transaction before the log forgets them
        if let Some(h) = &mut self.history {
            h.file.flush()?;
            h.file.get_ref().sync_all()?;
        }
        self.prune_history()?;
        self.wal = None;
        self.dict.checkpoint()?;
        for idx in &mut self.indexes {
            idx.flush()?;
        }
        self.fts.flush()?;
        self.geo.flush()?;
        {
            // the vector graph before the manifest: a crash between them leaves a graph with
            // nodes the log replays again (inserting twice is a no-op). An empty graph is saved
            // too: without the file, opening would scan every literal for vectors again.
            let mut v = self.vectors.write().unwrap_or_else(|e| e.into_inner());
            if v.dirty || !dir.join(VECTORS).exists() {
                v.save(&dir.join(VECTORS))?;
            }
        }
        self.manifest().write(&dir.join("MANIFEST"))?;
        let wal = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(dir.join("wal.log"))?;
        preallocate(&wal, 0, WAL_PREALLOC)?;
        wal.sync_all()?;
        self.wal_alloc = WAL_PREALLOC;
        {
            // everything logged so far is now in synced run files
            use std::sync::atomic::Ordering::SeqCst;
            let mut f = self.wal_sync.file.lock().unwrap_or_else(|e| e.into_inner());
            *f = Some(wal.try_clone()?);
            self.wal_sync
                .synced
                .fetch_max(self.wal_sync.logged.load(SeqCst), SeqCst);
        }
        self.wal = Some(BufWriter::new(wal));
        self.wal_bytes = 0;
        for idx in &self.indexes {
            idx.sweep()?;
        }
        self.fts.sweep()?;
        self.geo.sweep()?;
        Ok(())
    }

    /// Checkpoint, then merge each index into a single run file.
    pub fn optimize(&mut self) -> io::Result<()> {
        self.checkpoint()?;
        for idx in &mut self.indexes {
            idx.compact()?;
        }
        self.fts.compact()?;
        self.geo.compact()?;
        self.checkpoint()
    }

    fn nquad_line(&self, q: [Id; 4]) -> String {
        let d = &self.dict;
        if q[G] == DEFAULT_GRAPH {
            format!("{} {} {} .", d.term(q[S]), d.term(q[P]), d.term(q[O]))
        } else {
            format!(
                "{} {} {} {} .",
                d.term(q[S]),
                d.term(q[P]),
                d.term(q[O]),
                d.term(q[G])
            )
        }
    }

    /// The write-ahead log line of a parsed quad: the terms are serialized as they were
    /// parsed, which is exactly the term `intern` mapped them to (the dictionary is keyed
    /// by the term itself), so this is what `nquad_line` of the stored ids would print
    /// without the round trip through the dictionary.
    fn quad_line(out: &mut String, q: &Quad) {
        use std::fmt::Write;
        if let Some(g) = &q.g {
            let _ = write!(out, "{} {} {} {} .", q.s, q.p, q.o, g);
        } else {
            let _ = write!(out, "{} {} {} .", q.s, q.p, q.o);
        }
    }

    fn encode(&mut self, q: &Quad) -> [Id; 4] {
        [
            self.dict.intern(&q.s),
            self.dict.intern(&q.p),
            self.dict.intern(&q.o),
            q.g.as_ref().map_or(DEFAULT_GRAPH, |g| self.dict.intern(g)),
        ]
    }

    fn lookup(&self, q: &Quad) -> Option<[Id; 4]> {
        Some([
            self.dict.lookup(&q.s)?,
            self.dict.lookup(&q.p)?,
            self.dict.lookup(&q.o)?,
            match &q.g {
                Some(g) => self.dict.lookup(g)?,
                None => DEFAULT_GRAPH,
            },
        ])
    }

    fn has(&self, ids: [Id; 4]) -> bool {
        self.indexes[0].contains(&ids)
    }

    fn put(&mut self, ids: [Id; 4]) {
        for (i, order) in ORDERS.iter().enumerate() {
            self.indexes[i].put(permute(ids, order));
        }
        self.changed_subject(ids[S]);
    }

    fn del(&mut self, ids: [Id; 4]) {
        for (i, order) in ORDERS.iter().enumerate() {
            self.indexes[i].delete(permute(ids, order));
        }
        self.changed_subject(ids[S]);
    }

    fn changed_subject(&self, s: Id) {
        if let Some(f) = &self.feed {
            f.lock().unwrap_or_else(|e| e.into_inner()).insert(s);
        }
    }

    /// Apply changes without logging (WAL replay and migration).
    fn apply_unlogged(&mut self, remove: &[Quad], insert: &[Quad]) -> (usize, usize) {
        let saved = self.wal.take();
        let r = self.apply_inner(remove, insert, &mut String::new());
        self.wal = saved;
        r
    }

    fn apply_inner(
        &mut self,
        remove: &[Quad],
        insert: &[Quad],
        log: &mut String,
    ) -> (usize, usize) {
        let logging = self.wal.is_some();
        let (mut r, mut a) = (0, 0);
        // DRed: find what may depend on the removed triples while they still exist.
        let mut overdeleted: HashSet<Triple> = HashSet::new();
        let mut removed_triples: Vec<Triple> = Vec::new();
        if let Some(reasoner) = self.reasoner.take() {
            let gone: Vec<Triple> = remove
                .iter()
                .filter_map(|q| self.lookup(q))
                .filter(|ids| ids[G] != INFERRED && self.has(*ids))
                .map(|ids| [ids[S], ids[P], ids[O]])
                .collect();
            if !gone.is_empty() {
                overdeleted = reasoner.overdelete(self, &gone);
                removed_triples = gone;
            }
            self.reasoner = Some(reasoner);
        }
        for q in remove {
            if let Some(ids) = self.lookup(q) {
                if ids[G] != INFERRED && self.has(ids) {
                    self.del(ids);
                    self.explicit_count -= 1;
                    r += 1;
                    if let Some(c) = &mut self.capture {
                        c.0.push(q.clone());
                    }
                    if logging {
                        log.push_str("- ");
                        Self::quad_line(log, q);
                        log.push('\n');
                    }
                }
            }
        }
        let mut added: Vec<Triple> = Vec::new();
        for q in insert {
            let ids = self.encode(q);
            if self.has(ids) {
                continue;
            }
            self.put(ids);
            self.explicit_count += 1;
            a += 1;
            if let Some(c) = &mut self.capture {
                c.1.push(q.clone());
            }
            if let crate::rdf::Term::Literal(l) = &q.o {
                if self.fts_enabled {
                    self.index_text(ids[O], l);
                }
                self.index_geometry(ids[O], &q.o);
                self.index_vector(ids[O], &q.o);
            }
            added.push([ids[S], ids[P], ids[O]]);
            // An explicit statement supersedes an identical inferred one. There can only be
            // one while anything has been inferred (the counter tracks the INFERRED graph).
            if self.inferred_count > 0 {
                let inferred = [ids[S], ids[P], ids[O], INFERRED];
                if self.has(inferred) {
                    self.del(inferred);
                    self.inferred_count -= 1;
                }
            }
            if logging {
                log.push_str("+ ");
                Self::quad_line(log, q);
                log.push('\n');
            }
        }
        if r > 0 || a > 0 {
            self.version += 1;
            if !self.rank.is_empty() {
                self.rank_outdated = true;
            }
        }
        if self.reasoner.is_some() {
            if self.touches_rule_schema(removed_triples.iter().chain(&added))
                && self.compile_rules()
            {
                // the data's OWL list axioms changed the rules: derive everything again
                self.recompute_inferences();
            } else {
                if r > 0 {
                    self.retract(overdeleted, removed_triples);
                }
                if a > 0 {
                    self.infer(added);
                }
            }
        }
        (r, a)
    }

    /// Whether changed triples belong to OWL axioms that `Reasoner::compile` turns into rules.
    fn touches_rule_schema<'t>(&self, changed: impl Iterator<Item = &'t Triple>) -> bool {
        let Some(base) = &self.reasoner_base else {
            return false;
        };
        let preds = base.schema_predicates();
        !preds.is_empty() && changed.into_iter().any(|t| preds.contains(&t[1]))
    }

    /// Compile the rules of the data's OWL list axioms into the reasoner; true when they
    /// differ from the rules in use.
    fn compile_rules(&mut self) -> bool {
        let Some(base) = self.reasoner_base.clone() else {
            return false;
        };
        let Some(compiled) = Reasoner::compile(&base, self) else {
            return false;
        };
        let changed = self
            .reasoner
            .as_ref()
            .is_none_or(|r| r.signature() != compiled.signature());
        if changed {
            self.reasoner = Some(Arc::new(compiled));
        }
        changed
    }

    /// DRed: drop overdeleted inferences, then re-derive those that still
    /// have support and propagate from them.
    fn retract(&mut self, overdeleted: HashSet<Triple>, removed: Vec<Triple>) {
        let Some(reasoner) = self.reasoner.take() else {
            return;
        };
        for t in &overdeleted {
            let q = [t[0], t[1], t[2], INFERRED];
            if self.has(q) {
                self.del(q);
                self.inferred_count -= 1;
            }
        }
        let mut back = Vec::new();
        for t in overdeleted.iter().chain(removed.iter()) {
            if !Facts::contains(self, t) && reasoner.derivable(self, t) && self.add_inferred(*t) {
                back.push(*t);
            }
        }
        if !back.is_empty() {
            reasoner.run(self, back);
        }
        self.reasoner = Some(reasoner);
    }

    /// Apply a batch of removals then insertions durably. Returns
    /// (removed, inserted) counts of quads that actually changed.
    pub fn apply(&mut self, remove: &[Quad], insert: &[Quad]) -> io::Result<(usize, usize)> {
        let mut log = String::new();
        let res = self.apply_inner(remove, insert, &mut log);
        if let Some(batch) = &mut self.batch {
            // logged when the batch ends; no checkpoint meanwhile, or the run files would
            // hold part of a transaction the log does not have yet
            batch.push_str(&log);
            return Ok(res);
        }
        self.log_commit(&log)?;
        Ok(res)
    }

    /// Start collecting the changes of the following `apply` calls into one logged transaction
    /// (see `end_batch`). False when a batch is already open: it goes on, and the caller must
    /// not end it.
    pub fn begin_batch(&mut self) -> bool {
        if self.batch.is_some() {
            return false;
        }
        self.batch = Some(String::new());
        true
    }

    /// Whether a batch is open.
    pub fn batching(&self) -> bool {
        self.batch.is_some()
    }

    /// End the open batch: with `keep`, log its changes as one transaction; without, forget
    /// them (the caller has already undone them in memory, so the log matches the store).
    pub fn end_batch(&mut self, keep: bool) -> io::Result<()> {
        let Some(log) = self.batch.take() else {
            return Ok(());
        };
        self.log_commit(if keep { &log } else { "" })
    }

    /// Write a transaction's log lines with its commit marker, then checkpoint when the
    /// memtables or the log have grown large.
    fn log_commit(&mut self, log: &str) -> io::Result<()> {
        if let Some(w) = &mut self.wal {
            if !log.is_empty() {
                let mut log = log.to_string();
                self.last_commit = now_millis().max(self.last_commit + 1);
                log.push_str(&format!("c {}\n", self.last_commit));
                w.write_all(log.as_bytes())?;
                w.flush()?;
                if let Some(h) = &mut self.history {
                    h.file.write_all(log.as_bytes())?;
                    h.file.flush()?;
                    h.oldest.get_or_insert(self.last_commit);
                }
                let seq = self
                    .wal_sync
                    .logged
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                if SYNC_COMMITS.load(std::sync::atomic::Ordering::Relaxed) {
                    self.unsynced = Some(seq);
                }
                self.wal_bytes += log.len() as u64;
                if self.wal_bytes + (1 << 18) > self.wal_alloc {
                    // grow the zero-filled region ahead of the next commits
                    // (from the end of what is written: one large commit can pass the region)
                    let f = w.get_ref();
                    let from = self.wal_alloc.max(self.wal_bytes);
                    preallocate(f, from, WAL_PREALLOC)?;
                    f.sync_all()?;
                    self.wal_alloc = from + WAL_PREALLOC;
                }
            }
        }
        let mem: usize = self.indexes.iter().map(|i| i.mem_len()).sum();
        if self.dir.is_some() && (mem > MEM_LIMIT || self.wal_bytes > WAL_LIMIT) {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// (version, whether memtables or the log hold anything a checkpoint would fold away):
    /// the server checkpoints a store whose version stays the same while it is dirty.
    pub fn idle_state(&self) -> (u64, bool) {
        let dirty = self.dir.is_some()
            && (self.wal_bytes > 0
                || self.indexes.iter().any(|i| i.mem_len() > 0)
                || self.fts.mem_len() > 0);
        (self.version, dirty)
    }

    /// The files a query reads (dictionary and index runs), most used first, for warming
    /// the operating system's cache after a start.
    pub fn warm_files(&self) -> Vec<PathBuf> {
        let Some(dir) = &self.dir else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = ["terms.hash", "terms.idx", "terms.dat"]
            .iter()
            .map(|f| dir.join(f))
            .collect();
        for idx in &self.indexes {
            out.extend(idx.run_paths());
        }
        out
    }

    /// Offline bulk loading (`graphrd load`): add many statements without the write-ahead
    /// log. Each chunk's terms are collected on its own thread, only distinct terms go
    /// through the dictionary, and each index gets the new statements as one sorted run
    /// written straight to disk; the new manifest written at the end makes them live at once,
    /// so a crash during the load leaves the store as it was. Blank node labels are kept
    /// consistent across calls through `bnodes` (label as written -> stored label).
    /// Returns the number of statements added.
    pub fn bulk_insert(
        &mut self,
        chunks: Vec<Vec<Quad>>,
        bnodes: &mut HashMap<String, String>,
    ) -> io::Result<usize> {
        use std::thread;
        // start from empty memtables, so the new runs are the newest sources
        self.checkpoint()?;
        let fts = self.fts_enabled;
        // 1. per chunk: distinct terms and statements over local term numbers
        struct Local {
            terms: Vec<Term>,
            quads: Vec<[u32; 4]>,
            literal_objects: Vec<u32>,
        }
        const NO_GRAPH: u32 = u32::MAX;
        let locals: Vec<Local> = thread::scope(|sc| {
            let handles: Vec<_> = chunks
                .into_iter()
                .map(|chunk| {
                    sc.spawn(move || {
                        let mut map: HashMap<Term, u32> = HashMap::with_capacity(chunk.len());
                        let mut terms = Vec::new();
                        let mut quads = Vec::with_capacity(chunk.len());
                        let mut local = |t: Term, terms: &mut Vec<Term>| -> u32 {
                            if let Some(&i) = map.get(&t) {
                                return i;
                            }
                            let i = terms.len() as u32;
                            terms.push(t.clone());
                            map.insert(t, i);
                            i
                        };
                        let mut literal_objects = Vec::new();
                        for q in chunk {
                            let lit = q.o.is_literal()
                                && (fts
                                    || crate::geo::is_geometry_literal(&q.o)
                                    || crate::vector::is_vector_literal(&q.o));
                            let s = local(q.s, &mut terms);
                            let p = local(q.p, &mut terms);
                            let o = local(q.o, &mut terms);
                            if lit {
                                literal_objects.push(o);
                            }
                            let g = q.g.map_or(NO_GRAPH, |g| local(g, &mut terms));
                            quads.push([s, p, o, g]);
                        }
                        Local {
                            terms,
                            quads,
                            literal_objects,
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("bulk load worker"))
                .collect()
        });
        // 2. the dictionary, one distinct term at a time
        let mut globals: Vec<Vec<Id>> = Vec::with_capacity(locals.len());
        for l in &locals {
            let renamed: Vec<Term>;
            let terms: &[Term] = if l.terms.iter().any(|t| matches!(t, Term::Blank(_))) {
                renamed = l
                    .terms
                    .iter()
                    .map(|t| match t {
                        Term::Blank(b) => Term::Blank(
                            bnodes
                                .entry(b.clone())
                                .or_insert_with(crate::rdf::fresh_blank_id)
                                .clone(),
                        ),
                        t => t.clone(),
                    })
                    .collect();
                &renamed
            } else {
                &l.terms
            };
            globals.push(self.dict.intern_all(terms.iter())?);
        }
        // 3. statements over dictionary ids, sorted and distinct (SPOG order)
        let mut all: Vec<[Id; 4]> = thread::scope(|sc| {
            let handles: Vec<_> = locals
                .iter()
                .zip(&globals)
                .map(|(l, ids)| {
                    sc.spawn(move || {
                        l.quads
                            .iter()
                            .map(|q| {
                                [
                                    ids[q[0] as usize],
                                    ids[q[1] as usize],
                                    ids[q[2] as usize],
                                    if q[3] == NO_GRAPH {
                                        DEFAULT_GRAPH
                                    } else {
                                        ids[q[3] as usize]
                                    },
                                ]
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            let mut out = Vec::new();
            for h in handles {
                out.extend(h.join().expect("bulk load worker"));
            }
            out
        });
        all.sort_unstable();
        all.dedup();
        // 4. leave out what the store already has
        if self.explicit_count + self.inferred_count > 0 {
            let index = &self.indexes[0];
            let parts = thread::available_parallelism().map_or(4, |n| n.get());
            let size = all.len().div_ceil(parts).max(1);
            all = thread::scope(|sc| {
                let handles: Vec<_> = all
                    .chunks(size)
                    .map(|part| {
                        sc.spawn(move || {
                            part.iter()
                                .filter(|q| !index.contains(q))
                                .copied()
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().expect("bulk load worker"))
                    .collect()
            });
            // an explicit statement supersedes an identical inferred one
            if self.inferred_count > 0 {
                for q in &all {
                    let inferred = [q[S], q[P], q[O], INFERRED];
                    if self.has(inferred) {
                        self.del(inferred);
                        self.inferred_count -= 1;
                    }
                }
                for idx in &mut self.indexes {
                    idx.flush()?;
                }
            }
        }
        // the history gets the load as one transaction, before the manifest makes it live (a
        // crash in between leaves an entry that undoes nothing)
        if self.history.is_some() && !all.is_empty() {
            self.last_commit = now_millis().max(self.last_commit + 1);
            let mut text = String::new();
            for q in &all {
                text.push_str("+ ");
                text.push_str(&self.nquad_line(*q));
                text.push('\n');
            }
            text.push_str(&format!("c {}\n", self.last_commit));
            let at = self.last_commit;
            let h = self.history.as_mut().expect("checked above");
            h.file.write_all(text.as_bytes())?;
            h.file.flush()?;
            h.file.get_ref().sync_all()?;
            h.oldest.get_or_insert(at);
        }
        if let Some(f) = &self.feed {
            f.lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(all.iter().map(|q| q[S]));
        }
        // 5. one sorted run per index, built and written in parallel
        let added = all.len();
        thread::scope(|sc| -> io::Result<()> {
            let all = &all;
            let handles: Vec<_> = self
                .indexes
                .iter_mut()
                .zip(ORDERS.iter())
                .map(|(index, order)| {
                    sc.spawn(move || {
                        let mut keys: Vec<Key> = all.iter().map(|q| permute(*q, order)).collect();
                        if order != &ORDERS[0] {
                            keys.sort_unstable();
                        }
                        index.add_sorted(&keys)
                    })
                })
                .collect();
            for h in handles {
                h.join().expect("bulk load worker")?;
            }
            Ok(())
        })?;
        self.explicit_count += added as u64;
        if added > 0 {
            self.version += 1;
            if !self.rank.is_empty() {
                self.rank_outdated = true;
            }
        }
        let mut seen = HashSet::new();
        for (l, ids) in locals.iter().zip(&globals) {
            for &o in &l.literal_objects {
                let id = ids[o as usize];
                if seen.insert(id) {
                    let term = &l.terms[o as usize];
                    if let (true, Term::Literal(lit)) = (fts, term) {
                        self.index_text(id, lit);
                    }
                    self.index_geometry(id, term);
                    self.index_vector(id, term);
                }
            }
        }
        if self.reasoner.is_some() && added > 0 {
            let triples: Vec<Triple> = all.iter().map(|q| [q[S], q[P], q[O]]).collect();
            if self.touches_rule_schema(triples.iter()) && self.compile_rules() {
                self.recompute_inferences();
            } else {
                self.infer(triples);
            }
        }
        // the manifest naming the new runs (and the dictionary's new length) commits the load
        self.checkpoint()?;
        Ok(added)
    }

    /// The log position the current writer must see fsynced before acknowledging its
    /// commit (see `WalSync`). Taken while still holding the store lock.
    /// Waits until every transaction logged so far is on disk.
    pub fn sync_log(&self) -> io::Result<()> {
        if self.dir.is_none() {
            return Ok(());
        }
        self.wal_sync.wait(
            self.wal_sync
                .logged
                .load(std::sync::atomic::Ordering::SeqCst),
        )
    }

    /// The pending fsync, for the write guard to wait out after releasing the lock.
    pub fn take_unsynced(&mut self) -> Option<(std::sync::Arc<WalSync>, u64)> {
        self.unsynced.take().map(|seq| (self.wal_sync.clone(), seq))
    }

    /// Start recording applied changes (for validation and rollback).
    pub fn start_capture(&mut self) {
        self.capture = Some((Vec::new(), Vec::new()));
    }

    /// Stop recording; returns (removed, inserted) explicit quads.
    /// Whether changes are being recorded.
    pub fn capturing(&self) -> bool {
        self.capture.is_some()
    }

    /// How much has been recorded so far (removed, inserted).
    pub fn capture_mark(&self) -> (usize, usize) {
        self.capture
            .as_ref()
            .map_or((0, 0), |(r, i)| (r.len(), i.len()))
    }

    /// What was recorded after `mark` (removed, inserted).
    pub fn capture_since(&self, mark: (usize, usize)) -> (Vec<Quad>, Vec<Quad>) {
        self.capture
            .as_ref()
            .map_or_else(Default::default, |(r, i)| {
                (
                    r[mark.0.min(r.len())..].to_vec(),
                    i[mark.1.min(i.len())..].to_vec(),
                )
            })
    }

    /// Forgets what was recorded after `mark`.
    pub fn capture_truncate(&mut self, mark: (usize, usize)) {
        if let Some((r, i)) = &mut self.capture {
            r.truncate(mark.0);
            i.truncate(mark.1);
        }
    }

    /// What a transaction changed, when it was captured (for SHACL and undo).
    pub fn take_capture(&mut self) -> (Vec<Quad>, Vec<Quad>) {
        self.capture.take().unwrap_or_default()
    }

    /// Add quads; returns how many were new.
    pub fn insert(&mut self, quads: &[Quad]) -> io::Result<usize> {
        self.apply(&[], quads).map(|(_, a)| a)
    }

    /// Remove every explicit statement of a graph; returns how many went.
    pub fn clear_graph(&mut self, g: Option<Id>) -> io::Result<usize> {
        let ids: Vec<[Id; 4]> = self.explicit_matching(None, None, None, g).collect();
        let quads: Vec<Quad> = ids.into_iter().map(|q| self.decode(q)).collect();
        self.apply(&quads, &[]).map(|(r, _)| r)
    }

    // ------------------------------------------------------------ inference

    /// Select the inference ruleset. Materialization is redone only when it
    /// differs from the ruleset the stored inferences were made with.
    pub fn set_ruleset(&mut self, ruleset: &str, same_as: bool) {
        let wanted = (ruleset.to_string(), same_as);
        if self.ruleset == wanted {
            return;
        }
        self.ruleset = wanted;
        self.reasoner_base = Reasoner::new(ruleset, same_as, &self.dict).map(Arc::new);
        self.reasoner = self.reasoner_base.clone();
        self.compile_rules();
        self.recompute_inferences();
        if let Err(e) = self.checkpoint() {
            eprintln!("graphrd: checkpoint after ruleset change failed: {e}");
        }
    }

    fn clear_inferred(&mut self) {
        loop {
            let batch: Vec<[Id; 4]> = self
                .quads_matching(None, None, None, Some(INFERRED))
                .take(REASON_CHUNK)
                .collect();
            if batch.is_empty() {
                break;
            }
            for q in batch {
                self.del(q);
            }
        }
        self.inferred_count = 0;
    }

    /// Full rematerialization (after deletions or a ruleset change), streaming
    /// the explicit statements through the reasoner in chunks.
    fn recompute_inferences(&mut self) {
        self.clear_inferred();
        let Some(reasoner) = self.reasoner.take() else {
            return;
        };
        let mut from: Key = [0; 4];
        loop {
            let chunk: Vec<Key> = self.indexes[0]
                .range(from, [Id::MAX; 4])
                .take(REASON_CHUNK)
                .collect();
            let Some(last) = chunk.last().copied() else {
                break;
            };
            let mut seen = HashSet::new();
            let delta: Vec<Triple> = chunk
                .iter()
                .filter(|k| k[3] != INFERRED)
                .map(|k| [k[0], k[1], k[2]])
                .filter(|t| seen.insert(*t))
                .collect();
            reasoner.run(self, delta);
            if chunk.len() < REASON_CHUNK {
                break;
            }
            from = next_key(last);
        }
        self.reasoner = Some(reasoner);
    }

    fn infer(&mut self, added: Vec<Triple>) {
        let Some(reasoner) = self.reasoner.take() else {
            return;
        };
        reasoner.run(self, added);
        self.reasoner = Some(reasoner);
    }

    /// The number of inferred statements.
    pub fn inferred_len(&self) -> usize {
        self.inferred_count as usize
    }

    // ------------------------------------------------------------ RDF Rank

    /// PageRank over explicit links between resources, normalized so the
    /// most important node scores 1.
    pub fn compute_rank(&mut self) -> usize {
        let mut out_links: std::collections::HashMap<Id, Vec<Id>> =
            std::collections::HashMap::new();
        let mut nodes: std::collections::HashMap<Id, f64> = std::collections::HashMap::new();
        let lit = |st: &Store, id: Id| st.dict.term(id).is_literal();
        let mut literal_cache: std::collections::HashMap<Id, bool> =
            std::collections::HashMap::new();
        for q in self.explicit_matching(None, None, None, None) {
            let is_lit = *literal_cache.entry(q[O]).or_insert_with(|| lit(self, q[O]));
            nodes.entry(q[S]).or_insert(0.0);
            if !is_lit {
                nodes.entry(q[O]).or_insert(0.0);
                out_links.entry(q[S]).or_default().push(q[O]);
            }
        }
        let n = nodes.len().max(1) as f64;
        let d = 0.85;
        let mut rank: std::collections::HashMap<Id, f64> =
            nodes.keys().map(|&k| (k, 1.0 / n)).collect();
        for _ in 0..20 {
            let dangling: f64 = rank
                .iter()
                .filter(|(k, _)| !out_links.contains_key(k))
                .map(|(_, v)| v)
                .sum();
            let base = (1.0 - d) / n + d * dangling / n;
            let mut next: std::collections::HashMap<Id, f64> =
                nodes.keys().map(|&k| (k, base)).collect();
            for (s, targets) in &out_links {
                let share = d * rank[s] / targets.len() as f64;
                for t in targets {
                    *next.get_mut(t).unwrap() += share;
                }
            }
            rank = next;
        }
        let max = rank
            .values()
            .cloned()
            .fold(0.0, f64::max)
            .max(f64::MIN_POSITIVE);
        self.rank = Arc::new(
            rank.into_iter()
                .map(|(k, v)| (k, (v / max) as f32))
                .collect(),
        );
        self.rank_outdated = false;
        self.save_rank();
        self.rank.len()
    }

    /// Distinct objects of predicate `p` (jumping through POSG).
    pub fn distinct_objects(&self, p: Id) -> Vec<Id> {
        let mut out = Vec::new();
        let mut from = 0;
        loop {
            let next = self.indexes[1]
                .range([p, from, 0, 0], [p, Id::MAX, Id::MAX, Id::MAX])
                .next();
            let Some(k) = next else { break };
            out.push(k[1]);
            if k[1] == Id::MAX {
                break;
            }
            from = k[1] + 1;
        }
        out
    }

    /// How many changes were made (an MVCC clock for snapshots).
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Distinct IRIs used as subject, predicate or class, for autocomplete.
    pub fn resource_iris(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut add = |id: Id, st: &Store, out: &mut Vec<String>| {
            if seen.insert(id) {
                if let crate::rdf::Term::Iri(i) = st.dict.term(id) {
                    out.push(i);
                }
            }
        };
        // jump through distinct subjects (SPOG) and predicates (POSG)
        for idx in [0usize, 1] {
            let mut from = 1;
            loop {
                let next = self.indexes[idx]
                    .range([from, 0, 0, 0], [Id::MAX; 4])
                    .next();
                let Some(k) = next else { break };
                if k[0] >= INFERRED {
                    break;
                }
                add(k[0], self, &mut out);
                from = k[0] + 1;
            }
        }
        out
    }

    /// A resource's RDF Rank score.
    pub fn rank_of(&self, id: Id) -> Option<f32> {
        self.rank.get(&id).copied()
    }

    /// Every ranked resource with its score.
    pub fn ranked(&self) -> impl Iterator<Item = (Id, f32)> + '_ {
        self.rank.iter().map(|(k, v)| (*k, *v))
    }

    /// EMPTY, COMPUTED or OUTDATED.
    pub fn rank_status(&self) -> &'static str {
        match (self.rank.is_empty(), self.rank_outdated) {
            (true, _) => "EMPTY",
            (false, false) => "COMPUTED",
            (false, true) => "OUTDATED",
        }
    }

    fn save_rank(&self) {
        let Some(dir) = &self.dir else { return };
        let mut buf = Vec::with_capacity(self.rank.len() * 8);
        for (k, v) in self.rank.iter() {
            buf.extend_from_slice(&k.to_le_bytes());
            buf.extend_from_slice(&v.to_le_bytes());
        }
        let _ = fs::write(dir.join("rank.bin"), buf);
    }

    fn load_rank(&mut self) {
        let Some(dir) = &self.dir else { return };
        if let Ok(buf) = fs::read(dir.join("rank.bin")) {
            self.rank = Arc::new(
                buf.chunks_exact(8)
                    .map(|c| {
                        (
                            u32::from_le_bytes(c[..4].try_into().unwrap()),
                            f32::from_le_bytes(c[4..].try_into().unwrap()),
                        )
                    })
                    .collect(),
            );
        }
    }

    // ------------------------------------------------------------ full-text search

    fn index_text(&mut self, literal: Id, l: &crate::rdf::Literal) {
        let tokens = fts::tokenize(&l.value);
        let words = tokens.len() as u32;
        let stats = [fts::STATS, literal, words, 1];
        if self.fts.contains(&stats) {
            return; // indexed already
        }
        if self.fts_vocab.len() > 1_000_000 {
            self.fts_vocab.clear();
        }
        let (vocab, fts) = (&mut self.fts_vocab, &mut self.fts);
        // a word seen for the first time: stem it and write its vocabulary trigrams
        let mut stems = |tok: &str| {
            if let Some(k) = vocab.get(tok) {
                return *k;
            }
            *vocab.entry(tok.to_string()).or_insert_with(|| {
                for e in fts::vocab_entries(tok) {
                    fts.put(e);
                }
                fts::stem_keys(tok)
            })
        };
        for key in fts::posting_keys_of(&tokens, fts::lang_of(l.lang.as_deref()), &mut stems) {
            self.fts.put([key, literal, 0, 0]);
        }
        self.fts.put(stats);
        if let Some((docs, total)) = self.fts_stats.lock().unwrap().as_mut() {
            *docs += 1;
            *total += words as u64;
        }
    }

    /// Whether the full-text index is on.
    pub fn fts_enabled(&self) -> bool {
        self.fts_enabled
    }

    /// Turn the full-text index on (indexing every literal already stored)
    /// or off (dropping it).
    pub fn set_fts(&mut self, enabled: bool) {
        if enabled == self.fts_enabled {
            return;
        }
        self.fts_enabled = enabled;
        self.fts.clear();
        self.fts_vocab.clear();
        *self.fts_stats.lock().unwrap() = enabled.then_some((0, 0));
        if enabled {
            // Every distinct object, in OSPG order; index the literals.
            let mut from = 1;
            loop {
                let next = self.indexes[2].range([from, 0, 0, 0], [Id::MAX; 4]).next();
                let Some(k) = next else { break };
                let o = k[0];
                if o >= INFERRED {
                    break;
                }
                if let crate::rdf::Term::Literal(l) = self.dict.term(o) {
                    self.index_text(o, &l);
                }
                from = o + 1;
            }
        }
        if let Err(e) = self.checkpoint() {
            eprintln!("graphrd: checkpoint after full-text change failed: {e}");
        }
    }

    // ------------------------------------------------------------ history (time travel)

    /// Keep the change history for `retention` milliseconds (0: for ever), or stop keeping
    /// it (None: the history file is deleted). Needs a store on disk.
    pub fn set_history(&mut self, retention: Option<i64>) -> io::Result<()> {
        let Some(dir) = self.dir.clone() else {
            return Ok(());
        };
        match retention {
            None => {
                if self.history.take().is_some() || dir.join(HISTORY).exists() {
                    fs::remove_file(dir.join(HISTORY))?;
                }
                Ok(())
            }
            Some(ms) => {
                if self.history.is_none() {
                    self.open_history(ms)?;
                }
                if let Some(h) = &mut self.history {
                    h.retention = ms.max(0);
                }
                self.prune_history()
            }
        }
    }

    /// The kept history: (from when it is complete, retention in milliseconds), or None.
    pub fn history_window(&self) -> Option<(i64, i64)> {
        let h = self.history.as_ref()?;
        Some((self.history_start().unwrap_or(0), h.retention))
    }

    fn history_start(&self) -> Option<i64> {
        let mut first = String::new();
        let f = File::open(self.dir.as_ref()?.join(HISTORY)).ok()?;
        io::BufRead::read_line(&mut io::BufReader::new(f), &mut first).ok()?;
        first.strip_prefix("s ")?.trim().parse().ok()
    }

    fn open_history(&mut self, retention: i64) -> io::Result<()> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let path = dir.join(HISTORY);
        let mut oldest = None;
        if path.exists() {
            let text = read_log(&path)?;
            for line in text.lines() {
                if let Some(Some(at)) = commit_line(line) {
                    oldest = Some(at);
                    break;
                }
            }
            if let Some(at) = text.lines().rev().find_map(|l| commit_line(l).flatten()) {
                self.last_commit = self.last_commit.max(at);
            }
            // a crash can leave a torn transaction at the end; cut it off, or the next one
            // appended would continue its unfinished line
            let (mut keep, mut pos) = (0, 0);
            for line in text.split_inclusive('\n') {
                pos += line.len();
                let l = line.trim_end_matches(['\n', '\r']);
                if line.ends_with('\n') && (l.starts_with("s ") || commit_line(l).is_some()) {
                    keep = pos;
                }
            }
            if keep > 0 && (keep as u64) < fs::metadata(&path)?.len() {
                let f = OpenOptions::new().write(true).open(&path)?;
                f.set_len(keep as u64)?;
                f.sync_all()?;
            }
        } else {
            // complete from now on: written before any transaction can reach it
            let tmp = path.with_extension("tmp");
            fs::write(&tmp, format!("s {}\n", now_millis()))?;
            OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
            fs::rename(&tmp, &path)?;
        }
        let file = OpenOptions::new().append(true).open(&path)?;
        self.history = Some(History {
            file: BufWriter::new(file),
            retention,
            oldest,
        });
        Ok(())
    }

    /// Append the transactions of a write-ahead log text that the history does not have yet
    /// (those committed after its last one).
    fn history_catch_up(&mut self, wal: &str) -> io::Result<()> {
        let Some(h) = &mut self.history else {
            return Ok(());
        };
        let text = wal;
        let last = read_log(
            &self
                .dir
                .as_ref()
                .expect("history needs a directory")
                .join(HISTORY),
        )?
        .lines()
        .rev()
        .find_map(|l| commit_line(l).flatten())
        .unwrap_or(i64::MIN);
        let mut txn = String::new();
        for line in text.lines() {
            txn.push_str(line);
            txn.push('\n');
            if let Some(at) = commit_line(line) {
                if at.is_some_and(|at| at > last) {
                    h.file.write_all(txn.as_bytes())?;
                    h.oldest.get_or_insert(at.unwrap_or(0));
                }
                txn.clear();
            }
        }
        h.file.flush()
    }

    /// Drop the transactions older than the retention, once there are some to drop (allowing a
    /// tenth of the retention, at least a minute, past it, so the file is not rewritten at every
    /// checkpoint).
    fn prune_history(&mut self) -> io::Result<()> {
        let Some(h) = &mut self.history else {
            return Ok(());
        };
        let Some(oldest) = h.oldest else {
            return Ok(());
        };
        if h.retention == 0 {
            return Ok(());
        }
        let cutoff = now_millis() - h.retention;
        if oldest >= cutoff - (h.retention / 10).max(60_000) {
            return Ok(());
        }
        h.file.flush()?;
        let path = self
            .dir
            .as_ref()
            .expect("history needs a directory")
            .join(HISTORY);
        let text = read_log(&path)?;
        let mut out = format!("s {cutoff}\n");
        let mut txn = String::new();
        let mut new_oldest = None;
        for line in text.lines().filter(|l| !l.starts_with("s ")) {
            txn.push_str(line);
            txn.push('\n');
            if let Some(at) = commit_line(line) {
                let at = at.unwrap_or(0);
                if at >= cutoff {
                    new_oldest.get_or_insert(at);
                    out.push_str(&txn);
                }
                txn.clear();
            }
        }
        let tmp = path.with_extension("tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(out.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        h.file = BufWriter::new(OpenOptions::new().append(true).open(&path)?);
        h.oldest = new_oldest;
        Ok(())
    }

    /// The store as it was at `at` (Unix milliseconds): this store with every transaction
    /// committed after `at` undone, newest first, in memory. Works on a snapshot: the history
    /// file may also hold transactions committed after the snapshot was taken, and undoing
    /// those leaves the snapshot's state unchanged (each undo restores a statement's state from
    /// before the earliest undone transaction that touched it).
    pub fn as_of(&self, at: i64) -> Result<Store, String> {
        let mut past = self.fork();
        for (added, quads) in self.undo_after(at)? {
            if added {
                past.apply_unlogged(&quads, &[]);
            } else {
                past.apply_unlogged(&[], &quads);
            }
        }
        Ok(past)
    }

    /// What undoes the transactions committed after `at`: runs of (was added, statements),
    /// in the order to apply them (newest change first).
    fn undo_after(&self, at: i64) -> Result<Vec<(bool, Vec<Quad>)>, String> {
        let dir = self
            .dir
            .as_ref()
            .ok_or("time travel needs a repository on disk")?;
        let text = read_log(&dir.join(HISTORY)).map_err(|_| {
            "time travel: this repository keeps no history (set historyRetention)".to_string()
        })?;
        let start: i64 = text
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("s "))
            .and_then(|v| v.trim().parse().ok())
            .ok_or("time travel: damaged history file")?;
        if at < start {
            return Err(format!(
                "time travel: {at} is before the kept history, which starts at {start}"
            ));
        }
        // the changes to undo, oldest first: (added, statement lines)
        let mut undo: Vec<(bool, &str)> = Vec::new();
        let mut txn: Vec<(bool, &str)> = Vec::new();
        for line in text.lines().skip(1) {
            if let Some(t) = commit_line(line) {
                if t.unwrap_or(0) > at {
                    undo.append(&mut txn);
                }
                txn.clear();
            } else if let Some(q) = line.strip_prefix("+ ") {
                txn.push((true, q));
            } else if let Some(q) = line.strip_prefix("- ") {
                txn.push((false, q));
            }
        }
        // newest first, a run of same-kind changes at a time
        let mut out = Vec::new();
        let mut i = undo.len();
        while i > 0 {
            let added = undo[i - 1].0;
            let mut j = i;
            while j > 0 && undo[j - 1].0 == added {
                j -= 1;
            }
            let text: String = undo[j..i]
                .iter()
                .rev()
                .map(|(_, q)| format!("{q}\n"))
                .collect();
            let quads = turtle::parse_nquads_raw(&text)
                .map_err(|e| format!("time travel: damaged history file: {e}"))?;
            out.push((added, quads));
            i = j;
        }
        Ok(out)
    }

    /// Point-in-time restore: return this store to its state at `at`, durably, and cut its
    /// history there (the undone transactions leave it; the undo itself is not recorded).
    /// Returns the number of statements (removed, added).
    pub fn roll_back_to(&mut self, at: i64) -> Result<(usize, usize), String> {
        let undo = self.undo_after(at)?;
        let history = self.history.take();
        let mut changed = (0, 0);
        let mut result = Ok(());
        for (added, quads) in undo {
            let r = if added {
                self.apply(&quads, &[])
            } else {
                self.apply(&[], &quads)
            };
            match r {
                Ok((r, a)) => {
                    changed.0 += r;
                    changed.1 += a;
                }
                Err(e) => {
                    result = Err(e.to_string());
                    break;
                }
            }
        }
        self.history = history;
        result?;
        self.sync_log().map_err(|e| e.to_string())?;
        // the history keeps the transactions up to `at`
        let path = self
            .dir
            .as_ref()
            .expect("checked by undo_after")
            .join(HISTORY);
        let cut = (|| -> io::Result<()> {
            if let Some(h) = &mut self.history {
                h.file.flush()?;
            }
            let text = read_log(&path)?;
            let mut out = String::new();
            let mut txn = String::new();
            for line in text.lines() {
                if line.starts_with("s ") {
                    out.push_str(line);
                    out.push('\n');
                    continue;
                }
                txn.push_str(line);
                txn.push('\n');
                if let Some(t) = commit_line(line) {
                    if t.unwrap_or(0) <= at {
                        out.push_str(&txn);
                    }
                    txn.clear();
                }
            }
            let tmp = path.with_extension("tmp");
            {
                let mut f = File::create(&tmp)?;
                f.write_all(out.as_bytes())?;
                f.sync_all()?;
            }
            fs::rename(&tmp, &path)?;
            if let Some(h) = &mut self.history {
                h.file = BufWriter::new(OpenOptions::new().append(true).open(&path)?);
            }
            Ok(())
        })();
        cut.map_err(|e| e.to_string())?;
        self.checkpoint().map_err(|e| e.to_string())?;
        Ok(changed)
    }

    /// A copy of this store that takes changes in memory only, leaving this one as it is.
    fn fork(&self) -> Store {
        Store {
            dict: self.dict.clone(),
            indexes: self.indexes.each_ref().map(|i| i.fork()),
            dir: None,
            wal: None,
            wal_bytes: 0,
            wal_alloc: 0,
            reasoner: self.reasoner.clone(),
            reasoner_base: self.reasoner_base.clone(),
            ruleset: self.ruleset.clone(),
            explicit_count: self.explicit_count,
            inferred_count: self.inferred_count,
            fts: self.fts.fork(),
            fts_enabled: self.fts_enabled,
            fts_stats: self.fts_stats.clone(),
            fts_vocab: HashMap::new(),
            geo: self.geo.fork(),
            vectors: self.vectors.clone(),
            connectors: self.connectors.clone(),
            feed: None,
            capture: None,
            rank: self.rank.clone(),
            rank_outdated: self.rank_outdated,
            version: self.version,
            wal_sync: Default::default(),
            union_default_graph: self.union_default_graph,
            unsynced: None,
            last_commit: self.last_commit,
            history: None,
            batch: None,
        }
    }

    // ------------------------------------------------------------ spatial index

    /// Index a geometry literal (WKT or GeoJSON) under the cells around its bounding box;
    /// other terms are ignored. Like the full-text postings, entries of literals that are no
    /// longer used stay: candidates are always checked against the statements.
    fn index_geometry(&mut self, literal: Id, t: &Term) {
        if !crate::geo::is_geometry_literal(t) {
            return;
        }
        let Some(b) = crate::geo::literal_bbox(t) else {
            return;
        };
        for (z, level) in crate::geo::index_cells(b) {
            self.geo.put([(z >> 32) as u32, z as u32, level, literal]);
        }
    }

    /// Index a `g:vector` literal for nearest-neighbour search; other terms are ignored.
    fn index_vector(&mut self, literal: Id, t: &Term) {
        if !crate::vector::is_vector_literal(t) {
            return;
        }
        if let Some(v) = crate::vector::parse(t) {
            self.vectors
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(literal, v);
        }
    }

    /// Index every vector literal (a store written before the vector index, or whose graph file
    /// is missing).
    fn index_all_vectors(&mut self) {
        let mut from = 1;
        loop {
            let next = self.indexes[2].range([from, 0, 0, 0], [Id::MAX; 4]).next();
            let Some(k) = next else { break };
            let o = k[0];
            if o >= INFERRED {
                break;
            }
            let t = self.dict.term(o);
            self.index_vector(o, &t);
            from = o + 1;
        }
    }

    /// The `k` vector literals in the store most similar to `q` (cosine): (literal id,
    /// similarity), best first.
    pub fn nearest_vectors(&self, q: &[f32], k: usize) -> Vec<(Id, f32)> {
        let v = self.vectors.read().unwrap_or_else(|e| e.into_inner());
        v.search(q, k, &|id| {
            self.quads_matching(None, None, Some(id), None)
                .next()
                .is_some()
        })
    }

    /// Build the spatial index of a store written before it existed.
    fn index_all_geometries(&mut self) {
        let mut from = 1;
        loop {
            let next = self.indexes[2].range([from, 0, 0, 0], [Id::MAX; 4]).next();
            let Some(k) = next else { break };
            let o = k[0];
            if o >= INFERRED {
                break;
            }
            let t = self.dict.term(o);
            self.index_geometry(o, &t);
            from = o + 1;
        }
    }

    /// Geometry literals whose bounding box may meet `b` (CRS84 degrees): a superset of those
    /// that do, in no particular order.
    pub fn geo_candidates(&self, b: crate::geo::Bbox) -> Vec<Id> {
        let (ranges, coarse) = crate::geo::query_cells(b);
        let mut out = HashSet::new();
        for (from, to) in ranges {
            let last = to - 1;
            out.extend(
                self.geo
                    .range(
                        [(from >> 32) as u32, from as u32, 0, 0],
                        [(last >> 32) as u32, last as u32, Id::MAX, Id::MAX],
                    )
                    .map(|k| k[3]),
            );
        }
        for (z, level) in coarse {
            let (hi, lo) = ((z >> 32) as u32, z as u32);
            out.extend(
                self.geo
                    .range([hi, lo, level, 0], [hi, lo, level, Id::MAX])
                    .map(|k| k[3]),
            );
        }
        out.into_iter().collect()
    }

    /// Indexed literals and their average length in words.
    fn fts_corpus(&self) -> fts::Corpus {
        let mut stats = self.fts_stats.lock().unwrap();
        let (docs, total) = *stats.get_or_insert_with(|| {
            self.fts
                .range(
                    [fts::STATS, 0, 0, 0],
                    [fts::STATS, Id::MAX, Id::MAX, Id::MAX],
                )
                .filter(|k| k[3] == 1)
                .fold((0, 0), |(d, t), k| (d + 1, t + k[2] as u64))
        });
        fts::Corpus {
            docs: docs.max(1) as f64,
            avg_len: total as f64 / docs.max(1) as f64,
        }
    }

    /// The literals of a posting key.
    fn fts_postings(&self, key: u32) -> impl Iterator<Item = Id> + '_ {
        self.fts
            .range([key, 0, 0, 0], [key, Id::MAX, 0, 0])
            .filter(|k| k[2] == 0 && k[3] == 0)
            .map(|k| k[1])
    }

    /// The literals a clause can match (a superset: candidates are verified against the text).
    fn fts_candidates(&self, c: &fts::Clause) -> HashSet<Id> {
        match c.plan() {
            // read only the rarest key's postings; probe the others per candidate
            fts::Plan::All(keys) => {
                let rarest = *keys
                    .iter()
                    .min_by_key(|&&k| self.fts_postings(k).take(1024).count())
                    .expect("a clause has at least one key");
                let mut set: HashSet<Id> = self.fts_postings(rarest).collect();
                for &k in keys.iter().filter(|&&k| k != rarest) {
                    set.retain(|&id| self.fts.contains(&[k, id, 0, 0]));
                }
                set
            }
            fts::Plan::Any(keys) => keys.iter().flat_map(|&k| self.fts_postings(k)).collect(),
            fts::Plan::AtLeast(grams, need) => {
                // words of the vocabulary sharing enough trigrams, then their literals
                let mut words: HashMap<u32, usize> = HashMap::new();
                for &g in &grams {
                    for k in self
                        .fts
                        .range([g, 0, 0, 2], [g, u32::MAX, 0, 2])
                        .filter(|k| k[3] == 2)
                    {
                        *words.entry(k[1]).or_default() += 1;
                    }
                }
                words
                    .into_iter()
                    .filter(|&(_, n)| n >= need)
                    .flat_map(|(w, _)| self.fts_postings(w).collect::<Vec<_>>())
                    .collect()
            }
        }
    }

    /// Each positive clause's document frequency, in `FtsQuery::clauses` order.
    fn fts_dfs(&self, q: &fts::FtsQuery) -> Vec<f64> {
        q.clauses()
            .map(|c| self.fts_candidates(c).len() as f64)
            .collect()
    }

    /// A full-text query with what scoring it needs: its typo-tolerant forms, the corpus
    /// statistics, and (filled on first use) their document frequencies.
    pub fn fts_prepare(&self, query: &str) -> FtsPrepared {
        let forms = fts::parse_query(query)
            .forms()
            .into_iter()
            .map(|q| (q, Default::default()))
            .collect();
        FtsPrepared {
            forms,
            corpus: self.fts_enabled.then(|| self.fts_corpus()),
        }
    }

    /// The BM25 score of a literal for a prepared query, by the first of its forms that
    /// matches (the query itself, then its typo-tolerant fallbacks).
    pub fn fts_score(&self, p: &FtsPrepared, l: &crate::rdf::Literal) -> Option<f64> {
        let doc = fts::Doc::new(&l.value, fts::lang_of(l.lang.as_deref()));
        p.forms.iter().find_map(|(q, dfs)| {
            let dfs = if self.fts_enabled {
                dfs.get_or_init(|| self.fts_dfs(q)).as_slice()
            } else {
                &[]
            };
            q.score(&doc, p.corpus, dfs)
        })
    }

    /// Literals matching a full-text query, best first (BM25). A query that matches nothing
    /// is retried typo-tolerant. Literals no longer used by any statement are skipped.
    pub fn search(&self, query: &str, limit: usize) -> Vec<(Id, f64)> {
        self.search_prepared(&self.fts_prepare(query), limit)
    }

    /// The literal ids matching a prepared query, best first, at most `limit`.
    pub fn search_prepared(&self, p: &FtsPrepared, limit: usize) -> Vec<(Id, f64)> {
        if p.is_empty() || !self.fts_enabled {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for (q, dfs) in &p.forms {
            hits = self.search_query(q, p.corpus, dfs);
            if !hits.is_empty() {
                break;
            }
        }
        hits.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        hits.truncate(limit);
        hits
    }

    fn search_query(
        &self,
        q: &fts::FtsQuery,
        corpus: Option<fts::Corpus>,
        dfs: &std::cell::OnceCell<Vec<f64>>,
    ) -> Vec<(Id, f64)> {
        // each clause's candidates (their number is its document frequency); a group is the
        // union of its clauses, and groups intersect
        let mut sets: Vec<HashSet<Id>> = Vec::new();
        let mut counts = Vec::new();
        for g in &q.groups {
            let mut union = HashSet::new();
            for c in g {
                let set = self.fts_candidates(c);
                counts.push(set.len() as f64);
                union.extend(set);
            }
            sets.push(union);
        }
        let dfs = dfs.get_or_init(|| counts);
        sets.sort_by_key(|s| s.len());
        let mut cands = sets.remove(0);
        for s in &sets {
            cands.retain(|id| s.contains(id));
        }
        cands
            .into_iter()
            .filter_map(|id| {
                let crate::rdf::Term::Literal(l) = self.dict.term(id) else {
                    return None;
                };
                let score = q.score(
                    &fts::Doc::new(&l.value, fts::lang_of(l.lang.as_deref())),
                    corpus,
                    dfs,
                )?;
                self.quads_matching(None, None, Some(id), None)
                    .next()
                    .map(|_| (id, score))
            })
            .collect()
    }

    // ------------------------------------------------------------ reads

    /// A quad's terms from its ids.
    pub fn decode(&self, q: [Id; 4]) -> Quad {
        Quad {
            s: self.dict.term(q[S]),
            p: self.dict.term(q[P]),
            o: self.dict.term(q[O]),
            g: if q[G] == DEFAULT_GRAPH || q[G] == INFERRED {
                None
            } else {
                Some(self.dict.term(q[G]))
            },
        }
    }

    /// Number of explicit statements.
    pub fn len(&self) -> usize {
        self.explicit_count as usize
    }

    /// Quads matching a pattern (None = wildcard), including inferred ones.
    /// For the graph position, `Some(DEFAULT_GRAPH)` selects the default graph.
    pub fn quads_matching(
        &self,
        s: Option<Id>,
        p: Option<Id>,
        o: Option<Id>,
        g: Option<Id>,
    ) -> Box<dyn Iterator<Item = [Id; 4]> + '_> {
        let pat = [s, p, o, g];
        let mut best = (0usize, 0usize);
        for (i, order) in ORDERS.iter().enumerate() {
            let n = order.iter().take_while(|&&pos| pat[pos].is_some()).count();
            if n > best.1 {
                best = (i, n);
            }
        }
        let (idx, bound) = best;
        let order = &ORDERS[idx];
        let mut lo = [0; 4];
        let mut hi = [Id::MAX; 4];
        for k in 0..bound {
            let v = pat[order[k]].unwrap();
            lo[k] = v;
            hi[k] = v;
        }
        let iter = self.indexes[idx]
            .range(lo, hi)
            .map(move |k| unpermute(k, order));
        if pat.iter().filter(|x| x.is_some()).count() == bound {
            Box::new(iter)
        } else {
            Box::new(iter.filter(move |q| (0..4).all(|i| pat[i].is_none_or(|v| q[i] == v))))
        }
    }

    /// About how many quads match the pattern (see `Index::estimate`), without reading them.
    pub fn estimate(&self, s: Option<Id>, p: Option<Id>, o: Option<Id>, g: Option<Id>) -> usize {
        let pat = [s, p, o, g];
        let (mut idx, mut bound) = (0, 0);
        for (i, order) in ORDERS.iter().enumerate() {
            let n = order.iter().take_while(|&&pos| pat[pos].is_some()).count();
            if n > bound {
                (idx, bound) = (i, n);
            }
        }
        let order = &ORDERS[idx];
        let (mut lo, mut hi) = ([0; 4], [Id::MAX; 4]);
        for k in 0..bound {
            lo[k] = pat[order[k]].unwrap();
            hi[k] = lo[k];
        }
        self.indexes[idx].estimate(lo, hi)
    }

    /// Like `quads_matching`, but never returns inferred statements.
    pub fn explicit_matching(
        &self,
        s: Option<Id>,
        p: Option<Id>,
        o: Option<Id>,
        g: Option<Id>,
    ) -> Box<dyn Iterator<Item = [Id; 4]> + '_> {
        Box::new(self.quads_matching(s, p, o, g).filter(|q| q[G] != INFERRED))
    }

    /// Distinct named graphs (excluding the default and inferred graphs).
    pub fn graphs(&self) -> Vec<Id> {
        let mut out = Vec::new();
        let mut from = 1;
        while let Some(k) = self.indexes[3].range([from, 0, 0, 0], [Id::MAX; 4]).next() {
            if k[0] >= INFERRED {
                break;
            }
            out.push(k[0]);
            from = k[0] + 1;
        }
        out
    }

    /// The number of statements in a graph (all of them when `None`).
    pub fn count_graph(&self, g: Option<Id>) -> usize {
        match g {
            None => self.len(),
            Some(g) => self.indexes[3]
                .range([g, 0, 0, 0], [g, Id::MAX, Id::MAX, Id::MAX])
                .count(),
        }
    }
}

fn next_key(k: Key) -> Key {
    let mut k = k;
    for i in (0..4).rev() {
        if k[i] < Id::MAX {
            k[i] += 1;
            return k;
        }
        k[i] = 0;
    }
    [Id::MAX; 4]
}

/// The reasoner sees the store as one graph-agnostic set of triples.
impl Facts for Store {
    fn matching(&self, s: Option<Id>, p: Option<Id>, o: Option<Id>) -> Vec<Triple> {
        let mut seen = HashSet::new();
        self.quads_matching(s, p, o, None)
            .map(|q| [q[0], q[1], q[2]])
            .filter(|t| seen.insert(*t))
            .collect()
    }

    fn contains(&self, t: &Triple) -> bool {
        self.quads_matching(Some(t[0]), Some(t[1]), Some(t[2]), None)
            .next()
            .is_some()
    }

    fn add_inferred(&mut self, t: Triple) -> bool {
        if self.contains(&t) {
            return false;
        }
        self.put([t[0], t[1], t[2], INFERRED]);
        self.inferred_count += 1;
        true
    }

    fn is_literal(&self, id: Id) -> bool {
        id < INFERRED && self.dict.term(id).is_literal()
    }

    fn has_any(&self, s: Option<Id>, p: Option<Id>, o: Option<Id>) -> bool {
        self.quads_matching(s, p, o, None).next().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdf::Term;

    fn q(s: &str, p: &str, o: &str, g: Option<&str>) -> Quad {
        Quad {
            s: Term::iri(s),
            p: Term::iri(p),
            o: Term::iri(o),
            g: g.map(Term::iri),
        }
    }

    #[test]
    fn vector_search_in_queries_and_across_restarts() {
        let dir = std::env::temp_dir().join(format!("graphrd-vectors-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let v = |x: [f32; 3]| {
            format!(
                "\"[{}, {}, {}]\"^^<{}>",
                x[0],
                x[1],
                x[2],
                crate::vector::VECTOR
            )
        };
        let docs = [
            ("cat", [1.0, 0.1, 0.0], "small furry cat"),
            ("lion", [0.9, 0.3, 0.0], "big cat"),
            ("car", [0.0, 0.1, 1.0], "fast car"),
            ("bus", [0.1, 0.0, 0.9], "big bus"),
        ];
        let mut ttl = String::from("@prefix ex: <http://e/> . ");
        for (n, e, t) in docs {
            ttl.push_str(&format!("ex:{n} ex:emb {} ; ex:text \"{t}\" . ", v(e)));
        }
        let query = |st: &Store, q: &str| -> Vec<String> {
            let pf = std::collections::HashMap::new();
            let opts = crate::sparql::QueryOptions {
                base: "",
                prefixes: &pf,
                dataset: None,
                timeout_secs: None,
                infer: true,
                at_query: false,
            };
            match crate::sparql::query(
                st,
                &format!("PREFIX ex: <http://e/> PREFIX g: <http://graphrd.org/fn#> {q}"),
                &opts,
            )
            .unwrap()
            {
                crate::sparql::QueryResult::Solutions { rows, .. } => rows
                    .iter()
                    .map(|r| {
                        r[0].as_ref()
                            .map_or(String::new(), |t| t.value().to_string())
                    })
                    .collect(),
                _ => panic!(),
            }
        };
        let knn = format!(
            "SELECT ?d WHERE {{ ?e g:knn2 {} . ?d ex:emb ?e }}",
            v([1.0, 0.0, 0.0])
        );
        {
            let mut st = Store::open(&dir).unwrap();
            st.set_fts(true);
            st.insert(
                &crate::rdf::turtle::parse(&ttl, "", crate::rdf::turtle::Mode::Turtle).unwrap(),
            )
            .unwrap();
            let mut near = query(&st, &knn);
            near.sort();
            assert_eq!(near, ["http://e/cat", "http://e/lion"]);
            // hybrid: full-text matches reranked by similarity
            let hybrid = query(
                &st,
                &format!(
                    "SELECT ?d WHERE {{ ?t g:fts \"big\" . ?d ex:text ?t ; ex:emb ?e }} ORDER BY DESC(g:similarity(?e, {})) LIMIT 1",
                    v([0.0, 0.0, 1.0])
                ),
            );
            assert_eq!(hybrid, ["http://e/bus"]);
            st.checkpoint().unwrap();
            // a vector added after the checkpoint comes back from the log
            st.insert(
                &crate::rdf::turtle::parse(
                    &format!("<http://e/kitten> <http://e/emb> {} .", v([1.0, 0.05, 0.0])),
                    "",
                    crate::rdf::turtle::Mode::Turtle,
                )
                .unwrap(),
            )
            .unwrap();
            std::mem::forget(st);
        }
        let st = Store::open(&dir).unwrap();
        assert!(dir.join(VECTORS).exists());
        let mut near = query(&st, &knn);
        near.sort();
        assert_eq!(near, ["http://e/cat", "http://e/kitten"]);
        drop(st);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn owl2_rl_list_axioms_become_rules() {
        let ttl = |t: &str| {
            crate::rdf::turtle::parse(
                &format!(
                    "@prefix ex: <http://e/> . @prefix owl: <http://www.w3.org/2002/07/owl#> . {t}"
                ),
                "",
                crate::rdf::turtle::Mode::Turtle,
            )
            .unwrap()
        };
        let mut st = Store::memory();
        st.set_ruleset("owl2-rl-optimized", true);
        st.insert(&ttl("ex:ann a ex:Woman , ex:Parent . ex:x ex:hasParent ex:y . ex:y ex:hasBrother ex:z .
            ex:p1 a ex:Person ; ex:ssn \"1\" . ex:p2 a ex:Person ; ex:ssn \"1\" . ex:dog a ex:Dog .")).unwrap();
        fn has(st: &Store, s: &str, p: &str, o: &str) -> bool {
            let id = |x: &str| st.dict.lookup(&Term::iri(x));
            matches!((id(s), id(p), id(o)), (Some(s), Some(p), Some(o)) if Facts::contains(st, &[s, p, o]))
        }
        let rdf_type = format!("{}type", crate::rdf::RDF);
        let same = format!("{}sameAs", crate::rdf::OWL);
        assert!(
            !has(&st, "http://e/ann", &rdf_type, "http://e/Mother"),
            "no rule yet"
        );
        // the schema arrives after the data: the rules are compiled and everything derived again
        st.insert(&ttl("ex:Mother owl:intersectionOf ( ex:Woman ex:Parent ) . ex:Pet owl:unionOf ( ex:Dog ex:Cat ) .
            ex:hasUncle owl:propertyChainAxiom ( ex:hasParent ex:hasBrother ) . ex:Person owl:hasKey ( ex:ssn ) .
            ex:Weekday owl:oneOf ( ex:Mon ex:Tue ) .")).unwrap();
        assert!(
            has(&st, "http://e/ann", &rdf_type, "http://e/Mother"),
            "cls-int1"
        );
        assert!(
            has(&st, "http://e/dog", &rdf_type, "http://e/Pet"),
            "cls-uni"
        );
        assert!(
            has(&st, "http://e/Tue", &rdf_type, "http://e/Weekday"),
            "cls-oo"
        );
        assert!(
            has(&st, "http://e/x", "http://e/hasUncle", "http://e/z"),
            "prp-spo2"
        );
        assert!(has(&st, "http://e/p1", &same, "http://e/p2"), "prp-key");
        // a member of an intersection is a member of each part (cls-int2)
        st.insert(&ttl("ex:bea a ex:Mother .")).unwrap();
        assert!(
            has(&st, "http://e/bea", &rdf_type, "http://e/Woman")
                && has(&st, "http://e/bea", &rdf_type, "http://e/Parent")
        );
        // removing the axiom retracts what it derived
        let id = |st: &Store, x: &str| st.dict.lookup(&Term::iri(x)).unwrap();
        let head: Vec<Quad> = st
            .explicit_matching(
                Some(id(&st, "http://e/Mother")),
                Some(id(&st, &format!("{}intersectionOf", crate::rdf::OWL))),
                None,
                None,
            )
            .map(|q| st.decode(q))
            .collect();
        assert_eq!(head.len(), 1);
        st.apply(&head, &[]).unwrap();
        assert!(
            !has(&st, "http://e/ann", &rdf_type, "http://e/Mother"),
            "derivation withdrawn with its axiom"
        );
        assert!(
            has(&st, "http://e/dog", &rdf_type, "http://e/Pet"),
            "other axioms stay"
        );
    }

    #[test]
    fn bulk_insert_matches_ordinary_inserts() {
        let dir = std::env::temp_dir().join(format!("graphrd-bulk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let rdfs = "http://www.w3.org/2000/01/rdf-schema#";
        let ty = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
        let lit = |s: &str, p: &str, o: &str| Quad {
            s: Term::iri(s),
            p: Term::iri(p),
            o: Term::string(o),
            g: None,
        };
        let bn = |s: &str, p: &str, o: &str| Quad {
            s: Term::Blank(s.into()),
            p: Term::iri(p),
            o: Term::iri(o),
            g: None,
        };
        {
            let mut st = Store::open(&dir).unwrap();
            st.set_ruleset("rdfs", false);
            st.set_fts(true);
            // existing data: a schema triple, an explicit one, and one that becomes inferred
            st.insert(&[
                q("Dog", &format!("{rdfs}subClassOf"), "Animal", None),
                q("rex", ty, "Dog", None),
                q("old", "p", "o", None),
            ])
            .unwrap();
            assert!(st.has([
                st.dict.lookup(&Term::iri("rex")).unwrap(),
                st.dict.lookup(&Term::iri(ty)).unwrap(),
                st.dict.lookup(&Term::iri("Animal")).unwrap(),
                INFERRED
            ]));
            let inferred_before = st.inferred_count;
            let chunks = vec![
                vec![
                    q("fido", ty, "Dog", None),
                    q("old", "p", "o", None),
                    lit("fido", "name", "Fido the brave"),
                    bn("b1", "knows", "rex"),
                ],
                vec![
                    q("fido", ty, "Dog", None),
                    q("rex", ty, "Animal", None),
                    q("g1", "p", "o", Some("G")),
                    bn("b1", "likes", "fido"),
                ],
            ];
            // the same statements through the ordinary path, for comparison
            let mut plain = Store::memory();
            plain.set_ruleset("rdfs", false);
            plain
                .insert(&[
                    q("Dog", &format!("{rdfs}subClassOf"), "Animal", None),
                    q("rex", ty, "Dog", None),
                    q("old", "p", "o", None),
                ])
                .unwrap();
            plain.insert(&chunks.concat()).unwrap();
            let _ = inferred_before;
            let mut bnodes = HashMap::new();
            let added = st.bulk_insert(chunks, &mut bnodes).unwrap();
            assert_eq!(
                added, 6,
                "duplicates within the load and with the store are left out"
            );
            assert_eq!(
                (st.explicit_count, st.inferred_count),
                (plain.explicit_count, plain.inferred_count),
                "same as ordinary inserts"
            );
            let id = |s: &str| st.dict.lookup(&Term::iri(s)).unwrap();
            assert!(
                st.has([id("fido"), id(ty), id("Animal"), INFERRED]),
                "inference ran on the loaded statements"
            );
            assert!(st.has([id("rex"), id(ty), id("Animal"), DEFAULT_GRAPH]));
            assert!(
                !st.search("brave", 10).is_empty(),
                "full-text index updated"
            );
            let b = st.dict.lookup(&Term::Blank(bnodes["b1"].clone())).unwrap();
            assert_eq!(
                st.explicit_matching(Some(b), None, None, None).count(),
                2,
                "one blank node across chunks"
            );
            assert_eq!(st.len(), plain.len());
        }
        let st = Store::open(&dir).unwrap();
        assert_eq!(st.explicit_count, 9, "the load is durable without the log");
        assert!(st.dict.lookup(&Term::iri("g1")).is_some());
        drop(st);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn time_travel() {
        let dir = std::env::temp_dir().join(format!("graphrd-history-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let rdfs = "http://www.w3.org/2000/01/rdf-schema#";
        let ty = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
        let tick = || {
            std::thread::sleep(std::time::Duration::from_millis(3));
            let t = now_millis();
            std::thread::sleep(std::time::Duration::from_millis(3));
            t
        };
        let has = |st: &Store, s: &str, p: &str, o: &str| -> bool {
            let (Some(s), Some(p), Some(o)) = (
                st.dict.lookup(&Term::iri(s)),
                st.dict.lookup(&Term::iri(p)),
                st.dict.lookup(&Term::iri(o)),
            ) else {
                return false;
            };
            st.quads_matching(Some(s), Some(p), Some(o), None)
                .next()
                .is_some()
        };
        let (t0, t1, t2, t3, snap);
        {
            let mut st = Store::open(&dir).unwrap();
            assert!(
                st.as_of(now_millis())
                    .err()
                    .unwrap()
                    .contains("keeps no history")
            );
            st.set_ruleset("rdfs", false);
            st.set_history(Some(0)).unwrap();
            t0 = tick();
            st.insert(&[
                q("Dog", &format!("{rdfs}subClassOf"), "Animal", None),
                q("rex", ty, "Dog", None),
            ])
            .unwrap();
            t1 = tick();
            st.apply(&[q("rex", ty, "Dog", None)], &[q("fido", ty, "Dog", None)])
                .unwrap();
            t2 = tick();
            st.bulk_insert(vec![vec![q("tom", ty, "Cat", None)]], &mut HashMap::new())
                .unwrap();
            t3 = tick();
            snap = st.snapshot();
            st.insert(&[q("late", "p", "o", None)]).unwrap();
        }
        // restarted: the history goes on
        let mut st = Store::open(&dir).unwrap();
        st.set_history(Some(0)).unwrap();
        st.apply(&[q("fido", ty, "Dog", None)], &[]).unwrap();
        let past = |at: i64| st.as_of(at).unwrap();
        assert_eq!(past(t0).len(), 0);
        let p1 = past(t1);
        assert!(has(&p1, "rex", ty, "Animal"), "inference as it was");
        assert!(!has(&p1, "fido", ty, "Dog"));
        let p2 = past(t2);
        assert!(
            !has(&p2, "rex", ty, "Dog")
                && !has(&p2, "rex", ty, "Animal")
                && has(&p2, "fido", ty, "Animal")
        );
        assert!(!has(&p2, "tom", ty, "Cat"));
        assert!(has(&past(t3), "tom", ty, "Cat") && !has(&past(t3), "late", "p", "o"));
        assert!(!has(&past(now_millis()), "fido", ty, "Dog"));
        assert_eq!(past(now_millis()).len(), st.len());
        // a snapshot taken earlier reaches the same past, though the history has gone on
        assert_eq!(snap.as_of(t1).unwrap().len(), p1.len());
        assert!(has(&snap.as_of(t1).unwrap(), "rex", ty, "Dog"));
        // the live store is untouched by the views
        assert!(has(&st, "tom", ty, "Cat") && !has(&st, "rex", ty, "Dog"));
        assert!(
            st.as_of(t0 - 60_000)
                .err()
                .unwrap()
                .contains("before the kept history")
        );

        // a transaction in the log but not in the history (the process stopped between the
        // two writes) is added to the history at the next start
        st.insert(&[q("last", "p", "o", None)]).unwrap();
        let t4 = tick();
        let path = dir.join(HISTORY);
        let text = fs::read_to_string(&path).unwrap();
        let cut = text.trim_end().rfind("\nc ").unwrap();
        let cut = text[..cut]
            .rfind("\nc ")
            .map_or(0, |i| text[i + 1..].find('\n').unwrap() + i + 2);
        drop(st);
        fs::write(&path, &text[..cut]).unwrap();
        let st = Store::open(&dir).unwrap();
        assert!(has(&st.as_of(t4).unwrap(), "last", "p", "o"));
        assert!(!has(&st.as_of(t3).unwrap(), "last", "p", "o"));
        drop(st);

        // pruning past the retention
        fs::write(
            &path,
            "s 1000\n+ <http://a> <http://b> <http://c> .\nc 2000\n",
        )
        .unwrap();
        let mut st = Store::open(&dir).unwrap();
        st.set_history(Some(3_600_000)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("http://a") && text.starts_with("s "),
            "{text}"
        );
        assert!(st.as_of(1500).is_err());
        assert!(st.history_window().unwrap().0 > 2000);
        st.set_history(None).unwrap();
        assert!(!path.exists());
        drop(st);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_survives_commits_larger_than_its_preallocation() {
        let dir = std::env::temp_dir().join(format!("graphrd-prealloc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let big: Vec<Quad> = (0..120_000)
            .map(|i| {
                q(
                    &format!("http://example.org/subject/{i}"),
                    "http://example.org/p",
                    "http://example.org/o",
                    None,
                )
            })
            .collect();
        {
            let mut st = Store::open(&dir).unwrap();
            st.apply(&[], &big).unwrap(); // about 9 MB of log, far past the zero-filled region
            st.apply(&[], &[q("tail", "p", "o", None)]).unwrap();
            assert!(
                fs::metadata(dir.join("wal.log")).unwrap().len() > st.wal_bytes,
                "zero-filled ahead"
            );
            std::mem::forget(st);
        }
        let st = Store::open(&dir).unwrap();
        assert_eq!(st.len(), big.len() + 1, "everything replayed");
        drop(st);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn torn_transaction_is_dropped_whole() {
        let dir = std::env::temp_dir().join(format!("graphrd-torn-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        {
            let mut st = Store::open(&dir).unwrap();
            st.apply(&[], &[q("a", "p", "b", None)]).unwrap();
            st.apply(
                &[q("a", "p", "b", None)],
                &[q("c", "p", "d", None), q("e", "p", "f", None)],
            )
            .unwrap();
            std::mem::forget(st); // a crash: no checkpoint on the way out
        }
        // simulate a crash in the middle of writing a third transaction; the
        // WAL's zero-filled ahead region is at least as large as a commit, but
        // never assume its capacity — replace the tail explicitly
        let path = dir.join("wal.log");
        let bytes = fs::read(&path).unwrap();
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let torn = b"- <c> <p> <d> .\n+ <x> <p> <y> .\n+ <z> <p";
        let mut bytes = bytes[..end].to_vec();
        bytes.extend_from_slice(torn);
        fs::write(&path, bytes).unwrap();
        let st = Store::open(&dir).unwrap();
        let has = |s: &str| {
            st.dict.lookup(&Term::iri(s)).is_some_and(|id| {
                st.quads_matching(Some(id), None, None, None)
                    .next()
                    .is_some()
            })
        };
        assert!(
            !has("a") && has("c") && has("e"),
            "committed transactions replayed"
        );
        assert!(!has("x"), "the torn transaction left no trace");
        assert_eq!(st.len(), 2);
        drop(st);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn batch_reaches_the_log_whole_or_not_at_all() {
        let dir = std::env::temp_dir().join(format!("graphrd-batch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        {
            let mut st = Store::open(&dir).unwrap();
            st.apply(&[], &[q("a", "p", "b", None)]).unwrap();
            assert!(st.begin_batch());
            assert!(!st.begin_batch(), "a batch does not nest");
            st.apply(&[q("a", "p", "b", None)], &[]).unwrap();
            st.apply(&[], &[q("c", "p", "d", None)]).unwrap();
            st.checkpoint().unwrap(); // must not make part of the batch durable
            std::mem::forget(st); // a crash before the batch ends
        }
        {
            let mut st = Store::open(&dir).unwrap();
            assert_eq!(st.len(), 1, "none of the batch survived");
            assert!(st.begin_batch());
            st.apply(&[q("a", "p", "b", None)], &[]).unwrap();
            st.apply(&[], &[q("c", "p", "d", None)]).unwrap();
            st.end_batch(true).unwrap();
            std::mem::forget(st);
        }
        let st = Store::open(&dir).unwrap();
        assert_eq!(st.len(), 1);
        assert!(st.dict.lookup(&Term::iri("c")).is_some_and(|id| {
            st.quads_matching(Some(id), None, None, None)
                .next()
                .is_some()
        }));
        drop(st);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_torn_inside_a_character_still_opens() {
        let dir = std::env::temp_dir().join(format!("graphrd-torn-utf8-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let lit = |s: &str, o: &str| Quad {
            s: Term::iri(s),
            p: Term::iri("p"),
            o: Term::string(o),
            g: None,
        };
        {
            let mut st = Store::open(&dir).unwrap();
            st.set_history(Some(0)).unwrap();
            st.apply(&[], &[lit("a", "çağ")]).unwrap();
            st.apply(&[], &[lit("b", "şüphe")]).unwrap();
            std::mem::forget(st); // a crash: no checkpoint on the way out
        }
        // the crash tore a third transaction inside "ç" (0xC3 0xA7) in both logs
        let torn = b"+ <c> <p> \"\xC3";
        for name in ["wal.log", HISTORY] {
            let path = dir.join(name);
            let bytes = fs::read(&path).unwrap();
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            let mut bytes = bytes[..end].to_vec();
            bytes.extend_from_slice(torn);
            bytes.extend_from_slice(&[0; 64]);
            fs::write(&path, bytes).unwrap();
        }
        {
            let mut st = Store::open(&dir).expect("a torn log must not keep the store closed");
            assert_eq!(st.len(), 2, "both committed transactions replayed");
            st.apply(&[], &[lit("d", "after")]).unwrap();
            std::mem::forget(st);
        }
        let history = fs::read(dir.join(HISTORY)).unwrap();
        assert!(
            !history.windows(2).any(|w| w == b"\xC3+"),
            "the torn tail was cut off before appending"
        );
        let st = Store::open(&dir).unwrap();
        assert_eq!(st.len(), 3);
        let (start, _) = st.history_window().unwrap();
        let undone: usize = st
            .undo_after(start)
            .unwrap()
            .iter()
            .map(|(_, q)| q.len())
            .sum();
        assert_eq!(undone, 3, "history intact");
        drop(st);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_and_persist() {
        let dir = std::env::temp_dir().join(format!("graphrd-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        {
            let mut st = Store::open(&dir).unwrap();
            st.insert(&[
                q("a", "p", "b", None),
                q("a", "p", "c", Some("g")),
                q("x", "q", "b", None),
            ])
            .unwrap();
            let p = st.dict.lookup(&Term::iri("p"));
            assert_eq!(st.quads_matching(None, p, None, None).count(), 2);
            let b = st.dict.lookup(&Term::iri("b"));
            assert_eq!(
                st.quads_matching(None, None, b, Some(DEFAULT_GRAPH))
                    .count(),
                2
            );
            assert_eq!(st.graphs().len(), 1);
            st.apply(&[q("x", "q", "b", None)], &[]).unwrap();
        }
        // reopen: WAL replay only
        {
            let mut st = Store::open(&dir).unwrap();
            assert_eq!(st.len(), 2);
            st.checkpoint().unwrap();
            st.insert(&[q("n", "p", "m", None)]).unwrap();
        }
        // reopen: run files + WAL
        let st = Store::open(&dir).unwrap();
        assert_eq!(st.len(), 3);
        let a = st.dict.lookup(&Term::iri("a"));
        assert_eq!(st.quads_matching(a, None, None, None).count(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rdfs_inference_and_retraction() {
        let rdfs = "http://www.w3.org/2000/01/rdf-schema#";
        let ty = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
        let mut st = Store::memory();
        st.set_ruleset("rdfsplus-optimized", true);
        st.insert(&[
            q("Dog", &format!("{rdfs}subClassOf"), "Animal", None),
            q("Animal", &format!("{rdfs}subClassOf"), "Being", None),
            q("rex", ty, "Dog", None),
        ])
        .unwrap();
        let tyid = st.dict.lookup(&Term::iri(ty));
        let rex = st.dict.lookup(&Term::iri("rex"));
        assert_eq!(st.quads_matching(rex, tyid, None, None).count(), 3); // Dog; Animal, Being inferred
        assert_eq!(st.len(), 3);
        assert!(st.inferred_len() >= 3);
        st.apply(
            &[q("Dog", &format!("{rdfs}subClassOf"), "Animal", None)],
            &[],
        )
        .unwrap();
        assert_eq!(st.quads_matching(rex, tyid, None, None).count(), 1);
        assert_eq!(st.inferred_len(), 0);
    }

    #[test]
    fn dred_keeps_inferences_with_other_support() {
        let sc = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
        let ty = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
        let mut st = Store::memory();
        st.set_ruleset("rdfs-optimized", true);
        st.insert(&[
            q("Dog", sc, "Animal", None),
            q("Pet", sc, "Animal", None),
            q("Animal", sc, "Being", None),
            q("rex", ty, "Dog", None),
            q("rex", ty, "Pet", None),
        ])
        .unwrap();
        let (tyid, rex) = (
            st.dict.lookup(&Term::iri(ty)),
            st.dict.lookup(&Term::iri("rex")),
        );
        let types = |st: &Store| st.quads_matching(rex, tyid, None, None).count();
        assert_eq!(types(&st), 4); // Dog, Pet, Animal, Being
        st.apply(&[q("rex", ty, "Dog", None)], &[]).unwrap();
        assert_eq!(types(&st), 3); // Pet still supports Animal and Being
        st.apply(&[q("Pet", sc, "Animal", None)], &[]).unwrap();
        assert_eq!(types(&st), 1);
        // a removed explicit triple that is still inferable stays visible
        st.insert(&[q("rex", ty, "Animal", None), q("rex", ty, "Dog", None)])
            .unwrap();
        st.apply(&[q("rex", ty, "Animal", None)], &[]).unwrap();
        let animal = st.dict.lookup(&Term::iri("Animal"));
        assert_eq!(st.quads_matching(rex, tyid, animal, None).count(), 1);
    }

    #[test]
    fn custom_pie_ruleset() {
        let dir = std::env::temp_dir().join(format!("graphrd-pie-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let pie = dir.join("family.pie");
        fs::write(&pie, "// family rules\nPrefices\n{\n  ex : http://ex.org/\n}\nAxioms\n{\n  <ex:ancestorOf> <ex:kind> <ex:Transitive>\n}\nRules\n{\nId: parent_is_ancestor\n  x <ex:parentOf> y\n  ------------------------\n  x <ex:ancestorOf> y\n\nId: ancestor_transitive\n  x <ex:ancestorOf> y\n  y <ex:ancestorOf> z   [Constraint x != z]\n  ------------------------\n  x <ex:ancestorOf> z\n}\n").unwrap();
        let mut st = Store::memory();
        st.set_ruleset(pie.to_str().unwrap(), true);
        st.insert(&[
            q(
                "http://ex.org/a",
                "http://ex.org/parentOf",
                "http://ex.org/b",
                None,
            ),
            q(
                "http://ex.org/b",
                "http://ex.org/parentOf",
                "http://ex.org/c",
                None,
            ),
            q(
                "http://ex.org/c",
                "http://ex.org/parentOf",
                "http://ex.org/a",
                None,
            ),
        ])
        .unwrap();
        let anc = st.dict.lookup(&Term::iri("http://ex.org/ancestorOf"));
        let a = st.dict.lookup(&Term::iri("http://ex.org/a"));
        // a reaches b and c but, by the constraint, never derives "a ancestorOf a"
        assert_eq!(st.quads_matching(a, anc, None, None).count(), 2);
        assert_eq!(st.quads_matching(a, anc, a, None).count(), 0);
        assert_eq!(st.quads_matching(anc, None, None, None).count(), 1); // the axiom
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn inferences_persist_and_data_spills_to_disk() {
        let dir = std::env::temp_dir().join(format!("graphrd-scale-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let rdfs = "http://www.w3.org/2000/01/rdf-schema#";
        let ty = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
        {
            let mut st = Store::open(&dir).unwrap();
            st.set_ruleset("rdfs-optimized", true);
            st.insert(&[q("C", &format!("{rdfs}subClassOf"), "D", None)])
                .unwrap();
            for chunk in 0..4 {
                let batch: Vec<Quad> = (0..5_000)
                    .map(|i| q(&format!("e{chunk}-{i}"), ty, "C", None))
                    .collect();
                st.insert(&batch).unwrap();
                st.checkpoint().unwrap(); // several run files per index
            }
            assert_eq!(st.inferred_len(), 20_000);
            // delete half of one batch: dependent inferences are retracted
            let doomed: Vec<Quad> = (0..2_500)
                .map(|i| q(&format!("e0-{i}"), ty, "C", None))
                .collect();
            st.apply(&doomed, &[]).unwrap();
            assert_eq!(st.inferred_len(), 17_500);
        }
        let mut st = Store::open(&dir).unwrap();
        st.set_ruleset("rdfs-optimized", true); // same ruleset: no recompute
        assert_eq!(st.len(), 17_501);
        assert_eq!(st.inferred_len(), 17_500);
        let d = st.dict.lookup(&Term::iri("D"));
        // 17,500 inferred "x a D" plus the explicit "C subClassOf D"
        assert_eq!(st.quads_matching(None, None, d, None).count(), 17_501);
        let _ = fs::remove_dir_all(&dir);
    }
}
