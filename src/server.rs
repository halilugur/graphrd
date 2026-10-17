//! HTTP API: the repository REST protocol under /repositories, the management
//! REST API under /rest, and the static web UI.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::panic::AssertUnwindSafe;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::http::{self, Request, Response};
use crate::json::Json;
use crate::rdf::{self, Format, Quad, Term, turtle};
use crate::sparql::{self, QueryOptions, eval::QueryResult};
use crate::store::{DEFAULT_GRAPH, Id, Store};

/// The product version, as `--version` and `/rest/info/version` report it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const UI_HTML: &str = include_str!("../ui/index.html");
const UI_JS: &str = include_str!("../ui/app.js");
const UI_CSS: &str = include_str!("../ui/style.css");
/// A compatibility IRI some clients send for the default graph.
const COMPAT_NIL: &str = "http://rdf4j.org/schema/rdf4j#nil";
/// Its older spelling.
const COMPAT_NIL_OLD: &str = "http://www.openrdf.org/schema/sesame#nil";

const DEFAULT_NAMESPACES: [(&str, &str); 14] = [
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    ("xml", "http://www.w3.org/XML/1998/namespace"),
    ("fn", "http://www.w3.org/2005/xpath-functions#"),
    ("rdf4j", "http://rdf4j.org/schema/rdf4j#"),
    ("sesame", "http://www.openrdf.org/schema/sesame#"),
    ("g", "http://graphrd.org/fn#"),
    ("graph", "http://graphrd.org/graph#"),
    ("rank", "http://graphrd.org/rank#"),
    ("geo", "http://www.opengis.net/ont/geosparql#"),
    ("geof", "http://www.opengis.net/def/function/geosparql/"),
    ("uom", "http://www.opengis.net/def/uom/OGC/1.0/"),
];

/// Set by the offline tools: no per-request log lines on standard error.
pub static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn quiet() -> bool {
    QUIET.load(std::sync::atomic::Ordering::Relaxed)
}

fn rl<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}
fn wl<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}
/// Write a file only its owner can read (it holds a secret), replacing it whole.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp)?;
        std::io::Write::write_all(&mut f, bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Errors caused by the request text itself (answered with 400, not 500).
fn is_client_error(e: &str) -> bool {
    e.starts_with("SPARQL syntax")
        || e.starts_with("Protocol error")
        || e.contains("undefined prefix")
}

fn valid_repo_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !id.starts_with('.')
}

/// Snapshot isolation for a repository's store (multi-version concurrency): readers take the
/// latest published snapshot and never wait for a writer; writers are serialized, work on the
/// master copy, and publish a new snapshot when they finish, before acknowledging. A reader
/// sees each write completely or not at all.
pub struct StoreLock {
    writer: Mutex<()>,
    /// The master copy; touched only while holding `writer`.
    data: std::cell::UnsafeCell<Store>,
    /// Latest published snapshot (shared lock: readers only clone the `Arc`).
    current: RwLock<Arc<Store>>,
}

// Safety: `data` is accessed only by the holder of `writer`; snapshots are immutable.
unsafe impl Sync for StoreLock {}
unsafe impl Send for StoreLock {}

/// A read handle: a snapshot of the store, kept alive for the read.
pub struct StoreRead<'a>(Arc<Store>, std::marker::PhantomData<&'a StoreLock>);
/// The write handle: the only writer, and the fsync it waits for at the end.
pub struct StoreWrite<'a> {
    lock: &'a StoreLock,
    /// Released before waiting for the fsync, so the next writer can start.
    guard: Option<std::sync::MutexGuard<'a, ()>>,
    /// Do not wait for the log to reach the disk (see `write_relaxed`).
    relaxed: bool,
}

impl StoreLock {
    /// Wrap a store: one writer at a time, readers always see a consistent snapshot.
    pub fn new(mut store: Store) -> Self {
        let snap = Arc::new(store.snapshot());
        StoreLock {
            writer: Mutex::new(()),
            data: std::cell::UnsafeCell::new(store),
            current: RwLock::new(snap),
        }
    }

    /// A read handle on the current snapshot.
    pub fn read(&self) -> StoreRead<'_> {
        StoreRead(
            self.current
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            std::marker::PhantomData,
        )
    }

    /// The write handle; held until the commit is on disk.
    pub fn write(&self) -> StoreWrite<'_> {
        StoreWrite {
            guard: Some(lock(&self.writer)),
            lock: self,
            relaxed: false,
        }
    }

    /// A write that returns once its transaction is in the log, without waiting for the log
    /// to reach the disk: the next durable write's fsync, or `sync`, covers it. For records
    /// that may be lost in a power failure (logs, statistics), not for data a caller relies on.
    pub fn write_relaxed(&self) -> StoreWrite<'_> {
        StoreWrite {
            guard: Some(lock(&self.writer)),
            lock: self,
            relaxed: true,
        }
    }

    /// Waits until everything written so far is on disk.
    pub fn sync(&self) -> std::io::Result<()> {
        self.read().sync_log()
    }
}

impl Drop for StoreWrite<'_> {
    fn drop(&mut self) {
        // Safety: this guard still holds the writer lock.
        let store = unsafe { &mut *self.lock.data.get() };
        let pending = store.take_unsynced();
        let snap = Arc::new(store.snapshot());
        *self.lock.current.write().unwrap_or_else(|e| e.into_inner()) = snap;
        // Group commit: the writer lock is released before the fsync, so the next writer can
        // log its transaction meanwhile; the caller acknowledges only once the log is on disk.
        self.guard = None;
        if self.relaxed {
            return;
        }
        if let Some((sync, seq)) = pending {
            if let Err(e) = sync.wait(seq) {
                // The kernel may have dropped the dirty pages; continuing could acknowledge
                // writes that are not on disk. Restarting replays the log from disk.
                eprintln!("fatal: fsync of the write-ahead log failed: {e}");
                std::process::abort();
            }
        }
    }
}

impl std::ops::Deref for StoreRead<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        &self.0
    }
}

impl std::ops::Deref for StoreWrite<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        unsafe { &*self.lock.data.get() }
    }
}

impl std::ops::DerefMut for StoreWrite<'_> {
    fn deref_mut(&mut self) -> &mut Store {
        unsafe { &mut *self.lock.data.get() }
    }
}

/// One repository: its store, configuration, namespaces and import records.
pub struct Repo {
    /// The repository's id (its directory name).
    pub id: String,
    dir: PathBuf,
    config: RwLock<Json>,
    /// The store and its lock.
    pub store: StoreLock,
    namespaces: RwLock<Vec<(String, String)>>,
    imports: Mutex<Vec<Json>>,
    /// (store version, sorted (lower-case local name, IRI)) for autocomplete.
    autocomplete: Mutex<Option<(u64, Vec<(String, String)>)>>,
    /// Lives in memory only (`serve --memory`): nothing is written to disk.
    memory: bool,
    /// Whether the thread that feeds the search connectors runs.
    connector_thread: std::sync::atomic::AtomicBool,
}

impl Repo {
    fn open(dir: &Path) -> std::io::Result<Repo> {
        let config = Json::parse(&fs::read_to_string(dir.join("config.json"))?)
            .map_err(std::io::Error::other)?;
        Repo::with_store(dir, config, Store::open(&dir.join("storage"))?, false)
    }

    /// A repository that is never written to disk.
    fn memory(config: Json) -> Repo {
        Repo::with_store(Path::new(""), config, Store::memory(), true)
            .expect("a memory repository needs no files")
    }

    fn with_store(
        dir: &Path,
        config: Json,
        mut store: Store,
        memory: bool,
    ) -> std::io::Result<Repo> {
        let id = config.str_at(&["id"]).to_string();
        let (ruleset, same_as) = ruleset_of(&config);
        store.set_ruleset(&ruleset, same_as);
        store.set_fts(fts_of(&config));
        store.set_history(history_of(&config))?;
        store.union_default_graph = union_default_of(&config);
        let namespaces = match fs::read_to_string(dir.join("namespaces.json"))
            .ok()
            .and_then(|t| Json::parse(&t).ok())
        {
            Some(Json::Obj(fields)) => fields
                .into_iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
                .collect(),
            _ => DEFAULT_NAMESPACES
                .iter()
                .map(|(p, n)| (p.to_string(), n.to_string()))
                .collect(),
        };
        Ok(Repo {
            id,
            dir: dir.to_path_buf(),
            config: RwLock::new(config),
            store: StoreLock::new(store),
            namespaces: RwLock::new(namespaces),
            imports: Mutex::new(Vec::new()),
            autocomplete: Mutex::new(None),
            memory,
            connector_thread: Default::default(),
        })
    }

    fn save_config(&self) -> std::io::Result<()> {
        if self.memory {
            return Ok(());
        }
        fs::write(self.dir.join("config.json"), rl(&self.config).to_string())
    }

    fn save_namespaces(&self) -> std::io::Result<()> {
        if self.memory {
            return Ok(());
        }
        let obj = Json::Obj(
            rl(&self.namespaces)
                .iter()
                .map(|(p, n)| (p.clone(), Json::from(n)))
                .collect(),
        );
        fs::write(self.dir.join("namespaces.json"), obj.to_string())
    }

    /// Register prefixes declared in imported Turtle/TriG, so later queries
    /// can use them, without overriding existing ones.
    fn learn_prefixes(&self, text: &str, format: Format) {
        if !matches!(format, Format::Turtle | Format::TriG) {
            return;
        }
        let found = turtle::prefixes(text);
        if found.is_empty() {
            return;
        }
        let mut changed = false;
        {
            let mut ns = wl(&self.namespaces);
            for (p, n) in found {
                if !ns.iter().any(|(x, _)| *x == p) && !n.is_empty() {
                    ns.push((p, n));
                    changed = true;
                }
            }
        }
        if changed {
            let _ = self.save_namespaces();
        }
    }

    fn prefix_map(&self) -> HashMap<String, String> {
        rl(&self.namespaces).iter().cloned().collect()
    }

    fn title(&self) -> String {
        rl(&self.config).str_at(&["title"]).to_string()
    }
}

/// Whether the repository config enables SHACL validation.
/// Why a write transaction was undone.
enum Abort {
    Failed(String),
    /// SHACL refused the result (the validation report).
    Shacl(Json),
    /// Undoing failed too (the whole message).
    Undo(String),
}

/// Run `f` as one all-or-nothing write: what it changes is recorded, and undone if
/// it fails (or SHACL refuses the result). A recording that is already running (a
/// cluster's, which replicates the change) goes on; only this write's part of it is undone.
fn transaction<T>(
    repo: &Repo,
    st: &mut crate::store::Store,
    f: impl FnOnce(&mut crate::store::Store) -> Result<T, String>,
) -> Result<T, Abort> {
    let own = !st.capturing();
    if own {
        st.start_capture();
    }
    let mark = st.capture_mark();
    let result = f(st);
    let (removed, inserted) = st.capture_since(mark);
    let undo = |st: &mut crate::store::Store| -> Result<(), String> {
        if !(removed.is_empty() && inserted.is_empty()) {
            st.apply(&inserted, &removed).map_err(|e| e.to_string())?;
        }
        st.capture_truncate(mark);
        Ok(())
    };
    let out = match result {
        Err(e) => match undo(st) {
            Ok(()) => Err(Abort::Failed(e)),
            Err(u) => Err(Abort::Undo(format!(
                "The update failed, and undoing its first operations failed too: {u}"
            ))),
        },
        Ok(v) => match shacl_of(&rl(&repo.config))
            .then(|| shacl_report(st, &removed, &inserted))
            .flatten()
        {
            Some(report) => match undo(st) {
                Ok(()) => Err(Abort::Shacl(report)),
                Err(u) => Err(Abort::Undo(format!(
                    "SHACL violation, and the rollback failed: {u}"
                ))),
            },
            None => Ok(v),
        },
    };
    if own {
        st.take_capture();
    }
    out
}

/// The GraphQL request text: the `query` parameter, an `application/graphql` body
/// or the `query` member of a JSON body.
fn graphql_query(req: &Request) -> String {
    req.param("query").map(String::from).unwrap_or_else(|| {
        if req.content_type() == "application/graphql" {
            req.body_text()
        } else {
            Json::parse(&req.body_text())
                .map(|b| b.str_at(&["query"]).to_string())
                .unwrap_or_default()
        }
    })
}

fn shacl_of(config: &Json) -> bool {
    config
        .get("params")
        .and_then(|p| p.get("isShacl"))
        .map(|v| v.str_at(&["value"]) == "true")
        .unwrap_or(false)
}

/// Before a write: start recording changes when SHACL is on.
/// A SHACL check around one write: it records the write's changes (inside a recording that is
/// already running, such as a cluster's, from a mark in it).
#[derive(Clone, Copy)]
struct ShaclCheck {
    on: bool,
    own: bool,
    mark: (usize, usize),
}

fn shacl_begin(repo: &Repo, st: &mut Store) -> ShaclCheck {
    if !shacl_of(&rl(&repo.config)) {
        return ShaclCheck {
            on: false,
            own: false,
            mark: (0, 0),
        };
    }
    let own = !st.capturing();
    if own {
        st.start_capture();
    }
    ShaclCheck {
        on: true,
        own,
        mark: st.capture_mark(),
    }
}

/// `GET /rest/repositories/{id}/search?query=Q[&limit=10][&offset=0][&facet=IRI...][&pre=<b>][&post=</b>]`:
/// the statements whose literal matches a full-text query, best first, with the matching words
/// highlighted, and for each facet predicate the values the matching subjects have, counted.
fn fts_search(st: &Store, req: &Request) -> Json {
    let query = req.param("query").or(req.param("q")).unwrap_or("");
    let num = |k: &str, d: usize| {
        req.param(k)
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(d)
    };
    let (limit, offset) = (num("limit", 10).min(1000), num("offset", 0));
    let (pre, post) = (
        req.param("pre").unwrap_or("<b>"),
        req.param("post").unwrap_or("</b>"),
    );
    let p = st.fts_prepare(query);
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    for (id, score) in st.search_prepared(&p, usize::MAX) {
        for q in st.quads_matching(None, None, Some(id), None) {
            if seen.insert((q[0], q[1], id)) {
                rows.push((q[0], q[1], id, score));
            }
        }
    }
    let mut facets = Json::obj();
    for f in req
        .query
        .iter()
        .filter(|(k, _)| k == "facet")
        .map(|(_, v)| v.as_str())
    {
        let mut counts: HashMap<u32, usize> = HashMap::new();
        if let Some(pid) = st.dict.lookup(&Term::iri(f)) {
            let subjects: HashSet<u32> = rows.iter().map(|r| r.0).collect();
            for s in subjects {
                let values: HashSet<u32> = st
                    .quads_matching(Some(s), Some(pid), None, None)
                    .map(|q| q[2])
                    .collect();
                for v in values {
                    *counts.entry(v).or_default() += 1;
                }
            }
        }
        let mut counts: Vec<(Term, usize)> = counts
            .into_iter()
            .map(|(v, n)| (st.dict.term(v), n))
            .collect();
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        counts.truncate(100);
        facets.set(
            f,
            Json::Arr(
                counts
                    .into_iter()
                    .map(|(v, n)| Json::obj().with("value", v.to_string()).with("count", n))
                    .collect(),
            ),
        );
    }
    let hits = rows
        .iter()
        .skip(offset)
        .take(limit)
        .map(|&(s, pr, o, score)| {
            let lit = st.dict.term(o);
            let l = lit.as_literal().expect("full-text hits are literals");
            let lang = crate::fts::lang_of(l.lang.as_deref());
            let form = p.matching(&crate::fts::Doc::new(&l.value, lang));
            Json::obj()
                .with("subject", st.dict.term(s).to_string())
                .with("predicate", st.dict.term(pr).to_string())
                .with("literal", lit.to_string())
                .with("score", score)
                .with("highlight", form.highlight(&l.value, lang, pre, post))
        })
        .collect();
    Json::obj()
        .with("query", query)
        .with("total", rows.len())
        .with("hits", Json::Arr(hits))
        .with("facets", facets)
}

/// The SHACL report for a change, when it breaks the repository's shapes.
fn shacl_report(st: &Store, removed: &[Quad], inserted: &[Quad]) -> Option<Json> {
    let shapes = crate::shacl::Shapes::load(st)?;
    let (mut nodes, shapes_changed) = crate::shacl::affected(inserted);
    let (more, shapes_changed2) = crate::shacl::affected(removed);
    nodes.extend(more);
    let nodes = crate::shacl::with_referrers(st, nodes);
    let mut violations = if shapes_changed || shapes_changed2 {
        shapes.validate(st, None)
    } else {
        shapes.validate(st, Some(&nodes))
    };
    violations.retain(crate::shacl::blocking);
    if violations.is_empty() {
        return None;
    }
    Some(crate::shacl::report_json(&violations).with(
        "message",
        format!(
            "SHACL validation failed with {} violation(s); the change was rolled back",
            violations.len()
        ),
    ))
}

/// Ends the check without validating (the write failed).
fn shacl_abort(c: ShaclCheck, st: &mut Store) {
    if c.on && c.own {
        st.take_capture();
    }
}

/// After a successful write: validate what it touched; on violations undo
/// the write and return a 400 with the validation report.
fn shacl_end(c: ShaclCheck, st: &mut Store) -> Option<Response> {
    if !c.on {
        return None;
    }
    let (removed, inserted) = st.capture_since(c.mark);
    let report = shacl_report(st, &removed, &inserted);
    let resp = report.map(|report| {
        if let Err(e) = st.apply(&inserted, &removed) {
            return Response::text(
                500,
                format!("SHACL violation, and the rollback failed: {e}"),
            );
        }
        st.capture_truncate(c.mark);
        Response::json(400, report)
    });
    shacl_abort(c, st);
    resp
}

/// Whether the repository config enables the full-text index.
/// Whether queries see the union of all graphs as the default graph (the default), or only
/// the default graph itself (`unionDefaultGraph` = false, as in the SPARQL test suites).
fn union_default_of(config: &Json) -> bool {
    config
        .get("params")
        .and_then(|p| p.get("unionDefaultGraph"))
        .map(|v| v.str_at(&["value"]) != "false")
        .unwrap_or(true)
}

/// How long the change history for time travel is kept (`historyRetention`: a number of
/// seconds or `30m`, `12h`, `7d`; `unlimited`; empty or 0 = no history), in milliseconds
/// (0 = for ever).
fn history_of(config: &Json) -> Option<i64> {
    let v = config
        .get("params")
        .and_then(|p| p.get("historyRetention"))
        .map(|v| v.str_at(&["value"]).trim().to_string())
        .unwrap_or_default();
    parse_retention(&v)
}

fn parse_retention(v: &str) -> Option<i64> {
    if v == "unlimited" {
        return Some(0);
    }
    let (num, unit) = match v.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => (&v[..i], c),
        _ => (v, 's'),
    };
    let n: f64 = num.trim().parse().ok().filter(|n: &f64| *n > 0.0)?;
    let ms = match unit {
        's' => 1_000.0,
        'm' => 60_000.0,
        'h' => 3_600_000.0,
        'd' => 86_400_000.0,
        _ => return None,
    };
    Some((n * ms).max(1.0) as i64)
}

/// An `as-of` time: Unix milliseconds, or an xsd:dateTime / xsd:date (UTC when it has no
/// timezone).
pub fn parse_as_of(v: &str) -> Result<i64, String> {
    if let Ok(ms) = v.trim().parse::<i64>() {
        return Ok(ms);
    }
    let dt = sparql::func::parse_datetime(v).ok_or_else(|| {
        format!("Protocol error: as-of {v:?} is not a dateTime or Unix milliseconds")
    })?;
    Ok((dt.epoch_seconds() * 1000.0).round() as i64)
}

fn fts_of(config: &Json) -> bool {
    config
        .get("params")
        .and_then(|p| p.get("enableFtsIndex"))
        .map(|v| v.str_at(&["value"]) == "true")
        .unwrap_or(false)
}

/// (ruleset, owl:sameAs enabled) from a repository config.
fn ruleset_of(config: &Json) -> (String, bool) {
    let params = config.get("params");
    let value = |k: &str| {
        params
            .and_then(|p| p.get(k))
            .map(|v| v.str_at(&["value"]).to_string())
            .unwrap_or_default()
    };
    let mut ruleset = value("ruleset");
    if ruleset.is_empty() {
        ruleset = "empty".into();
    }
    // A path ending in ".pie" is a custom rule file; anything else
    // names a built-in ruleset (a "builtin:" prefix is accepted too).
    let ruleset = ruleset.trim_start_matches("builtin:").to_string();
    (ruleset, value("disableSameAs") != "true")
}

/// What a request needs to be allowed.
#[derive(Debug, Clone, PartialEq)]
enum Need {
    Public,
    /// Any signed-in user.
    User,
    Read(String),
    Write(String),
    /// Create, configure or delete repositories.
    Manage,
    /// Manage users and security.
    Admin,
}

#[derive(Clone)]
struct User {
    salt: String,
    hash: String,
    authorities: Vec<String>,
}

/// Users, roles and tokens. Credentials: HTTP Basic or
/// `Authorization: Bearer <token>` from `POST /rest/login`. Authorities are
/// ROLE_ADMIN, ROLE_REPO_MANAGER, ROLE_USER, READ_REPO_<id> and
/// WRITE_REPO_<id> (`*` matches every repository).
pub struct Auth {
    bootstrap: (String, String),
    users: Mutex<BTreeMap<String, User>>,
    /// session token -> user and when it was issued
    tokens: Mutex<HashMap<String, (String, std::time::Instant)>>,
    /// user -> failed password checks and when the first of them happened
    failures: Mutex<HashMap<String, (u32, std::time::Instant)>>,
    path: Mutex<Option<PathBuf>>,
    /// Digest of an already verified Basic header -> user, so the slow
    /// password hash runs once per credential, not once per request.
    basic_ok: Mutex<HashMap<[u8; 32], String>>,
    /// API keys by id (`grd_<id>_<secret>`; only a digest of the secret is kept).
    apikeys: Mutex<BTreeMap<String, ApiKey>>,
    /// Access tokens of an OpenID Connect provider (`serve --oidc-issuer`).
    oidc: Option<crate::oidc::Provider>,
    /// A directory checking the passwords of users that are not local (`serve --ldap-url`).
    ldap: Option<crate::ldap::Directory>,
    /// Directory users signed in, with their authorities and when the directory said so.
    external: Mutex<HashMap<String, (User, std::time::Instant)>>,
}

/// How long a directory sign-in is trusted before the password is checked with it again.
const EXTERNAL_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// A key a program uses instead of a password; it acts as its user.
#[derive(Clone)]
struct ApiKey {
    user: String,
    name: String,
    digest: String,
    created: i64,
}

/// Session tokens from /rest/login last this long.
const TOKEN_LIFETIME: std::time::Duration = std::time::Duration::from_secs(12 * 3600);
/// This many wrong passwords for a user within LOCKOUT lock it for LOCKOUT.
const MAX_FAILURES: u32 = 10;
const LOCKOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 32 random bytes as hex: from the operating system's generator, or (where there is none)
/// a digest of the time, the process and a counter.
fn random_hex() -> String {
    use std::io::Read;
    let mut b = [0u8; 32];
    if fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_ok()
    {
        return hex(&b);
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let local = 0u8;
    hex(&crate::sparql::hash::sha256(
        format!("{nanos}:{n}:{}:{:p}", std::process::id(), &local).as_bytes(),
    ))
}

fn password_hash(salt: &str, password: &str) -> String {
    // Iterated salted SHA-256 (no external crypto crates are available).
    let mut h = crate::sparql::hash::sha256(format!("{salt}:{password}").as_bytes());
    for _ in 0..10_000 {
        let mut buf = h.to_vec();
        buf.extend_from_slice(salt.as_bytes());
        h = crate::sparql::hash::sha256(&buf);
    }
    hex(&h)
}

impl User {
    fn new(password: &str, authorities: Vec<String>) -> User {
        let salt = random_hex()[..16].to_string();
        User {
            hash: password_hash(&salt, password),
            salt,
            authorities,
        }
    }

    fn has(&self, a: &str) -> bool {
        self.authorities.iter().any(|x| x == a)
    }

    fn allows(&self, need: &Need) -> bool {
        if self.has("ROLE_ADMIN") {
            return true;
        }
        let manager = self.has("ROLE_REPO_MANAGER");
        let repo_right = |kind: &str, repo: &str| {
            self.has(&format!("{kind}_REPO_{repo}")) || self.has(&format!("{kind}_REPO_*"))
        };
        match need {
            Need::Public | Need::User => true,
            Need::Read(r) => manager || repo_right("READ", r) || repo_right("WRITE", r),
            Need::Write(r) => manager || repo_right("WRITE", r),
            Need::Manage => manager,
            Need::Admin => false,
        }
    }

    fn json(&self, name: &str) -> Json {
        Json::obj()
            .with("username", name)
            .with(
                "grantedAuthorities",
                Json::Arr(self.authorities.iter().map(Json::from).collect()),
            )
            .with(
                "authorities",
                Json::Arr(self.authorities.iter().map(Json::from).collect()),
            )
            .with("appSettings", app_settings())
            .with("external", false)
    }
}

impl Auth {
    /// Security on, with this administrator guaranteed to exist.
    /// Also accept the access tokens of this OpenID Connect provider.
    pub fn with_oidc(mut self, p: crate::oidc::Provider) -> Auth {
        self.oidc = Some(p);
        self
    }

    /// Also check the passwords of users that are not local with this directory.
    pub fn with_ldap(mut self, d: crate::ldap::Directory) -> Auth {
        self.ldap = Some(d);
        self
    }

    /// A local user, or a directory user signed in a moment ago.
    fn lookup(&self, name: &str, ttl: std::time::Duration) -> Option<User> {
        if let Some(u) = lock(&self.users).get(name).cloned() {
            return Some(u);
        }
        lock(&self.external)
            .get(name)
            .filter(|(_, at)| at.elapsed() < ttl)
            .map(|(u, _)| u.clone())
    }

    /// Users and sign-in, seeded with one administrator.
    pub fn new(user: &str, password: &str) -> Auth {
        Auth {
            bootstrap: (user.into(), password.into()),
            users: Mutex::new(BTreeMap::new()),
            tokens: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
            path: Mutex::new(None),
            basic_ok: Mutex::new(HashMap::new()),
            apikeys: Mutex::new(BTreeMap::new()),
            oidc: None,
            ldap: None,
            external: Mutex::new(HashMap::new()),
        }
    }

    fn load(&self, path: Option<&Path>) {
        let mut users = BTreeMap::new();
        if let Some(Json::Arr(list)) = path
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|t| Json::parse(&t).ok())
        {
            for u in list {
                let auths = u
                    .get("authorities")
                    .and_then(|a| a.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                users.insert(
                    u.str_at(&["username"]).to_string(),
                    User {
                        salt: u.str_at(&["salt"]).into(),
                        hash: u.str_at(&["hash"]).into(),
                        authorities: auths,
                    },
                );
            }
        }
        let (name, pass) = &self.bootstrap;
        let d = |s: &str| crate::sparql::hash::sha256(s.as_bytes());
        let ok = users.get(name).is_some_and(|u: &User| {
            d(&u.hash) == d(&password_hash(&u.salt, pass)) && u.has("ROLE_ADMIN")
        });
        // (without --auth, with OpenID Connect only, there is no local administrator)
        if !ok && !name.is_empty() {
            users.insert(
                name.clone(),
                User::new(pass, vec!["ROLE_ADMIN".into(), "ROLE_USER".into()]),
            );
        }
        *lock(&self.users) = users;
        *lock(&self.path) = path.map(Path::to_path_buf);
        self.save();
        self.load_keys();
    }

    fn keys_path(&self) -> Option<PathBuf> {
        lock(&self.path)
            .as_ref()
            .map(|p| p.with_file_name("apikeys.json"))
    }

    fn load_keys(&self) {
        let Some(path) = self.keys_path() else { return };
        let Some(Json::Arr(list)) = fs::read_to_string(path)
            .ok()
            .and_then(|t| Json::parse(&t).ok())
        else {
            return;
        };
        let mut keys = lock(&self.apikeys);
        for k in list {
            keys.insert(
                k.str_at(&["id"]).to_string(),
                ApiKey {
                    user: k.str_at(&["user"]).into(),
                    name: k.str_at(&["name"]).into(),
                    digest: k.str_at(&["digest"]).into(),
                    created: k.get("created").and_then(|v| v.as_f64()).unwrap_or(0.0) as i64,
                },
            );
        }
    }

    fn save_keys(&self) {
        let Some(path) = self.keys_path() else { return };
        let list: Vec<Json> = lock(&self.apikeys)
            .iter()
            .map(|(id, k)| {
                Json::obj()
                    .with("id", id.as_str())
                    .with("user", k.user.as_str())
                    .with("name", k.name.as_str())
                    .with("digest", k.digest.as_str())
                    .with("created", k.created)
            })
            .collect();
        let tmp = path.with_extension("tmp");
        if fs::write(&tmp, Json::Arr(list).to_string()).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }

    /// The user an API key belongs to.
    fn key_user(&self, key: &str) -> Option<(String, User)> {
        let rest = key.trim().strip_prefix("grd_")?;
        let (id, secret) = rest.split_once('_')?;
        let k = lock(&self.apikeys).get(id).cloned()?;
        let d = |s: &str| crate::sparql::hash::sha256(s.as_bytes());
        // digests compared as digests, so the comparison takes the same time for every key
        if d(&k.digest) != d(&hex(&crate::sparql::hash::sha256(secret.as_bytes()))) {
            return None;
        }
        let user = lock(&self.users).get(&k.user).cloned()?;
        Some((k.user, user))
    }

    fn save(&self) {
        let Some(path) = lock(&self.path).clone() else {
            return;
        };
        let list: Vec<Json> = lock(&self.users)
            .iter()
            .map(|(n, u)| {
                Json::obj()
                    .with("username", n.as_str())
                    .with("salt", u.salt.as_str())
                    .with("hash", u.hash.as_str())
                    .with(
                        "authorities",
                        Json::Arr(u.authorities.iter().map(Json::from).collect()),
                    )
            })
            .collect();
        let tmp = path.with_extension("tmp");
        if fs::write(&tmp, Json::Arr(list).to_string()).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }

    fn check_password(&self, user: &str, password: &str) -> Option<User> {
        // a user with too many wrong passwords is locked for a while (guessing protection)
        if lock(&self.failures)
            .get(user)
            .is_some_and(|(n, since)| *n >= MAX_FAILURES && since.elapsed() < LOCKOUT)
        {
            return None;
        }
        let mut u = lock(&self.users).get(user).cloned();
        // Compare digests of the hashes so timing does not leak where they differ; an unknown
        // user costs the same hashing as a known one.
        let d = |s: &str| crate::sparql::hash::sha256(s.as_bytes());
        let ok = match (&u, &self.ldap) {
            (Some(u), _) => d(&password_hash(&u.salt, password)) == d(&u.hash),
            // not a local user: the directory decides
            (None, Some(dir)) => match dir.authenticate(user, password) {
                Ok(authorities) => {
                    let found = User {
                        salt: String::new(),
                        hash: String::new(),
                        authorities,
                    };
                    lock(&self.external)
                        .insert(user.to_string(), (found.clone(), std::time::Instant::now()));
                    u = Some(found);
                    true
                }
                Err(e) => {
                    if !quiet() {
                        eprintln!("LDAP sign-in of {user:?} refused: {e}");
                    }
                    false
                }
            },
            (None, None) => {
                let _ = password_hash("unknown-user", password);
                false
            }
        };
        let mut failures = lock(&self.failures);
        if ok {
            failures.remove(user);
            return u;
        }
        let e = failures
            .entry(user.to_string())
            .or_insert((0, std::time::Instant::now()));
        if e.1.elapsed() >= LOCKOUT {
            *e = (0, std::time::Instant::now());
        }
        e.0 += 1;
        if e.0 == MAX_FAILURES {
            eprintln!(
                "security: {MAX_FAILURES} wrong passwords for {user:?}; locked for {} minutes",
                LOCKOUT.as_secs() / 60
            );
        }
        // keep the table small: forget old entries
        if failures.len() > 10_000 {
            failures.retain(|_, (_, since)| since.elapsed() < LOCKOUT);
        }
        None
    }

    fn issue_token(&self, user: &str) -> String {
        let token = random_hex();
        let mut tokens = lock(&self.tokens);
        tokens.retain(|_, (_, at)| at.elapsed() < TOKEN_LIFETIME);
        tokens.insert(token.clone(), (user.to_string(), std::time::Instant::now()));
        token
    }

    /// The signed-in user, if the request carries valid credentials.
    fn user(&self, req: &Request) -> Option<(String, User)> {
        if let Some(k) = req.header("x-api-key") {
            return self.key_user(k);
        }
        let h = req.header("authorization")?;
        if let Some(k) = h.strip_prefix("ApiKey ") {
            return self.key_user(k);
        }
        if let Some(b64) = h
            .strip_prefix("Basic ")
            .or_else(|| h.strip_prefix("basic "))
        {
            let key = crate::sparql::hash::sha256(b64.trim().as_bytes());
            let cached = lock(&self.basic_ok).get(&key).cloned();
            // (a directory user's cached sign-in lapses, and the password is checked again)
            if let Some(user) = cached
                .as_ref()
                .and_then(|name| self.lookup(name, EXTERNAL_TTL))
            {
                return Some((cached.unwrap(), user));
            }
            let raw = base64_decode(b64.trim())?;
            let text = String::from_utf8_lossy(&raw);
            let (u, p) = text.split_once(':').unwrap_or((&text, ""));
            let user = self.check_password(u, p)?;
            lock(&self.basic_ok).insert(key, u.to_string());
            return Some((u.to_string(), user));
        }
        let tok = h.strip_prefix("Bearer ")?.trim();
        let session = lock(&self.tokens)
            .get(tok)
            .filter(|(_, at)| at.elapsed() < TOKEN_LIFETIME)
            .map(|(n, _)| n.clone());
        if let Some(name) = session {
            let user = self.lookup(&name, TOKEN_LIFETIME)?;
            return Some((name, user));
        }
        // an identity provider's access token (a JWT)
        let provider = self.oidc.as_ref()?;
        match provider.verify(tok) {
            Ok((name, authorities)) => Some((
                name,
                User {
                    salt: String::new(),
                    hash: String::new(),
                    authorities,
                },
            )),
            Err(e) => {
                if !quiet() {
                    eprintln!("OpenID Connect token refused: {e}");
                }
                None
            }
        }
    }
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let bytes: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= val(c)? << (18 - 6 * i);
        }
        let n = chunk.len().saturating_sub(1);
        for i in 0..n {
            out.push((acc >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

/// The server: every repository, security and settings.
pub struct App {
    data: PathBuf,
    ui_dir: Option<PathBuf>,
    repos: RwLock<BTreeMap<String, Arc<Repo>>>,
    saved_queries: Mutex<Vec<Json>>,
    default_repo: Mutex<String>,
    auth: Option<Auth>,
    cluster: std::sync::OnceLock<Arc<crate::raft::Raft>>,
    /// Serializes replicated data writes so captured change sets do not mix.
    cluster_write: Mutex<()>,
    /// Refuse every change (`serve --read-only`).
    pub read_only: bool,
    /// Answer cross-origin requests (`serve --cors`).
    pub cors: bool,
    /// Time limit for queries that do not set one (`serve --timeout`).
    pub default_timeout: Option<u64>,
    /// Everything in memory, nothing on disk (`serve --memory`).
    memory: bool,
    /// The audit log (`serve --audit-log`): one JSON line per change, sign-in and refusal.
    pub audit: Option<Mutex<fs::File>>,
    /// Also log reads (`serve --audit-reads`).
    pub audit_reads: bool,
    /// The language model that turns questions into SPARQL, if any (see `configure_llm`).
    llm: RwLock<Option<Arc<crate::nlq::Llm>>>,
    /// Its settings from the command line and environment, and those saved from the UI
    /// (`llm.json` in the data directory), which take precedence.
    llm_startup: Mutex<crate::nlq::Settings>,
    llm_saved: Mutex<Option<crate::nlq::Settings>>,
}

impl App {
    /// Open a data directory (creating the layout) and load every repository.
    pub fn open(data: &Path, ui_dir: Option<PathBuf>, auth: Option<Auth>) -> std::io::Result<App> {
        let repo_root = data.join("repositories");
        fs::create_dir_all(&repo_root)?;
        if let Some(a) = &auth {
            a.load(Some(&data.join("users.json")));
        }
        let mut repos = BTreeMap::new();
        for entry in fs::read_dir(&repo_root)? {
            let path = entry?.path();
            if path.join("config.json").exists() {
                match Repo::open(&path) {
                    Ok(r) => {
                        if !quiet() {
                            eprintln!(
                                "loaded repository {} ({} statements)",
                                r.id,
                                r.store.read().len()
                            );
                        }
                        repos.insert(r.id.clone(), Arc::new(r));
                    }
                    Err(e) => eprintln!("skipping {}: {e}", path.display()),
                }
            }
        }
        let saved = fs::read_to_string(data.join("saved-queries.json"))
            .ok()
            .and_then(|t| Json::parse(&t).ok())
            .and_then(|j| j.as_array().map(|a| a.to_vec()))
            .unwrap_or_default();
        Ok(App {
            data: data.to_path_buf(),
            ui_dir,
            repos: RwLock::new(repos),
            saved_queries: Mutex::new(saved),
            default_repo: Mutex::new(String::new()),
            auth,
            cluster: std::sync::OnceLock::new(),
            cluster_write: Mutex::new(()),
            read_only: false,
            cors: false,
            default_timeout: None,
            memory: false,
            audit: None,
            audit_reads: false,
            llm: RwLock::new(None),
            llm_startup: Mutex::new(Default::default()),
            llm_saved: Mutex::new(None),
        })
    }

    /// A server that keeps everything in memory and never touches the disk; its
    /// repositories, users and saved queries end with the process (`serve --memory`).
    pub fn open_memory(ui_dir: Option<PathBuf>, auth: Option<Auth>) -> App {
        if let Some(a) = &auth {
            a.load(None);
        }
        App {
            data: PathBuf::new(),
            ui_dir,
            repos: RwLock::new(BTreeMap::new()),
            saved_queries: Mutex::new(Vec::new()),
            default_repo: Mutex::new(String::new()),
            auth,
            cluster: std::sync::OnceLock::new(),
            cluster_write: Mutex::new(()),
            read_only: false,
            cors: false,
            default_timeout: None,
            memory: true,
            audit: None,
            audit_reads: false,
            llm: RwLock::new(None),
            llm_startup: Mutex::new(Default::default()),
            llm_saved: Mutex::new(None),
        }
    }

    /// The permission a request needs.
    fn need(segs: &[&str], req: &Request) -> Need {
        let m = req.method.as_str();
        let read_or_write = |repo: &str, write: bool| {
            if write {
                Need::Write(repo.into())
            } else {
                Need::Read(repo.into())
            }
        };
        match segs {
            ["rest", "login"] | ["rest", "logout"] | ["protocol"] => Need::Public,
            ["metrics"] => Need::User,
            ["rest", "security", "all"] | ["rest", "info", "version"] if m == "GET" => Need::Public,
            ["rest", "security", "authenticated-user"] => Need::User,
            ["rest", "security", "users", name] if m == "PATCH" => {
                Need::Write(format!("\u{0}self:{name}"))
            }
            ["rest", "llm", "status"] => Need::User,
            ["rest", "security", ..]
            | ["rest", "roles", ..]
            | ["rest", "recovery", ..]
            | ["rest", "llm", ..] => Need::Admin,
            ["rest", "rank", ..] if m != "GET" => Need::Manage,
            ["rest", "repositories"] => {
                if m == "GET" {
                    Need::User
                } else {
                    Need::Manage
                }
            }
            [
                "rest",
                "repositories",
                "all" | "default-config" | "ruleset",
                ..,
            ] => Need::User,
            ["rest", "repositories", id] => {
                if m == "GET" {
                    Need::Read(id.to_string())
                } else {
                    Need::Manage
                }
            }
            ["rest", "repositories", id, "import", ..] => read_or_write(id, m != "GET"),
            ["rest", "repositories", _, "connectors", ..] if m != "GET" => Need::Manage,
            ["rest", "repositories", _, "restart"] => Need::Manage,
            ["rest", "repositories", id, "graphql"] => read_or_write(
                id,
                m == "POST" && crate::graphql::is_mutation(&graphql_query(req)),
            ),
            ["rest", "repositories", id, ..] => Need::Read(id.to_string()),
            ["rest", ..] => Need::User,
            ["repositories"] => Need::User,
            ["repositories", id] => match m {
                "GET" => Need::Read(id.to_string()),
                "POST" => {
                    let update = req.content_type() == "application/sparql-update"
                        || req.params().iter().any(|(k, _)| k == "update");
                    read_or_write(id, update)
                }
                _ => Need::Manage,
            },
            ["repositories", id, ..] => read_or_write(id, m != "GET" && m != "HEAD"),
            _ => Need::Public, // static files
        }
    }

    /// The evaluation plan of a query on a repository (`graphrd query --explain`).
    pub fn explain(
        &self,
        repo: &str,
        text: &str,
        base: &str,
        infer: bool,
    ) -> Result<String, String> {
        let repo = self
            .repo(repo)
            .ok_or_else(|| format!("no repository {repo}"))?;
        let prefixes = repo.prefix_map();
        let opts = QueryOptions {
            base,
            prefixes: &prefixes,
            dataset: None,
            timeout_secs: None,
            infer,
            at_query: false,
        };
        let st = repo.store.read();
        sparql::explain(&st, text, &opts)
    }

    /// Merge each repository's indexes into single run files (`graphrd optimize`).
    pub fn optimize(&self) -> Result<Vec<String>, String> {
        let mut done = Vec::new();
        for repo in rl(&self.repos).values() {
            repo.store
                .write()
                .optimize()
                .map_err(|e| format!("{}: {e}", repo.id))?;
            done.push(repo.id.clone());
        }
        Ok(done)
    }

    /// Offline bulk loading (`graphrd load`, see `Store::bulk_insert`). None when the
    /// repository validates SHACL: its statements must take the checked path.
    pub fn bulk_insert(
        &self,
        id: &str,
        chunks: Vec<Vec<Quad>>,
        bnodes: &mut HashMap<String, String>,
    ) -> Result<Option<usize>, String> {
        let repo = self.repo(id).ok_or_else(|| format!("no repository {id}"))?;
        if shacl_of(&rl(&repo.config)) {
            return Ok(None);
        }
        let n = repo
            .store
            .write()
            .bulk_insert(chunks, bnodes)
            .map_err(|e| e.to_string())?;
        crate::metrics::STATEMENTS_LOADED.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(Some(n))
    }

    /// Point-in-time restore of one repository (see `Store::roll_back_to`).
    pub fn roll_back(&self, id: &str, at: i64) -> Result<(usize, usize), String> {
        let repo = self.repo(id).ok_or_else(|| format!("no repository {id}"))?;
        let r = repo.store.write().roll_back_to(at);
        r
    }

    /// The ids of the repositories.
    pub fn repository_ids(&self) -> Vec<String> {
        rl(&self.repos).keys().cloned().collect()
    }

    /// Whether writes to a repository are validated with SHACL.
    pub fn validates_shacl(&self, id: &str) -> bool {
        self.repo(id).is_some_and(|r| shacl_of(&rl(&r.config)))
    }

    /// Remember the prefixes a Turtle or TriG document declares (as uploads do).
    pub fn learn_prefixes(&self, id: &str, text: &str, format: Format) {
        if let Some(r) = self.repo(id) {
            r.learn_prefixes(text, format);
        }
    }

    // ------------------------------------------------------------ audit log

    fn audit_request(&self, req: &Request, resp: &Response, took: std::time::Duration) {
        let Some(file) = &self.audit else { return };
        let p = req.path.as_str();
        let api = p.starts_with("/rest")
            || p.starts_with("/repositories")
            || p == "/sparql"
            || p == "/query"
            || p == "/update"
            || p == "/metrics"
            || p == "/mcp";
        let sign_in = p == "/rest/login" || p == "/rest/logout";
        let change = !matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS");
        let refused = resp.status == 401 || resp.status == 403;
        let update = req
            .params()
            .iter()
            .find(|(k, _)| k == "update")
            .map(|(_, v)| v.clone())
            .or_else(|| {
                (req.content_type() == "application/sparql-update").then(|| req.body_text())
            });
        // a query sent by POST is a read
        let read_by_post = change
            && update.is_none()
            && (req.content_type() == "application/sparql-query"
                || req.params().iter().any(|(k, _)| k == "query"));
        if !api || !(sign_in || refused || (change && !read_by_post) || self.audit_reads) {
            return;
        }
        // who: verified for an accepted request; for a refused one only the name it claimed
        // (checking the password again would count as another failed attempt)
        let user = if sign_in {
            Json::parse(&req.body_text())
                .ok()
                .map(|b| b.str_at(&["username"]).to_string())
                .filter(|u| !u.is_empty())
                .or_else(|| self.auth.as_ref().and_then(|a| a.user(req)).map(|u| u.0))
        } else if refused {
            req.header("authorization")
                .and_then(|h| h.strip_prefix("Basic "))
                .and_then(|b| base64_decode(b.trim()))
                .map(|raw| {
                    String::from_utf8_lossy(&raw)
                        .split(':')
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
        } else {
            self.auth.as_ref().and_then(|a| a.user(req)).map(|u| u.0)
        };
        let segs: Vec<&str> = p.trim_matches('/').split('/').collect();
        let repository = match segs.as_slice() {
            ["repositories", id, ..] | ["rest", "repositories", id, ..] => Some(id.to_string()),
            _ => None,
        };
        let mut line = Json::obj()
            .with("time", now_millis())
            .with("user", user)
            .with("method", req.method.as_str())
            .with("path", p)
            .with("repository", repository)
            .with("status", resp.status as usize)
            .with("ms", took.as_millis() as usize)
            .with("client", req.header("x-forwarded-for").map(String::from));
        if let Some(u) = update {
            line.set("update", u.chars().take(2000).collect::<String>());
        }
        let text = line.to_string() + "\n";
        use std::io::Write;
        let mut f = lock(file);
        let _ = f.write_all(text.as_bytes());
    }

    // ------------------------------------------------------------ search connectors

    /// `/rest/repositories/{id}/connectors[/{name}]`: list, read, create (PUT) and delete
    /// search connectors (see `connector`).
    fn connectors_endpoint(&self, repo: &Arc<Repo>, rest: &[&str], req: &Request) -> Response {
        let list = || {
            repo.store
                .read()
                .connectors
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        };
        match (req.method.as_str(), rest) {
            ("GET", []) => Response::json(
                200,
                Json::Arr(list().values().map(|c| c.status_json()).collect()),
            ),
            ("GET", [name]) => match list().get(*name) {
                Some(c) => Response::json(200, c.status_json()),
                None => Response::text(404, format!("No connector {name}")),
            },
            ("PUT" | "POST", [name]) => {
                if !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    return Response::text(
                        400,
                        "A connector name has letters, digits, - and _ only",
                    );
                }
                let def = match Json::parse(&req.body_text())
                    .map_err(|e| e.to_string())
                    .and_then(|j| crate::connector::Connector::from_json(name, &j))
                {
                    Ok(d) => d,
                    Err(e) => return Response::text(400, e),
                };
                if let Err(e) = def.ensure_index() {
                    return Response::text(
                        502,
                        format!("The search engine at {} is not usable: {e}", def.url),
                    );
                }
                self.add_connector(repo, def, true);
                self.save_connectors(repo);
                Response::json(201, list()[*name].status_json())
            }
            ("DELETE", [name]) => {
                let removed = repo
                    .store
                    .read()
                    .connectors
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(*name);
                let Some(c) = removed else {
                    return Response::text(404, format!("No connector {name}"));
                };
                self.save_connectors(repo);
                if req.param("dropIndex").is_some_and(|v| v == "true") {
                    if let Err(e) = c.def.drop_index() {
                        return Response::text(
                            502,
                            format!("Removed, but dropping the index failed: {e}"),
                        );
                    }
                }
                Response::empty(204)
            }
            _ => Response::text(405, "Method not allowed"),
        }
    }

    /// Start a connector: its first sync covers every entity it indexes (when `full`).
    fn add_connector(&self, repo: &Arc<Repo>, def: crate::connector::Connector, full: bool) {
        let running = crate::connector::Running::new(def);
        let mut st = repo.store.write();
        if full {
            running
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(running.def.entities(&st));
        }
        if st.feed.is_none() {
            st.feed = Some(Default::default());
        }
        st.connectors
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(running.def.name.clone(), running);
        drop(st);
        if !repo
            .connector_thread
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let repo = repo.clone();
            let mut previous: Vec<Id> = Vec::new();
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    // a deleted repository (only this thread still holds it) stops its thread
                    if Arc::strong_count(&repo) == 1 {
                        return;
                    }
                    // the snapshot is taken after the changes are collected, so it has them all
                    let feed = repo.store.read().feed.clone();
                    let changed: Vec<Id> = feed
                        .map(|f| {
                            f.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .drain()
                                .collect()
                        })
                        .unwrap_or_default();
                    let st = repo.store.read();
                    // a writer records a subject a moment before it publishes the change: each
                    // subject is synced again in the next round, which sees it for sure
                    let mut subjects = changed.clone();
                    subjects.extend(previous.iter().copied());
                    previous = changed;
                    let conns: Vec<_> = st
                        .connectors
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .values()
                        .cloned()
                        .collect();
                    for c in conns {
                        c.round(&st, &subjects);
                    }
                }
            });
        }
    }

    fn save_connectors(&self, repo: &Repo) {
        if repo.memory {
            return;
        }
        let all = Json::Obj(
            repo.store
                .read()
                .connectors
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(n, c)| (n.clone(), c.def.to_json()))
                .collect(),
        );
        let _ = fs::write(repo.dir.join("connectors.json"), all.to_string());
    }

    /// Start the connectors saved with the repositories (after a restart they catch up with a
    /// full sync, since changes made while the server was down were not recorded for them).
    pub fn start_connectors(&self) {
        for repo in rl(&self.repos).values().cloned().collect::<Vec<_>>() {
            let Some(Json::Obj(defs)) = fs::read_to_string(repo.dir.join("connectors.json"))
                .ok()
                .and_then(|t| Json::parse(&t).ok())
            else {
                continue;
            };
            for (name, j) in defs {
                match crate::connector::Connector::from_json(&name, &j) {
                    Ok(def) => self.add_connector(&repo, def, true),
                    Err(e) => eprintln!("connector {name} of {}: {e}", repo.id),
                }
            }
        }
    }

    /// Background upkeep: a repository whose writes have stopped for a few seconds is
    /// checkpointed (its memtables become run files and its log is emptied), and the
    /// memory freed goes back to the operating system. Idle servers stay small, and a
    /// restart has no log to replay.
    pub fn start_maintenance(self: &Arc<Self>) {
        let app = Arc::downgrade(self);
        std::thread::spawn(move || {
            let mut seen: HashMap<String, u64> = HashMap::new();
            let mut last_requests = crate::metrics::responses_total();
            let mut busy = false;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let Some(app) = app.upgrade() else { return };
                let repos: Vec<Arc<Repo>> = rl(&app.repos).values().cloned().collect();
                let mut freed = false;
                for repo in repos {
                    let (version, dirty) = repo.store.read().idle_state();
                    let quiet = seen.insert(repo.id.clone(), version) == Some(version);
                    if dirty && quiet {
                        let mut st = repo.store.write();
                        // checked again under the writer lock
                        if st.idle_state() == (version, true) {
                            if let Err(e) = st.checkpoint() {
                                eprintln!("checkpoint of {}: {e}", repo.id);
                            }
                            freed = true;
                        }
                    }
                }
                // trim once the server has gone quiet after requests (an upload's body and the
                // statements parsed from it are freed by then)
                let requests = crate::metrics::responses_total();
                let quiet_now = requests == last_requests;
                if freed || (busy && quiet_now) {
                    release_memory();
                }
                busy = !quiet_now;
                last_requests = requests;
            }
        });
    }

    /// After a start: read the files queries use, so the first queries do not wait for the
    /// disk, and run one small query per repository to set up what a query needs.
    pub fn warm_up(self: &Arc<Self>) {
        let app = self.clone();
        std::thread::spawn(move || {
            let repos: Vec<Arc<Repo>> = rl(&app.repos).values().cloned().collect();
            let mut buf = vec![0u8; 1 << 20];
            for repo in &repos {
                let files = repo.store.read().warm_files();
                for f in files {
                    if let Ok(mut file) = fs::File::open(&f) {
                        use std::io::Read;
                        while matches!(file.read(&mut buf), Ok(n) if n > 0) {}
                    }
                }
                let prefixes = repo.prefix_map();
                let opts = QueryOptions {
                    base: "",
                    prefixes: &prefixes,
                    dataset: None,
                    timeout_secs: Some(5),
                    infer: true,
                    at_query: false,
                };
                let st = repo.store.read();
                let _ = sparql::query(&st, "SELECT * { ?s ?p ?o } LIMIT 1", &opts);
            }
        });
    }

    /// `GET /metrics`: Prometheus text exposition format.
    fn metrics(&self) -> Response {
        use std::fmt::Write as _;
        let mut out = String::new();
        crate::metrics::render(&mut out);
        let repos = rl(&self.repos).clone();
        let _ = writeln!(
            out,
            "# HELP graphrd_repositories Number of repositories.\n# TYPE graphrd_repositories gauge\ngraphrd_repositories {}",
            repos.len()
        );
        let _ = writeln!(
            out,
            "# HELP graphrd_statements Statements in a repository (explicit and inferred).\n# TYPE graphrd_statements gauge"
        );
        for (id, r) in &repos {
            let _ = writeln!(
                out,
                "graphrd_statements{{repository=\"{}\"}} {}",
                crate::metrics::escape_label(id),
                r.store.read().len()
            );
        }
        if let Some(raft) = self.cluster.get() {
            let s = raft.status();
            let leader = (s.str_at(&["role"]) == "leader") as u8;
            let term = s.get("term").and_then(|t| t.as_f64()).unwrap_or(0.0);
            let commit = s.get("commitIndex").and_then(|t| t.as_f64()).unwrap_or(0.0);
            let applied = s
                .get("appliedIndex")
                .and_then(|t| t.as_f64())
                .unwrap_or(0.0);
            let _ = writeln!(
                out,
                "# HELP graphrd_cluster_leader 1 when this node is the Raft leader.\n# TYPE graphrd_cluster_leader gauge\ngraphrd_cluster_leader {leader}"
            );
            let _ = writeln!(
                out,
                "# HELP graphrd_cluster_term Current Raft term.\n# TYPE graphrd_cluster_term gauge\ngraphrd_cluster_term {term}"
            );
            let _ = writeln!(
                out,
                "# HELP graphrd_cluster_commit_index Raft commit index.\n# TYPE graphrd_cluster_commit_index gauge\ngraphrd_cluster_commit_index {commit}"
            );
            let _ = writeln!(
                out,
                "# HELP graphrd_cluster_applied_index Raft applied index.\n# TYPE graphrd_cluster_applied_index gauge\ngraphrd_cluster_applied_index {applied}"
            );
        }
        Response::new(200, "text/plain; version=0.0.4; charset=utf-8", out)
    }

    fn repo(&self, id: &str) -> Option<Arc<Repo>> {
        rl(&self.repos).get(id).cloned()
    }

    fn save_queries(&self) {
        if self.memory {
            return;
        }
        let _ = fs::write(
            self.data.join("saved-queries.json"),
            Json::Arr(lock(&self.saved_queries).clone()).to_string(),
        );
    }

    /// One HTTP request: routing, timing, the access log and the audit trail.
    pub fn handle(&self, req: &Request) -> Response {
        let t0 = std::time::Instant::now();
        if self.cors && req.method == "OPTIONS" {
            // CORS preflight
            return Response::empty(204)
                .with_header("Access-Control-Allow-Origin", "*")
                .with_header(
                    "Access-Control-Allow-Methods",
                    "GET, HEAD, POST, PUT, DELETE, PATCH, OPTIONS",
                )
                .with_header(
                    "Access-Control-Allow-Headers",
                    req.header("access-control-request-headers")
                        .unwrap_or("Content-Type, Accept, Authorization"),
                )
                .with_header("Access-Control-Max-Age", "86400");
        }
        let mut resp = std::panic::catch_unwind(AssertUnwindSafe(|| self.route(req)))
            .unwrap_or_else(|_| Response::text(500, "internal error"));
        if self.cors {
            resp = resp.with_header("Access-Control-Allow-Origin", "*");
        }
        crate::metrics::response(resp.status);
        if self.audit.is_some() {
            self.audit_request(req, &resp, t0.elapsed());
        }
        if req.path.starts_with("/rest")
            || req.path.starts_with("/repositories")
            || resp.status >= 400 && !req.path.contains('.')
        {
            if !quiet() {
                crate::log::line(format!(
                    "{} {} -> {} ({} ms)",
                    req.method,
                    req.path,
                    resp.status,
                    t0.elapsed().as_millis()
                ));
            }
        }
        resp
    }

    /// The repository behind the standard `/sparql` endpoint: the default repository
    /// (`serve --default-repo`, or chosen in the UI), else the only one, else "default".
    fn endpoint_repo(&self) -> Option<String> {
        let chosen = lock(&self.default_repo).clone();
        let repos = rl(&self.repos);
        if repos.contains_key(&chosen) {
            return Some(chosen);
        }
        if repos.len() == 1 {
            return repos.keys().next().cloned();
        }
        repos.contains_key("default").then(|| "default".to_string())
    }

    /// The repository the standard /sparql endpoint answers.
    pub fn set_default_repo(&self, id: &str) {
        *lock(&self.default_repo) = id.to_string();
    }

    fn route(&self, req: &Request) -> Response {
        let mut segs: Vec<&str> = req
            .path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        // standard SPARQL endpoint names
        let endpoint;
        match segs.as_slice() {
            ["sparql" | "query" | "update"] => {
                let Some(id) = self.endpoint_repo() else {
                    return Response::text(
                        404,
                        "No default repository: start the server with --default-repo or use /repositories/{id}",
                    );
                };
                endpoint = id;
                segs = vec!["repositories", endpoint.as_str()];
            }
            ["repositories", _, "sparql"] => {
                segs.pop();
            }
            _ => {}
        }
        let m = req.method.as_str();
        if let ["cluster", "raft", rpc] = segs.as_slice() {
            return self.raft_rpc(rpc, req);
        }
        if let Some(raft) = self.cluster.get() {
            if matches!(segs.as_slice(), ["rest", "cluster", ..]) && m == "GET" {
                // the leader's view (replication of every member) when asked for
                if req
                    .header("x-graphrd-consistency")
                    .is_some_and(|v| v.eq_ignore_ascii_case("leader"))
                {
                    if let Some(r) = self.forward_to_leader(req) {
                        return r;
                    }
                }
                return Response::json(200, raft.status());
            }
        }
        if self.read_only
            && !matches!(m, "GET" | "HEAD")
            && matches!(
                Self::need(&segs, req),
                Need::Write(_) | Need::Manage | Need::Admin
            )
        {
            return Response::json(403, Json::obj().with("message", "This server is read-only"));
        }
        if let Some(auth) = &self.auth {
            let need = Self::need(&segs, req);
            if need != Need::Public {
                match auth.user(req) {
                    None => {
                        let resp = Response::json(
                            401,
                            Json::obj().with("message", "Authentication required"),
                        );
                        // Browsers show a native prompt for Basic challenges; the UI handles 401 itself.
                        // Scripted browser requests (fetch, XHR) say so in Sec-Fetch-Mode; only a page
                        // the person opens (navigate) or a client without it gets the challenge.
                        let scripted = req
                            .header("sec-fetch-mode")
                            .is_some_and(|m| !m.eq_ignore_ascii_case("navigate"));
                        return if scripted
                            || req.header("x-graphrd-ui").is_some()
                            || req.header("x-requested-with").is_some()
                        {
                            resp
                        } else {
                            resp.with_header("WWW-Authenticate", "Basic realm=\"GraphRD\"")
                        };
                    }
                    Some((name, user)) => {
                        let own_password =
                            matches!(&need, Need::Write(w) if *w == format!("\u{0}self:{name}"));
                        if !own_password && !user.allows(&need) {
                            return Response::json(
                                403,
                                Json::obj()
                                    .with("message", format!("{name} is not allowed to do this")),
                            );
                        }
                    }
                }
            }
        }
        // a read that must see every acknowledged write is answered by the leader
        if self.cluster.get().is_some()
            && matches!(m, "GET" | "HEAD")
            && req
                .header("x-graphrd-consistency")
                .is_some_and(|v| v.eq_ignore_ascii_case("leader"))
        {
            if let Some(r) = self.forward_to_leader(req) {
                return r;
            }
        }
        if self.cluster.get().is_some() && !matches!(m, "GET" | "HEAD") {
            let need = Self::need(&segs, req);
            if matches!(need, Need::Write(_) | Need::Manage) {
                return self.clustered_write(&segs, req, need);
            }
        }
        self.dispatch(&segs, req)
    }

    fn dispatch(&self, segs: &[&str], req: &Request) -> Response {
        let m = req.method.as_str();
        match segs {
            ["protocol"] => Response::text(200, "12"),
            ["metrics"] if m == "GET" => self.metrics(),
            ["repositories"] => self.list_repositories_sparql(req),
            ["repositories", id, rest @ ..] => {
                let Some(repo) = self.repo(id) else {
                    if m == "PUT" && rest.is_empty() {
                        return self.create_repo_from_ttl(id, req);
                    }
                    return Response::text(404, format!("Unknown repository: {id}"));
                };
                self.repository_endpoint(&repo, rest, req)
            }
            ["rest", rest @ ..] => self.rest(rest, req),
            _ => self.static_file(&req.path),
        }
    }

    // ------------------------------------------------------------ static UI

    fn static_file(&self, path: &str) -> Response {
        // GraphRD's own UI is compiled into the binary.
        match path {
            "/ui/app.js" => {
                return Response::new(200, "application/javascript;charset=UTF-8", UI_JS)
                    .with_header("Cache-Control", "no-cache");
            }
            "/ui/style.css" => {
                return Response::new(200, "text/css;charset=UTF-8", UI_CSS)
                    .with_header("Cache-Control", "no-cache");
            }
            "/ui" | "/ui/" => return Response::new(200, "text/html;charset=UTF-8", UI_HTML),
            _ => {}
        }
        let Some(root) = &self.ui_dir else {
            let last = path.rsplit('/').next().unwrap_or("");
            if last.contains('.') {
                return Response::text(404, "Not found");
            }
            return Response::new(200, "text/html;charset=UTF-8", UI_HTML);
        };
        let rel: PathBuf = Path::new(path.trim_start_matches('/'))
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s),
                _ => None,
            })
            .collect();
        let mut file = root.join(&rel);
        if rel.as_os_str().is_empty() || !file.is_file() {
            // SPA routes fall back to index.html; missing assets are 404.
            let last = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if last.contains('.') && !last.ends_with(".html") {
                return Response::text(404, "Not found");
            }
            file = root.join("index.html");
        }
        match fs::read(&file) {
            Ok(body) => {
                let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
                let ct = match ext {
                    "html" => "text/html;charset=UTF-8",
                    "js" | "mjs" => "application/javascript;charset=UTF-8",
                    "css" => "text/css;charset=UTF-8",
                    "json" | "map" => "application/json;charset=UTF-8",
                    "svg" => "image/svg+xml",
                    "png" => "image/png",
                    "jpg" | "jpeg" => "image/jpeg",
                    "gif" => "image/gif",
                    "ico" => "image/x-icon",
                    "woff" => "font/woff",
                    "woff2" => "font/woff2",
                    "ttf" => "font/ttf",
                    "eot" => "application/vnd.ms-fontobject",
                    "txt" => "text/plain;charset=UTF-8",
                    "xml" => "application/xml",
                    _ => "application/octet-stream",
                };
                let cache = if ext == "html" {
                    "no-cache"
                } else {
                    "max-age=3600"
                };
                Response::new(200, ct, body).with_header("Cache-Control", cache)
            }
            Err(_) => Response::text(404, "Not found"),
        }
    }

    // ------------------------------------------------------------ repository protocol

    fn repo_json(&self, r: &Repo, req: &Request) -> Json {
        let host = req.header("host").unwrap_or("localhost:7200");
        let uri = format!("http://{host}/repositories/{}", r.id);
        let cfg = rl(&r.config);
        Json::obj()
            .with("id", r.id.as_str())
            .with("title", cfg.str_at(&["title"]))
            .with("uri", uri.as_str())
            .with("externalUrl", uri.as_str())
            .with("local", true)
            .with("type", "graphrd")
            .with("location", "")
            .with("readable", true)
            .with(
                "writable",
                cfg.get("params")
                    .and_then(|p| p.get("readOnly"))
                    .map(|v| v.str_at(&["value"]) != "true")
                    .unwrap_or(true),
            )
            .with("unsupported", false)
            .with("state", "RUNNING")
    }

    fn list_repositories_sparql(&self, req: &Request) -> Response {
        let host = req.header("host").unwrap_or("localhost:7200");
        let vars: Vec<String> = ["uri", "id", "title", "readable", "writable"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let rows: Vec<Vec<Option<Term>>> = rl(&self.repos)
            .values()
            .map(|r| {
                vec![
                    Some(Term::iri(format!("http://{host}/repositories/{}", r.id))),
                    Some(Term::string(r.id.clone())),
                    Some(Term::string(r.title())),
                    Some(Term::boolean(true)),
                    Some(Term::boolean(true)),
                ]
            })
            .collect();
        let f = sparql::negotiate_results(req.header("accept").unwrap_or(""));
        Response::new(
            200,
            f.media_type(),
            sparql::write_solutions(&vars, &rows, f),
        )
    }

    fn repository_endpoint(&self, repo: &Arc<Repo>, rest: &[&str], req: &Request) -> Response {
        let m = req.method.as_str();
        match (m, rest) {
            ("GET" | "POST", []) => self.sparql_endpoint(repo, req),
            ("DELETE", []) => self.delete_repo(&repo.id),
            (_, ["statements"]) => self.statements(repo, req),
            ("GET", ["size"]) => {
                let st = repo.store.read();
                let ctxs = self.context_params(&st, req, "context");
                let n = match ctxs {
                    None => st.len(),
                    Some(list) => list
                        .iter()
                        .map(|g| g.map_or(0, |g| st.count_graph(Some(g))))
                        .sum(),
                };
                Response::text(200, n.to_string())
            }
            ("GET", ["contexts"]) => {
                let st = repo.store.read();
                let rows: Vec<Vec<Option<Term>>> = st
                    .graphs()
                    .into_iter()
                    .map(|g| vec![Some(st.dict.term(g).clone())])
                    .collect();
                let f = sparql::negotiate_results(req.header("accept").unwrap_or(""));
                Response::new(
                    200,
                    f.media_type(),
                    sparql::write_solutions(&["contextID".to_string()], &rows, f),
                )
            }
            ("GET", ["namespaces"]) => {
                let rows: Vec<Vec<Option<Term>>> = rl(&repo.namespaces)
                    .iter()
                    .map(|(p, n)| {
                        vec![Some(Term::string(p.clone())), Some(Term::string(n.clone()))]
                    })
                    .collect();
                let f = sparql::negotiate_results(req.header("accept").unwrap_or(""));
                Response::new(
                    200,
                    f.media_type(),
                    sparql::write_solutions(
                        &["prefix".to_string(), "namespace".to_string()],
                        &rows,
                        f,
                    ),
                )
            }
            ("DELETE", ["namespaces"]) => {
                wl(&repo.namespaces).clear();
                let _ = repo.save_namespaces();
                Response::empty(204)
            }
            (_, ["namespaces", prefix]) => {
                let prefix = prefix.to_string();
                match m {
                    "GET" => match rl(&repo.namespaces).iter().find(|(p, _)| *p == prefix) {
                        Some((_, n)) => Response::text(200, n.clone()),
                        None => Response::text(404, "Undefined prefix"),
                    },
                    "PUT" | "POST" => {
                        let ns = req.body_text().trim().to_string();
                        if ns.is_empty() {
                            return Response::text(400, "No namespace name found in request body");
                        }
                        let mut list = wl(&repo.namespaces);
                        match list.iter_mut().find(|(p, _)| *p == prefix) {
                            Some(e) => e.1 = ns,
                            None => list.push((prefix, ns)),
                        }
                        drop(list);
                        let _ = repo.save_namespaces();
                        Response::empty(204)
                    }
                    "DELETE" => {
                        wl(&repo.namespaces).retain(|(p, _)| *p != prefix);
                        let _ = repo.save_namespaces();
                        Response::empty(204)
                    }
                    _ => Response::text(405, "Method not allowed"),
                }
            }
            (_, ["rdf-graphs", "service"]) => self.graph_store(repo, req),
            (_, ["rdf-graphs", name]) => {
                let host = req.header("host").unwrap_or("localhost:7200");
                let iri = format!("http://{host}/repositories/{}/rdf-graphs/{name}", repo.id);
                self.graph_store_target(repo, req, Some(Term::iri(iri)))
            }
            ("GET", ["rdf-graphs"]) => self.repository_endpoint(repo, &["contexts"], req),
            ("POST", ["transactions"]) => Response::text(
                501,
                "Transactions are not supported; send statements directly",
            ),
            _ => Response::text(404, "Not found"),
        }
    }

    /// Parse `context` parameters: None = all; Some(list) where each entry is
    /// Some(graph id) or None for a graph that does not exist.
    fn context_params(&self, st: &Store, req: &Request, name: &str) -> Option<Vec<Option<Id>>> {
        let vals: Vec<&str> = req
            .query
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(
            vals.iter()
                .map(|v| match *v {
                    "null" | "" => Some(DEFAULT_GRAPH),
                    v => parse_nt_term(v).and_then(|t| match &t {
                        Term::Iri(i) if i == COMPAT_NIL || i == COMPAT_NIL_OLD => {
                            Some(DEFAULT_GRAPH)
                        }
                        t => st.dict.lookup(t),
                    }),
                })
                .collect(),
        )
    }

    /// The SPARQL 1.1 Protocol: queries by GET or POST (form or direct), updates by POST
    /// (form or direct); a GET without a query returns the service description.
    fn sparql_endpoint(&self, repo: &Arc<Repo>, req: &Request) -> Response {
        let mut params = req.params();
        let count = |k: &str| params.iter().filter(|(n, _)| n == k).count();
        let (queries, updates) = (count("query"), count("update"));
        if queries > 1 || updates > 1 || queries + updates > 1 {
            return Response::text(400, "Send exactly one query or update");
        }
        let ct = req.content_type();
        // the protocol's bodies are UTF-8
        let utf8 = req.header("content-type").is_none_or(|h| {
            h.split(';').skip(1).all(|p| {
                let p = p.trim().to_ascii_lowercase();
                p.strip_prefix("charset=")
                    .is_none_or(|c| matches!(c.trim_matches('"'), "utf-8" | "utf8" | "us-ascii"))
            })
        });
        if !params.iter().any(|(k, _)| k == "baseURI") {
            // the service-defined base IRI is the endpoint itself
            let host = req.header("host").unwrap_or("localhost");
            params.push(("baseURI".into(), format!("http://{host}{}", req.path)));
        }
        match req.method.as_str() {
            "GET" | "HEAD" => {
                if updates > 0 {
                    return Response::text(405, "Updates must be sent with POST")
                        .with_header("Allow", "POST");
                }
                match params.iter().find(|(k, _)| k == "query") {
                    Some((_, q)) => {
                        let q = q.clone();
                        self.run_query(repo, &q, &params, req.header("accept").unwrap_or(""))
                    }
                    None => self.service_description(repo, req),
                }
            }
            "POST" => match ct.as_str() {
                "application/sparql-update" | "application/sparql-query" if !utf8 => {
                    Response::text(415, "The request body must be UTF-8")
                }
                "application/sparql-update" if queries + updates == 0 => {
                    self.run_update(repo, &req.body_text(), &params)
                }
                "application/sparql-query" if queries + updates == 0 => self.run_query(
                    repo,
                    &req.body_text(),
                    &params,
                    req.header("accept").unwrap_or(""),
                ),
                "application/sparql-update" | "application/sparql-query" => Response::text(
                    400,
                    "A query or update goes in the body or a parameter, not both",
                ),
                "application/x-www-form-urlencoded" => {
                    if let Some((_, u)) = params.iter().find(|(k, _)| k == "update") {
                        let u = u.clone();
                        return self.run_update(repo, &u, &params);
                    }
                    match params.iter().find(|(k, _)| k == "query") {
                        Some((_, q)) => {
                            let q = q.clone();
                            self.run_query(repo, &q, &params, req.header("accept").unwrap_or(""))
                        }
                        None => Response::text(400, "Missing parameter: query or update"),
                    }
                }
                _ => Response::text(
                    415,
                    "Send application/x-www-form-urlencoded, application/sparql-query or application/sparql-update",
                ),
            },
            _ => Response::text(405, "Use GET or POST").with_header("Allow", "GET, POST"),
        }
    }

    /// SPARQL 1.1 Service Description of a repository's endpoint.
    fn service_description(&self, repo: &Arc<Repo>, req: &Request) -> Response {
        const SD: &str = "http://www.w3.org/ns/sparql-service-description#";
        const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
        let host = req.header("host").unwrap_or("localhost");
        let endpoint = Term::iri(format!("http://{host}{}", req.path));
        let sd = |l: &str| Term::iri(format!("{SD}{l}"));
        let service = Term::fresh_blank();
        let mut q = Vec::new();
        let mut add = |s: &Term, p: Term, o: Term| {
            q.push(Quad {
                s: s.clone(),
                p,
                o,
                g: None,
            })
        };
        add(&service, Term::iri(RDF_TYPE), sd("Service"));
        add(&service, sd("endpoint"), endpoint);
        for l in ["SPARQL10Query", "SPARQL11Query", "SPARQL11Update"] {
            add(&service, sd("supportedLanguage"), sd(l));
        }
        for f in ["BasicFederatedQuery", "EmptyGraphs"] {
            add(&service, sd("feature"), sd(f));
        }
        let config = rl(&repo.config).clone();
        if union_default_of(&config) {
            add(&service, sd("feature"), sd("UnionDefaultGraph"));
        }
        let (ruleset, _) = ruleset_of(&config);
        let regime = match ruleset.as_str() {
            "" | "empty" => "Simple",
            r if r.starts_with("owl") => "OWL-RDF-Based",
            _ => "RDFS",
        };
        add(
            &service,
            sd("defaultEntailmentRegime"),
            Term::iri(format!("http://www.w3.org/ns/entailment/{regime}")),
        );
        let fmt = |f: &str| Term::iri(format!("http://www.w3.org/ns/formats/{f}"));
        for f in [
            "SPARQL_Results_JSON",
            "SPARQL_Results_XML",
            "SPARQL_Results_CSV",
            "SPARQL_Results_TSV",
            "Turtle",
            "N-Triples",
            "N-Quads",
            "TriG",
            "RDF_XML",
            "JSON-LD",
        ] {
            add(&service, sd("resultFormat"), fmt(f));
        }
        for f in [
            "Turtle",
            "N-Triples",
            "N-Quads",
            "TriG",
            "RDF_XML",
            "JSON-LD",
        ] {
            add(&service, sd("inputFormat"), fmt(f));
        }
        let dataset = Term::fresh_blank();
        add(&service, sd("defaultDataset"), dataset.clone());
        add(&dataset, Term::iri(RDF_TYPE), sd("Dataset"));
        let graph = Term::fresh_blank();
        add(&dataset, sd("defaultGraph"), graph.clone());
        add(&graph, Term::iri(RDF_TYPE), sd("Graph"));
        add(
            &graph,
            Term::iri("http://rdfs.org/ns/void#triples"),
            Term::integer(repo.store.read().len() as i64),
        );
        let f = sparql::negotiate_rdf(req.header("accept").unwrap_or("text/turtle"));
        let ns = vec![
            ("sd".to_string(), SD.to_string()),
            ("void".to_string(), "http://rdfs.org/ns/void#".to_string()),
        ];
        match sparql::write_graph(&q, f, &ns) {
            Ok(body) => Response::new(200, &format!("{};charset=UTF-8", f.media_type()), body),
            Err(e) => Response::text(500, e),
        }
    }

    fn run_query(
        &self,
        repo: &Arc<Repo>,
        query: &str,
        params: &[(String, String)],
        accept: &str,
    ) -> Response {
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let prefixes = repo.prefix_map();
        let default: Vec<String> = params
            .iter()
            .filter(|(k, _)| k == "default-graph-uri")
            .map(|(_, v)| v.clone())
            .collect();
        let named: Vec<String> = params
            .iter()
            .filter(|(k, _)| k == "named-graph-uri")
            .map(|(_, v)| v.clone())
            .collect();
        let dataset = (!default.is_empty() || !named.is_empty())
            .then(|| sparql::ast::Dataset { default, named });
        let timeout = get("timeout")
            .and_then(|t| t.parse().ok())
            .or(self.default_timeout);
        let base = get("baseURI").unwrap_or_default();
        let infer = get("infer").is_none_or(|v| v != "false");
        // infer=at-query: inference at query time, from the schema (see sparql::reasoning)
        let at_query = get("infer").is_some_and(|v| v == "at-query");
        let opts = QueryOptions {
            base: &base,
            prefixes: &prefixes,
            dataset,
            timeout_secs: timeout,
            infer,
            at_query,
        };
        let t0 = std::time::Instant::now();
        let result = {
            let st = repo.store.read();
            match get("as-of") {
                // time travel: the query runs on the repository as it was then
                Some(t) => parse_as_of(&t)
                    .and_then(|at| st.as_of(at).map_err(|e| format!("Protocol error: {e}")))
                    .and_then(|past| sparql::query(&past, query, &opts)),
                None => sparql::query(&st, query, &opts),
            }
        };
        crate::metrics::finished("query", &repo.id, query, t0.elapsed(), result.is_ok());
        match result {
            Err(e) => Response::text(if is_client_error(&e) { 400 } else { 500 }, e),
            Ok(QueryResult::Solutions { vars, mut rows }) => {
                // Extension: limit/offset request parameters page the result.
                let offset: usize = get("offset").and_then(|v| v.parse().ok()).unwrap_or(0);
                if offset > 0 {
                    rows.drain(..offset.min(rows.len()));
                }
                if let Some(limit) = get("limit").and_then(|v| v.parse::<usize>().ok()) {
                    if limit > 0 {
                        rows.truncate(limit);
                    }
                }
                let f = sparql::negotiate_results(accept);
                let mt = if accept.contains("application/x-sparqlstar-results+json")
                    && f == sparql::ResultFormat::Json
                {
                    "application/x-sparqlstar-results+json"
                } else {
                    f.media_type()
                };
                Response::new(
                    200,
                    &format!("{mt};charset=UTF-8"),
                    sparql::write_solutions(&vars, &rows, f),
                )
            }
            Ok(QueryResult::Boolean(b)) => {
                let f = sparql::negotiate_results(accept);
                Response::new(200, f.media_type(), sparql::write_boolean(b, f))
            }
            Ok(QueryResult::Graph(quads)) => {
                let f = sparql::negotiate_rdf(accept);
                let ns = rl(&repo.namespaces).clone();
                match sparql::write_graph(&quads, f, &ns) {
                    Ok(body) => {
                        Response::new(200, &format!("{};charset=UTF-8", f.media_type()), body)
                    }
                    Err(e) => Response::text(500, e),
                }
            }
        }
    }

    fn run_update(&self, repo: &Arc<Repo>, update: &str, params: &[(String, String)]) -> Response {
        let base = params
            .iter()
            .find(|(k, _)| k == "baseURI")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let prefixes = repo.prefix_map();
        let mut st = repo.store.write();
        let t0 = std::time::Instant::now();
        let using: Vec<String> = params
            .iter()
            .filter(|(k, _)| k == "using-graph-uri")
            .map(|(_, v)| v.clone())
            .collect();
        let using_named: Vec<String> = params
            .iter()
            .filter(|(k, _)| k == "using-named-graph-uri")
            .map(|(_, v)| v.clone())
            .collect();
        let dataset =
            (!using.is_empty() || !using_named.is_empty()).then(|| sparql::ast::Dataset {
                default: using,
                named: using_named,
            });
        let result = transaction(repo, &mut st, |st| {
            let r = sparql::update_with_dataset(st, update, &base, &prefixes, dataset);
            crate::metrics::finished("update", &repo.id, update, t0.elapsed(), r.is_ok());
            r
        });
        match result {
            Ok(()) => Response::empty(204),
            Err(Abort::Failed(e)) => Response::text(if is_client_error(&e) { 400 } else { 500 }, e),
            Err(Abort::Shacl(report)) => Response::json(400, report),
            Err(Abort::Undo(e)) => Response::text(500, e),
        }
    }

    /// `POST /rest/repositories/{id}/graphql` with a mutation: one transaction.
    fn graphql_mutation(&self, repo: &Arc<Repo>, query: &str, vars: &Json) -> Response {
        let mut st = repo.store.write();
        let mut failure = None;
        let result = transaction(repo, &mut st, |st| {
            crate::graphql::execute_mutation(st, query, vars).map_err(|body| {
                failure = Some(body);
                String::new()
            })
        });
        match result {
            Ok(body) => Response::json(200, body),
            Err(Abort::Failed(_)) => Response::json(200, failure.unwrap_or_else(Json::obj)),
            Err(Abort::Shacl(report)) => Response::json(
                200,
                Json::obj().with(
                    "errors",
                    Json::Arr(vec![
                        Json::obj()
                            .with("message", "SHACL violation")
                            .with("extensions", report),
                    ]),
                ),
            ),
            Err(Abort::Undo(e)) => Response::text(500, e),
        }
    }

    fn statements(&self, repo: &Arc<Repo>, req: &Request) -> Response {
        let m = req.method.as_str();
        let ct = req.content_type();
        if m == "POST"
            && (ct == "application/x-www-form-urlencoded" || ct == "application/sparql-update")
        {
            let params = req.params();
            if ct == "application/sparql-update" {
                return self.run_update(repo, &req.body_text(), &params);
            }
            if let Some((_, u)) = params.iter().find(|(k, _)| k == "update") {
                return self.run_update(repo, u, &params);
            }
            return Response::text(400, "Missing parameter: update");
        }
        match m {
            "GET" => {
                let st = repo.store.read();
                let term = |k: &str| {
                    req.param(k)
                        .map(|v| parse_nt_term(v).and_then(|t| st.dict.lookup(&t)))
                };
                let (s, p, o) = (term("subj"), term("pred"), term("obj"));
                if matches!(s, Some(None)) || matches!(p, Some(None)) || matches!(o, Some(None)) {
                    return self.rdf_response(repo, Vec::new(), req);
                }
                let (s, p, o) = (s.flatten(), p.flatten(), o.flatten());
                let infer = req.param("infer").is_none_or(|v| v != "false");
                let mut quads = Vec::new();
                match self.context_params(&st, req, "context") {
                    None if infer => {
                        quads.extend(st.quads_matching(s, p, o, None).map(|q| st.decode(q)))
                    }
                    None => quads.extend(st.explicit_matching(s, p, o, None).map(|q| st.decode(q))),
                    Some(list) => {
                        for g in list.into_iter().flatten() {
                            quads.extend(st.quads_matching(s, p, o, Some(g)).map(|q| st.decode(q)));
                        }
                    }
                }
                drop(st);
                self.rdf_response(repo, quads, req)
            }
            "POST" | "PUT" => {
                let format = Format::from_media_type(&ct)
                    .or_else(|| req.param("format").and_then(Format::from_media_type))
                    .unwrap_or(Format::Turtle);
                // a plain IRI or one in angle brackets (N-Triples style) are both accepted
                let base = req
                    .param("baseURI")
                    .unwrap_or("")
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string();
                let text = req.body_text();
                let targets: Vec<Option<Term>> = req
                    .query
                    .iter()
                    .filter(|(k, _)| k == "context")
                    .map(|(_, v)| match v.as_str() {
                        "null" | "" => None,
                        v => parse_nt_term(v)
                            .filter(|t| t.value() != COMPAT_NIL && t.value() != COMPAT_NIL_OLD),
                    })
                    .collect();
                // extension: lenient=true skips statements that do not parse and reports them
                let lenient = req.param("lenient").is_some_and(|v| v == "true");
                let mut skipped = Vec::new();
                let parsed = match if lenient {
                    rdf::parse_lenient(&text, format, &base, None, &mut skipped)
                } else {
                    rdf::parse(&text, format, &base, None)
                } {
                    Ok(q) => q,
                    Err(e) => {
                        return Response::text(
                            400,
                            format!("Could not parse {}: {e}", format.media_type()),
                        );
                    }
                };
                repo.learn_prefixes(&text, format);
                let mut st = repo.store.write();
                let shacl = shacl_begin(repo, &mut st);
                if m == "PUT" {
                    let r = if targets.is_empty() {
                        st.clear_graph(None)
                    } else {
                        let mut total = Ok(0);
                        for t in &targets {
                            let gid = match t {
                                None => Some(DEFAULT_GRAPH),
                                Some(t) => st.dict.lookup(t),
                            };
                            if let Some(g) = gid {
                                total = st.clear_graph(Some(g));
                            }
                        }
                        total
                    };
                    if let Err(e) = r {
                        shacl_abort(shacl, &mut st);
                        return Response::text(500, e.to_string());
                    }
                }
                let quads: Vec<Quad> = if targets.is_empty() {
                    parsed
                } else {
                    targets
                        .iter()
                        .flat_map(|g| {
                            parsed.iter().map(move |q| Quad {
                                g: g.clone(),
                                ..q.clone()
                            })
                        })
                        .collect()
                };
                match st.insert(&quads) {
                    Ok(n) => {
                        crate::metrics::STATEMENTS_LOADED
                            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                        shacl_end(shacl, &mut st).unwrap_or_else(|| {
                            if lenient {
                                let errors: Vec<Json> = skipped
                                    .iter()
                                    .take(20)
                                    .map(|e| Json::from(e.as_str()))
                                    .collect();
                                Response::json(
                                    200,
                                    Json::obj()
                                        .with("added", n)
                                        .with("skipped", skipped.len())
                                        .with("errors", Json::Arr(errors)),
                                )
                            } else {
                                Response::empty(204)
                            }
                        })
                    }
                    Err(e) => {
                        shacl_abort(shacl, &mut st);
                        Response::text(500, e.to_string())
                    }
                }
            }
            "DELETE" => {
                let mut st = repo.store.write();
                let shacl = shacl_begin(repo, &mut st);
                let term = |st: &Store, k: &str| {
                    req.param(k)
                        .map(|v| parse_nt_term(v).and_then(|t| st.dict.lookup(&t)))
                };
                let (s, p, o) = (term(&st, "subj"), term(&st, "pred"), term(&st, "obj"));
                if matches!(s, Some(None)) || matches!(p, Some(None)) || matches!(o, Some(None)) {
                    shacl_abort(shacl, &mut st);
                    return Response::empty(204);
                }
                let (s, p, o) = (s.flatten(), p.flatten(), o.flatten());
                let graphs: Vec<Option<Id>> = match self.context_params(&st, req, "context") {
                    None => vec![None],
                    Some(list) => list.into_iter().flatten().map(Some).collect(),
                };
                let mut doomed = Vec::new();
                for g in graphs {
                    doomed.extend(st.explicit_matching(s, p, o, g).map(|q| st.decode(q)));
                }
                match st.apply(&doomed, &[]) {
                    Ok(_) => shacl_end(shacl, &mut st).unwrap_or_else(|| Response::empty(204)),
                    Err(e) => {
                        shacl_abort(shacl, &mut st);
                        Response::text(500, e.to_string())
                    }
                }
            }
            _ => Response::text(405, "Method not allowed"),
        }
    }

    fn rdf_response(&self, repo: &Arc<Repo>, quads: Vec<Quad>, req: &Request) -> Response {
        let accept = req.header("accept").unwrap_or("");
        let mut f = sparql::negotiate_rdf(accept);
        if accept.is_empty()
            || accept.contains("*/*")
                && Format::from_media_type(accept.split(',').next().unwrap_or("")).is_none()
        {
            f = if quads.iter().any(|q| q.g.is_some()) {
                Format::TriG
            } else {
                Format::Turtle
            };
        }
        let quads = if f.supports_graphs() {
            quads
        } else {
            quads.into_iter().map(|q| Quad { g: None, ..q }).collect()
        };
        let ns = rl(&repo.namespaces).clone();
        match sparql::write_graph(&quads, f, &ns) {
            Ok(body) => Response::new(200, &format!("{};charset=UTF-8", f.media_type()), body)
                .with_header(
                    "Content-Disposition",
                    &format!("attachment; filename=statements.{}", f.extension()),
                ),
            Err(e) => Response::text(500, e),
        }
    }

    /// SPARQL 1.1 Graph Store HTTP Protocol (indirect graph identification).
    fn graph_store(&self, repo: &Arc<Repo>, req: &Request) -> Response {
        let target = if req.param("default").is_some() {
            None
        } else if let Some(g) = req.param("graph") {
            Some(Term::iri(g))
        } else {
            return Response::text(400, "Specify ?default or ?graph=");
        };
        self.graph_store_target(repo, req, target)
    }

    fn graph_store_target(
        &self,
        repo: &Arc<Repo>,
        req: &Request,
        target: Option<Term>,
    ) -> Response {
        let gid = |st: &Store| match &target {
            None => Some(DEFAULT_GRAPH),
            Some(t) => st.dict.lookup(t),
        };
        match req.method.as_str() {
            "GET" => {
                let st = repo.store.read();
                let quads: Vec<Quad> = match gid(&st) {
                    Some(g) => st
                        .quads_matching(None, None, None, Some(g))
                        .map(|q| Quad {
                            g: None,
                            ..st.decode(q)
                        })
                        .collect(),
                    None => return Response::text(404, "Graph not found"),
                };
                drop(st);
                self.rdf_response(repo, quads, req)
            }
            "PUT" | "POST" => {
                let format = Format::from_media_type(&req.content_type()).unwrap_or(Format::Turtle);
                let base = target
                    .as_ref()
                    .map(|t| t.value().to_string())
                    .unwrap_or_default();
                let parsed = match rdf::parse(&req.body_text(), format, &base, None) {
                    Ok(q) => q,
                    Err(e) => return Response::text(400, e),
                };
                let quads: Vec<Quad> = parsed
                    .into_iter()
                    .map(|q| Quad {
                        g: target.clone(),
                        ..q
                    })
                    .collect();
                let mut st = repo.store.write();
                let shacl = shacl_begin(repo, &mut st);
                if req.method == "PUT" {
                    if let Some(g) = gid(&st) {
                        if let Err(e) = st.clear_graph(Some(g)) {
                            return Response::text(500, e.to_string());
                        }
                    }
                }
                match st.insert(&quads) {
                    Ok(_) => shacl_end(shacl, &mut st).unwrap_or_else(|| Response::empty(204)),
                    Err(e) => {
                        shacl_abort(shacl, &mut st);
                        Response::text(500, e.to_string())
                    }
                }
            }
            "DELETE" => {
                let mut st = repo.store.write();
                let shacl = shacl_begin(repo, &mut st);
                match gid(&st) {
                    Some(g) => match st.clear_graph(Some(g)) {
                        Ok(_) => shacl_end(shacl, &mut st).unwrap_or_else(|| Response::empty(204)),
                        Err(e) => Response::text(500, e.to_string()),
                    },
                    None => Response::text(404, "Graph not found"),
                }
            }
            _ => Response::text(405, "Method not allowed"),
        }
    }

    // ------------------------------------------------------------ repository management

    fn default_params() -> Json {
        let p = |name: &str, label: &str, value: &str| {
            Json::obj()
                .with("name", name)
                .with("label", label)
                .with("value", value)
        };
        Json::Obj(vec![
            ("title".into(), p("title", "Repository description", "")),
            (
                "ruleset".into(),
                p("ruleset", "Ruleset", "rdfsplus-optimized"),
            ),
            (
                "disableSameAs".into(),
                p("disableSameAs", "Disable owl:sameAs", "true"),
            ),
            (
                "checkForInconsistencies".into(),
                p(
                    "checkForInconsistencies",
                    "Enable consistency checks",
                    "false",
                ),
            ),
            (
                "entityIndexSize".into(),
                p("entityIndexSize", "Entity index size", "10000000"),
            ),
            (
                "entityIdSize".into(),
                p("entityIdSize", "Entity ID bit-size", "32"),
            ),
            (
                "enablePredicateList".into(),
                p("enablePredicateList", "Use predicate indices", "true"),
            ),
            (
                "enableContextIndex".into(),
                p("enableContextIndex", "Use context index", "true"),
            ),
            (
                "enableFtsIndex".into(),
                p(
                    "enableFtsIndex",
                    "Enable full-text search (FTS) index",
                    "false",
                ),
            ),
            (
                "historyRetention".into(),
                p(
                    "historyRetention",
                    "Keep change history for time travel (e.g. 7d; empty = off)",
                    "",
                ),
            ),
            (
                "enableLiteralIndex".into(),
                p("enableLiteralIndex", "Use literal index", "true"),
            ),
            (
                "inMemoryLiteralProperties".into(),
                p(
                    "inMemoryLiteralProperties",
                    "Cache literal language tags",
                    "true",
                ),
            ),
            (
                "baseURL".into(),
                p("baseURL", "Base URL", "http://example.org/owlim#"),
            ),
            (
                "defaultNS".into(),
                p(
                    "defaultNS",
                    "Default namespaces for imports(';' delimited)",
                    "",
                ),
            ),
            (
                "imports".into(),
                p("imports", "Imported RDF files(';' delimited)", ""),
            ),
            ("readOnly".into(), p("readOnly", "Read-only", "false")),
            (
                "queryTimeout".into(),
                p("queryTimeout", "Query timeout (seconds)", "0"),
            ),
            (
                "queryLimitResults".into(),
                p("queryLimitResults", "Limit query results", "0"),
            ),
            (
                "throwQueryEvaluationExceptionOnTimeout".into(),
                p(
                    "throwQueryEvaluationExceptionOnTimeout",
                    "Throw exception on query timeout",
                    "false",
                ),
            ),
            (
                "isShacl".into(),
                p("isShacl", "Enable SHACL validation", "false"),
            ),
            (
                "unionDefaultGraph".into(),
                p(
                    "unionDefaultGraph",
                    "Default graph is the union of all graphs",
                    "true",
                ),
            ),
            (
                "storageFolder".into(),
                p("storageFolder", "Storage folder", "storage"),
            ),
            (
                "repositoryType".into(),
                p("repositoryType", "Repository type", "file-repository"),
            ),
        ])
    }

    fn create_repo(&self, id: &str, title: &str, params: Json) -> Result<Arc<Repo>, Response> {
        if !valid_repo_id(id) {
            return Err(Response::text(
                400,
                "Invalid repository id: use letters, digits, '-', '_' and '.'",
            ));
        }
        let mut repos = wl(&self.repos);
        if repos.contains_key(id) {
            return Err(Response::text(
                409,
                format!("Repository {id} already exists."),
            ));
        }
        let dir = self.data.join("repositories").join(id);
        let mut merged = Self::default_params();
        if let Json::Obj(fields) = params {
            for (k, v) in fields {
                merged.set(&k, v);
            }
        }
        let config = Json::obj()
            .with("id", id)
            .with("title", title)
            .with("type", "graphrd")
            .with("location", "")
            .with("params", merged);
        let io = |e: std::io::Error| Response::text(500, e.to_string());
        if self.memory {
            let repo = Arc::new(Repo::memory(config));
            repos.insert(id.to_string(), repo.clone());
            return Ok(repo);
        }
        fs::create_dir_all(&dir).map_err(io)?;
        fs::write(dir.join("config.json"), config.to_string()).map_err(io)?;
        let repo = Arc::new(Repo::open(&dir).map_err(io)?);
        repo.save_namespaces().map_err(io)?;
        repos.insert(id.to_string(), repo.clone());
        eprintln!("created repository {id}");
        Ok(repo)
    }

    /// `PUT /repositories/{id}` with a Turtle repository config.
    fn create_repo_from_ttl(&self, id: &str, req: &Request) -> Response {
        let title = turtle::parse(&req.body_text(), "", turtle::Mode::Turtle)
            .ok()
            .and_then(|qs| {
                qs.into_iter()
                    .find(|q| {
                        q.p.value().ends_with("#repositoryLabel") || q.p.value().ends_with("#label")
                    })
                    .map(|q| q.o.value().to_string())
            })
            .unwrap_or_default();
        match self.create_repo(id, &title, Json::obj()) {
            Ok(_) => Response::empty(204),
            Err(r) => r,
        }
    }

    fn delete_repo(&self, id: &str) -> Response {
        let removed = wl(&self.repos).remove(id);
        match removed {
            Some(repo) => {
                let (dir, memory) = (repo.dir.clone(), repo.memory);
                drop(repo);
                if memory {
                    return Response::empty(200);
                }
                if let Err(e) = fs::remove_dir_all(&dir) {
                    return Response::text(
                        500,
                        format!("Removed from server but could not delete files: {e}"),
                    );
                }
                eprintln!("deleted repository {id}");
                Response::empty(200)
            }
            None => Response::text(404, format!("Unknown repository: {id}")),
        }
    }

    // ------------------------------------------------------------ management REST

    fn rest(&self, segs: &[&str], req: &Request) -> Response {
        let m = req.method.as_str();
        let ok = |j: Json| Response::json(200, j);
        match (m, segs) {
            ("GET", ["info", "version"]) => ok(Json::obj()
                .with("product", "GraphRD")
                .with("version", VERSION)),
            ("GET", ["info", "properties"]) => ok(Json::Arr(Vec::new())),
            ("GET", ["info", "rpc-address"]) => Response::text(200, ""),
            ("GET", ["info", "external-url"]) => {
                let host = req.header("host").unwrap_or("localhost:7200");
                Response::text(200, format!("http://{host}/"))
            }
            ("GET", ["info", "data"]) => ok(Json::obj()),
            ("GET", ["security", "all"]) => ok(security_config(self.auth.is_some())),
            ("POST", ["login"]) => {
                let Some(auth) = &self.auth else {
                    return ok(admin_user());
                };
                let body = Json::parse(&req.body_text()).unwrap_or(Json::obj());
                let name = body.str_at(&["username"]);
                if let Some(user) = auth.check_password(name, body.str_at(&["password"])) {
                    let token = auth.issue_token(name);
                    Response::json(200, user.json(name).with("token", token.as_str()))
                        .with_header("Authorization", &format!("Bearer {token}"))
                        .with_header("Access-Control-Expose-Headers", "Authorization")
                } else {
                    Response::json(
                        401,
                        Json::obj().with("message", "Wrong username or password"),
                    )
                }
            }
            ("POST", ["logout"]) => {
                if let (Some(auth), Some(tok)) = (
                    &self.auth,
                    req.header("authorization")
                        .and_then(|h| h.strip_prefix("Bearer ")),
                ) {
                    lock(&auth.tokens).remove(tok.trim());
                }
                Response::empty(200)
            }
            ("GET", ["security"]) => ok(Json::Bool(false)),
            ("GET", ["security", "authenticated-user"]) => {
                match self.auth.as_ref().and_then(|a| a.user(req)) {
                    Some((name, user)) => ok(user.json(&name)),
                    None => ok(admin_user()),
                }
            }
            (_, ["security", "users", ..]) if self.auth.is_some() => {
                self.users_endpoint(&segs[2..], req)
            }
            (_, ["security", "apikeys", ..]) if self.auth.is_some() => {
                self.apikeys_endpoint(&segs[2..], req)
            }
            ("GET", ["security", "users", "admin"]) => ok(admin_user()),
            ("GET", ["security", "free-access"]) => ok(Json::obj()
                .with("enabled", false)
                .with("authorities", Json::Arr(vec![]))
                .with("appSettings", app_settings())),
            ("GET", ["security", "users", _]) => ok(admin_user()),
            ("PATCH" | "PUT", ["security", "users", _]) => Response::empty(200),
            ("GET", ["roles"]) => ok(Json::Arr(vec![])),
            ("GET", ["locations"]) => ok(Json::Arr(vec![location_json(&lock(&self.default_repo))])),
            ("GET", ["locations", "active"]) => ok(location_json(&lock(&self.default_repo))),
            ("GET", ["locations", "active", "default-repository"]) => {
                ok(Json::obj().with("defaultRepository", lock(&self.default_repo).as_str()))
            }
            ("POST", ["locations", "active", "default-repository"]) => {
                let body = Json::parse(&req.body_text()).unwrap_or(Json::Null);
                *lock(&self.default_repo) = body.str_at(&["repository"]).to_string();
                Response::empty(200)
            }
            ("GET", ["autocomplete", "enabled"]) => ok(Json::Bool(true)),
            ("GET", ["autocomplete", "status"]) => Response::text(200, "READY"),
            ("GET", ["autocomplete", "query"]) => {
                let id = req
                    .header("x-graphrd-repository")
                    .or(req.param("repository"))
                    .unwrap_or("")
                    .to_string();
                let Some(repo) = self.repo(&id) else {
                    return Response::text(
                        400,
                        "Name the repository in the X-GraphRD-Repository header or ?repository=",
                    );
                };
                ok(Json::obj().with(
                    "suggestions",
                    Json::Arr(autocomplete(&repo, req.param("q").unwrap_or(""))),
                ))
            }
            ("GET", ["rank", "status"]) | ("POST", ["rank", "compute"]) => {
                let id = req
                    .header("x-graphrd-repository")
                    .or(req.param("repository"))
                    .unwrap_or("")
                    .to_string();
                let Some(repo) = self.repo(&id) else {
                    return Response::text(
                        400,
                        "Name the repository in the X-GraphRD-Repository header or ?repository=",
                    );
                };
                if m == "GET" {
                    return Response::text(200, repo.store.read().rank_status());
                }
                let n = repo.store.write().compute_rank();
                Response::json(200, Json::obj().with("status", "COMPUTED").with("nodes", n))
            }
            ("GET" | "POST", ["recovery", "backup"]) => self.backup(req),
            ("POST", ["recovery", "restore"]) => self.restore(req),
            (_, ["llm", tail @ ..]) => self.llm_endpoint(tail, req),
            ("GET", ["monitor", "infrastructure"]) => ok(monitor_infrastructure()),
            ("GET", ["monitor", "structures"]) => {
                use std::sync::atomic::Ordering::Relaxed;
                ok(Json::obj()
                    .with(
                        "cacheHit",
                        crate::metrics::CACHE_HITS.load(Relaxed) as usize,
                    )
                    .with(
                        "cacheMiss",
                        crate::metrics::CACHE_MISSES.load(Relaxed) as usize,
                    ))
            }
            ("GET", ["monitor", "slow-queries"]) => ok(Json::Arr(
                crate::metrics::slow_queries()
                    .into_iter()
                    .map(|e| {
                        Json::obj()
                            .with("time", e.at_millis)
                            .with("repository", e.repository)
                            .with("kind", e.kind)
                            .with("ms", e.millis as usize)
                            .with("text", e.text)
                    })
                    .collect(),
            )),
            ("GET", ["monitor", "repository", _, "query", "active"])
            | ("GET", ["monitor", "repository", _, "operations"]) => ok(Json::obj()
                .with("status", "INFORMATION")
                .with("allRunningOperations", Json::Arr(vec![]))),
            ("GET", ["monitor", "repository", _]) => ok(Json::Arr(vec![])),
            ("GET", ["monitor", "backup"]) => ok(Json::Null),
            ("GET", ["guides"]) => ok(Json::Arr(vec![])),
            ("GET", ["connectors"]) => ok(Json::Arr(vec![])),
            ("GET", ["sparql-templates"])
            | ("GET", ["sql-views"])
            | ("GET", ["explore-graph", "config"]) => ok(Json::Arr(vec![])),
            ("GET", ["explore-graph", "saved"]) => ok(Json::Arr(vec![])),
            ("POST", ["sparql", "add-known-prefixes"]) => self.add_known_prefixes(req),
            (_, ["sparql", "saved-queries"]) => self.saved_queries(req),
            ("GET", ["cluster", ..]) | ("GET", ["repositories", "cluster"]) => {
                Response::text(404, "No cluster")
            }
            ("GET", ["repositories", "all"]) => {
                let list: Vec<Json> = rl(&self.repos)
                    .values()
                    .map(|r| self.repo_json(r, req))
                    .collect();
                ok(Json::Obj(vec![(String::new(), Json::Arr(list))]))
            }
            ("GET", ["repositories"]) => ok(Json::Arr(
                rl(&self.repos)
                    .values()
                    .map(|r| self.repo_json(r, req))
                    .collect(),
            )),
            ("POST", ["repositories"]) => {
                let body = match Json::parse(&req.body_text()) {
                    Ok(b) => b,
                    Err(e) => return Response::text(400, e),
                };
                let id = body.str_at(&["id"]).to_string();
                let params = body.get("params").cloned().unwrap_or(Json::obj());
                let title = match body.str_at(&["title"]) {
                    "" => params.str_at(&["title", "value"]).to_string(),
                    t => t.to_string(),
                };
                match self.create_repo(&id, &title, params) {
                    Ok(_) => Response::empty(201),
                    Err(r) => r,
                }
            }
            ("GET", ["repositories", "default-config", kind]) => ok(Json::obj()
                .with("id", "")
                .with("title", "")
                .with("type", *kind)
                .with("location", "")
                .with("params", Self::default_params())),
            ("GET", ["repositories", "ruleset", ..]) | ("GET", ["rulesets"]) => ok(Json::Arr(
                crate::reason::RULESETS
                    .iter()
                    .map(|r| Json::from(*r))
                    .collect(),
            )),
            (_, ["repositories", id, rest @ ..]) => {
                let Some(repo) = self.repo(id) else {
                    return Response::text(404, format!("Unknown repository: {id}"));
                };
                self.rest_repository(&repo, rest, req)
            }
            _ => Response::text(
                404,
                format!("Not implemented: {} /rest/{}", m, segs.join("/")),
            ),
        }
    }

    /// Set the language model from the startup settings, or from the settings saved in the UI when
    /// there are some (those failing to apply fall back to the startup ones, with a warning).
    /// Returns a line for the log when a model is in use.
    pub fn configure_llm(&self, startup: crate::nlq::Settings) -> Result<Option<String>, String> {
        *lock(&self.llm_startup) = startup.clone();
        let saved = (!self.memory)
            .then(|| fs::read_to_string(self.data.join("llm.json")).ok())
            .flatten()
            .and_then(|t| Json::parse(&t).ok())
            .map(|j| crate::nlq::Settings::from_json(&j, None));
        if let Some(saved) = saved {
            match saved.resolve() {
                Ok(llm) => {
                    *lock(&self.llm_saved) = Some(saved);
                    let llm = llm.map(|l| Arc::new(l.with_source("settings saved in the UI")));
                    let line = llm.as_ref().map(|l| l.describe());
                    *wl(&self.llm) = llm;
                    return Ok(line);
                }
                Err(e) => eprintln!(
                    "warning: the language model settings saved in the UI ({}) are not used: {e}",
                    self.data.join("llm.json").display()
                ),
            }
        }
        let llm = startup.resolve()?.map(Arc::new);
        let line = llm.as_ref().map(|l| l.describe());
        *wl(&self.llm) = llm;
        Ok(line)
    }

    fn llm_state(&self) -> Response {
        let active = rl(&self.llm).clone();
        let saved = lock(&self.llm_saved).clone();
        Response::json(
            200,
            Json::obj()
                .with("enabled", active.is_some())
                .with("active", active.as_ref().map_or(Json::Null, |l| l.info()))
                .with("saved", saved.map_or(Json::Null, |s| s.to_json(false)))
                .with("startup", lock(&self.llm_startup).to_json(false))
                .with("persistent", !self.memory),
        )
    }

    /// `/rest/llm`: the language model settings (administrators). GET shows what is in use and
    /// what is saved (never the key), PUT saves and applies new settings, DELETE goes back to
    /// the startup settings, `POST /rest/llm/test` calls the model once. `GET /rest/llm/status`
    /// (any user) only says whether questions can be asked.
    fn llm_endpoint(&self, rest: &[&str], req: &Request) -> Response {
        let active = rl(&self.llm).clone();
        match (req.method.as_str(), rest) {
            ("GET", ["status"]) => Response::json(
                200,
                Json::obj()
                    .with("enabled", active.is_some())
                    .with("model", active.as_ref().map(|l| l.model.clone())),
            ),
            ("GET", []) => self.llm_state(),
            ("PUT", []) => {
                let body = match Json::parse(&req.body_text()) {
                    Ok(b @ Json::Obj(_)) => b,
                    _ => {
                        return Response::text(
                            400,
                            "expected a JSON object: {\"api\", \"url\", \"key\", \"model\", \"effort\", \"timeoutSecs\"}",
                        );
                    }
                };
                let previous_key = lock(&self.llm_saved).as_ref().and_then(|s| s.key.clone());
                let settings = crate::nlq::Settings::from_json(&body, previous_key);
                let llm = match settings.resolve() {
                    Ok(l) => l.map(|l| Arc::new(l.with_source("settings saved in the UI"))),
                    Err(e) => return Response::text(400, e),
                };
                if !self.memory {
                    let path = self.data.join("llm.json");
                    if let Err(e) =
                        write_private(&path, settings.to_json(true).to_string().as_bytes())
                    {
                        return Response::text(500, format!("{}: {e}", path.display()));
                    }
                }
                *lock(&self.llm_saved) = Some(settings);
                if let Some(l) = &llm {
                    eprintln!("questions in plain language: {}", l.describe());
                }
                *wl(&self.llm) = llm;
                self.llm_state()
            }
            ("DELETE", []) => {
                if !self.memory {
                    let _ = fs::remove_file(self.data.join("llm.json"));
                }
                *lock(&self.llm_saved) = None;
                let startup = lock(&self.llm_startup).clone();
                *wl(&self.llm) = startup.resolve().ok().flatten().map(Arc::new);
                self.llm_state()
            }
            ("POST", ["test"]) => {
                // the settings in the body when given (to try them before saving), else those in use
                let llm = match Json::parse(&req.body_text()) {
                    Ok(b @ Json::Obj(_)) => {
                        let previous_key =
                            lock(&self.llm_saved).as_ref().and_then(|s| s.key.clone());
                        match crate::nlq::Settings::from_json(&b, previous_key).resolve() {
                            Ok(Some(l)) => Arc::new(l),
                            Ok(None) => {
                                return Response::text(
                                    400,
                                    "These settings name no model: give an endpoint, a key or both",
                                );
                            }
                            Err(e) => return Response::text(400, e),
                        }
                    }
                    _ => match active {
                        Some(l) => l,
                        None => return Response::text(400, "No language model is configured"),
                    },
                };
                let t0 = std::time::Instant::now();
                match llm.ping() {
                    Ok(answer) => Response::json(
                        200,
                        Json::obj()
                            .with("ok", true)
                            .with("model", llm.model.as_str())
                            .with("answer", answer)
                            .with("ms", t0.elapsed().as_millis() as usize),
                    ),
                    Err(e) => Response::json(
                        200,
                        Json::obj()
                            .with("ok", false)
                            .with("model", llm.model.as_str())
                            .with("error", e),
                    ),
                }
            }
            _ => Response::text(405, "Method not allowed"),
        }
    }

    /// `POST /rest/repositories/{id}/nl-query` with `{"question": "...", "run": true}`: the SPARQL
    /// the configured language model writes for the question, and with `run` its results.
    fn nl_query(&self, repo: &Arc<Repo>, req: &Request) -> Response {
        let Some(llm) = rl(&self.llm).clone() else {
            return Response::text(
                501,
                "No language model is configured: set one under AI Assistant in the UI, or start the server with --llm-url / --llm-model or ANTHROPIC_API_KEY / OPENAI_API_KEY (see docs/nl-query.md)",
            );
        };
        let body = match Json::parse(&req.body_text()) {
            Ok(b) => b,
            Err(e) => {
                return Response::text(400, format!("expected a JSON body with \"question\": {e}"));
            }
        };
        let question = body.str_at(&["question"]).to_string();
        if question.trim().is_empty() {
            return Response::text(400, "\"question\" is missing or empty");
        }
        let prefixes = repo.prefix_map();
        // the summary is made under the read lock; the model is asked without it
        let schema = crate::nlq::cached_schema(&repo.store.read(), &repo.id, &prefixes);
        let answer = match crate::nlq::ask(&llm, &schema, &question, &prefixes) {
            Ok(a) => a,
            Err(e) => return Response::text(502, e),
        };
        let mut out = Json::obj()
            .with("question", question.as_str())
            .with("sparql", answer.sparql.as_str())
            .with("model", llm.model.as_str())
            .with("attempts", answer.attempts);
        if body
            .get("run")
            .is_some_and(|r| matches!(r, Json::Bool(true)))
        {
            let params: Vec<(String, String)> = req
                .params()
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let r = self.run_query(
                repo,
                &answer.sparql,
                &params,
                "application/sparql-results+json",
            );
            let text = String::from_utf8_lossy(&r.body).to_string();
            if r.status == 200 {
                out.set("results", Json::parse(&text).unwrap_or(Json::from(text)));
            } else {
                out.set("error", text);
            }
        }
        Response::json(200, out)
    }

    fn rest_repository(&self, repo: &Arc<Repo>, rest: &[&str], req: &Request) -> Response {
        let m = req.method.as_str();
        match (m, rest) {
            ("POST", ["nl-query"]) => self.nl_query(repo, req),
            ("GET", []) => Response::json(200, rl(&repo.config).clone()),
            ("PUT", []) => {
                let body = match Json::parse(&req.body_text()) {
                    Ok(b) => b,
                    Err(e) => return Response::text(400, e),
                };
                if !body.str_at(&["id"]).is_empty() && body.str_at(&["id"]) != repo.id {
                    return Response::text(400, "Renaming repositories is not supported");
                }
                {
                    let mut cfg = wl(&repo.config);
                    if let Some(p) = body.get("params") {
                        // the parameters given replace those of the same name; the others stay
                        let mut params = cfg.get("params").cloned().unwrap_or_else(Json::obj);
                        if let Json::Obj(fields) = p {
                            for (k, v) in fields {
                                params.set(k, v.clone());
                            }
                        }
                        cfg.set("params", params);
                        let t = p.str_at(&["title", "value"]);
                        if !t.is_empty() {
                            cfg.set("title", t);
                        }
                    }
                    if !body.str_at(&["title"]).is_empty() {
                        cfg.set("title", body.str_at(&["title"]));
                    }
                }
                let (ruleset, same_as) = ruleset_of(&rl(&repo.config));
                let fts = fts_of(&rl(&repo.config));
                let history = history_of(&rl(&repo.config));
                let mut st = repo.store.write();
                st.set_ruleset(&ruleset, same_as);
                st.set_fts(fts);
                let kept = st.set_history(history);
                drop(st);
                if let Err(e) = kept {
                    return Response::text(500, e.to_string());
                }
                match repo.save_config() {
                    Ok(()) => Response::empty(200),
                    Err(e) => Response::text(500, e.to_string()),
                }
            }
            ("DELETE", []) => self.delete_repo(&repo.id),
            ("GET", ["history"]) => {
                // the window time travel (`as-of`) can reach
                let st = repo.store.write();
                let body = match st.history_window() {
                    Some((start, retention)) => Json::obj()
                        .with("enabled", true)
                        .with("start", start)
                        .with("retentionMs", retention),
                    None => Json::obj().with("enabled", false),
                };
                drop(st);
                Response::json(200, body)
            }
            ("GET", ["search"]) => {
                let st = repo.store.read();
                if !st.fts_enabled() {
                    return Response::text(
                        400,
                        "Full-text search is not enabled for this repository (set enableFtsIndex)",
                    );
                }
                Response::json(200, fts_search(&st, req))
            }
            ("GET", ["size"]) => {
                let st = repo.store.read();
                let (e, i) = (st.len(), st.inferred_len());
                Response::json(
                    200,
                    Json::obj()
                        .with("inferred", i)
                        .with("total", e + i)
                        .with("explicit", e),
                )
            }
            ("POST", ["restart"]) => Response::empty(200),
            ("GET" | "POST", ["graphql", ..]) => {
                if req.param("sdl").is_some() || rest.get(1) == Some(&"schema") {
                    return Response::new(
                        200,
                        "text/plain;charset=UTF-8",
                        crate::graphql::Schema::build(&repo.store.read(), 200).sdl(),
                    );
                }
                let query = graphql_query(req);
                let body = Json::parse(&req.body_text()).unwrap_or(Json::obj());
                let vars = body
                    .get("variables")
                    .cloned()
                    .filter(|v| matches!(v, Json::Obj(_)))
                    .unwrap_or(Json::obj());
                if req.method == "POST" && crate::graphql::is_mutation(&query) {
                    return self.graphql_mutation(repo, &query, &vars);
                }
                Response::json(
                    200,
                    crate::graphql::execute(&repo.store.read(), &query, vars),
                )
            }
            (_, ["connectors", ..]) => self.connectors_endpoint(repo, &rest[1..], req),
            ("GET", ["shacl", "validate"]) => {
                let st = repo.store.read();
                let violations = crate::shacl::Shapes::load(&st)
                    .map(|s| s.validate(&st, None))
                    .unwrap_or_default();
                if req.param("format") == Some("turtle")
                    || req.header("accept").is_some_and(|a| a.contains("turtle"))
                {
                    Response::new(
                        200,
                        "text/turtle;charset=UTF-8",
                        crate::shacl::report_turtle(&violations),
                    )
                } else {
                    Response::json(200, crate::shacl::report_json(&violations))
                }
            }
            ("GET", ["import", "settings", "default"]) => {
                Response::json(200, default_import_settings())
            }
            ("GET", ["import", "upload"]) => {
                Response::json(200, Json::Arr(lock(&repo.imports).clone()))
            }
            ("GET", ["import", "server"]) => Response::json(200, Json::Arr(self.server_files())),
            ("DELETE", ["import", "upload"]) | ("DELETE", ["import", "server"]) => {
                let names: Vec<String> = req
                    .query
                    .iter()
                    .filter(|(k, _)| k == "name" || k == "names")
                    .flat_map(|(_, v)| v.split(',').map(String::from).collect::<Vec<_>>())
                    .collect();
                if names.is_empty() {
                    if let Ok(Json::Arr(list)) = Json::parse(&req.body_text()) {
                        let ns: Vec<String> = list
                            .iter()
                            .filter_map(|j| j.as_str().map(String::from))
                            .collect();
                        lock(&repo.imports)
                            .retain(|r| !ns.iter().any(|n| n == r.str_at(&["name"])));
                    } else {
                        lock(&repo.imports).clear();
                    }
                } else {
                    lock(&repo.imports).retain(|r| !names.iter().any(|n| n == r.str_at(&["name"])));
                }
                Response::empty(200)
            }
            ("POST", ["import", "upload", "text"])
            | ("POST", ["import", "upload", "update", "text"]) => {
                let settings = match Json::parse(&req.body_text()) {
                    Ok(s) => s,
                    Err(e) => return Response::text(400, e),
                };
                let data = settings.str_at(&["data"]).to_string();
                let format =
                    Format::from_media_type(settings.str_at(&["format"])).unwrap_or(Format::Turtle);
                self.import(repo, &settings, "text", data.into_bytes(), format);
                Response::empty(200)
            }
            ("POST", ["import", "upload", "file"])
            | ("POST", ["import", "upload", "update", "file"]) => {
                let parts = http::parse_multipart(req);
                let settings = parts
                    .iter()
                    .find(|p| p.name == "importSettings")
                    .and_then(|p| Json::parse(&String::from_utf8_lossy(&p.data)).ok())
                    .unwrap_or(Json::obj());
                let Some(file) = parts.iter().find(|p| p.name == "file") else {
                    return Response::text(400, "No file in upload");
                };
                let name = file
                    .filename
                    .clone()
                    .unwrap_or_else(|| settings.str_at(&["name"]).to_string());
                let format = Format::from_media_type(settings.str_at(&["format"]))
                    .or_else(|| Format::from_extension(&name))
                    .unwrap_or(Format::Turtle);
                let mut settings = settings;
                settings.set("name", name.as_str());
                self.import(repo, &settings, "file", file.data.clone(), format);
                Response::empty(200)
            }
            ("POST", ["import", "upload", "url"])
            | ("POST", ["import", "upload", "update", "url"]) => {
                let settings = Json::parse(&req.body_text()).unwrap_or(Json::obj());
                let url = settings.str_at(&["data"]).to_string();
                let mut s = settings.clone();
                s.set("name", url.as_str());
                match http::fetch(&url, rdf::RDF_ACCEPT, std::time::Duration::from_secs(60)) {
                    Ok((ctype, body)) => {
                        let format = Format::from_media_type(s.str_at(&["format"]))
                            .or_else(|| {
                                Format::from_media_type(&ctype)
                                    .filter(|_| !ctype.starts_with("text/plain"))
                            })
                            .or_else(|| {
                                Format::from_extension(url.split(['?', '#']).next().unwrap_or(&url))
                            })
                            .unwrap_or(Format::Turtle);
                        if s.str_at(&["baseURI"]).is_empty() {
                            s.set("baseURI", url.as_str());
                        }
                        self.import(repo, &s, "url", body, format);
                    }
                    Err(e) => self.record_import(repo, &s, "url", "ERROR", &e, 0, 0),
                }
                Response::empty(200)
            }
            ("POST", ["import", "server"]) if self.memory => Response::text(
                400,
                "Server files are not available on a server started with --memory",
            ),
            ("POST", ["import", "server"]) => {
                let body = Json::parse(&req.body_text()).unwrap_or(Json::obj());
                let settings = body
                    .get("importSettings")
                    .cloned()
                    .filter(|s| !matches!(s, Json::Null))
                    .unwrap_or_else(default_import_settings);
                let names: Vec<String> = body
                    .get("fileNames")
                    .and_then(|a| a.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|j| j.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let dir = self.data.join("import");
                for name in names {
                    let Some(safe) = Path::new(&name).file_name() else {
                        continue;
                    };
                    let path = dir.join(safe);
                    let mut s = settings.clone();
                    s.set("name", name.as_str());
                    match fs::read(&path) {
                        Ok(bytes) => {
                            let format = Format::from_media_type(s.str_at(&["format"]))
                                .or_else(|| Format::from_extension(&name))
                                .unwrap_or(Format::Turtle);
                            self.import(repo, &s, "file", bytes, format);
                        }
                        Err(e) => {
                            self.record_import(repo, &s, "file", "ERROR", &e.to_string(), 0, 0)
                        }
                    }
                }
                Response::empty(200)
            }
            ("GET", ["import", ..]) => Response::json(200, Json::Arr(vec![])),
            _ => Response::text(404, format!("Not implemented: {} {}", m, req.path)),
        }
    }

    // ------------------------------------------------------------ cluster

    /// Enable clustering: start Raft with this app as its state machine.
    pub fn join_cluster(self: &Arc<Self>, raft: Arc<crate::raft::Raft>) {
        let app = self.clone();
        let _ = self.cluster.set(raft.clone());
        // snapshots for members that fall behind the compacted log: a backup archive of every
        // repository, installed by replacing this node's repositories
        let (maker, installer) = (Arc::downgrade(self), Arc::downgrade(self));
        raft.set_snapshots(crate::raft::Snapshots {
            make: Arc::new(move || {
                let Some(app) = maker.upgrade() else {
                    return Vec::new();
                };
                let req = Request {
                    method: "GET".into(),
                    path: "/rest/recovery/backup".into(),
                    query: Vec::new(),
                    headers: Vec::new(),
                    body: Vec::new(),
                };
                app.backup(&req).body
            }),
            install: Arc::new(move |data: &[u8]| {
                let app = installer.upgrade().ok_or("shutting down")?;
                let names: std::collections::HashSet<String> = crate::tar::read(data)?
                    .into_iter()
                    .filter_map(|(n, _)| {
                        n.strip_prefix("repositories/")
                            .and_then(|r| r.split('/').next())
                            .map(String::from)
                    })
                    .collect();
                for id in rl(&app.repos).keys().cloned().collect::<Vec<_>>() {
                    if !names.contains(&id) {
                        app.delete_repo(&id);
                    }
                }
                if names.is_empty() {
                    return Ok(());
                }
                let req = Request {
                    method: "POST".into(),
                    path: "/rest/recovery/restore".into(),
                    query: Vec::new(),
                    headers: vec![("content-type".into(), "application/x-tar".into())],
                    body: data.to_vec(),
                };
                let r = app.restore(&req);
                if r.status >= 300 {
                    Err(String::from_utf8_lossy(&r.body).into_owned())
                } else {
                    Ok(())
                }
            }),
        });
        raft.start(Arc::new(move |op: &Json| app.apply_op(op)));
    }

    fn raft_rpc(&self, rpc: &str, req: &Request) -> Response {
        let Some(raft) = self.cluster.get() else {
            return Response::text(404, "Not a cluster member");
        };
        if !raft.secret_ok(req.header("x-graphrd-cluster")) {
            return Response::text(403, "Bad cluster secret");
        }
        if rpc == "snapshot" {
            let n = |k: &str| req.param(k).and_then(|v| v.parse().ok()).unwrap_or(0u64);
            return Response::json(
                200,
                raft.handle_snapshot(
                    n("term"),
                    req.param("leader").unwrap_or(""),
                    n("index"),
                    n("indexTerm"),
                    &req.body,
                ),
            );
        }
        let body = Json::parse(&req.body_text()).unwrap_or(Json::obj());
        match rpc {
            "vote" => Response::json(200, raft.handle_vote(&body)),
            "append" => Response::json(200, raft.handle_append(&body)),
            _ => Response::text(404, "Unknown cluster call"),
        }
    }

    /// On a follower: the leader's answer to `req` (or 503 when there is no leader). None on
    /// the leader itself.
    fn forward_to_leader(&self, req: &Request) -> Option<Response> {
        let raft = self.cluster.get().expect("cluster");
        // Just after an election a follower may not have heard from the new leader yet:
        // wait for one to be known rather than failing the request.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut leader = None;
        while !raft.is_leader() {
            leader = raft.leader_host();
            if leader.is_some() || std::time::Instant::now() > deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !raft.is_leader() {
            let Some(leader) = leader else {
                return Some(Response::text(
                    503,
                    "No cluster leader is elected yet; retry shortly",
                ));
            };
            let mut path = http::percent_encode_path(&req.path);
            if !req.query.is_empty() {
                path.push('?');
                path.push_str(
                    &req.query
                        .iter()
                        .map(|(k, v)| {
                            format!("{}={}", http::percent_encode(k), http::percent_encode(v))
                        })
                        .collect::<Vec<_>>()
                        .join("&"),
                );
            }
            // a leader that stopped answering is being replaced: retry with the next one
            let mut leader = leader;
            let result = loop {
                match http::call(
                    &leader,
                    &req.method,
                    &path,
                    &req.headers,
                    &req.body,
                    std::time::Duration::from_secs(60),
                ) {
                    Ok(r) => break Ok(r),
                    Err(e) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        if raft.is_leader() {
                            return None;
                        }
                        match raft.leader_host() {
                            Some(l) => leader = l,
                            None => {
                                let _ = e;
                            }
                        }
                    }
                    Err(e) => break Err(e),
                }
            };
            return Some(match result {
                Ok((status, headers, body)) => {
                    let mut r = Response {
                        status,
                        headers: Vec::new(),
                        body,
                    };
                    for (k, v) in headers {
                        if !["content-length", "connection", "transfer-encoding"]
                            .contains(&k.to_ascii_lowercase().as_str())
                        {
                            r.headers.push((k, v));
                        }
                    }
                    r.with_header("X-GraphRD-Leader", &leader)
                }
                Err(e) => Response::text(
                    503,
                    format!("Cannot reach the cluster leader at {leader}: {e}"),
                ),
            });
        }
        None
    }

    /// Writes in a cluster: followers forward to the leader; the leader runs
    /// the write, captures the resulting change set and replicates it.
    fn clustered_write(&self, segs: &[&str], req: &Request, need: Need) -> Response {
        if let Some(r) = self.forward_to_leader(req) {
            return r;
        }
        let raft = self.cluster.get().expect("cluster");
        let timeout = std::time::Duration::from_secs(10);
        let repo_id = match &need {
            Need::Write(r) => Some(r.clone()),
            _ => None,
        };
        // namespace changes and repository management replicate as operations
        let is_ns = matches!(segs, ["repositories", _, "namespaces", ..]);
        if let (Some(id), false) = (&repo_id, is_ns) {
            let Some(repo) = self.repo(id) else {
                return self.dispatch(segs, req);
            };
            let _serial = lock(&self.cluster_write);
            repo.store.write().start_capture();
            let resp = self.dispatch(segs, req);
            let (removed, inserted) = repo.store.write().take_capture();
            if resp.status >= 300 || (removed.is_empty() && inserted.is_empty()) {
                return resp;
            }
            let nq = |q: &[Quad]| rdf::write::serialize(q, Format::NQuads, &[]).unwrap_or_default();
            let op = Json::obj()
                .with("kind", "apply")
                .with("repo", id.as_str())
                .with("remove", nq(&removed))
                .with("insert", nq(&inserted));
            if raft.propose(op, timeout).is_err() {
                let _ = repo.store.write().apply(&inserted, &removed); // undo: not committed
                return Response::text(
                    503,
                    "The write could not be replicated to a majority of the cluster and was undone",
                );
            }
            return resp;
        }
        let resp = self.dispatch(segs, req);
        if resp.status >= 300 {
            return resp;
        }
        let op = match segs {
            ["rest", "repositories"] => {
                let id = Json::parse(&req.body_text())
                    .ok()
                    .map(|b| b.str_at(&["id"]).to_string())
                    .unwrap_or_default();
                self.repo(&id).map(|r| {
                    Json::obj()
                        .with("kind", "config")
                        .with("repo", id.as_str())
                        .with("config", rl(&r.config).clone())
                })
            }
            ["rest", "repositories", id] | ["repositories", id] => match self.repo(id) {
                Some(r) => Some(
                    Json::obj()
                        .with("kind", "config")
                        .with("repo", *id)
                        .with("config", rl(&r.config).clone()),
                ),
                None => Some(Json::obj().with("kind", "delete").with("repo", *id)),
            },
            ["repositories", id, "namespaces", ..] => self.repo(id).map(|r| {
                let ns = Json::Obj(
                    rl(&r.namespaces)
                        .iter()
                        .map(|(p, n)| (p.clone(), Json::from(n)))
                        .collect(),
                );
                Json::obj()
                    .with("kind", "namespaces")
                    .with("repo", *id)
                    .with("namespaces", ns)
            }),
            _ => None,
        };
        if let Some(op) = op {
            if raft.propose(op, timeout).is_err() {
                return Response::text(
                    503,
                    "The change was applied here but could not be replicated to a majority of the cluster",
                );
            }
        }
        resp
    }

    /// The cluster state machine: apply one committed operation. Every
    /// operation is idempotent, so replaying the log is safe.
    fn apply_op(&self, op: &Json) {
        let id = op.str_at(&["repo"]).to_string();
        match op.str_at(&["kind"]) {
            "apply" => {
                let Some(repo) = self.repo(&id) else { return };
                let remove = turtle::parse_nquads_raw(op.str_at(&["remove"])).unwrap_or_default();
                let insert = turtle::parse_nquads_raw(op.str_at(&["insert"])).unwrap_or_default();
                if let Err(e) = repo.store.write().apply(&remove, &insert) {
                    eprintln!("cluster: applying a change to {id} failed: {e}");
                }
            }
            "config" => {
                let cfg = op.get("config").cloned().unwrap_or(Json::obj());
                let repo = match self.repo(&id) {
                    Some(r) => r,
                    None => match self.create_repo(
                        &id,
                        cfg.str_at(&["title"]),
                        cfg.get("params").cloned().unwrap_or(Json::obj()),
                    ) {
                        Ok(r) => r,
                        Err(r) => {
                            eprintln!(
                                "cluster: creating {id} failed: {}",
                                String::from_utf8_lossy(&r.body)
                            );
                            return;
                        }
                    },
                };
                *wl(&repo.config) = cfg;
                let _ = repo.save_config();
                let (ruleset, same_as) = ruleset_of(&rl(&repo.config));
                let fts = fts_of(&rl(&repo.config));
                let mut st = repo.store.write();
                st.set_ruleset(&ruleset, same_as);
                st.set_fts(fts);
            }
            "delete" => {
                if self.repo(&id).is_some() {
                    self.delete_repo(&id);
                }
            }
            "namespaces" => {
                if let (Some(repo), Some(Json::Obj(ns))) = (self.repo(&id), op.get("namespaces")) {
                    *wl(&repo.namespaces) = ns
                        .iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect();
                    let _ = repo.save_namespaces();
                }
            }
            _ => {}
        }
    }

    /// A tar archive with each repository's config, namespaces and explicit
    /// statements (N-Quads). POST may name repositories: {"repositories":[...]}.
    fn backup(&self, req: &Request) -> Response {
        if self.memory {
            return Response::text(
                400,
                "A server started with --memory has nothing on disk to back up",
            );
        }
        let wanted: Option<Vec<String>> = Json::parse(&req.body_text()).ok().and_then(|b| {
            b.get("repositories").and_then(|r| r.as_array()).map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
        });
        let repos: Vec<Arc<Repo>> = rl(&self.repos)
            .values()
            .filter(|r| wanted.as_ref().is_none_or(|w| w.contains(&r.id)))
            .cloned()
            .collect();
        let mut entries = Vec::new();
        for repo in repos {
            let base = format!("repositories/{}", repo.id);
            entries.push((
                format!("{base}/config.json"),
                rl(&repo.config).to_string().into_bytes(),
            ));
            let ns = Json::Obj(
                rl(&repo.namespaces)
                    .iter()
                    .map(|(p, n)| (p.clone(), Json::from(n)))
                    .collect(),
            );
            entries.push((
                format!("{base}/namespaces.json"),
                ns.to_string().into_bytes(),
            ));
            let st = repo.store.read();
            let quads: Vec<Quad> = st
                .explicit_matching(None, None, None, None)
                .map(|q| st.decode(q))
                .collect();
            drop(st);
            match rdf::write::serialize(&quads, Format::NQuads, &[]) {
                Ok(nq) => entries.push((format!("{base}/statements.nq"), nq.into_bytes())),
                Err(e) => return Response::text(500, e),
            }
        }
        let stamp = now_millis() / 1000;
        Response::new(200, "application/x-tar", crate::tar::write(&entries)).with_header(
            "Content-Disposition",
            &format!("attachment; filename=graphrd-backup-{stamp}.tar"),
        )
    }

    /// Restore repositories from a backup archive (raw tar body or a
    /// multipart "file" part). Existing repositories with the same id are replaced.
    fn restore(&self, req: &Request) -> Response {
        if self.memory {
            return Response::text(
                400,
                "Restore is not available on a server started with --memory",
            );
        }
        let body = if req.content_type().starts_with("multipart/") {
            match http::parse_multipart(req)
                .into_iter()
                .find(|p| p.name == "file")
            {
                Some(p) => p.data,
                None => return Response::text(400, "No file part in the upload"),
            }
        } else {
            req.body.clone()
        };
        let entries = match crate::tar::read(&body) {
            Ok(e) => e,
            Err(e) => return Response::text(400, e),
        };
        let mut by_repo: BTreeMap<String, HashMap<String, Vec<u8>>> = BTreeMap::new();
        for (name, data) in entries {
            let parts: Vec<&str> = name.split('/').collect();
            if let ["repositories", id, file] = parts.as_slice() {
                by_repo
                    .entry(id.to_string())
                    .or_default()
                    .insert(file.to_string(), data);
            }
        }
        if by_repo.is_empty() {
            return Response::text(400, "The archive contains no repositories");
        }
        let mut restored = Vec::new();
        for (id, files) in by_repo {
            let Some(cfg) = files
                .get("config.json")
                .and_then(|c| Json::parse(&String::from_utf8_lossy(c)).ok())
            else {
                return Response::text(400, format!("{id}: config.json is missing or invalid"));
            };
            if self.repo(&id).is_some() {
                let r = self.delete_repo(&id);
                if r.status >= 300 {
                    return r;
                }
            }
            let repo = match self.create_repo(
                &id,
                cfg.str_at(&["title"]),
                cfg.get("params").cloned().unwrap_or(Json::obj()),
            ) {
                Ok(r) => r,
                Err(r) => return r,
            };
            if let Some(Json::Obj(ns)) = files
                .get("namespaces.json")
                .and_then(|n| Json::parse(&String::from_utf8_lossy(n)).ok())
            {
                *wl(&repo.namespaces) = ns
                    .into_iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
                    .collect();
                let _ = repo.save_namespaces();
            }
            let text = files
                .get("statements.nq")
                .map(|d| String::from_utf8_lossy(d).into_owned())
                .unwrap_or_default();
            let quads = match turtle::parse_nquads_raw(&text) {
                Ok(q) => q,
                Err(e) => return Response::text(400, format!("{id}: {e}")),
            };
            let mut st = repo.store.write();
            for chunk in quads.chunks(100_000) {
                if let Err(e) = st.insert(chunk) {
                    return Response::text(500, format!("{id}: {e}"));
                }
            }
            if let Err(e) = st.checkpoint() {
                return Response::text(500, format!("{id}: {e}"));
            }
            restored.push(
                Json::obj()
                    .with("id", id.as_str())
                    .with("statements", st.len()),
            );
        }
        Response::json(200, Json::obj().with("restored", Json::Arr(restored)))
    }

    /// API keys: `/rest/security/apikeys[/{id}]` (administrators). A key is shown once, when it
    /// is made.
    fn apikeys_endpoint(&self, rest: &[&str], req: &Request) -> Response {
        let auth = self.auth.as_ref().expect("security enabled");
        match (req.method.as_str(), rest) {
            ("GET", []) => Response::json(
                200,
                Json::Arr(
                    lock(&auth.apikeys)
                        .iter()
                        .map(|(id, k)| {
                            Json::obj()
                                .with("id", id.as_str())
                                .with("user", k.user.as_str())
                                .with("name", k.name.as_str())
                                .with("created", k.created)
                        })
                        .collect(),
                ),
            ),
            ("POST", []) => {
                let body = Json::parse(&req.body_text()).unwrap_or(Json::obj());
                let user = body.str_at(&["user"]).to_string();
                if !lock(&auth.users).contains_key(&user) {
                    return Response::text(
                        400,
                        format!("No user {user:?}: a key acts as an existing user"),
                    );
                }
                let id = random_hex()[..12].to_string();
                let secret = random_hex();
                let key = ApiKey {
                    user: user.clone(),
                    name: body.str_at(&["name"]).to_string(),
                    digest: hex(&crate::sparql::hash::sha256(secret.as_bytes())),
                    created: now_millis(),
                };
                lock(&auth.apikeys).insert(id.clone(), key);
                auth.save_keys();
                Response::json(
                    201,
                    Json::obj()
                        .with("id", id.as_str())
                        .with("user", user.as_str())
                        .with("key", format!("grd_{id}_{secret}")),
                )
            }
            ("DELETE", [id]) => {
                if lock(&auth.apikeys).remove(*id).is_none() {
                    return Response::text(404, format!("No API key {id}"));
                }
                auth.save_keys();
                Response::empty(204)
            }
            _ => Response::text(405, "Method not allowed"),
        }
    }

    /// User management: /rest/security/users[/{name}].
    fn users_endpoint(&self, rest: &[&str], req: &Request) -> Response {
        let auth = self.auth.as_ref().expect("security enabled");
        let body = Json::parse(&req.body_text()).unwrap_or(Json::obj());
        let authorities = || -> Vec<String> {
            body.get("grantedAuthorities")
                .or_else(|| body.get("authorities"))
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_else(|| vec!["ROLE_USER".into()])
        };
        let valid_name = |n: &str| {
            !n.is_empty()
                && n.len() <= 64
                && n.chars().all(|c| c.is_alphanumeric() || "-_.@".contains(c))
        };
        let resp = match (req.method.as_str(), rest) {
            ("GET", []) => Response::json(
                200,
                Json::Arr(lock(&auth.users).iter().map(|(n, u)| u.json(n)).collect()),
            ),
            ("GET", [name]) => match lock(&auth.users).get(*name) {
                Some(u) => Response::json(200, u.json(name)),
                None => Response::text(404, format!("No user {name}")),
            },
            ("POST", [name]) => {
                let pass = body.str_at(&["password"]);
                if !valid_name(name) || pass.is_empty() {
                    return Response::text(
                        400,
                        "A user needs a name (letters, digits, - _ . @) and a password",
                    );
                }
                let mut users = lock(&auth.users);
                if users.contains_key(*name) {
                    return Response::text(409, format!("User {name} already exists"));
                }
                users.insert(name.to_string(), User::new(pass, authorities()));
                Response::empty(201)
            }
            ("PUT" | "PATCH", [name]) => {
                let mut users = lock(&auth.users);
                let Some(u) = users.get_mut(*name) else {
                    return Response::text(404, format!("No user {name}"));
                };
                let pass = body.str_at(&["password"]);
                if !pass.is_empty() {
                    *u = User {
                        authorities: u.authorities.clone(),
                        ..User::new(pass, Vec::new())
                    };
                }
                if req.method == "PUT"
                    && (body.get("grantedAuthorities").is_some()
                        || body.get("authorities").is_some())
                {
                    if *name == auth.bootstrap.0 && !authorities().iter().any(|a| a == "ROLE_ADMIN")
                    {
                        return Response::text(
                            400,
                            "The bootstrap administrator must keep ROLE_ADMIN",
                        );
                    }
                    u.authorities = authorities();
                }
                drop(users);
                if !pass.is_empty() {
                    lock(&auth.tokens).retain(|_, (n, _)| n != name);
                    lock(&auth.basic_ok).retain(|_, n| n != name);
                }
                Response::empty(200)
            }
            ("DELETE", [name]) => {
                if *name == auth.bootstrap.0 {
                    return Response::text(400, "The bootstrap administrator cannot be deleted");
                }
                if lock(&auth.users).remove(*name).is_none() {
                    return Response::text(404, format!("No user {name}"));
                }
                lock(&auth.tokens).retain(|_, (n, _)| n != name);
                lock(&auth.basic_ok).retain(|_, n| n != name);
                Response::empty(200)
            }
            _ => Response::text(405, "Method not allowed"),
        };
        if resp.status < 300 && req.method != "GET" {
            auth.save();
        }
        resp
    }

    fn server_files(&self) -> Vec<Json> {
        if self.memory {
            return Vec::new();
        }
        let dir = self.data.join("import");
        let _ = fs::create_dir_all(&dir);
        let mut out = Vec::new();
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                let Ok(meta) = e.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis() as i64);
                out.push(
                    Json::obj()
                        .with("name", name.as_str())
                        .with("type", "file")
                        .with("size", meta.len() as usize)
                        .with("lastModified", modified)
                        .with("status", "NONE"),
                );
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn record_import(
        &self,
        repo: &Repo,
        settings: &Json,
        kind: &str,
        status: &str,
        message: &str,
        added: usize,
        size: usize,
    ) {
        crate::metrics::STATEMENTS_LOADED
            .fetch_add(added as u64, std::sync::atomic::Ordering::Relaxed);
        let name = settings.str_at(&["name"]).to_string();
        let rec = Json::obj()
            .with("name", name.as_str())
            .with("type", kind)
            .with("status", status)
            .with("message", message)
            .with("context", settings.str_at(&["context"]))
            .with(
                "baseURI",
                settings.get("baseURI").cloned().unwrap_or(Json::Null),
            )
            .with(
                "format",
                settings.get("format").cloned().unwrap_or(Json::Null),
            )
            .with(
                "data",
                if kind == "text" {
                    settings.get("data").cloned().unwrap_or(Json::Null)
                } else {
                    Json::Null
                },
            )
            .with(
                "replaceGraphs",
                settings
                    .get("replaceGraphs")
                    .cloned()
                    .unwrap_or(Json::Arr(vec![])),
            )
            .with("imported", now_millis())
            .with("lastModified", now_millis())
            .with("addedStatements", added)
            .with("removedStatements", 0usize)
            .with("numReplacedGraphs", 0usize)
            .with("size", size)
            .with(
                "parserSettings",
                settings
                    .get("parserSettings")
                    .cloned()
                    .unwrap_or(Json::obj()),
            );
        let mut list = lock(&repo.imports);
        list.retain(|r| r.str_at(&["name"]) != name);
        list.push(rec);
    }

    /// Parse and load data synchronously, recording the outcome for the UI.
    fn import(&self, repo: &Repo, settings: &Json, kind: &str, bytes: Vec<u8>, format: Format) {
        let size = bytes.len();
        let text = String::from_utf8_lossy(&bytes);
        let base = match settings.str_at(&["baseURI"]) {
            "" => format!(
                "file:/uploaded/{}",
                http::percent_encode(settings.str_at(&["name"]))
            ),
            b => b.to_string(),
        };
        let ctx = settings.str_at(&["context"]);
        let graph = (!ctx.is_empty() && ctx != "default").then(|| Term::iri(ctx));
        let mut skipped = Vec::new();
        let parsed = if settings.get("lenient") == Some(&Json::Bool(true)) {
            rdf::parse_lenient(&text, format, &base, graph.as_ref(), &mut skipped)
        } else {
            rdf::parse(&text, format, &base, graph.as_ref())
        };
        let result = parsed.and_then(|quads| {
            let mut st = repo.store.write();
            let shacl = shacl_begin(repo, &mut st);
            // replaceGraphs: clear the named graphs first
            if let Some(Json::Arr(gs)) = settings.get("replaceGraphs") {
                for g in gs.iter().filter_map(|g| g.as_str()) {
                    let gid = if g == "default" {
                        Some(DEFAULT_GRAPH)
                    } else {
                        st.dict.lookup(&Term::iri(g))
                    };
                    if let Some(gid) = gid {
                        st.clear_graph(Some(gid)).map_err(|e| e.to_string())?;
                    }
                }
            }
            let n = st.insert(&quads).map_err(|e| e.to_string())?;
            match shacl_end(shacl, &mut st) {
                Some(r) => Err(String::from_utf8_lossy(&r.body).into_owned()),
                None => Ok(n),
            }
        });
        if result.is_ok() {
            repo.learn_prefixes(&text, format);
        }
        match result {
            Ok(n) if !skipped.is_empty() => {
                let msg = format!(
                    "Imported with {} statements skipped; first: {}",
                    skipped.len(),
                    skipped[0]
                );
                self.record_import(repo, settings, kind, "DONE", &msg, n, size)
            }
            Ok(n) => self.record_import(
                repo,
                settings,
                kind,
                "DONE",
                &format!("Imported successfully in {}.", "0s"),
                n,
                size,
            ),
            Err(e) => self.record_import(repo, settings, kind, "ERROR", &e, 0, size),
        }
    }

    fn add_known_prefixes(&self, req: &Request) -> Response {
        let body = req.body_text();
        let query = Json::parse(&body)
            .ok()
            .and_then(|j| j.as_str().map(String::from))
            .unwrap_or(body);
        // Add PREFIX declarations for prefixes used but not declared, using any repository's namespaces.
        let mut known: BTreeMap<String, String> = DEFAULT_NAMESPACES
            .iter()
            .map(|(p, n)| (p.to_string(), n.to_string()))
            .collect();
        for r in rl(&self.repos).values() {
            for (p, n) in rl(&r.namespaces).iter() {
                known.insert(p.clone(), n.clone());
            }
        }
        let lower = query.to_ascii_lowercase();
        let mut header = String::new();
        for (p, n) in &known {
            let used = query.contains(&format!("{p}:"))
                && !lower.contains(&format!("prefix {}:", p.to_ascii_lowercase()));
            if used {
                header.push_str(&format!("PREFIX {p}: <{n}>\n"));
            }
        }
        Response::text(200, format!("{header}{query}"))
    }

    fn saved_queries(&self, req: &Request) -> Response {
        let name = req.param("name").map(String::from);
        match req.method.as_str() {
            "GET" => {
                let list = lock(&self.saved_queries);
                match name {
                    Some(n) => match list.iter().find(|q| q.str_at(&["name"]) == n) {
                        Some(q) => Response::json(200, q.clone()),
                        None => Response::text(404, "Not found"),
                    },
                    None => Response::json(200, Json::Arr(list.clone())),
                }
            }
            "POST" => {
                let Ok(mut q) = Json::parse(&req.body_text()) else {
                    return Response::text(400, "Bad JSON");
                };
                if q.get("owner").is_none() {
                    q.set("owner", "admin");
                }
                let mut list = lock(&self.saved_queries);
                if list
                    .iter()
                    .any(|x| x.str_at(&["name"]) == q.str_at(&["name"]))
                {
                    return Response::text(409, "A query with this name already exists");
                }
                list.push(q);
                drop(list);
                self.save_queries();
                Response::empty(201)
            }
            "PUT" => {
                let Ok(mut q) = Json::parse(&req.body_text()) else {
                    return Response::text(400, "Bad JSON");
                };
                if q.get("owner").is_none() {
                    q.set("owner", "admin");
                }
                let old = req
                    .param("oldQueryName")
                    .unwrap_or(q.str_at(&["name"]))
                    .to_string();
                let mut list = lock(&self.saved_queries);
                list.retain(|x| x.str_at(&["name"]) != old);
                list.push(q);
                drop(list);
                self.save_queries();
                Response::empty(200)
            }
            "DELETE" => {
                if let Some(n) = name {
                    lock(&self.saved_queries).retain(|x| x.str_at(&["name"]) != n);
                    self.save_queries();
                }
                Response::empty(200)
            }
            _ => Response::text(405, "Method not allowed"),
        }
    }
}

/// PostgreSQL clients reach repositories through the same users and permissions.
impl crate::pgwire::Backend for App {
    fn auth_required(&self) -> bool {
        self.auth.is_some()
    }

    fn authenticate(
        &self,
        user: &str,
        password: Option<&str>,
        database: &str,
    ) -> Result<(), String> {
        if self.repo(database).is_none() {
            return Err(format!(
                "database \"{database}\" does not exist (use a repository id)"
            ));
        }
        let Some(auth) = &self.auth else {
            return Ok(());
        };
        let u = auth
            .check_password(user, password.unwrap_or(""))
            .ok_or("password authentication failed")?;
        if u.allows(&Need::Read(database.to_string())) {
            Ok(())
        } else {
            Err(format!("{user} may not read {database}"))
        }
    }

    fn with_store(
        &self,
        database: &str,
        f: &mut dyn FnMut(&Store) -> Result<crate::pgwire::Rows, String>,
    ) -> Result<crate::pgwire::Rows, String> {
        let repo = self
            .repo(database)
            .ok_or_else(|| format!("database \"{database}\" does not exist"))?;
        let st = repo.store.read();
        f(&st)
    }

    fn with_store_mut(
        &self,
        user: &str,
        database: &str,
        f: &mut dyn FnMut(&mut Store) -> Result<crate::pgwire::Rows, String>,
    ) -> Result<crate::pgwire::Rows, String> {
        let repo = self
            .repo(database)
            .ok_or_else(|| format!("database \"{database}\" does not exist"))?;
        if self.read_only {
            return Err("This server is read-only".into());
        }
        if let Some(auth) = &self.auth {
            if !lock(&auth.users)
                .get(user)
                .is_some_and(|u| u.allows(&Need::Write(database.to_string())))
            {
                return Err(format!("{user} may not write to {database}"));
            }
        }
        let mut run = || {
            let mut st = repo.store.write();
            transaction(&repo, &mut st, |st| f(st)).map_err(|a| match a {
                Abort::Failed(e) | Abort::Undo(e) => e,
                Abort::Shacl(report) => format!("SHACL violation: {report}"),
            })
        };
        let Some(raft) = self.cluster.get() else {
            return run();
        };
        // in a cluster the leader runs the write and replicates what it changed
        if !raft.is_leader() {
            return Err(match raft.leader_host() {
                Some(l) => {
                    format!("this node is not the cluster leader; write through the leader ({l})")
                }
                None => "no cluster leader is elected yet; retry shortly".into(),
            });
        }
        let _serial = lock(&self.cluster_write);
        repo.store.write().start_capture();
        let result = run();
        let (removed, inserted) = repo.store.write().take_capture();
        if result.is_ok() && !(removed.is_empty() && inserted.is_empty()) {
            let nq = |q: &[Quad]| rdf::write::serialize(q, Format::NQuads, &[]).unwrap_or_default();
            let op = Json::obj()
                .with("kind", "apply")
                .with("repo", database)
                .with("remove", nq(&removed))
                .with("insert", nq(&inserted));
            if raft
                .propose(op, std::time::Duration::from_secs(10))
                .is_err()
            {
                let _ = repo.store.write().apply(&inserted, &removed);
                return Err(
                    "The write could not be replicated to a majority of the cluster and was undone"
                        .into(),
                );
            }
        }
        result
    }
}

fn local_name(iri: &str) -> &str {
    iri.rsplit(['#', '/', ':']).next().unwrap_or(iri)
}

/// Up to 100 IRIs whose local name starts with `q` (case-insensitive),
/// shortest first, from an index rebuilt only when the data changed.
fn autocomplete(repo: &Repo, q: &str) -> Vec<Json> {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let version = repo.store.read().version();
    let mut cache = lock(&repo.autocomplete);
    if cache.as_ref().is_none_or(|(v, _)| *v != version) {
        let iris = repo.store.read().resource_iris();
        let mut names: Vec<(String, String)> = iris
            .into_iter()
            .map(|i| (local_name(&i).to_lowercase(), i))
            .collect();
        names.sort();
        *cache = Some((version, names));
    }
    let names = &cache.as_ref().unwrap().1;
    let start = names.partition_point(|(n, _)| n.as_str() < q.as_str());
    let mut hits: Vec<&(String, String)> = names[start..]
        .iter()
        .take_while(|(n, _)| n.starts_with(&q))
        .take(1000)
        .collect();
    hits.sort_by_key(|(n, _)| n.len());
    hits.into_iter()
        .take(100)
        .map(|(_, iri)| {
            let local = local_name(iri);
            Json::obj()
                .with("type", "uri")
                .with("value", iri.as_str())
                .with(
                    "description",
                    format!(
                        "<b>{}</b>{}",
                        &local[..q.len().min(local.len())],
                        &local[q.len().min(local.len())..]
                    ),
                )
        })
        .collect()
}

fn app_settings() -> Json {
    Json::obj()
        .with("DEFAULT_VIS_GRAPH_SCHEMA", true)
        .with("DEFAULT_INFERENCE", true)
        .with("DEFAULT_SAMEAS", true)
        .with("IGNORE_SHARED_QUERIES", false)
        .with("EXECUTE_COUNT", true)
}

fn admin_user() -> Json {
    Json::obj()
        .with("username", "admin")
        .with(
            "authorities",
            Json::Arr(vec![
                "ROLE_ADMIN".into(),
                "ROLE_USER".into(),
                "ROLE_REPO_MANAGER".into(),
                "ROLE_MONITORING".into(),
            ]),
        )
        .with("appSettings", app_settings())
        .with("external", false)
        .with("grantedAuthoritiesUiModel", Json::Arr(vec![]))
}

fn security_config(enabled: bool) -> Json {
    let settings = || {
        Json::obj()
            .with("enabled", false)
            .with("authorities", Json::Arr(vec![]))
            .with("appSettings", app_settings())
    };
    Json::obj()
        .with("enabled", enabled)
        .with("passwordLoginEnabled", true)
        .with("freeAccess", settings())
        .with("overrideAuth", settings())
        .with("openIdEnabled", false)
        .with("hasExternalAuth", false)
        .with("authImplementation", "Local")
        .with("allowSecurityToggle", true)
        .with("additionalAuthSources", Json::Arr(vec![]))
        .with("methodSettings", Json::obj())
}

fn location_json(default_repo: &str) -> Json {
    Json::obj()
        .with("uri", "")
        .with("label", "Local")
        .with("username", Json::Null)
        .with("password", Json::Null)
        .with("authType", "NONE")
        .with("locationType", "local")
        .with("active", true)
        .with("local", true)
        .with("system", true)
        .with("errorMsg", Json::Null)
        .with(
            "defaultRepository",
            if default_repo.is_empty() {
                Json::Null
            } else {
                Json::from(default_repo)
            },
        )
}

fn monitor_infrastructure() -> Json {
    Json::obj()
        .with(
            "heapMemoryUsage",
            Json::obj()
                .with("max", 0usize)
                .with("committed", 0usize)
                .with("used", 0usize),
        )
        .with(
            "nonHeapMemoryUsage",
            Json::obj()
                .with("max", 0usize)
                .with("committed", 0usize)
                .with("used", 0usize),
        )
        .with(
            "storageMemory",
            Json::obj()
                .with("dataDirUsed", 0usize)
                .with("workDirUsed", 0usize)
                .with("logsDirUsed", 0usize)
                .with("dataDirFree", 0usize)
                .with("workDirFree", 0usize)
                .with("logsDirFree", 0usize),
        )
        .with("threadCount", 0usize)
        .with("cpuLoad", 0.0)
        .with("classCount", 0usize)
        .with("gcCount", 0usize)
        .with("openFileDescriptors", 0usize)
        .with("maxFileDescriptors", 0usize)
}

fn default_import_settings() -> Json {
    Json::obj()
        .with("name", Json::Null)
        .with("status", "NONE")
        .with("message", "")
        .with("context", "")
        .with("replaceGraphs", Json::Arr(vec![]))
        .with("baseURI", Json::Null)
        .with("forceSerial", false)
        .with("type", Json::Null)
        .with("format", Json::Null)
        .with("data", Json::Null)
        .with(
            "parserSettings",
            Json::obj()
                .with("preserveBNodeIds", false)
                .with("failOnUnknownDataTypes", false)
                .with("verifyDataTypeValues", false)
                .with("normalizeDataTypeValues", false)
                .with("failOnUnknownLanguageTags", false)
                .with("verifyLanguageTags", true)
                .with("normalizeLanguageTags", false)
                .with("stopOnError", true),
        )
}

/// Parse one RDF term in N-Triples syntax (as used in protocol parameters).
/// Give memory freed by the allocator back to the operating system (glibc keeps freed
/// small allocations in its heaps otherwise, so an idle server would stay as large as its
/// busiest moment).
pub fn release_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // Safety: malloc_trim only returns free memory; it has no other effect.
        unsafe {
            malloc_trim(0);
        }
    }
}

/// One N-Triples term (`<iri>`, `"literal"`, `_:blank`); `None` when it is none.
pub fn parse_nt_term(s: &str) -> Option<Term> {
    let s = s.trim();
    if s.starts_with('<') || s.starts_with('"') || s.starts_with("_:") {
        let doc = format!("<urn:x:s> <urn:x:p> {s} .");
        let mut q = turtle::parse_nquads_raw(&doc).ok()?;
        return q.pop().map(|q| q.o);
    }
    if s.contains(':') {
        return Some(Term::iri(s));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relaxed_writes_reach_the_disk_on_sync() {
        let dir = std::env::temp_dir().join(format!("graphrd-relaxed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let q = |o: &str| crate::rdf::Quad {
            s: crate::rdf::Term::Iri("http://x/s".into()),
            p: crate::rdf::Term::Iri("http://x/p".into()),
            o: crate::rdf::Term::Iri(o.into()),
            g: None,
        };
        let lock = StoreLock::new(Store::open(&dir).unwrap());
        lock.write().apply(&[], &[q("http://x/a")]).unwrap();
        lock.write_relaxed().apply(&[], &[q("http://x/b")]).unwrap();
        lock.sync().unwrap();
        assert_eq!(lock.read().len(), 2);
        drop(lock);
        assert_eq!(Store::open(&dir).unwrap().len(), 2, "both survive a reopen");
        let _ = fs::remove_dir_all(&dir);
    }

    struct TestApp {
        app: App,
        dir: PathBuf,
    }

    impl Drop for TestApp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn app(name: &str, auth: Option<Auth>) -> TestApp {
        let dir = std::env::temp_dir().join(format!("graphrd-http-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        TestApp {
            app: App::open(&dir, None, auth).unwrap(),
            dir,
        }
    }

    fn req(method: &str, target: &str, headers: &[(&str, &str)], body: &str) -> Request {
        let (path, q) = target.split_once('?').unwrap_or((target, ""));
        Request {
            method: method.into(),
            path: http::percent_decode(path, false),
            query: http::parse_query(q),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn call(
        t: &TestApp,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, String) {
        let r = t.app.handle(&req(method, target, headers, body));
        (r.status, String::from_utf8_lossy(&r.body).into_owned())
    }

    const JSON_CT: (&str, &str) = ("Content-Type", "application/json");

    #[test]
    fn repository_lifecycle_and_protocol() {
        let t = app("life", None);
        let (s, _) = call(
            &t,
            "POST",
            "/rest/repositories",
            &[JSON_CT],
            r#"{"id":"zoo","title":"Zoo","params":{"ruleset":{"value":"rdfs-optimized"}}}"#,
        );
        assert_eq!(s, 201);
        let (s, _) = call(
            &t,
            "POST",
            "/rest/repositories",
            &[JSON_CT],
            r#"{"id":"zoo"}"#,
        );
        assert_eq!(s, 409);
        let (s, _) = call(
            &t,
            "POST",
            "/rest/repositories",
            &[JSON_CT],
            r#"{"id":"../evil"}"#,
        );
        assert_eq!(s, 400);

        let ttl = "@prefix ex: <http://ex.org/> . @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
                   ex:Dog rdfs:subClassOf ex:Animal . ex:rex a ex:Dog ; ex:name \"Rex\" .";
        let (s, _) = call(
            &t,
            "POST",
            "/repositories/zoo/statements",
            &[("Content-Type", "text/turtle")],
            ttl,
        );
        assert_eq!(s, 204);
        let (_, size) = call(&t, "GET", "/rest/repositories/zoo/size", &[], "");
        assert!(
            size.contains("\"explicit\":3") && size.contains("\"inferred\":1"),
            "{size}"
        );

        // prefixes from the upload became namespaces, usable without PREFIX
        let (s, body) = call(
            &t,
            "GET",
            "/repositories/zoo?query=SELECT%20%3Ft%20WHERE%20%7B%20ex%3Arex%20a%20%3Ft%20%7D",
            &[("Accept", "text/csv")],
            "",
        );
        assert_eq!(s, 200);
        assert_eq!(body.lines().count(), 3, "{body}"); // header + Dog + Animal
        let (_, body) = call(
            &t,
            "GET",
            "/repositories/zoo?infer=false&query=SELECT%20%3Ft%20WHERE%20%7B%20ex%3Arex%20a%20%3Ft%20%7D",
            &[("Accept", "text/csv")],
            "",
        );
        assert_eq!(body.lines().count(), 2, "{body}");

        // update + named graph + contexts + Graph Store protocol
        let (s, _) = call(
            &t,
            "POST",
            "/repositories/zoo/statements",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            "update=INSERT%20DATA%20%7B%20GRAPH%20%3Chttp%3A%2F%2Fg%3E%20%7B%20%3Chttp%3A%2F%2Fa%3E%20%3Chttp%3A%2F%2Fb%3E%201%20%7D%20%7D",
        );
        assert_eq!(s, 204);
        let (_, ctx) = call(
            &t,
            "GET",
            "/repositories/zoo/contexts",
            &[("Accept", "application/sparql-results+json")],
            "",
        );
        assert!(ctx.contains("http://g"));
        let (s, nt) = call(
            &t,
            "GET",
            "/repositories/zoo/rdf-graphs/service?graph=http%3A%2F%2Fg",
            &[("Accept", "application/n-triples")],
            "",
        );
        assert_eq!(s, 200);
        assert!(
            nt.contains(
                "<http://a> <http://b> \"1\"^^<http://www.w3.org/2001/XMLSchema#integer> ."
            ),
            "{nt}"
        );
        let (s, _) = call(
            &t,
            "DELETE",
            "/repositories/zoo/statements?context=%3Chttp%3A%2F%2Fg%3E",
            &[],
            "",
        );
        assert_eq!(s, 204);
        let (_, n) = call(&t, "GET", "/repositories/zoo/size", &[], "");
        assert_eq!(n, "3");

        // syntax errors are client errors
        let (s, _) = call(&t, "GET", "/repositories/zoo?query=SELEC%20nope", &[], "");
        assert_eq!(s, 400);

        // persistence: reopen the data directory
        let reopened = App::open(&t.dir, None, None).unwrap();
        let r = reopened.handle(&req("GET", "/rest/repositories/zoo/size", &[], ""));
        assert!(String::from_utf8_lossy(&r.body).contains("\"explicit\":3"));
        drop(reopened);

        let (s, _) = call(&t, "DELETE", "/rest/repositories/zoo", &[], "");
        assert_eq!(s, 200);
        let (s, _) = call(&t, "GET", "/repositories/zoo/size", &[], "");
        assert_eq!(s, 404);
    }

    #[test]
    fn import_text_and_saved_queries() {
        let t = app("import", None);
        call(
            &t,
            "POST",
            "/rest/repositories",
            &[JSON_CT],
            r#"{"id":"r"}"#,
        );
        let settings = r#"{"name":"snippet","type":"text","format":"application/n-triples","data":"<http://a> <http://b> <http://c> .","context":"http://ctx"}"#;
        let (s, _) = call(
            &t,
            "POST",
            "/rest/repositories/r/import/upload/text",
            &[JSON_CT],
            settings,
        );
        assert_eq!(s, 200);
        let (_, list) = call(&t, "GET", "/rest/repositories/r/import/upload", &[], "");
        assert!(
            list.contains("\"status\":\"DONE\"") && list.contains("\"addedStatements\":1"),
            "{list}"
        );
        let (_, n) = call(
            &t,
            "GET",
            "/repositories/r/size?context=%3Chttp%3A%2F%2Fctx%3E",
            &[],
            "",
        );
        assert_eq!(n, "1");

        let (s, _) = call(
            &t,
            "POST",
            "/rest/sparql/saved-queries",
            &[JSON_CT],
            r#"{"name":"all","body":"SELECT * {?s ?p ?o}"}"#,
        );
        assert_eq!(s, 201);
        let (_, list) = call(&t, "GET", "/rest/sparql/saved-queries", &[], "");
        assert!(list.contains("\"name\":\"all\""));
    }

    #[test]
    fn authentication() {
        let t = app("auth", Some(Auth::new("admin", "s3cret")));
        // public discovery endpoints stay reachable
        let (s, body) = call(&t, "GET", "/rest/security/all", &[], "");
        assert_eq!(s, 200);
        assert!(body.contains("\"enabled\":true"));
        assert_eq!(call(&t, "GET", "/", &[], "").0, 200);
        // protected endpoints need credentials
        assert_eq!(call(&t, "GET", "/rest/repositories", &[], "").0, 401);
        assert_eq!(call(&t, "GET", "/repositories", &[], "").0, 401);
        // Basic auth (admin:s3cret)
        assert_eq!(
            call(
                &t,
                "GET",
                "/rest/repositories",
                &[("Authorization", "Basic YWRtaW46czNjcmV0")],
                ""
            )
            .0,
            200
        );
        assert_eq!(
            call(
                &t,
                "GET",
                "/rest/repositories",
                &[("Authorization", "Basic YWRtaW46d3Jvbmc=")],
                ""
            )
            .0,
            401
        );
        // token login
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/login",
                &[JSON_CT],
                r#"{"username":"admin","password":"nope"}"#
            )
            .0,
            401
        );
        let r = t.app.handle(&req(
            "POST",
            "/rest/login",
            &[JSON_CT],
            r#"{"username":"admin","password":"s3cret"}"#,
        ));
        assert_eq!(r.status, 200);
        let token = r
            .headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(token.starts_with("Bearer "));
        assert_eq!(
            call(
                &t,
                "GET",
                "/rest/repositories",
                &[("Authorization", &token)],
                ""
            )
            .0,
            200
        );
        call(&t, "POST", "/rest/logout", &[("Authorization", &token)], "");
        assert_eq!(
            call(
                &t,
                "GET",
                "/rest/repositories",
                &[("Authorization", &token)],
                ""
            )
            .0,
            401
        );
    }

    #[test]
    fn multi_user_permissions() {
        let t = app("users", Some(Auth::new("admin", "s3cret")));
        let admin = ("Authorization", "Basic YWRtaW46czNjcmV0"); // admin:s3cret
        for id in ["zoo", "lab"] {
            assert_eq!(
                call(
                    &t,
                    "POST",
                    "/rest/repositories",
                    &[JSON_CT, admin],
                    &format!(r#"{{"id":"{id}"}}"#)
                )
                .0,
                201
            );
        }
        call(
            &t,
            "POST",
            "/repositories/zoo/statements",
            &[("Content-Type", "application/n-triples"), admin],
            "<http://a> <http://b> <http://c> .",
        );
        // alice may read zoo only; bob may write lab; carol manages repositories
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/security/users/alice",
                &[JSON_CT, admin],
                r#"{"password":"pw-a","grantedAuthorities":["ROLE_USER","READ_REPO_zoo"]}"#
            )
            .0,
            201
        );
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/security/users/bob",
                &[JSON_CT, admin],
                r#"{"password":"pw-b","grantedAuthorities":["ROLE_USER","WRITE_REPO_lab"]}"#
            )
            .0,
            201
        );
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/security/users/carol",
                &[JSON_CT, admin],
                r#"{"password":"pw-c","grantedAuthorities":["ROLE_USER","ROLE_REPO_MANAGER"]}"#
            )
            .0,
            201
        );
        let alice = ("Authorization", "Basic YWxpY2U6cHctYQ=="); // alice:pw-a
        let bob = ("Authorization", "Basic Ym9iOnB3LWI="); // bob:pw-b
        let carol = ("Authorization", "Basic Y2Fyb2w6cHctYw=="); // carol:pw-c
        let q = "/repositories/zoo?query=SELECT%20*%20%7B%3Fs%20%3Fp%20%3Fo%7D";
        assert_eq!(call(&t, "GET", q, &[alice], "").0, 200);
        assert_eq!(call(&t, "GET", q, &[bob], "").0, 403);
        let upd = [("Content-Type", "application/sparql-update")];
        assert_eq!(
            call(
                &t,
                "POST",
                "/repositories/zoo/statements",
                &[upd[0], alice],
                "INSERT DATA { <http://x> <http://y> <http://z> }"
            )
            .0,
            403
        );
        assert_eq!(
            call(
                &t,
                "POST",
                "/repositories/lab/statements",
                &[upd[0], bob],
                "INSERT DATA { <http://x> <http://y> <http://z> }"
            )
            .0,
            204
        );
        assert_eq!(call(&t, "GET", "/repositories/lab/size", &[bob], "").0, 200); // write implies read
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/repositories",
                &[JSON_CT, bob],
                r#"{"id":"nope"}"#
            )
            .0,
            403
        );
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/repositories",
                &[JSON_CT, carol],
                r#"{"id":"new"}"#
            )
            .0,
            201
        );
        assert_eq!(call(&t, "GET", "/rest/security/users", &[carol], "").0, 403);
        // users change their own password, then the old one stops working
        assert_eq!(
            call(
                &t,
                "PATCH",
                "/rest/security/users/alice",
                &[JSON_CT, alice],
                r#"{"password":"pw-a2"}"#
            )
            .0,
            200
        );
        assert_eq!(call(&t, "GET", q, &[alice], "").0, 401);
        assert_eq!(
            call(
                &t,
                "PATCH",
                "/rest/security/users/bob",
                &[JSON_CT, alice],
                r#"{"password":"x"}"#
            )
            .0,
            401
        );
        // the bootstrap admin is protected; users persist across restarts
        assert_eq!(
            call(&t, "DELETE", "/rest/security/users/admin", &[admin], "").0,
            400
        );
        let reopened = App::open(&t.dir, None, Some(Auth::new("admin", "s3cret"))).unwrap();
        let r = reopened.handle(&req("GET", "/rest/security/users/bob", &[admin], ""));
        assert_eq!(r.status, 200);
        assert!(String::from_utf8_lossy(&r.body).contains("WRITE_REPO_lab"));
    }

    #[test]
    fn browsers_scripts_get_no_basic_challenge() {
        let t = app("challenge", Some(Auth::new("admin", "s3cret")));
        let challenge = |headers: &[(&str, &str)]| {
            let r = t.app.handle(&req("GET", "/rest/repositories", headers, ""));
            assert_eq!(r.status, 401);
            r.headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("www-authenticate"))
        };
        assert!(challenge(&[]), "API clients are challenged");
        assert!(
            challenge(&[("Sec-Fetch-Mode", "navigate")]),
            "so is a page opened in the browser"
        );
        assert!(
            !challenge(&[("Sec-Fetch-Mode", "cors")]),
            "a script in the page is not: it shows its own sign-in"
        );
        assert!(!challenge(&[("X-GraphRD-UI", "1")]));
    }

    #[test]
    fn password_guessing_locks_the_user() {
        let t = app("lockout", Some(Auth::new("admin", "s3cret")));
        let good = ("Authorization", "Basic YWRtaW46czNjcmV0"); // admin:s3cret
        let bad = ("Authorization", "Basic YWRtaW46d3Jvbmc="); // admin:wrong
        assert_eq!(call(&t, "GET", "/rest/repositories", &[good], "").0, 200);
        for _ in 0..MAX_FAILURES {
            assert_eq!(call(&t, "GET", "/rest/repositories", &[bad], "").0, 401);
        }
        // a new credential (not the cached one) is checked, and refused while locked
        let fresh = App::open(&t.dir, None, Some(Auth::new("admin", "s3cret"))).unwrap();
        for _ in 0..MAX_FAILURES {
            fresh.handle(&req("GET", "/rest/repositories", &[bad], ""));
        }
        assert_eq!(
            fresh
                .handle(&req("GET", "/rest/repositories", &[good], ""))
                .status,
            401,
            "locked even with the right password"
        );
    }

    #[test]
    fn a_failed_update_changes_nothing() {
        let t = app("atomic", None);
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/repositories",
                &[JSON_CT],
                r#"{"id":"r"}"#
            )
            .0,
            201
        );
        let upd = ("Content-Type", "application/sparql-update");
        assert_eq!(
            call(
                &t,
                "POST",
                "/repositories/r/statements",
                &[upd],
                "INSERT DATA { GRAPH <http://g> { <http://a> <http://p> 1 } }"
            )
            .0,
            204
        );
        // the second operation fails (nothing listens there), so the first is undone
        let r = call(
            &t,
            "POST",
            "/repositories/r/statements",
            &[upd],
            "DROP GRAPH <http://g> ; INSERT DATA { <http://b> <http://p> 2 } ; LOAD <http://127.0.0.1:9/nothing.ttl>",
        );
        assert!(r.0 >= 400, "{r:?}");
        let (_, size) = call(&t, "GET", "/repositories/r/size", &[], "");
        assert_eq!(
            size.trim(),
            "1",
            "the graph is still there and nothing was added"
        );
    }

    #[test]
    fn shacl_validation_rolls_back_bad_writes() {
        let t = app("shacl", None);
        assert_eq!(call(&t, "POST", "/rest/repositories", &[JSON_CT], r#"{"id":"s","params":{"isShacl":{"value":"true"},"ruleset":{"value":"rdfs-optimized"}}}"#).0, 201);
        let shapes = "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://ex.org/> . @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
            ex:PersonShape a sh:NodeShape ; sh:targetClass ex:Person ;\n\
              sh:property [ sh:path ex:name ; sh:minCount 1 ; sh:maxCount 1 ; sh:datatype xsd:string ] ;\n\
              sh:property [ sh:path ex:age ; sh:datatype xsd:integer ; sh:minInclusive 0 ; sh:maxInclusive 150 ] ;\n\
              sh:property [ sh:path ex:email ; sh:pattern \"^[^@]+@[^@]+$\" ] ;\n\
              sh:property [ sh:path ex:worksFor ; sh:class ex:Org ] .";
        let ctx = "/repositories/s/statements?context=%3Chttp%3A%2F%2Frdf4j.org%2Fschema%2Frdf4j%23SHACLShapeGraph%3E";
        assert_eq!(
            call(&t, "POST", ctx, &[("Content-Type", "text/turtle")], shapes).0,
            204
        );
        let upd = ("Content-Type", "application/sparql-update");
        let ok = "PREFIX ex: <http://ex.org/> INSERT DATA { ex:Acme a ex:Org . ex:ann a ex:Person ; ex:name \"Ann\" ; ex:age 30 ; ex:email \"ann@acme.org\" ; ex:worksFor ex:Acme }";
        assert_eq!(
            call(&t, "POST", "/repositories/s/statements", &[upd], ok).0,
            204
        );
        // each of these breaks one constraint and must be rejected and rolled back
        for bad in [
            "PREFIX ex: <http://ex.org/> INSERT DATA { ex:bob a ex:Person ; ex:age 40 }", // minCount name
            "PREFIX ex: <http://ex.org/> INSERT DATA { ex:cy a ex:Person ; ex:name \"Cy\" ; ex:age 200 }", // range
            "PREFIX ex: <http://ex.org/> INSERT DATA { ex:di a ex:Person ; ex:name \"Di\" ; ex:email \"nope\" }", // pattern
            "PREFIX ex: <http://ex.org/> INSERT DATA { ex:ann ex:name \"Annie\" }", // maxCount
            "PREFIX ex: <http://ex.org/> DELETE DATA { ex:Acme a ex:Org }", // class, via a delete
        ] {
            let (status, body) = call(&t, "POST", "/repositories/s/statements", &[upd], bad);
            assert_eq!(status, 400, "{bad}");
            assert!(body.contains("\"conforms\":false"), "{body}");
        }
        let (_, n) = call(&t, "GET", "/repositories/s/size?context=null", &[], "");
        assert_eq!(n, "6", "rejected writes must leave the data unchanged"); // Acme + 5 about Ann
        // a subclass instance is validated too (rdfs:subClassOf* in data)
        let sub = "PREFIX ex: <http://ex.org/> PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> INSERT DATA { ex:Student rdfs:subClassOf ex:Person . ex:ed a ex:Student }";
        assert_eq!(
            call(&t, "POST", "/repositories/s/statements", &[upd], sub).0,
            400
        );
        let (status, report) = call(&t, "GET", "/rest/repositories/s/shacl/validate", &[], "");
        assert_eq!(status, 200);
        assert!(report.contains("\"conforms\":true"), "{report}");
    }

    #[test]
    fn shacl_logical_qualified_and_sparql_constraints() {
        let t = app("shacl2", None);
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/repositories",
                &[JSON_CT],
                r#"{"id":"s","params":{"isShacl":{"value":"true"}}}"#
            )
            .0,
            201
        );
        let shapes = "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://ex.org/> . @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
            ex:Named a sh:NodeShape ; sh:property [ sh:path ex:name ; sh:minCount 1 ] .\n\
            ex:OrderShape a sh:NodeShape ; sh:targetClass ex:Order ; sh:closed true ; sh:ignoredProperties ( <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> ex:total ) ;\n\
              sh:property [ sh:path ex:id ; sh:or ( [ sh:datatype xsd:integer ] [ sh:datatype xsd:string ] ) ] ;\n\
              sh:property [ sh:path ex:line ; sh:qualifiedValueShape [ sh:class ex:Gift ] ; sh:qualifiedMaxCount 1 ] ;\n\
              sh:property [ sh:path ( ex:customer ex:name ) ; sh:minCount 1 ] ;\n\
              sh:property [ sh:path ex:customer ; sh:node ex:Named ; sh:not [ sh:hasValue ex:banned ] ] ;\n\
              sh:property [ sh:path ex:note ; sh:maxLength 5 ; sh:severity sh:Warning ] ;\n\
              sh:sparql [ sh:select \"SELECT $this WHERE { $this <http://ex.org/total> ?t FILTER (?t < 0) }\" ] .\n\
            ex:BigShape a sh:NodeShape ; sh:target [ a sh:SPARQLTarget ; sh:select \"SELECT ?this WHERE { ?this <http://ex.org/total> ?t FILTER (?t > 100) }\" ] ;\n\
              sh:property [ sh:path ex:approvedBy ; sh:minCount 1 ] .";
        let ctx = "/repositories/s/statements?context=%3Chttp%3A%2F%2Frdf4j.org%2Fschema%2Frdf4j%23SHACLShapeGraph%3E";
        assert_eq!(
            call(&t, "POST", ctx, &[("Content-Type", "text/turtle")], shapes).0,
            204
        );
        let upd = ("Content-Type", "application/sparql-update");
        // a warning (sh:note too long) is reported but does not block the write
        let ok = "PREFIX ex: <http://ex.org/> INSERT DATA { ex:ann ex:name \"Ann\" . ex:g1 a ex:Gift . ex:o1 a ex:Order ; ex:id 7 ; ex:customer ex:ann ; ex:line ex:g1 ; ex:total 5 ; ex:note \"too long\" }";
        let r = call(&t, "POST", "/repositories/s/statements", &[upd], ok);
        assert_eq!(r.0, 204, "{}", r.1);
        let (_, report) = call(&t, "GET", "/rest/repositories/s/shacl/validate", &[], "");
        assert!(
            report.contains("\"conforms\":false")
                && report.contains("\"resultSeverity\":\"Warning\""),
            "{report}"
        );
        for (bad, component) in [
            ("ex:o1 ex:id true", "Or"),
            ("ex:g2 a ex:Gift . ex:o1 ex:line ex:g2", "QualifiedMaxCount"),
            ("ex:bob a ex:Thing . ex:o1 ex:customer ex:bob", "Node"),
            ("ex:o1 ex:total -1", "SPARQL"),
            ("ex:o1 ex:colour \"red\"", "Closed"),
            (
                "ex:banned ex:name \"B\" . ex:o1 ex:customer ex:banned",
                "Not",
            ),
            ("ex:o2 ex:total 500", "MinCount"),
        ] {
            let (status, body) = call(
                &t,
                "POST",
                "/repositories/s/statements",
                &[upd],
                &format!("PREFIX ex: <http://ex.org/> INSERT DATA {{ {bad} }}"),
            );
            assert_eq!(status, 400, "{bad}");
            assert!(
                body.contains(&format!("#{component}ConstraintComponent")),
                "{bad}: {body}"
            );
        }
    }

    #[test]
    fn full_text_search_endpoint_with_facets() {
        let t = app("ftsrest", None);
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/repositories",
                &[JSON_CT],
                r#"{"id":"f","params":{"enableFtsIndex":{"value":"true"}}}"#
            )
            .0,
            201
        );
        let data = "@prefix ex: <http://ex.org/> .\n\
            ex:a a ex:Book ; ex:title \"Graph databases in practice\"@en .\n\
            ex:b a ex:Book ; ex:title \"Relational database design\"@en .\n\
            ex:c a ex:Article ; ex:title \"Veritabanları ve grafikler\"@tr .\n\
            ex:d a ex:Article ; ex:title \"A database of graphs\" .";
        assert_eq!(
            call(
                &t,
                "POST",
                "/repositories/f/statements",
                &[("Content-Type", "text/turtle")],
                data
            )
            .0,
            204
        );
        let (status, body) = call(
            &t,
            "GET",
            "/rest/repositories/f/search?query=database&facet=http%3A%2F%2Fwww.w3.org%2F1999%2F02%2F22-rdf-syntax-ns%23type",
            &[],
            "",
        );
        assert_eq!(status, 200, "{body}");
        let j = Json::parse(&body).unwrap();
        assert_eq!(j.get("total").and_then(|t| t.as_f64()), Some(3.0), "{body}");
        assert!(
            body.contains(r#"Relational <b>database</b> design"#)
                && body.contains(r#"Graph <b>databases</b> in practice"#),
            "{body}"
        );
        assert!(
            body.contains(r#"{"value":"<http://ex.org/Book>","count":2}"#)
                && body.contains(r#"{"value":"<http://ex.org/Article>","count":1}"#),
            "{body}"
        );
        // a misspelling still finds it; Turkish words are stemmed
        let (_, body) = call(
            &t,
            "GET",
            "/rest/repositories/f/search?query=relatoinal&pre=%5B&post=%5D",
            &[],
            "",
        );
        assert!(
            body.contains(r#""highlight":"[Relational] database design""#),
            "{body}"
        );
        let (_, body) = call(
            &t,
            "GET",
            "/rest/repositories/f/search?query=veritaban%C4%B1&limit=1",
            &[],
            "",
        );
        assert!(
            body.contains("\"total\":1") && body.contains("http://ex.org/c"),
            "{body}"
        );
        assert_eq!(
            call(&t, "GET", "/rest/repositories/f/search?query=x", &[], "").0,
            200
        );
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/repositories",
                &[JSON_CT],
                r#"{"id":"nofts"}"#
            )
            .0,
            201
        );
        assert_eq!(
            call(
                &t,
                "GET",
                "/rest/repositories/nofts/search?query=x",
                &[],
                ""
            )
            .0,
            400
        );
    }

    #[test]
    fn graphql_mutations_are_transactions() {
        let t = app("gqlmut", None);
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/repositories",
                &[JSON_CT],
                r#"{"id":"g","params":{"isShacl":{"value":"true"}}}"#
            )
            .0,
            201
        );
        let shapes = "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://ex.org/> .\n\
            ex:PersonShape a sh:NodeShape ; sh:targetClass ex:Person ; sh:property [ sh:path ex:name ; sh:minCount 1 ; sh:maxCount 1 ] .";
        let ctx = "/repositories/g/statements?context=%3Chttp%3A%2F%2Frdf4j.org%2Fschema%2Frdf4j%23SHACLShapeGraph%3E";
        assert_eq!(
            call(&t, "POST", ctx, &[("Content-Type", "text/turtle")], shapes).0,
            204
        );
        let data = "@prefix ex: <http://ex.org/> . ex:ann a ex:Person ; ex:name \"Ann\" .";
        assert_eq!(
            call(
                &t,
                "POST",
                "/repositories/g/statements",
                &[("Content-Type", "text/turtle")],
                data
            )
            .0,
            204
        );
        let gql = ("Content-Type", "application/graphql");
        let (status, body) = call(
            &t,
            "POST",
            "/rest/repositories/g/graphql",
            &[gql],
            r#"mutation { createPerson(id: "http://ex.org/bo", input: {name: "Bo"}) { id name } }"#,
        );
        assert_eq!(
            (status, body.as_str()),
            (
                200,
                r#"{"data":{"createPerson":{"id":"http://ex.org/bo","name":"Bo"}}}"#
            )
        );
        // the second field breaks a shape: the whole request is undone, the first field too
        let bad = r#"mutation { createPerson(id: "http://ex.org/cy", input: {name: "Cy"}) { id } updatePerson(id: "http://ex.org/ann", input: {name: ["A", "B"]}) { id } }"#;
        let (_, body) = call(&t, "POST", "/rest/repositories/g/graphql", &[gql], bad);
        assert!(
            body.contains("SHACL violation") && body.contains("\"conforms\":false"),
            "{body}"
        );
        let (_, body) = call(
            &t,
            "POST",
            "/rest/repositories/g/graphql",
            &[gql],
            r#"mutation { deletePerson(id: "http://ex.org/nobody") { id } }"#,
        );
        assert!(body.contains("No Person with id"), "{body}");
        let (_, n) = call(&t, "GET", "/repositories/g/size?context=null", &[], "");
        assert_eq!(n, "4", "Ann and Bo, nothing of the failed requests");
        // a mutation over GET is refused; queries need read access only, mutations write access
        let (_, body) = call(
            &t,
            "GET",
            "/rest/repositories/g/graphql?query=mutation%7BdeletePerson(id%3A%22http%3A%2F%2Fex.org%2Fbo%22)%7D",
            &[],
            "",
        );
        assert!(body.contains("needs a POST"), "{body}");
        let segs = ["rest", "repositories", "g", "graphql"];
        assert_eq!(
            App::need(
                &segs,
                &req(
                    "POST",
                    "/rest/repositories/g/graphql",
                    &[gql],
                    "{ Person { id } }"
                )
            ),
            Need::Read("g".into())
        );
        assert_eq!(
            App::need(
                &segs,
                &req(
                    "POST",
                    "/rest/repositories/g/graphql",
                    &[gql],
                    "mutation { deletePerson(id: \"x\") }"
                )
            ),
            Need::Write("g".into())
        );
    }

    #[test]
    fn backup_restore_and_rdf_rank() {
        let t = app("backup", None);
        call(
            &t,
            "POST",
            "/rest/repositories",
            &[JSON_CT],
            r#"{"id":"r","title":"Kept","params":{"ruleset":{"value":"rdfs-optimized"}}}"#,
        );
        let ttl = "@prefix ex: <http://ex.org/> . ex:a ex:links ex:hub . ex:b ex:links ex:hub . ex:c ex:links ex:hub . ex:hub ex:links ex:a . _:x ex:label \"blank\" . ex:hub ex:in ex:g .";
        call(
            &t,
            "POST",
            "/repositories/r/statements",
            &[("Content-Type", "text/turtle")],
            ttl,
        );
        call(
            &t,
            "POST",
            "/repositories/r/statements?context=%3Chttp%3A%2F%2Fex.org%2Fg%3E",
            &[("Content-Type", "text/turtle")],
            "<http://ex.org/x> <http://ex.org/y> \"in a graph\" .",
        );
        // RDF Rank: the hub collects the most links
        assert_eq!(
            call(&t, "GET", "/rest/rank/status?repository=r", &[], "").1,
            "EMPTY"
        );
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/rank/compute",
                &[("X-GraphRD-Repository", "r")],
                ""
            )
            .0,
            200
        );
        assert_eq!(
            call(&t, "GET", "/rest/rank/status?repository=r", &[], "").1,
            "COMPUTED"
        );
        let q = "PREFIX rank: <http://graphrd.org/rank#> SELECT ?n WHERE { ?n rank:score3 ?r } ORDER BY DESC(?r) LIMIT 1";
        let (_, csv) = call(
            &t,
            "GET",
            &format!("/repositories/r?query={}", http::percent_encode(q)),
            &[("Accept", "text/csv")],
            "",
        );
        assert!(csv.contains("http://ex.org/hub"), "{csv}");
        call(
            &t,
            "POST",
            "/repositories/r/statements",
            &[("Content-Type", "text/turtle")],
            "<http://ex.org/new> <http://ex.org/links> <http://ex.org/a> .",
        );
        assert_eq!(
            call(&t, "GET", "/rest/rank/status?repository=r", &[], "").1,
            "OUTDATED"
        );
        // backup, wipe, restore
        let r = t.app.handle(&req("GET", "/rest/recovery/backup", &[], ""));
        assert_eq!(r.status, 200);
        let archive = r.body.clone();
        assert_eq!(call(&t, "DELETE", "/rest/repositories/r", &[], "").0, 200);
        let mut restore = req(
            "POST",
            "/rest/recovery/restore",
            &[("Content-Type", "application/x-tar")],
            "",
        );
        restore.body = archive;
        let r = t.app.handle(&restore);
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        let (_, size) = call(&t, "GET", "/rest/repositories/r/size", &[], "");
        assert!(size.contains("\"explicit\":8"), "{size}");
        let (_, cfg) = call(&t, "GET", "/rest/repositories/r", &[], "");
        assert!(
            cfg.contains("Kept") && cfg.contains("rdfs-optimized"),
            "{cfg}"
        );
        let (_, n) = call(
            &t,
            "GET",
            "/repositories/r/size?context=%3Chttp%3A%2F%2Fex.org%2Fg%3E",
            &[],
            "",
        );
        assert_eq!(n, "1");
    }

    #[test]
    fn autocomplete_suggests_by_local_name() {
        let t = app("ac", None);
        call(
            &t,
            "POST",
            "/rest/repositories",
            &[JSON_CT],
            r#"{"id":"a"}"#,
        );
        call(
            &t,
            "POST",
            "/repositories/a/statements",
            &[("Content-Type", "text/turtle")],
            "@prefix ex: <http://ex.org/> . ex:Person ex:hasName \"x\" . ex:Personnel ex:hasName \"y\" . ex:Planet ex:orbits ex:Sun .",
        );
        let (_, body) = call(
            &t,
            "GET",
            "/rest/autocomplete/query?repository=a&q=pers",
            &[],
            "",
        );
        assert!(
            body.contains("http://ex.org/Person\"")
                && body.contains("Personnel")
                && !body.contains("Planet"),
            "{body}"
        );
        call(
            &t,
            "POST",
            "/repositories/a/statements",
            &[("Content-Type", "text/turtle")],
            "<http://ex.org/Persona> <http://ex.org/hasName> \"z\" .",
        );
        let (_, body) = call(
            &t,
            "GET",
            "/rest/autocomplete/query?repository=a&q=has",
            &[],
            "",
        );
        assert!(body.contains("hasName"), "{body}");
        let (_, body) = call(
            &t,
            "GET",
            "/rest/autocomplete/query?repository=a&q=persona",
            &[],
            "",
        );
        assert!(
            body.contains("Persona"),
            "index refreshes after writes: {body}"
        );
    }

    #[test]
    fn prometheus_metrics_and_slow_query_log() {
        let t = app("metrics", None);
        call(
            &t,
            "POST",
            "/rest/repositories",
            &[JSON_CT],
            r#"{"id":"m"}"#,
        );
        call(
            &t,
            "POST",
            "/repositories/m/statements",
            &[("Content-Type", "application/n-triples")],
            "<http://a> <http://b> <http://c> .",
        );
        crate::metrics::SLOW_MS.store(1, std::sync::atomic::Ordering::Relaxed);
        let slow = "SELECT (COUNT(*) AS ?n) { VALUES ?i { 1 2 3 4 5 6 7 8 9 10 } VALUES ?j { 1 2 3 4 5 6 7 8 9 10 } VALUES ?k { 1 2 3 4 5 6 7 8 9 10 } VALUES ?l { 1 2 3 4 5 6 7 8 9 10 } FILTER(STRLEN(STR(?i * ?j * ?k * ?l)) > 0) }";
        let (s, _) = call(
            &t,
            "GET",
            &format!("/repositories/m?query={}", http::percent_encode(slow)),
            &[],
            "",
        );
        crate::metrics::SLOW_MS.store(0, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(s, 200);
        let (s, body) = call(&t, "GET", "/metrics", &[], "");
        assert_eq!(s, 200);
        assert!(
            body.contains("graphrd_statements{repository=\"m\"} 1\n"),
            "{body}"
        );
        assert!(body.contains("# TYPE graphrd_query_duration_seconds histogram"));
        assert!(body.contains("graphrd_http_responses_total{code=\"2xx\"}"));
        let (_, log) = call(&t, "GET", "/rest/monitor/slow-queries", &[], "");
        assert!(log.contains("VALUES ?i"), "{log}");
    }

    #[test]
    fn standard_sparql_endpoint_and_service_description() {
        let t = app("sparql-alias", None);
        assert_eq!(
            call(&t, "GET", "/sparql?query=ASK%7B%7D", &[], "").0,
            404,
            "no repository yet"
        );
        call(
            &t,
            "POST",
            "/rest/repositories",
            &[JSON_CT],
            r#"{"id":"only"}"#,
        );
        let upd = ("Content-Type", "application/sparql-update");
        assert_eq!(
            call(
                &t,
                "POST",
                "/sparql",
                &[upd],
                "INSERT DATA { <http://a> <http://b> <rel> }"
            )
            .0,
            204
        );
        let (s, body) = call(
            &t,
            "GET",
            "/sparql?query=SELECT%20%3Fo%20%7B%3Fs%20%3Fp%20%3Fo%7D",
            &[("Accept", "text/csv"), ("Host", "h:1")],
            "",
        );
        assert_eq!(s, 200);
        assert!(
            body.contains("http://localhost/rel") || body.contains("/rel"),
            "relative IRIs resolve against the endpoint: {body}"
        );
        assert_eq!(
            call(&t, "GET", "/sparql?update=CLEAR%20ALL", &[], "").0,
            405
        );
        assert_eq!(
            call(
                &t,
                "POST",
                "/sparql",
                &[("Content-Type", "text/plain")],
                "ASK {}"
            )
            .0,
            415
        );
        assert_eq!(
            call(
                &t,
                "GET",
                "/sparql?query=ASK%7B%7D&query=ASK%7B%7D",
                &[],
                ""
            )
            .0,
            400
        );
        let (s, sd) = call(
            &t,
            "GET",
            "/repositories/only/sparql",
            &[("Accept", "text/turtle"), ("Host", "h:1")],
            "",
        );
        assert_eq!(s, 200);
        assert!(
            sd.contains("<http://h:1/repositories/only/sparql>") && sd.contains("SPARQL11Update"),
            "{sd}"
        );
    }

    #[test]
    fn memory_server_writes_nothing() {
        let app = App::open_memory(None, Some(Auth::new("admin", "pw")));
        let admin = ("Authorization", "Basic YWRtaW46cHc=");
        let r = |m: &str, t: &str, h: &[(&str, &str)], b: &str| app.handle(&req(m, t, h, b));
        assert_eq!(
            r(
                "POST",
                "/rest/repositories",
                &[JSON_CT, admin],
                r#"{"id":"m","params":{"ruleset":{"value":"rdfs"}}}"#
            )
            .status,
            201
        );
        let ttl = "@prefix ex: <http://ex/> . ex:Dog rdfs:subClassOf ex:Animal . ex:rex a ex:Dog .";
        let ttl = format!("@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> . {ttl}");
        assert_eq!(
            r(
                "POST",
                "/repositories/m/statements",
                &[("Content-Type", "text/turtle"), admin],
                &ttl
            )
            .status,
            204
        );
        let resp = r(
            "GET",
            "/repositories/m?query=ASK%7B%3Chttp%3A%2F%2Fex%2Frex%3E%20a%20%3Chttp%3A%2F%2Fex%2FAnimal%3E%7D",
            &[("Accept", "application/sparql-results+json"), admin],
            "",
        );
        assert!(
            String::from_utf8_lossy(&resp.body).contains("true"),
            "inference works in memory"
        );
        assert_eq!(
            r(
                "POST",
                "/rest/security/users/bob",
                &[JSON_CT, admin],
                r#"{"password":"x","grantedAuthorities":["ROLE_USER"]}"#
            )
            .status,
            201
        );
        assert_eq!(r("POST", "/rest/recovery/backup", &[admin], "").status, 400);
        assert_eq!(r("DELETE", "/repositories/m", &[admin], "").status, 200);
        assert!(
            !Path::new("repositories").exists() && !Path::new("users.json").exists(),
            "nothing written to the working directory"
        );
    }

    #[test]
    fn api_keys_and_audit_log() {
        let mut t = app("apikeys", Some(Auth::new("admin", "pw")));
        let log = std::env::temp_dir().join(format!("graphrd-audit-{}.jsonl", std::process::id()));
        let _ = fs::remove_file(&log);
        t.app.audit = Some(Mutex::new(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .unwrap(),
        ));
        let admin = ("Authorization", "Basic YWRtaW46cHc=");
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/repositories",
                &[JSON_CT, admin],
                r#"{"id":"k"}"#
            )
            .0,
            201
        );
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/security/users/ci",
                &[JSON_CT, admin],
                r#"{"password":"x-long-pw","grantedAuthorities":["ROLE_USER","WRITE_REPO_k"]}"#
            )
            .0,
            201
        );
        let (s, body) = call(
            &t,
            "POST",
            "/rest/security/apikeys",
            &[JSON_CT, admin],
            r#"{"user":"ci","name":"build"}"#,
        );
        assert_eq!(s, 201, "{body}");
        let made = Json::parse(&body).unwrap();
        let key = made.str_at(&["key"]).to_string();
        assert!(key.starts_with("grd_"));
        let upd = ("Content-Type", "application/sparql-update");
        assert_eq!(
            call(
                &t,
                "POST",
                "/repositories/k/statements",
                &[upd, ("X-API-Key", &key)],
                "INSERT DATA { <http://a> <http://b> 1 }"
            )
            .0,
            204
        );
        let auth = format!("ApiKey {key}");
        assert_eq!(
            call(
                &t,
                "GET",
                "/repositories/k/size",
                &[("Authorization", &auth)],
                ""
            )
            .0,
            200
        );
        assert_eq!(
            call(
                &t,
                "GET",
                "/rest/security/users",
                &[("X-API-Key", &key)],
                ""
            )
            .0,
            403,
            "a key has its user's rights only"
        );
        let forged = format!("{}z", &key[..key.len() - 1]);
        assert_eq!(
            call(
                &t,
                "GET",
                "/repositories/k/size",
                &[("X-API-Key", &forged)],
                ""
            )
            .0,
            401
        );
        let (_, list) = call(&t, "GET", "/rest/security/apikeys", &[admin], "");
        assert!(
            list.contains("build") && !list.contains(&key[16..]),
            "listed without the secret: {list}"
        );
        assert_eq!(
            call(
                &t,
                "POST",
                "/rest/login",
                &[JSON_CT],
                r#"{"username":"admin","password":"wrong"}"#
            )
            .0,
            401
        );
        assert_eq!(
            call(
                &t,
                "DELETE",
                &format!("/rest/security/apikeys/{}", made.str_at(&["id"])),
                &[admin],
                ""
            )
            .0,
            204
        );
        assert_eq!(
            call(
                &t,
                "GET",
                "/repositories/k/size",
                &[("X-API-Key", &key)],
                ""
            )
            .0,
            401,
            "a deleted key no longer works"
        );
        let lines: Vec<Json> = fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|l| Json::parse(l).unwrap())
            .collect();
        let find = |pred: &dyn Fn(&Json) -> bool| lines.iter().find(|l| pred(l)).cloned();
        let write = find(&|l| l.str_at(&["path"]) == "/repositories/k/statements")
            .expect("the write is logged");
        assert_eq!(
            (write.str_at(&["user"]), write.str_at(&["repository"])),
            ("ci", "k")
        );
        assert!(write.str_at(&["update"]).contains("INSERT DATA"));
        let login =
            find(&|l| l.str_at(&["path"]) == "/rest/login").expect("the failed sign-in is logged");
        assert_eq!(
            (
                login.str_at(&["user"]),
                login.get("status").and_then(|v| v.as_f64())
            ),
            ("admin", Some(401.0))
        );
        assert!(
            find(&|l| l.str_at(&["path"]) == "/rest/security/users"
                && l.get("status").and_then(|v| v.as_f64()) == Some(403.0))
            .is_some(),
            "refusals are logged"
        );
        assert!(
            find(&|l| l.str_at(&["path"]) == "/repositories/k/size"
                && l.get("status").and_then(|v| v.as_f64()) == Some(200.0))
            .is_none(),
            "reads are not, by default"
        );
        let _ = fs::remove_file(&log);
    }

    #[test]
    fn base64() {
        assert_eq!(base64_decode("YWRtaW46czNjcmV0").unwrap(), b"admin:s3cret");
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
    }
}
