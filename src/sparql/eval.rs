//! SPARQL algebra evaluation over the quad store.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

use super::ast::*;
use super::func::{self, Num};
use crate::fastmap::{FastMap, FastSet};
use crate::rdf::{Quad, Term};
use crate::store::{DEFAULT_GRAPH, INFERRED, Id, Store};

/// The full-text search predicate: `?literal g:fts "query"`.
pub const ONTO_FTS: &str = "http://graphrd.org/fn#fts";
/// RDF Rank scores: rank:score, rank:score3, rank:score5 ... (rounded to n digits)
pub const RDF_RANK: &str = "http://graphrd.org/rank#score";
/// `?v g:knn ?q` (10 nearest) and `?v g:knnN ?q` (N nearest): vector search (see `vector`).
pub const KNN: &str = "http://graphrd.org/fn#knn";
/// Hash join probes of at least this many solutions run on several threads.
const PARALLEL_PROBE: usize = 100_000;
/// Rows whose terms are read from the dictionary together (see `Dict::read_many`).
const PREFETCH: usize = 4_096;
/// Result cells past which a query's result terms are read in bulk rather than through the
/// dictionary's term cache (a quarter of its 200,000 terms: larger results would only evict it).
const BULK_RESULT_TERMS: usize = 50_000;
/// Index lookups after which a `p+` / `p*` walk may read all of `p`'s edges in one scan instead.
const PATH_SCAN: usize = 20_000;
/// Edges scanned in the time of one index lookup, for deciding when a walk scans instead.
const SCAN_PER_LOOKUP: usize = 256;

/// Ids at or above this are query-local terms not present in the store.
const LOCAL: Id = 1 << 31;

/// A solution: one value per variable slot, `None` for unbound.
pub type Sol = Vec<Option<Id>>;

/// Below this many solutions a BGP step does one index lookup per solution; above, it tries
/// a hash join.
const HASH_JOIN_MIN: usize = 2_000;

/// Sort by the high 32 bits (two stable passes of 16 bits); the order within a key is left as is.
fn radix_sort_high(v: &mut Vec<u64>) {
    let mut buf = vec![0u64; v.len()];
    for shift in [32, 48] {
        let mut at = vec![0usize; 65_537];
        for &x in v.iter() {
            at[((x >> shift) & 0xffff) as usize + 1] += 1;
        }
        for i in 1..at.len() {
            at[i] += at[i - 1];
        }
        for &x in v.iter() {
            let k = ((x >> shift) & 0xffff) as usize;
            buf[at[k]] = x;
            at[k] += 1;
        }
        std::mem::swap(v, &mut buf);
    }
}

/// A predicate's edges sorted by the node they leave (`(from << 32) | to`), with a directory:
/// `dir[k]` is the first edge whose `from >> shift` is at least `k`, so finding a node's edges
/// searches a short range instead of the whole list.
struct Edges {
    all: Vec<u64>,
    dir: Vec<u32>,
    shift: u32,
}

impl Edges {
    fn new(all: Vec<u64>) -> Edges {
        let max = all.last().map_or(0, |&x| x >> 32);
        // about four edges per directory entry
        let mut shift = 0;
        while (max >> shift) as usize > all.len() / 4 + 1 {
            shift += 1;
        }
        let mut dir = vec![0u32; (max >> shift) as usize + 2];
        let mut i = 0;
        for (k, d) in dir.iter_mut().enumerate() {
            while i < all.len() && ((all[i] >> 32) >> shift) < k as u64 {
                i += 1;
            }
            *d = i as u32;
        }
        Edges { all, dir, shift }
    }

    /// The nodes `n` has an edge to.
    fn from(&self, n: Id) -> impl Iterator<Item = Id> + '_ {
        let k = (n as u64 >> self.shift) as usize;
        let range = match (self.dir.get(k), self.dir.get(k + 1)) {
            (Some(&a), Some(&b)) => &self.all[a as usize..b as usize],
            _ => &[][..],
        };
        let at = range.partition_point(|&x| ((x >> 32) as Id) < n);
        range[at..]
            .iter()
            .take_while(move |&&x| (x >> 32) as Id == n)
            .map(|&x| x as Id)
    }
}

/// The nodes a path walk has reached, in order. A hash set while small; past a few thousand
/// nodes, a bit per dictionary id below `limit` (ids above it stay in the hash set).
struct Reached {
    list: Vec<Id>,
    small: FastSet<Id>,
    bits: Vec<u64>,
    limit: Id,
}

impl Reached {
    fn new(limit: Id) -> Reached {
        Reached {
            list: Vec::new(),
            small: FastSet::default(),
            bits: Vec::new(),
            limit,
        }
    }

    /// Whether `id` is new.
    fn insert(&mut self, id: Id) -> bool {
        if !self.bits.is_empty() && id < self.limit {
            let (w, b) = ((id / 64) as usize, id % 64);
            if self.bits[w] >> b & 1 == 1 {
                return false;
            }
            self.bits[w] |= 1 << b;
        } else {
            if !self.small.insert(id) {
                return false;
            }
            if self.bits.is_empty() && self.small.len() > 4_096 {
                self.bits = vec![0; self.limit as usize / 64 + 1];
                let limit = self.limit;
                for &x in self.small.iter().filter(|&&x| x < limit) {
                    self.bits[(x / 64) as usize] |= 1 << (x % 64);
                }
                self.small.retain(|&x| x >= limit);
            }
        }
        self.list.push(id);
        true
    }
}

/// `rows` in chunks of `PREFETCH`, `ahead` called on each chunk before its rows are handed on.
fn prefetched<'a, T: 'a>(
    mut rows: impl Iterator<Item = T> + 'a,
    mut ahead: impl FnMut(&[T]) + 'a,
) -> impl Iterator<Item = T> + 'a {
    let mut chunk: std::collections::VecDeque<T> = std::collections::VecDeque::new();
    std::iter::from_fn(move || {
        if chunk.is_empty() {
            chunk.extend(rows.by_ref().take(PREFETCH));
            ahead(chunk.make_contiguous());
        }
        chunk.pop_front()
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Active {
    /// The default graph: union of all graphs, or the FROM graphs.
    Default,
    Named(Id),
}

/// What a query returned.
pub enum QueryResult {
    /// SELECT: the visible variables' names and a row of terms per solution.
    Solutions {
        /// The visible variables' names.
        vars: Vec<String>,
        /// One row of terms per solution.
        rows: Vec<Vec<Option<Term>>>,
    },
    /// ASK.
    Boolean(bool),
    /// CONSTRUCT/DESCRIBE: the constructed statements.
    Graph(Vec<Quad>),
}

/// The store, variables and settings one query runs with.
pub struct Ctx<'a> {
    /// The store to read.
    pub store: &'a Store,
    nvars: usize,
    vars: &'a VarTable,
    local: RefCell<Vec<Term>>,
    local_map: RefCell<HashMap<Term, Id>>,
    /// Terms of the rows a loop is about to read, read from the dictionary together (`prefetch`).
    bulk: RefCell<FastMap<Id, Term>>,
    /// FROM graphs; None = union of everything.
    default_graphs: Option<Vec<Id>>,
    /// FROM NAMED graphs; None = all named graphs.
    named_graphs: Option<Vec<Id>>,
    /// The query's base IRI.
    pub base: String,
    deadline: Option<std::time::Instant>,
    /// Include inferred statements in default-graph matches (`infer=true`).
    include_inferred: bool,
    /// BNODE(str): the same string gives the same blank node within one solution.
    bnodes: RefCell<HashMap<(Sol, String), Term>>,
    /// The solution whose SELECT expressions are being evaluated.
    row: RefCell<Option<Sol>>,
    /// Pre-bound variables (SHACL's $this and friends).
    seed: Option<Sol>,
    /// Full-text queries prepared for this query (their document frequencies are read once).
    fts: RefCell<HashMap<String, std::rc::Rc<crate::store::FtsPrepared>>>,
    /// Results of SERVICE calls, by (endpoint, query), so each is fetched once per query.
    services: RefCell<HashMap<(String, String), std::rc::Rc<Vec<Sol>>>>,
    /// Answers of SERVICE blocks fetched in parallel when the query started (see
    /// `prefetch_services`), not yet turned into solutions.
    prefetched: RefCell<HashMap<(String, String), Result<std::sync::Arc<ServiceRows>, String>>>,
    /// Dictionary lookups of the query's constants, done once per query rather than once per
    /// solution (the shared dictionary is the hottest contention point under many readers).
    ids: RefCell<HashMap<Term, Option<Id>>>,
    iri_ids: RefCell<HashMap<String, Option<Id>>>,
    /// Plain triple-pattern scans stop once the output holds this many solutions (set only
    /// while a capped evaluation needs no more; see `eval_capped`).
    match_cap: std::cell::Cell<usize>,
    /// Query-time inference: the schema, and the default graphs it applies to.
    at_query: Option<(super::reasoning::AtQuery, Option<Vec<Id>>)>,
}

type R<T> = Result<T, String>;

impl<'a> Ctx<'a> {
    /// A fresh evaluation context with a time limit.
    pub fn new(
        store: &'a Store,
        vars: &'a VarTable,
        base: &str,
        timeout_secs: Option<u64>,
    ) -> Self {
        Ctx {
            store,
            nvars: vars.len(),
            vars,
            local: RefCell::new(Vec::new()),
            local_map: RefCell::new(HashMap::new()),
            bulk: RefCell::new(FastMap::default()),
            default_graphs: None,
            named_graphs: None,
            base: base.to_string(),
            deadline: timeout_secs
                .filter(|&s| s > 0)
                .map(|s| std::time::Instant::now() + std::time::Duration::from_secs(s)),
            include_inferred: true,
            services: RefCell::new(HashMap::new()),
            prefetched: RefCell::new(HashMap::new()),
            ids: RefCell::new(HashMap::new()),
            iri_ids: RefCell::new(HashMap::new()),
            match_cap: std::cell::Cell::new(usize::MAX),
            at_query: None,
            bnodes: RefCell::new(HashMap::new()),
            row: RefCell::new(None),
            seed: None,
            fts: RefCell::new(HashMap::new()),
        }
        .with_default_graph()
    }

    fn with_default_graph(mut self) -> Self {
        if !self.store.union_default_graph {
            // the default graph proper, with what was inferred from it
            self.default_graphs = Some(vec![DEFAULT_GRAPH, INFERRED]);
        }
        self
    }

    /// Answer patterns of the default graph with query-time inference (`reasoning`).
    pub fn set_at_query(&mut self) {
        self.at_query = super::reasoning::AtQuery::build(self.store)
            .map(|q| (q, self.graphs_for(Active::Default)));
    }

    /// A triple pattern under query-time inference; None when inference adds nothing to it.
    fn match_reasoned(
        &self,
        tp: &TriplePattern,
        s: &Sol,
        graphs: &Option<Vec<Id>>,
        out: &mut Vec<Sol>,
    ) -> Option<R<()>> {
        let (aq, scope) = self.at_query.as_ref()?;
        if graphs != scope {
            return None; // inside GRAPH: as stated
        }
        let Predicate::Path(Path::Iri(i)) = &tp.p else {
            return None;
        };
        let pid = self.lookup_iri(i)?;
        let (Ok(sid), Ok(oid)) = (self.resolve(&tp.s, s), self.resolve(&tp.o, s)) else {
            return Some(Ok(()));
        };
        let mut emit = |a: Id, b: Id| {
            let mut n = s.clone();
            if self.bind(&mut n, &tp.s, a) && self.bind(&mut n, &tp.o, b) {
                out.push(n);
            }
        };
        if pid == aq.ty {
            let mut pairs: FastSet<(Id, Id)> = FastSet::default();
            match oid {
                Some(c) => {
                    for sub in aq.subclasses(c) {
                        pairs.extend(
                            self.scan(sid, Some(aq.ty), Some(sub), None)
                                .map(|q| (q[0], c)),
                        );
                    }
                    for p in aq.typing_properties(c, false) {
                        pairs.extend(self.scan(sid, Some(p), None, None).map(|q| (q[0], c)));
                    }
                    for p in aq.typing_properties(c, true) {
                        pairs.extend(
                            self.scan(None, Some(p), sid, None)
                                .filter(|q| !self.term(q[2]).is_literal())
                                .map(|q| (q[2], c)),
                        );
                    }
                }
                None => {
                    for q in self.scan(sid, Some(aq.ty), None, None).collect::<Vec<_>>() {
                        pairs.extend(aq.superclasses(q[2]).into_iter().map(|t| (q[0], t)));
                    }
                    if aq.has_typing() {
                        // subjects take the domains of their properties, objects (not literals)
                        // the ranges
                        let literal = |id: Id| self.term(id).is_literal();
                        let (as_subject, as_object) = match sid {
                            Some(x) => (
                                self.scan(Some(x), None, None, None).collect::<Vec<_>>(),
                                self.scan(None, None, Some(x), None).collect::<Vec<_>>(),
                            ),
                            None => {
                                let all: Vec<[Id; 4]> = self.scan(None, None, None, None).collect();
                                (all.clone(), all)
                            }
                        };
                        for q in as_subject {
                            pairs.extend(aq.typed_by(q[1], false).into_iter().map(|t| (q[0], t)));
                        }
                        for q in as_object {
                            let types = aq.typed_by(q[1], true);
                            if !types.is_empty() && !literal(q[2]) {
                                pairs.extend(types.into_iter().map(|t| (q[2], t)));
                            }
                        }
                    }
                }
            }
            for (a, b) in pairs {
                emit(a, b);
            }
            return Some(Ok(()));
        }
        let iri = |id: Id| self.term(id).value().to_string();
        let path = aq.path_for(pid, &iri)?;
        Some(self.path(&path, sid, oid, graphs).map(|pairs| {
            for (a, b) in pairs {
                emit(a, b);
            }
        }))
    }

    /// Let inferred statements take part in matches, or not.
    pub fn set_infer(&mut self, infer: bool) {
        self.include_inferred = infer;
        if !infer {
            if let Some(gs) = &mut self.default_graphs {
                gs.retain(|&g| g != INFERRED);
            }
        }
    }

    /// Store scan honouring the inference setting. Without a graph the scan reads the default
    /// graph, the RDF merge of all graphs: a triple stored in several graphs appears once. Every
    /// index used for such a scan keeps the graph last, so copies of a triple are adjacent.
    fn scan(
        &self,
        s: Option<Id>,
        p: Option<Id>,
        o: Option<Id>,
        g: Option<Id>,
    ) -> Box<dyn Iterator<Item = [Id; 4]> + '_> {
        if g.is_some() {
            return self.store.quads_matching(s, p, o, g);
        }
        let it = if self.include_inferred {
            self.store.quads_matching(s, p, o, g)
        } else {
            self.store.explicit_matching(s, p, o, g)
        };
        let mut last: Option<[Id; 3]> = None;
        Box::new(it.filter(move |q| {
            let t = [q[0], q[1], q[2]];
            last.replace(t) != Some(t)
        }))
    }

    /// Scan over a list of default graphs (FROM): their merge, so each triple once.
    fn scan_graphs(
        &self,
        s: Option<Id>,
        p: Option<Id>,
        o: Option<Id>,
        graphs: &Option<Vec<Id>>,
    ) -> Vec<[Id; 4]> {
        match graphs {
            None => self.scan(s, p, o, None).collect(),
            Some(gs) if gs.len() == 1 => self.scan(s, p, o, Some(gs[0])).collect(),
            Some(gs) => {
                let mut seen = FastSet::default();
                gs.iter()
                    .flat_map(|&g| self.scan(s, p, o, Some(g)).collect::<Vec<_>>())
                    .filter(|q| seen.insert([q[0], q[1], q[2]]))
                    .collect()
            }
        }
    }

    /// Restrict the query to a dataset (FROM/FROM NAMED or the protocol's parameters).
    pub fn set_dataset(&mut self, ds: Option<&Dataset>) {
        let Some(ds) = ds else { return };
        let ids = |list: &[String]| -> Vec<Id> {
            list.iter()
                .filter_map(|i| self.store.dict.lookup(&Term::Iri(i.clone())))
                .collect()
        };
        // pseudo-graphs: graph:explicit / graph:implicit
        if ds.default.len() == 1 && ds.default[0] == "http://graphrd.org/graph#explicit" {
            self.include_inferred = false;
            return;
        }
        if ds.default.len() == 1 && ds.default[0] == "http://graphrd.org/graph#implicit" {
            self.default_graphs = Some(vec![INFERRED]);
            return;
        }
        if ds.default.is_empty() && !ds.named.is_empty() {
            // FROM NAMED without FROM: the default graph is empty
            self.default_graphs = Some(Vec::new());
        }
        if !ds.default.is_empty() {
            self.default_graphs = Some(
                ds.default
                    .iter()
                    .flat_map(|g| {
                        // these IRIs name the real default graph: ours, and the compatibility spellings
                        if g == "urn:graphrd:default"
                            || g == "http://rdf4j.org/schema/rdf4j#nil"
                            || g == "http://www.openrdf.org/schema/sesame#nil"
                        {
                            vec![DEFAULT_GRAPH]
                        } else {
                            ids(std::slice::from_ref(g))
                        }
                    })
                    .collect(),
            );
        }
        if !ds.named.is_empty() || !ds.default.is_empty() {
            self.named_graphs = Some(ids(&ds.named));
        }
    }

    /// Run `text` at a remote SPARQL endpoint (SPARQL 1.1 Protocol) and return its solutions
    /// in this query's variable numbering.
    fn service(&self, endpoint: &str, text: &str) -> R<std::rc::Rc<Vec<Sol>>> {
        let key = (endpoint.to_string(), text.to_string());
        if let Some(r) = self.services.borrow().get(&key) {
            return Ok(r.clone());
        }
        let fetched = self.prefetched.borrow_mut().remove(&key);
        let rows = match fetched {
            Some(r) => r?,
            None => fetch_service(endpoint, text, self.service_timeout())?,
        };
        let (vars, rows) = &*rows;
        let slots: Vec<Option<Var>> = vars
            .iter()
            .map(|v| self.vars.names.iter().position(|n| n == v))
            .collect();
        let sols: Vec<Sol> = rows
            .iter()
            .map(|row| {
                let mut sol = vec![None; self.nvars];
                for (slot, t) in slots.iter().zip(row) {
                    if let (Some(slot), Some(t)) = (slot, t) {
                        sol[*slot] = Some(self.intern(t));
                    }
                }
                sol
            })
            .collect();
        let sols = std::rc::Rc::new(sols);
        self.services.borrow_mut().insert(key, sols.clone());
        Ok(sols)
    }

    fn service_timeout(&self) -> std::time::Duration {
        use std::sync::atomic::Ordering::Relaxed;
        let own = match SERVICE_TIMEOUT_SECS.load(Relaxed) {
            0 => std::time::Duration::from_secs(300),
            s => std::time::Duration::from_secs(s),
        };
        match self.deadline {
            Some(d) => d
                .saturating_duration_since(std::time::Instant::now())
                .min(own),
            None => own,
        }
    }

    /// Fetch every SERVICE block with a fixed endpoint at once, each on its own thread, before
    /// evaluation needs them: a federated query then waits for its slowest endpoint, not for
    /// the sum of them. (Blocks with a variable endpoint are fetched when reached.)
    fn prefetch_services(&self, p: &Pattern) {
        fn collect(p: &Pattern, out: &mut Vec<(String, String)>) {
            match p {
                Pattern::Service(VarOrTerm::Term(Term::Iri(e)), _, _, text) => {
                    let k = (e.clone(), text.clone());
                    if !out.contains(&k) {
                        out.push(k);
                    }
                }
                Pattern::Join(a, b)
                | Pattern::LeftJoin(a, b, _)
                | Pattern::Union(a, b)
                | Pattern::Minus(a, b) => {
                    collect(a, out);
                    collect(b, out);
                }
                Pattern::Filter(_, inner)
                | Pattern::Extend(inner, ..)
                | Pattern::Graph(_, inner) => collect(inner, out),
                Pattern::SubSelect(q) => collect(&q.pattern, out),
                _ => {}
            }
        }
        let mut wanted = Vec::new();
        collect(p, &mut wanted);
        if wanted.len() < 2 {
            return;
        }
        let timeout = self.service_timeout();
        let fetched: Vec<_> = std::thread::scope(|sc| {
            let handles: Vec<_> = wanted
                .iter()
                .map(|(e, t)| sc.spawn(move || fetch_service(e, t, timeout)))
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err("SERVICE call failed".into()))
                })
                .collect()
        });
        self.prefetched
            .borrow_mut()
            .extend(wanted.into_iter().zip(fetched));
    }

    fn check_time(&self) -> R<()> {
        match self.deadline {
            Some(d) if std::time::Instant::now() > d => Err("Query evaluation timed out".into()),
            _ => Ok(()),
        }
    }

    /// Read the dictionary terms of `ids` in bulk ahead of a loop that decodes them one by one
    /// (they replace the previous chunk's).
    fn prefetch(&self, ids: impl Iterator<Item = Id>) {
        let mut ids: Vec<Id> = ids.filter(|&id| id < LOCAL).collect();
        let mut bulk = self.bulk.borrow_mut();
        bulk.clear();
        self.store.dict.read_many(&mut ids, &mut bulk);
    }

    /// The term an id stands for (query-local terms included).
    pub fn term(&self, id: Id) -> Term {
        if id >= LOCAL {
            self.local.borrow()[(id - LOCAL) as usize].clone()
        } else if let Some(t) = self.bulk.borrow().get(&id) {
            t.clone()
        } else {
            self.store.dict.term(id).clone()
        }
    }

    /// The id of a term, interning it locally when the store does not know it.
    pub fn intern(&self, t: &Term) -> Id {
        if let Some(id) = self.lookup(t) {
            return id;
        }
        if let Some(&id) = self.local_map.borrow().get(t) {
            return id;
        }
        let mut local = self.local.borrow_mut();
        let id = LOCAL + local.len() as Id;
        local.push(t.clone());
        self.local_map.borrow_mut().insert(t.clone(), id);
        id
    }

    /// Pre-bind variables: the query's patterns, filters, EXISTS and subqueries see them as
    /// constants.
    pub fn set_seed(&mut self, bindings: &[(Var, Term)]) {
        let mut sol = self.empty_sol();
        for (v, t) in bindings {
            sol[*v] = Some(self.intern(t));
        }
        self.seed = Some(sol);
    }

    fn empty_sol(&self) -> Sol {
        vec![None; self.nvars]
    }

    fn named(&self) -> Vec<Id> {
        match &self.named_graphs {
            Some(g) => g.clone(),
            None => self.store.graphs(),
        }
    }

    // ------------------------------------------------------------ patterns

    fn eval(&self, p: &Pattern, g: Active, seed: &Sol) -> R<Vec<Sol>> {
        self.check_time()?;
        Ok(match p {
            Pattern::Bgp(tps) => self.bgp(tps, vec![seed.clone()], g)?,
            Pattern::Join(a, b) => {
                let left = self.eval(a, g, seed)?;
                if let Pattern::Bgp(tps) = &**b {
                    self.bgp(tps, left, g)?
                } else if let Pattern::Service(..) = &**b {
                    // bind join: earlier patterns can bind the endpoint variable; the
                    // remote results are cached, so a fixed endpoint is still called once
                    let mut out = Vec::new();
                    for l in left {
                        out.extend(self.eval(b, g, &l)?);
                    }
                    out
                } else {
                    let right = self.eval(b, g, seed)?;
                    join(left, right)
                }
            }
            Pattern::LeftJoin(a, b, cond) => {
                let left = self.eval(a, g, seed)?;
                let mut out = Vec::new();
                if let Pattern::Bgp(tps) = &**b {
                    for l in left {
                        let ext = self.bgp(tps, vec![l.clone()], g)?;
                        let mut any = false;
                        for e in ext {
                            if cond.as_ref().is_none_or(|c| self.filter(c, &e, g)) {
                                out.push(e);
                                any = true;
                            }
                        }
                        if !any {
                            out.push(l);
                        }
                    }
                } else {
                    let right = self.eval(b, g, seed)?;
                    let mut index = Compatible::new(&right);
                    for l in left {
                        let mut any = false;
                        index.each(&l, false, |r| {
                            let m = merge(&l, r).expect("compatible solutions merge");
                            if cond.as_ref().is_none_or(|c| self.filter(c, &m, g)) {
                                out.push(m);
                                any = true;
                            }
                            true
                        });
                        if !any {
                            out.push(l);
                        }
                    }
                }
                out
            }
            Pattern::Union(a, b) => {
                let mut l = self.eval(a, g, seed)?;
                l.extend(self.eval(b, g, seed)?);
                l
            }
            Pattern::Minus(a, b) => {
                let left = self.eval(a, g, seed)?;
                let right = self.eval(b, g, &self.empty_sol())?;
                // a row goes when a compatible right row shares a variable with it
                let mut index = Compatible::new(&right);
                left.into_iter()
                    .filter(|l| {
                        let mut found = false;
                        index.each(l, true, |_| {
                            found = true;
                            false
                        });
                        !found
                    })
                    .collect()
            }
            Pattern::Filter(e, inner) => {
                let spatial = match &**inner {
                    Pattern::Bgp(tps) => self.spatial_bgp(e, tps, seed, g)?,
                    _ => None,
                };
                let mut sols = match spatial {
                    Some(sols) => sols,
                    None => self.eval(inner, g, seed)?,
                };
                match single_var(e) {
                    // a filter that reads one variable is decided once per distinct value
                    // (given up when the first rows show the values hardly repeat)
                    Some(v) if sols.len() > 32 => {
                        let mut memo: FastMap<Option<Id>, bool> = FastMap::default();
                        let mut seen = 0usize;
                        let mut use_memo = true;
                        let ids: Vec<Id> = sols.iter().map(|s| s[v].unwrap_or(0)).collect();
                        sols.retain(|s| {
                            if !use_memo && seen % PREFETCH == 0 {
                                // the terms of the next rows, read from the dictionary in bulk
                                // (with the memo on, values repeat and are mostly cached)
                                self.prefetch(
                                    ids[seen..ids.len().min(seen + PREFETCH)].iter().copied(),
                                );
                            }
                            seen += 1;
                            if seen == 512 && memo.len() * 4 > seen * 3 {
                                use_memo = false;
                            }
                            if use_memo {
                                *memo.entry(s[v]).or_insert_with(|| self.filter(e, s, g))
                            } else {
                                self.filter(e, s, g)
                            }
                        });
                        sols
                    }
                    _ => sols.into_iter().filter(|s| self.filter(e, s, g)).collect(),
                }
            }
            Pattern::Extend(inner, v, e) => {
                let sols = self.eval(inner, g, seed)?;
                let mut out = Vec::with_capacity(sols.len());
                for mut s in sols {
                    if s[*v].is_none() {
                        if let Some(t) = self.expr(e, &s, g) {
                            s[*v] = Some(self.intern(&t));
                        }
                    }
                    out.push(s);
                }
                out
            }
            Pattern::Values(vars, rows) => {
                let mut out = Vec::new();
                for row in rows {
                    let mut s = seed.clone();
                    let mut ok = true;
                    for (v, t) in vars.iter().zip(row) {
                        if let Some(t) = t {
                            let id = self.intern(t);
                            match s[*v] {
                                Some(x) if x != id => ok = false,
                                _ => s[*v] = Some(id),
                            }
                        }
                    }
                    if ok {
                        out.push(s);
                    }
                }
                out
            }
            Pattern::Graph(name, inner) => match name {
                VarOrTerm::Triple(_) => Vec::new(), // a quoted triple never names a graph
                VarOrTerm::Term(t) => {
                    let Some(id) = self.lookup(t) else {
                        return Ok(Vec::new());
                    };
                    if !self.named().contains(&id) {
                        return Ok(Vec::new());
                    }
                    self.eval(inner, Active::Named(id), seed)?
                }
                VarOrTerm::Var(v) => {
                    let graphs = match seed[*v] {
                        Some(id) => vec![id],
                        None => self.named(),
                    };
                    let mut out = Vec::new();
                    for gid in graphs {
                        // the graph variable is not in scope inside the pattern
                        let mut s = seed.clone();
                        s[*v] = None;
                        for mut r in self.eval(inner, Active::Named(gid), &s)? {
                            match r[*v] {
                                Some(x) if x != gid => continue,
                                _ => r[*v] = Some(gid),
                            }
                            out.push(r);
                        }
                    }
                    out
                }
            },
            Pattern::SubSelect(q) => {
                let (_, sols) = self.select_in(q, g, self.seed.as_ref())?;
                sols.into_iter().filter_map(|s| merge(seed, &s)).collect()
            }
            Pattern::Service(name, _, silent, text) => {
                let endpoint = match name {
                    VarOrTerm::Term(Term::Iri(i)) => Ok(i.clone()),
                    VarOrTerm::Var(v) => match seed[*v].map(|id| self.term(id)) {
                        Some(Term::Iri(i)) => Ok(i),
                        _ => Err("SERVICE endpoint variable is not bound to an IRI".to_string()),
                    },
                    _ => Err("SERVICE endpoint must be an IRI".to_string()),
                };
                match endpoint.and_then(|ep| self.service(&ep, text)) {
                    Ok(rows) => rows.iter().filter_map(|r| merge(seed, r)).collect(),
                    // SILENT: a failing endpoint contributes one empty solution
                    Err(_) if *silent => vec![seed.clone()],
                    Err(e) => return Err(e),
                }
            }
        })
    }

    /// A BGP under a FILTER that the spatial index can narrow: the filter has a conjunct
    /// relating a geometry variable the BGP binds (as an object) to a constant geometry, or to
    /// another variable the rest of the BGP binds (see `geo::filter_box`). The variable is then
    /// bound up front to the index's candidates instead of every geometry being tried. The
    /// rows are a superset of those passing the filter; the caller still applies it. None when
    /// nothing applies.
    fn spatial_bgp(
        &self,
        e: &Expr,
        tps: &[TriplePattern],
        seed: &Sol,
        g: Active,
    ) -> R<Option<Vec<Sol>>> {
        let mut calls = Vec::new();
        spatial_calls(e, &mut calls);
        // too large a box finds nearly everything: then the plain plan is as good
        let narrow = |b: crate::geo::Bbox| b.2 - b.0 <= 180.0 && b.3 - b.1 <= 90.0;
        for (name, args, radius) in calls {
            for (vi, oi) in [(0, 1), (1, 0)] {
                let Expr::Var(v) = args[vi] else { continue };
                if seed[v].is_some() || !tps.iter().any(|tp| tp.o == VarOrTerm::Var(v)) {
                    continue;
                }
                let terms = |other: Option<Term>| -> Vec<Option<Term>> {
                    args.iter()
                        .enumerate()
                        .map(|(i, a)| match a {
                            _ if i == oi => other.clone(),
                            Expr::Const(t) => Some(t.clone()),
                            _ => None,
                        })
                        .collect()
                };
                let bbox = |other: Term| {
                    crate::geo::filter_box(name, &terms(Some(other)), &|i| i == vi, radius)
                        .filter(|b| narrow(*b))
                };
                let seeded = |base: &Sol, b: crate::geo::Bbox| -> Vec<Sol> {
                    self.store
                        .geo_candidates(b)
                        .into_iter()
                        .map(|id| {
                            let mut s = base.clone();
                            s[v] = Some(id);
                            s
                        })
                        .collect()
                };
                match &args[oi] {
                    Expr::Const(t) => {
                        let Some(b) = bbox(t.clone()) else { continue };
                        return Ok(Some(self.bgp(tps, seeded(seed, b), g)?));
                    }
                    &Expr::Var(w) if w != v && crate::geo::indexable(name, radius) => {
                        // the patterns not joined to `v` (other than through `w`) run first and
                        // bind `w`; then `v` gets the candidates around each of `w`'s geometries
                        let mut joined = vec![v];
                        let mut rest_idx: Vec<usize> = Vec::new();
                        loop {
                            let before = rest_idx.len();
                            for (i, tp) in tps.iter().enumerate() {
                                if !rest_idx.contains(&i) && joined.iter().any(|&x| mentions(tp, x))
                                {
                                    rest_idx.push(i);
                                    let mut vars = Vec::new();
                                    tp.s.vars(&mut vars);
                                    tp.o.vars(&mut vars);
                                    if let Predicate::Var(p) = tp.p {
                                        vars.push(p);
                                    }
                                    for x in vars {
                                        if x != w && !joined.contains(&x) {
                                            joined.push(x);
                                        }
                                    }
                                }
                            }
                            if rest_idx.len() == before {
                                break;
                            }
                        }
                        let (first, rest): (Vec<TriplePattern>, Vec<TriplePattern>) = (
                            tps.iter()
                                .enumerate()
                                .filter(|(i, _)| !rest_idx.contains(i))
                                .map(|(_, tp)| tp.clone())
                                .collect(),
                            rest_idx.iter().map(|&i| tps[i].clone()).collect(),
                        );
                        if first.is_empty()
                            || !(seed[w].is_some() || first.iter().any(|tp| mentions(tp, w)))
                        {
                            continue;
                        }
                        let mut boxes: FastMap<Id, Option<crate::geo::Bbox>> = FastMap::default();
                        let (mut seeds, mut plain) = (Vec::new(), Vec::new());
                        for l in self.bgp(&first, vec![seed.clone()], g)? {
                            // an unbound `w` makes the filter an error: the row cannot pass
                            let Some(wid) = l[w] else { continue };
                            match *boxes.entry(wid).or_insert_with(|| bbox(self.term(wid))) {
                                Some(b) => seeds.extend(seeded(&l, b)),
                                None => plain.push(l),
                            }
                        }
                        let mut out = self.bgp(&rest, seeds, g)?;
                        out.extend(self.bgp(&rest, plain, g)?);
                        return Ok(Some(out));
                    }
                    _ => {}
                }
            }
        }
        Ok(None)
    }

    /// Like `eval`, but may stop once `cap` solutions are found: for a query that keeps only
    /// its first rows (LIMIT without ORDER BY, grouping or DISTINCT), any `cap` solutions are
    /// a correct answer, so the rest are never computed.
    fn eval_capped(&self, p: &Pattern, g: Active, seed: &Sol, cap: usize) -> R<Vec<Sol>> {
        if cap == usize::MAX {
            return self.eval(p, g, seed);
        }
        self.check_time()?;
        Ok(match p {
            Pattern::Bgp(tps) => self.bgp_capped(tps, vec![seed.clone()], g, cap)?,
            Pattern::Join(a, b) if matches!(&**b, Pattern::Bgp(_)) => {
                let Pattern::Bgp(tps) = &**b else {
                    unreachable!()
                };
                let left = self.eval(a, g, seed)?;
                self.bgp_capped(tps, left, g, cap)?
            }
            Pattern::LeftJoin(a, b, cond) => {
                // every left solution yields at least one result
                let left = self.eval_capped(a, g, seed, cap)?;
                let mut out = Vec::new();
                for l in left {
                    if out.len() >= cap {
                        break;
                    }
                    let ext = match &**b {
                        Pattern::Bgp(tps) => self.bgp(tps, vec![l.clone()], g)?,
                        other => self.eval(other, g, &l)?,
                    };
                    let before = out.len();
                    for e in ext {
                        if cond.as_ref().is_none_or(|c| self.filter(c, &e, g)) {
                            out.push(e);
                        }
                    }
                    if out.len() == before {
                        out.push(l);
                    }
                }
                out
            }
            Pattern::Union(a, b) => {
                let mut l = self.eval_capped(a, g, seed, cap)?;
                if l.len() < cap {
                    l.extend(self.eval_capped(b, g, seed, cap - l.len())?);
                }
                l
            }
            Pattern::Extend(inner, ..) => {
                let sols = self.eval_capped(inner, g, seed, cap)?;
                return self.extend_all(p, sols, g);
            }
            Pattern::Graph(VarOrTerm::Term(t), inner) => {
                let Some(id) = self.lookup(t) else {
                    return Ok(Vec::new());
                };
                if !self.named().contains(&id) {
                    return Ok(Vec::new());
                }
                self.eval_capped(inner, Active::Named(id), seed, cap)?
            }
            _ => self.eval(p, g, seed)?,
        })
    }

    fn extend_all(&self, p: &Pattern, sols: Vec<Sol>, g: Active) -> R<Vec<Sol>> {
        let Pattern::Extend(_, v, e) = p else {
            return Ok(sols);
        };
        let mut out = Vec::with_capacity(sols.len());
        for mut s in sols {
            if s[*v].is_none() {
                if let Some(t) = self.expr(e, &s, g) {
                    s[*v] = Some(self.intern(&t));
                }
            }
            out.push(s);
        }
        Ok(out)
    }

    /// A basic graph pattern that may stop after `cap` solutions: the input is joined in
    /// growing chunks, and the last pattern's scan stops at the cap.
    fn bgp_capped(
        &self,
        tps: &[TriplePattern],
        input: Vec<Sol>,
        g: Active,
        cap: usize,
    ) -> R<Vec<Sol>> {
        if tps.is_empty() || input.is_empty() {
            return Ok(input);
        }
        let bound: FastSet<Var> = input[0]
            .iter()
            .enumerate()
            .filter(|(_, v)| v.is_some())
            .map(|(i, _)| i)
            .collect();
        let order = self.order_bgp(tps, &bound);
        let graphs = self.graphs_for(g);
        // all but the last pattern run on chunks of the solutions so far
        let (last, init) = order.split_last().expect("not empty");
        let mut pending = input;
        let mut rest = init;
        if !init.is_empty() && pending.len() == 1 {
            // a single seed (the usual start) is first expanded by one pattern, so that
            // there are solutions to split into chunks
            let mut first = Vec::new();
            self.match_triple(init[0], &pending[0], &graphs, &mut first)?;
            pending = first;
            rest = &init[1..];
        }
        let mut out = Vec::new();
        let mut chunk = 64usize;
        let mut it = pending.into_iter().peekable();
        while it.peek().is_some() && out.len() < cap {
            self.check_time()?;
            let mut sols: Vec<Sol> = it.by_ref().take(chunk).collect();
            for tp in rest {
                // large chunks (a pattern that rarely matches) keep the hash join
                let joined = if sols.len() >= HASH_JOIN_MIN {
                    self.hash_join(tp, &sols, &graphs)?
                } else {
                    None
                };
                sols = match joined {
                    Some(j) => j,
                    None => {
                        let mut next = Vec::new();
                        for s in &sols {
                            self.match_triple(tp, s, &graphs, &mut next)?;
                        }
                        next
                    }
                };
                if sols.is_empty() {
                    break;
                }
            }
            if sols.len() >= HASH_JOIN_MIN {
                if let Some(j) = self.hash_join(last, &sols, &graphs)? {
                    out.extend(j);
                    chunk = (chunk * 2).min(1 << 14);
                    continue;
                }
            }
            self.match_cap.set(cap);
            let r = sols.iter().try_for_each(|s| {
                if out.len() >= cap {
                    Ok(())
                } else {
                    self.match_triple(last, s, &graphs, &mut out)
                }
            });
            self.match_cap.set(usize::MAX);
            r?;
            chunk = (chunk * 2).min(1 << 14);
        }
        out.truncate(cap);
        Ok(out)
    }

    fn filter(&self, e: &Expr, s: &Sol, g: Active) -> bool {
        if let Some(r) = self.string_test(e, s) {
            return r.unwrap_or(false);
        }
        self.expr(e, s, g)
            .as_ref()
            .and_then(func::ebv)
            .unwrap_or(false)
    }

    /// `CONTAINS`, `STRSTARTS`, `STRENDS` or `REGEX` of a variable and constants, tested on
    /// the variable's term where it lies (read in bulk, or query-local) rather than on a copy:
    /// a filter over many strings would otherwise copy each. None when `e` has another shape;
    /// Some(None) for an error (an unbound variable, a term that is not a string).
    fn string_test(&self, e: &Expr, s: &Sol) -> Option<Option<bool>> {
        let Expr::Call(name, args) = e else {
            return None;
        };
        if !matches!(
            name.as_str(),
            "CONTAINS" | "STRSTARTS" | "STRENDS" | "REGEX"
        ) {
            return None;
        }
        let (Some(Expr::Var(v)), rest) = (args.first(), args.get(1..).unwrap_or(&[])) else {
            return None;
        };
        let constant = |i: usize| match rest.get(i) {
            Some(Expr::Const(t)) => Some(Some(t)),
            None => Some(None),
            Some(_) => None,
        };
        let consts = [constant(0)?, constant(1)?];
        if rest.len() > 2 {
            return None;
        }
        let Some(id) = s[*v] else {
            return Some(None);
        };
        Some(self.with_term(id, |t| func::string_test(name, t, consts)))
    }

    /// Call `f` with the term an id stands for, borrowed where the query holds it (its own
    /// terms, or those read in bulk) and from the dictionary otherwise.
    fn with_term<T>(&self, id: Id, f: impl FnOnce(&Term) -> T) -> T {
        if id >= LOCAL {
            return f(&self.local.borrow()[(id - LOCAL) as usize]);
        }
        if let Some(t) = self.bulk.borrow().get(&id) {
            return f(t);
        }
        f(&self.store.dict.term(id))
    }

    /// The store id of a constant, memoized for this query.
    fn lookup(&self, t: &Term) -> Option<Id> {
        if let Some(id) = self.ids.borrow().get(t) {
            return *id;
        }
        let id = self.store.dict.lookup(t);
        self.ids.borrow_mut().insert(t.clone(), id);
        id
    }

    /// `lookup` for an IRI given as a string (paths and predicates).
    fn lookup_iri(&self, i: &str) -> Option<Id> {
        if let Some(id) = self.iri_ids.borrow().get(i) {
            return *id;
        }
        let id = self.store.dict.lookup(&Term::Iri(i.to_string()));
        self.iri_ids.borrow_mut().insert(i.to_string(), id);
        id
    }

    fn resolve(&self, vt: &VarOrTerm, s: &Sol) -> Result<Option<Id>, ()> {
        match vt {
            VarOrTerm::Var(v) => Ok(s[*v]),
            VarOrTerm::Term(t) => match self.lookup(t) {
                Some(id) => Ok(Some(id)),
                None => Err(()), // constant not in the store: no matches
            },
            VarOrTerm::Triple(_) => match self.ground(vt, s) {
                Some(t) => self.lookup(&t).map(Some).ok_or(()),
                None => Ok(None),
            },
        }
    }

    /// The term a pattern position denotes under `s`, if fully bound.
    fn ground(&self, vt: &VarOrTerm, s: &Sol) -> Option<Term> {
        match vt {
            VarOrTerm::Var(v) => s[*v].map(|id| self.term(id)),
            VarOrTerm::Term(t) => Some(t.clone()),
            VarOrTerm::Triple(p) => Some(Term::triple(
                self.ground(&p[0], s)?,
                self.ground(&p[1], s)?,
                self.ground(&p[2], s)?,
            )),
        }
    }

    /// Bind a pattern position to a matched term id. Top-level constants
    /// were already enforced by the index lookup; nested ones are compared.
    fn bind(&self, s: &mut Sol, vt: &VarOrTerm, id: Id) -> bool {
        self.bind_at(s, vt, id, false)
    }

    fn bind_at(&self, s: &mut Sol, vt: &VarOrTerm, id: Id, nested: bool) -> bool {
        match vt {
            VarOrTerm::Var(v) => match s[*v] {
                Some(x) => x == id,
                None => {
                    s[*v] = Some(id);
                    true
                }
            },
            VarOrTerm::Term(t) => !nested || self.term(id) == *t,
            VarOrTerm::Triple(pat) => {
                let Term::Triple(t) = self.term(id) else {
                    return false;
                };
                (0..3).all(|k| {
                    let cid = self.intern(&t[k]);
                    self.bind_at(s, &pat[k], cid, true)
                })
            }
        }
    }

    /// Instantiate a template position (fresh blank nodes per solution).
    fn inst(&self, vt: &VarOrTerm, s: &Sol, bnodes: &mut HashMap<String, Term>) -> Option<Term> {
        match vt {
            // a blank node of the pattern used as a template (CONSTRUCT WHERE): fresh per solution
            VarOrTerm::Var(v) if self.vars.is_hidden(*v) => Some(
                bnodes
                    .entry(format!("?{v}"))
                    .or_insert_with(Term::fresh_blank)
                    .clone(),
            ),
            VarOrTerm::Var(v) => s[*v].map(|id| self.term(id)),
            VarOrTerm::Term(t) => Some(self.inst_term(t, bnodes)),
            VarOrTerm::Triple(p) => {
                let (a, b, c) = (
                    self.inst(&p[0], s, bnodes)?,
                    self.inst(&p[1], s, bnodes)?,
                    self.inst(&p[2], s, bnodes)?,
                );
                (!a.is_literal() && b.is_iri()).then(|| Term::triple(a, b, c))
            }
        }
    }

    fn inst_term(&self, t: &Term, bnodes: &mut HashMap<String, Term>) -> Term {
        match t {
            Term::Blank(b) => bnodes
                .entry(b.clone())
                .or_insert_with(Term::fresh_blank)
                .clone(),
            Term::Triple(q) => Term::triple(
                self.inst_term(&q[0], bnodes),
                self.inst_term(&q[1], bnodes),
                self.inst_term(&q[2], bnodes),
            ),
            other => other.clone(),
        }
    }

    fn graphs_for(&self, g: Active) -> Option<Vec<Id>> {
        match g {
            Active::Named(id) => Some(vec![id]),
            Active::Default => self.default_graphs.clone(),
        }
    }

    /// Order triple patterns greedily by how many positions are bound.
    fn order_bgp<'p>(
        &self,
        tps: &'p [TriplePattern],
        bound: &FastSet<Var>,
    ) -> Vec<&'p TriplePattern> {
        let mut bound = bound.clone();
        let mut left: Vec<&TriplePattern> = tps.iter().collect();
        let mut out = Vec::new();
        while !left.is_empty() {
            let score = |tp: &TriplePattern| -> i32 {
                let b = |vt: &VarOrTerm| match vt {
                    VarOrTerm::Term(_) => true,
                    VarOrTerm::Var(v) => bound.contains(v),
                    VarOrTerm::Triple(_) => {
                        let mut vs = Vec::new();
                        vt.vars(&mut vs);
                        vs.iter().all(|v| bound.contains(v))
                    }
                };
                let p_bound = match &tp.p {
                    Predicate::Var(v) => bound.contains(v),
                    Predicate::Path(Path::Iri(_)) => true,
                    Predicate::Path(_) => false,
                };
                let path_penalty =
                    matches!(&tp.p, Predicate::Path(p) if !matches!(p, Path::Iri(_)));
                (b(&tp.s) as i32) * 4 + (b(&tp.o) as i32) * 3 + (p_bound as i32) * 2
                    - (path_penalty as i32) * 3
            };
            // equally bound patterns go smallest first, by the indexes' estimate of how many
            // statements match their constants
            let size = |tp: &TriplePattern| -> usize {
                let c = |vt: &VarOrTerm| match vt {
                    VarOrTerm::Term(t) if !matches!(t, Term::Triple(_)) => Some(self.lookup(t)),
                    _ => None,
                };
                let p = match &tp.p {
                    Predicate::Path(Path::Iri(i))
                        if i != ONTO_FTS
                            && !i.starts_with(RDF_RANK)
                            && !i.starts_with(KNN)
                            && !i.starts_with(crate::connector::NAMESPACE) =>
                    {
                        Some(self.lookup_iri(i))
                    }
                    Predicate::Path(Path::Iri(_)) => return 0, // special predicates: few results
                    _ => None,
                };
                match (c(&tp.s), p, c(&tp.o)) {
                    (Some(None), _, _) | (_, Some(None), _) | (_, _, Some(None)) => 0, // a constant not in the store: nothing
                    (s, p, o) => self
                        .store
                        .estimate(s.flatten(), p.flatten(), o.flatten(), None),
                }
            };
            let (idx, _) = left
                .iter()
                .enumerate()
                .max_by_key(|(i, tp)| (score(tp), std::cmp::Reverse(size(tp)), -(*i as i32)))
                .unwrap();
            let tp = left.remove(idx);
            let mut vs = Vec::new();
            tp.s.vars(&mut vs);
            tp.o.vars(&mut vs);
            bound.extend(vs);
            if let Predicate::Var(v) = &tp.p {
                bound.insert(*v);
            }
            out.push(tp);
        }
        out
    }

    fn bgp(&self, tps: &[TriplePattern], input: Vec<Sol>, g: Active) -> R<Vec<Sol>> {
        if tps.is_empty() || input.is_empty() {
            return Ok(input);
        }
        let bound: FastSet<Var> = input[0]
            .iter()
            .enumerate()
            .filter(|(_, v)| v.is_some())
            .map(|(i, _)| i)
            .collect();
        let order = self.order_bgp(tps, &bound);
        let graphs = self.graphs_for(g);
        let mut sols = input;
        for tp in order {
            self.check_time()?;
            if sols.len() >= HASH_JOIN_MIN {
                if let Some(joined) = self.hash_join(tp, &sols, &graphs)? {
                    sols = joined;
                    if sols.is_empty() {
                        break;
                    }
                    continue;
                }
            }
            let mut next = Vec::new();
            for s in &sols {
                self.match_triple(tp, s, &graphs, &mut next)?;
            }
            sols = next;
            if sols.is_empty() {
                break;
            }
        }
        Ok(sols)
    }

    /// Join many solutions with a plain triple pattern by scanning the pattern once and
    /// probing a hash table on the shared variables, instead of one index lookup per solution.
    /// None when the pattern or the solutions do not suit it (the caller then loops).
    fn hash_join(
        &self,
        tp: &TriplePattern,
        sols: &[Sol],
        graphs: &Option<Vec<Id>>,
    ) -> R<Option<Vec<Sol>>> {
        enum Pos {
            Const(Option<Id>),
            Var(Var),
        }
        let pos = |vt: &VarOrTerm| match vt {
            VarOrTerm::Var(v) => Some(Pos::Var(*v)),
            VarOrTerm::Term(t) if !matches!(t, Term::Triple(_)) => Some(Pos::Const(self.lookup(t))),
            _ => None,
        };
        let p = match &tp.p {
            Predicate::Var(v) => Pos::Var(*v),
            Predicate::Path(Path::Iri(i))
                if i != ONTO_FTS
                    && !i.starts_with(RDF_RANK)
                    && !i.starts_with(KNN)
                    && !i.starts_with(crate::connector::NAMESPACE) =>
            {
                Pos::Const(self.lookup_iri(i))
            }
            _ => return Ok(None),
        };
        let (Some(s), Some(o)) = (pos(&tp.s), pos(&tp.o)) else {
            return Ok(None);
        };
        let positions = [s, p, o];
        if positions.iter().any(|x| matches!(x, Pos::Const(None))) {
            return Ok(Some(Vec::new()));
        }
        // variables: (position, var, bound in the solutions); boundness must be uniform
        let mut vars: Vec<(usize, Var, bool)> = Vec::new();
        for (i, x) in positions.iter().enumerate() {
            if let Pos::Var(v) = x {
                if vars.iter().any(|w| w.1 == *v) {
                    return Ok(None);
                }
                let bound = sols[0][*v].is_some();
                if sols.iter().any(|s| s[*v].is_some() != bound) {
                    return Ok(None);
                }
                vars.push((i, *v, bound));
            }
        }
        if !vars.iter().any(|v| v.2) {
            return Ok(None); // nothing shared: a cross product either way
        }
        let constant = |i: usize| {
            if let Pos::Const(c) = positions[i] {
                c
            } else {
                None
            }
        };
        // scanning a row costs a small fraction of an index lookup, but a pattern far larger
        // than the solutions is better probed per solution: give up early
        let limit = sols.len() * 16;
        let rows: Vec<[Id; 4]> = match graphs {
            None => {
                let mut v = Vec::new();
                for q in self.scan(constant(0), constant(1), constant(2), None) {
                    if v.len() >= limit {
                        return Ok(None);
                    }
                    v.push(q);
                }
                v
            }
            _ => {
                let v = self.scan_graphs(constant(0), constant(1), constant(2), graphs);
                if v.len() > limit {
                    return Ok(None);
                }
                v
            }
        };
        let key_of = |get: &dyn Fn(usize, Var) -> Id| -> [Id; 3] {
            let mut k = [0; 3];
            for &(i, v, bound) in &vars {
                if bound {
                    k[i] = get(i, v);
                }
            }
            k
        };
        let mut table: FastMap<[Id; 3], Vec<usize>> = FastMap::default();
        for (r, q) in rows.iter().enumerate() {
            table.entry(key_of(&|i, _| q[i])).or_default().push(r);
        }
        let probe = |part: &[Sol]| -> Vec<Sol> {
            let mut out = Vec::new();
            for s in part {
                let Some(matches) = table.get(&key_of(&|_, v| s[v].unwrap_or(0))) else {
                    continue;
                };
                for &r in matches {
                    let mut next = s.clone();
                    for &(i, v, bound) in &vars {
                        if !bound {
                            next[v] = Some(rows[r][i]);
                        }
                    }
                    out.push(next);
                }
            }
            out
        };
        self.check_time()?;
        // large probes are split over the cores (the table is shared, read only)
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(8);
        if sols.len() < PARALLEL_PROBE || threads < 2 {
            return Ok(Some(probe(sols)));
        }
        let parts: Vec<Vec<Sol>> = std::thread::scope(|sc| {
            let handles: Vec<_> = sols
                .chunks(sols.len().div_ceil(threads))
                .map(|part| sc.spawn(|| probe(part)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("probe thread"))
                .collect()
        });
        let mut out = Vec::with_capacity(parts.iter().map(Vec::len).sum());
        for p in parts {
            out.extend(p);
        }
        Ok(Some(out))
    }

    fn fts_prepared(&self, query: &str) -> std::rc::Rc<crate::store::FtsPrepared> {
        self.fts
            .borrow_mut()
            .entry(query.to_string())
            .or_insert_with(|| std::rc::Rc::new(self.store.fts_prepare(query)))
            .clone()
    }

    /// `g:ftsScore(?literal, "query")` (BM25) and `g:ftsHighlight(?literal, "query" [, pre, post])`.
    fn fts_function(&self, name: &str, args: &[Option<Term>]) -> Option<Term> {
        let l = args.first()?.as_ref()?.as_literal()?;
        let q = &args.get(1)?.as_ref()?.as_literal()?.value;
        let p = self.fts_prepared(q);
        if name.ends_with("ftsScore") {
            return Some(func::Num::Double(self.store.fts_score(&p, l)?).to_term());
        }
        let arg = |i: usize, default: &str| {
            args.get(i)
                .and_then(|a| a.as_ref())
                .map_or(default.to_string(), |t| t.value().to_string())
        };
        let lang = crate::fts::lang_of(l.lang.as_deref());
        // the words of whichever form of the query matched (the typo-tolerant one as a fallback)
        let query = p.matching(&crate::fts::Doc::new(&l.value, lang));
        let text = query.highlight(&l.value, lang, &arg(2, "<b>"), &arg(3, "</b>"));
        Some(match &l.lang {
            Some(tag) => Term::lang(text, tag),
            None => Term::string(text),
        })
    }

    /// `?literal g:fts "query"`: the full-text search predicate.
    fn match_fts(&self, tp: &TriplePattern, s: &Sol, out: &mut Vec<Sol>) -> R<()> {
        let Some(Term::Literal(q)) = self.ground(&tp.o, s) else {
            return Err("g:fts needs a literal query string as its object".into());
        };
        if !self.store.fts_enabled() {
            return Err(
                "Full-text search is not enabled for this repository (set enableFtsIndex)".into(),
            );
        }
        match self.ground(&tp.s, s) {
            Some(Term::Literal(l)) => {
                if self
                    .store
                    .fts_score(&self.fts_prepared(&q.value), &l)
                    .is_some()
                {
                    out.push(s.clone());
                }
            }
            Some(_) => {}
            None => {
                for (id, _) in self
                    .store
                    .search_prepared(&self.fts_prepared(&q.value), usize::MAX)
                {
                    let mut n = s.clone();
                    if self.bind(&mut n, &tp.s, id) {
                        out.push(n);
                    }
                }
            }
        }
        Ok(())
    }

    /// `?node rank:score ?rank` (optionally with N decimal digits).
    /// `?e <http://graphrd.org/connector/NAME> "query"`: the entities the search engine behind
    /// connector NAME finds, best first (see `connector`).
    fn match_connector(
        &self,
        tp: &TriplePattern,
        s: &Sol,
        name: &str,
        out: &mut Vec<Sol>,
    ) -> R<()> {
        let conn = self
            .store
            .connectors
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
            .ok_or_else(|| format!("No search connector {name} in this repository"))?;
        let Some(Term::Literal(q)) = self.ground(&tp.o, s) else {
            return Err(format!(
                "connector {name} needs a query string as its object"
            ));
        };
        let hits = conn.def.search(&q.value, 1000)?;
        let ids: Vec<Id> = hits
            .iter()
            .filter_map(|(iri, _)| self.lookup(&Term::Iri(iri.clone())))
            .collect();
        match self.resolve(&tp.s, s) {
            Ok(Some(id)) => {
                if ids.contains(&id) {
                    out.push(s.clone());
                }
            }
            Ok(None) => {
                for id in ids {
                    let mut n = s.clone();
                    if self.bind(&mut n, &tp.s, id) {
                        out.push(n);
                    }
                }
            }
            Err(()) => {}
        }
        Ok(())
    }

    /// `?v g:knnN ?q`: the N vector literals nearest to the vector `?q` (bound or constant).
    fn match_knn(&self, tp: &TriplePattern, s: &Sol, k: usize, out: &mut Vec<Sol>) -> R<()> {
        let q = self
            .ground(&tp.o, s)
            .and_then(|t| crate::vector::parse(&t))
            .ok_or("g:knn needs a vector (\"[0.1, 0.2, ...]\"^^g:vector) as its object")?;
        let found = self.store.nearest_vectors(&q, k);
        match self.resolve(&tp.s, s) {
            Ok(Some(id)) => {
                if found.iter().any(|r| r.0 == id) {
                    out.push(s.clone());
                }
            }
            Ok(None) => {
                for (id, _) in found {
                    let mut n = s.clone();
                    if self.bind(&mut n, &tp.s, id) {
                        out.push(n);
                    }
                }
            }
            Err(()) => {}
        }
        Ok(())
    }

    fn match_rank(
        &self,
        tp: &TriplePattern,
        s: &Sol,
        digits: Option<usize>,
        out: &mut Vec<Sol>,
    ) -> R<()> {
        let value = |r: f32| {
            let r = match digits {
                Some(d) => (r as f64 * 10f64.powi(d as i32)).round() / 10f64.powi(d as i32),
                None => r as f64,
            };
            Term::typed(format!("{r}"), "http://www.w3.org/2001/XMLSchema#float")
        };
        let emit = |id: Id, r: f32, out: &mut Vec<Sol>| {
            let mut n = s.clone();
            let rid = self.intern(&value(r));
            if self.bind(&mut n, &tp.s, id) && self.bind(&mut n, &tp.o, rid) {
                out.push(n);
            }
        };
        match self.resolve(&tp.s, s) {
            Ok(Some(id)) => {
                if let Some(r) = self.store.rank_of(id) {
                    emit(id, r, out);
                }
            }
            Ok(None) => {
                for (id, r) in self.store.ranked() {
                    emit(id, r, out);
                }
            }
            Err(()) => {}
        }
        Ok(())
    }

    fn match_triple(
        &self,
        tp: &TriplePattern,
        s: &Sol,
        graphs: &Option<Vec<Id>>,
        out: &mut Vec<Sol>,
    ) -> R<()> {
        if let Predicate::Path(Path::Iri(i)) = &tp.p {
            if i == ONTO_FTS {
                return self.match_fts(tp, s, out);
            }
            if let Some(digits) = i.strip_prefix(RDF_RANK) {
                return self.match_rank(tp, s, digits.parse().ok(), out);
            }
            if let Some(name) = i.strip_prefix(crate::connector::NAMESPACE) {
                return self.match_connector(tp, s, name, out);
            }
            if let Some(digits) = i.strip_prefix(KNN) {
                if digits.is_empty() || digits.bytes().all(|b| b.is_ascii_digit()) {
                    return self.match_knn(tp, s, digits.parse().unwrap_or(10), out);
                }
            }
        }
        if self.at_query.is_some() {
            if let Some(r) = self.match_reasoned(tp, s, graphs, out) {
                return r;
            }
        }
        if let Predicate::Path(path) = &tp.p {
            if !matches!(path, Path::Iri(_)) {
                // a path may have length zero, which matches a constant that is not in the
                // store: such constants become query-local terms instead of failing the match
                let local = |vt: &VarOrTerm| match vt {
                    VarOrTerm::Term(t) => Ok(Some(self.intern(t))),
                    // a variable bound to a term that is not in the graph matches nothing
                    _ => self.resolve(vt, s).and_then(|id| {
                        if id.is_some_and(|x| x >= LOCAL) {
                            Err(())
                        } else {
                            Ok(id)
                        }
                    }),
                };
                let (Ok(sid), Ok(oid)) = (local(&tp.s), local(&tp.o)) else {
                    return Ok(());
                };
                for (a, b) in self.path(path, sid, oid, graphs)? {
                    let mut n = s.clone();
                    if self.bind(&mut n, &tp.s, a) && self.bind(&mut n, &tp.o, b) {
                        out.push(n);
                    }
                }
                return Ok(());
            }
        }
        let Ok(sid) = self.resolve(&tp.s, s) else {
            return Ok(());
        };
        let Ok(oid) = self.resolve(&tp.o, s) else {
            return Ok(());
        };
        if sid.is_some_and(|x| x >= LOCAL) || oid.is_some_and(|x| x >= LOCAL) {
            return Ok(()); // local terms never occur in the store
        }
        match &tp.p {
            Predicate::Path(Path::Iri(_)) | Predicate::Var(_) => {
                let pid = match &tp.p {
                    Predicate::Var(v) => s[*v],
                    Predicate::Path(Path::Iri(i)) => match self.lookup_iri(i) {
                        Some(id) => Some(id),
                        None => return Ok(()),
                    },
                    _ => unreachable!(),
                };
                if pid.is_some_and(|x| x >= LOCAL) {
                    return Ok(());
                }
                // returns the number of solutions so far
                let mut emit = |q: [Id; 4]| {
                    let mut n = s.clone();
                    if self.bind(&mut n, &tp.s, q[0])
                        && bind_pred(&mut n, &tp.p, q[1])
                        && self.bind(&mut n, &tp.o, q[2])
                    {
                        out.push(n);
                    }
                    out.len()
                };
                let cap = self.match_cap.get();
                if cap == usize::MAX {
                    match graphs {
                        None => self.scan(sid, pid, oid, None).for_each(|q| {
                            emit(q);
                        }),
                        Some(gs) if gs.len() == 1 => {
                            self.scan(sid, pid, oid, Some(gs[0])).for_each(|q| {
                                emit(q);
                            })
                        }
                        _ => self
                            .scan_graphs(sid, pid, oid, graphs)
                            .into_iter()
                            .for_each(|q| {
                                emit(q);
                            }),
                    }
                } else {
                    let scan: Box<dyn Iterator<Item = [Id; 4]>> = match graphs {
                        None => Box::new(self.scan(sid, pid, oid, None)),
                        Some(gs) if gs.len() == 1 => {
                            Box::new(self.scan(sid, pid, oid, Some(gs[0])))
                        }
                        _ => Box::new(self.scan_graphs(sid, pid, oid, graphs).into_iter()),
                    };
                    for q in scan {
                        if emit(q) >= cap {
                            break;
                        }
                    }
                }
            }
            Predicate::Path(path) => {
                for (a, b) in self.path(path, sid, oid, graphs)? {
                    let mut n = s.clone();
                    if self.bind(&mut n, &tp.s, a) && self.bind(&mut n, &tp.o, b) {
                        out.push(n);
                    }
                }
            }
        }
        Ok(())
    }

    fn step(
        &self,
        p: &str,
        s: Option<Id>,
        o: Option<Id>,
        graphs: &Option<Vec<Id>>,
    ) -> Vec<(Id, Id)> {
        let Some(pid) = self.lookup_iri(p) else {
            return Vec::new();
        };
        match graphs {
            None => self
                .scan(s, Some(pid), o, None)
                .map(|q| (q[0], q[2]))
                .collect(),
            _ => self
                .scan_graphs(s, Some(pid), o, graphs)
                .into_iter()
                .map(|q| (q[0], q[2]))
                .collect(),
        }
    }

    /// Every edge of predicate `pid` as `(from << 32) | to`, sorted by `from`: the subject, or the
    /// object when walking `backwards`. `est` is about how many there are.
    fn path_edges(
        &self,
        pid: Id,
        backwards: bool,
        graphs: &Option<Vec<Id>>,
        est: usize,
    ) -> Vec<u64> {
        let pack = |q: [Id; 4]| {
            if backwards {
                (q[2] as u64) << 32 | q[0] as u64
            } else {
                (q[0] as u64) << 32 | q[2] as u64
            }
        };
        let mut all = Vec::with_capacity(est + est / 8);
        match graphs {
            None => all.extend(self.scan(None, Some(pid), None, None).map(pack)),
            _ => all.extend(
                self.scan_graphs(None, Some(pid), None, graphs)
                    .into_iter()
                    .map(pack),
            ),
        }
        // the predicate's index is in object order, so a backward walk usually needs no sort
        if !all.is_sorted_by_key(|&x| x >> 32) {
            radix_sort_high(&mut all);
        }
        all
    }

    fn all_nodes(&self, graphs: &Option<Vec<Id>>) -> Vec<Id> {
        let mut seen = FastSet::default();
        let gl: Vec<Option<Id>> = match graphs {
            None => vec![None],
            Some(gs) => gs.iter().map(|&g| Some(g)).collect(),
        };
        for g in gl {
            for q in self.scan(None, None, None, g) {
                seen.insert(q[0]);
                seen.insert(q[2]);
            }
        }
        seen.into_iter().collect()
    }

    /// All (start, end) pairs connected by `path`, honouring bound ends.
    fn path(
        &self,
        path: &Path,
        s: Option<Id>,
        o: Option<Id>,
        graphs: &Option<Vec<Id>>,
    ) -> R<Vec<(Id, Id)>> {
        self.check_time()?;
        Ok(match path {
            Path::Iri(p) => self.step(p, s, o, graphs),
            Path::Inverse(inner) => self
                .path(inner, o, s, graphs)?
                .into_iter()
                .map(|(a, b)| (b, a))
                .collect(),
            Path::Alt(alts) => {
                let mut out = Vec::new();
                for a in alts {
                    out.extend(self.path(a, s, o, graphs)?);
                }
                out
            }
            Path::Seq(parts) => {
                if parts.len() == 1 {
                    return self.path(&parts[0], s, o, graphs);
                }
                let (first, rest) = (&parts[0], Path::Seq(parts[1..].to_vec()));
                let mut out = Vec::new();
                if s.is_none() && o.is_some() {
                    // evaluate from the bound end
                    for (mid, end) in self.path(&rest, None, o, graphs)? {
                        for (start, _) in self.path(first, None, Some(mid), graphs)? {
                            out.push((start, end));
                        }
                    }
                } else {
                    for (start, mid) in self.path(first, s, None, graphs)? {
                        for (_, end) in self.path(&rest, Some(mid), o, graphs)? {
                            out.push((start, end));
                        }
                    }
                }
                out
            }
            Path::ZeroOrOne(inner) => {
                let mut set: FastSet<(Id, Id)> = FastSet::default();
                match (s, o) {
                    (Some(a), Some(b)) if a == b => {
                        set.insert((a, b));
                    }
                    (Some(a), None) => {
                        set.insert((a, a));
                    }
                    (None, Some(b)) => {
                        set.insert((b, b));
                    }
                    (None, None) => {
                        for n in self.all_nodes(graphs) {
                            set.insert((n, n));
                        }
                    }
                    _ => {}
                }
                set.extend(self.path(inner, s, o, graphs)?);
                set.into_iter().collect()
            }
            Path::ZeroOrMore(inner) | Path::OneOrMore(inner) => {
                let zero = matches!(path, Path::ZeroOrMore(_));
                let starts: Vec<(Id, bool)> = match (s, o) {
                    (Some(a), _) => vec![(a, false)],
                    (None, Some(b)) => vec![(b, true)],
                    (None, None) => self
                        .all_nodes(graphs)
                        .into_iter()
                        .map(|n| (n, false))
                        .collect(),
                };
                let mut out = Vec::new();
                // A walk over a plain predicate that reaches many nodes reads all its edges in one
                // scan (sorted by the end it walks from) instead of one index lookup per node.
                let mut edges: [Option<Edges>; 2] = [None, None];
                let mut lookups = 0usize;
                let limit = self.store.dict.count();
                for (start, backwards) in starts {
                    // the nodes in the order they are reached, which is also the queue of the walk
                    let mut reached = Reached::new(limit);
                    let mut next = |n: Id, lookups: &mut usize, found: &mut Vec<Id>| -> R<()> {
                        found.clear();
                        if let Some(pid) = match &**inner {
                            Path::Iri(p) => self.lookup_iri(p),
                            _ => None,
                        } {
                            let e = &mut edges[backwards as usize];
                            if e.is_none() && *lookups >= PATH_SCAN {
                                // ... once the lookups so far cost about what the scan would (one
                                // lookup ~ 10 µs, one scanned edge ~ 40 ns), so a walk never takes
                                // much more than twice as long as the better of the two ways
                                let est = self.store.estimate(None, Some(pid), None, None);
                                if *lookups * SCAN_PER_LOOKUP >= est {
                                    *e = Some(Edges::new(
                                        self.path_edges(pid, backwards, graphs, est),
                                    ));
                                }
                            }
                            if let Some(e) = e {
                                found.extend(e.from(n));
                                return Ok(());
                            }
                        }
                        *lookups += 1;
                        if backwards {
                            found.extend(
                                self.path(inner, None, Some(n), graphs)?
                                    .into_iter()
                                    .map(|(a, _)| a),
                            );
                        } else {
                            found.extend(
                                self.path(inner, Some(n), None, graphs)?
                                    .into_iter()
                                    .map(|(_, b)| b),
                            );
                        }
                        Ok(())
                    };
                    let mut found = Vec::new();
                    if zero {
                        reached.insert(start);
                    } else {
                        next(start, &mut lookups, &mut found)?;
                        found.iter().for_each(|&m| _ = reached.insert(m));
                    }
                    let mut head = 0;
                    while head < reached.list.len() {
                        let n = reached.list[head];
                        head += 1;
                        if head % 65_536 == 0 {
                            self.check_time()?;
                        }
                        next(n, &mut lookups, &mut found)?;
                        found.iter().for_each(|&m| _ = reached.insert(m));
                    }
                    for &n in &reached.list {
                        let pair = if backwards { (n, start) } else { (start, n) };
                        if s.is_none_or(|x| x == pair.0) && o.is_none_or(|x| x == pair.1) {
                            out.push(pair);
                        }
                    }
                }
                out
            }
            Path::Negated(set) => {
                let fwd: FastSet<Id> = set
                    .iter()
                    .filter(|(_, inv)| !inv)
                    .filter_map(|(i, _)| self.lookup_iri(i))
                    .collect();
                let inv: FastSet<Id> = set
                    .iter()
                    .filter(|(_, inv)| *inv)
                    .filter_map(|(i, _)| self.lookup_iri(i))
                    .collect();
                let has_fwd = set.iter().any(|(_, i)| !i) || set.is_empty();
                let has_inv = set.iter().any(|(_, i)| *i);
                let mut out = Vec::new();
                if has_fwd {
                    out.extend(
                        self.scan_graphs(s, None, o, graphs)
                            .into_iter()
                            .filter(|q| !fwd.contains(&q[1]))
                            .map(|q| (q[0], q[2])),
                    );
                }
                if has_inv {
                    out.extend(
                        self.scan_graphs(o, None, s, graphs)
                            .into_iter()
                            .filter(|q| !inv.contains(&q[1]))
                            .map(|q| (q[2], q[0])),
                    );
                }
                out
            }
        })
    }

    // ------------------------------------------------------------ expressions

    fn expr(&self, e: &Expr, s: &Sol, g: Active) -> Option<Term> {
        match e {
            Expr::Var(v) => s[*v].map(|id| self.term(id)),
            Expr::Const(t) => Some(t.clone()),
            Expr::Or(a, b) => {
                let x = self.expr(a, s, g).as_ref().and_then(func::ebv);
                let y = self.expr(b, s, g).as_ref().and_then(func::ebv);
                match (x, y) {
                    (Some(true), _) | (_, Some(true)) => Some(Term::boolean(true)),
                    (Some(false), Some(false)) => Some(Term::boolean(false)),
                    _ => None,
                }
            }
            Expr::And(a, b) => {
                let x = self.expr(a, s, g).as_ref().and_then(func::ebv);
                let y = self.expr(b, s, g).as_ref().and_then(func::ebv);
                match (x, y) {
                    (Some(false), _) | (_, Some(false)) => Some(Term::boolean(false)),
                    (Some(true), Some(true)) => Some(Term::boolean(true)),
                    _ => None,
                }
            }
            Expr::Not(a) => Some(Term::boolean(!func::ebv(&self.expr(a, s, g)?)?)),
            Expr::Cmp(op, a, b) => {
                let (x, y) = (self.expr(a, s, g)?, self.expr(b, s, g)?);
                let r = match op {
                    CmpOp::Eq => func::equals(&x, &y)?,
                    CmpOp::Ne => !func::equals(&x, &y)?,
                    _ => {
                        let o = func::compare(&x, &y)?;
                        match op {
                            CmpOp::Lt => o.is_lt(),
                            CmpOp::Gt => o.is_gt(),
                            CmpOp::Le => o.is_le(),
                            _ => o.is_ge(),
                        }
                    }
                };
                Some(Term::boolean(r))
            }
            Expr::In(a, list, negated) => {
                let x = self.expr(a, s, g)?;
                let mut error = false;
                for item in list {
                    match self.expr(item, s, g).and_then(|y| func::equals(&x, &y)) {
                        Some(true) => return Some(Term::boolean(!negated)),
                        Some(false) => {}
                        None => error = true,
                    }
                }
                if error {
                    None
                } else {
                    Some(Term::boolean(*negated))
                }
            }
            Expr::Arith(op, a, b) => {
                let x = func::num(&self.expr(a, s, g)?)?;
                let y = func::num(&self.expr(b, s, g)?)?;
                Some(func::arith(*op, x, y)?.to_term())
            }
            Expr::Neg(a) => {
                let x = func::num(&self.expr(a, s, g)?)?;
                Some(func::arith('*', x, Num::Int(-1))?.to_term())
            }
            Expr::Bound(v) => Some(Term::boolean(s[*v].is_some())),
            Expr::Exists(p, negated) => {
                let found = self.eval(p, g, s).map(|r| !r.is_empty()).ok()?;
                Some(Term::boolean(found != *negated))
            }
            Expr::Call(name, args) => match name.as_str() {
                "IF" => {
                    let c = func::ebv(&self.expr(args.first()?, s, g)?)?;
                    self.expr(args.get(if c { 1 } else { 2 })?, s, g)
                }
                "COALESCE" => args.iter().find_map(|a| self.expr(a, s, g)),
                "BNODE" if args.len() == 1 => {
                    let label = func::string_lit(&self.expr(&args[0], s, g)?)?.0.to_string();
                    let row = self.row.borrow().clone().unwrap_or_else(|| s.clone());
                    Some(
                        self.bnodes
                            .borrow_mut()
                            .entry((row, label))
                            .or_insert_with(Term::fresh_blank)
                            .clone(),
                    )
                }
                "http://graphrd.org/fn#similarity" => {
                    // cosine similarity of two vectors (literals of numbers)
                    let a = crate::vector::parse(&self.expr(args.first()?, s, g)?)?;
                    let b = crate::vector::parse(&self.expr(args.get(1)?, s, g)?)?;
                    crate::vector::cosine(&a, &b).map(|x| {
                        Term::typed(format!("{x}"), "http://www.w3.org/2001/XMLSchema#double")
                    })
                }
                "http://graphrd.org/fn#ftsScore" | "http://graphrd.org/fn#ftsHighlight" => {
                    let vals: Vec<Option<Term>> = args.iter().map(|a| self.expr(a, s, g)).collect();
                    self.fts_function(name, &vals)
                }
                _ => {
                    if let Some(r) = self.string_test(e, s) {
                        return r.map(Term::boolean);
                    }
                    let vals: Vec<Option<Term>> = args.iter().map(|a| self.expr(a, s, g)).collect();
                    func::call(name, &vals, &self.base)
                }
            },
        }
    }

    // ------------------------------------------------------------ queries

    /// Run a SELECT-shaped query; returns (projected vars, solutions).
    pub fn select(&self, q: &Query) -> R<(Vec<Var>, Vec<Sol>)> {
        self.select_in(q, Active::Default, self.seed.as_ref())
    }

    /// A query evaluated against the active graph `g` (a subquery inside GRAPH sees that graph).
    fn select_in(&self, q: &Query, g: Active, seed: Option<&Sol>) -> R<(Vec<Var>, Vec<Sol>)> {
        let counted = if seed.is_some() {
            None
        } else {
            self.count_one_pattern(q, g)?
        };
        let mut sols = match counted {
            Some(grouped) => grouped,
            None => {
                // only the first OFFSET + LIMIT solutions of an unordered, ungrouped,
                // non-DISTINCT query (without a trailing VALUES, which joins later) are kept
                let plain = q.aggregates.is_empty()
                    && q.group_by.is_empty()
                    && q.having.is_empty()
                    && q.order_by.is_empty()
                    && q.values.is_none();
                let distinct = matches!(
                    &q.form,
                    Form::Select { distinct: true, .. } | Form::Select { reduced: true, .. }
                );
                let cap = match q.limit {
                    Some(l) if plain && !distinct => q.offset.saturating_add(l),
                    _ => usize::MAX,
                };
                let sols =
                    self.eval_capped(&q.pattern, g, seed.unwrap_or(&self.empty_sol()), cap)?;
                if !q.aggregates.is_empty() || !q.group_by.is_empty() {
                    self.group(q, sols)?
                } else {
                    sols
                }
            }
        };
        for h in &q.having {
            sols.retain(|s| self.filter(h, s, Active::Default));
        }
        // a trailing VALUES joins after grouping, before the SELECT expressions
        if let Some(values) = &q.values {
            let vals = self.eval(values, Active::Default, &self.empty_sol())?;
            sols = join(sols, vals);
        }
        let projection = match &q.form {
            Form::Select { projection, .. } => projection.clone(),
            _ => None,
        };
        let vars: Vec<Var> = match &projection {
            Some(p) => p.iter().map(|(_, v)| *v).collect(),
            None => {
                // SELECT *: the pattern's variables, then those of a trailing VALUES
                let mut vs = visible_vars(&q.pattern, self.vars);
                if let Some(values) = &q.values {
                    for v in visible_vars(values, self.vars) {
                        if !vs.contains(&v) {
                            vs.push(v);
                        }
                    }
                }
                vs
            }
        };
        if let Some(p) = &projection {
            for s in sols.iter_mut() {
                // BNODE(str) is keyed on the solution as it was before the SELECT expressions
                *self.row.borrow_mut() = Some(s.clone());
                for (e, v) in p {
                    if let Some(e) = e {
                        if s[*v].is_none() {
                            if let Some(t) = self.expr(e, s, Active::Default) {
                                s[*v] = Some(self.intern(&t));
                            }
                        }
                    }
                }
            }
            *self.row.borrow_mut() = None;
        }
        if !q.order_by.is_empty() {
            sols = self.order(q, sols);
        }
        // project
        let mut projected: Vec<Sol> = sols
            .into_iter()
            .map(|s| {
                let mut p = vec![None; self.nvars];
                for &v in &vars {
                    p[v] = s[v];
                }
                p
            })
            .collect();
        if let Form::Select {
            distinct, reduced, ..
        } = &q.form
        {
            if *distinct || *reduced {
                let mut seen = FastSet::default();
                projected.retain(|s| seen.insert(vars.iter().map(|&v| s[v]).collect::<Vec<_>>()));
            }
        }
        let start = q.offset.min(projected.len());
        let end = match q.limit {
            Some(l) => start.saturating_add(l).min(projected.len()),
            None => projected.len(),
        };
        projected.truncate(end);
        projected.drain(..start);
        Ok((vars, projected))
    }

    /// ORDER BY. The first key is computed once per distinct value (once per distinct id when
    /// it reads one variable); with a LIMIT, only the rows whose first key is among the
    /// OFFSET + LIMIT smallest distinct values can be in the answer, so the others are dropped
    /// before sorting; the distinct values left are ranked once and rows sort by rank.
    fn order(&self, q: &Query, sols: Vec<Sol>) -> Vec<Sol> {
        use std::cmp::Ordering;
        let (first, desc0) = &q.order_by[0];
        let first_var = single_var(first);
        let mut vals: Vec<Option<Term>> = Vec::new();
        let mut by_term: HashMap<Option<Term>, u32> = HashMap::new();
        let mut by_id: FastMap<Option<Id>, u32> = FastMap::default();
        let mut idx: Vec<u32> = Vec::with_capacity(sols.len());
        for s in &sols {
            if let Some(v) = first_var {
                if let Some(&i) = by_id.get(&s[v]) {
                    idx.push(i);
                    continue;
                }
            }
            let t = self.expr(first, s, Active::Default);
            let i = match by_term.get(&t) {
                Some(&i) => i,
                None => {
                    vals.push(t.clone());
                    by_term.insert(t, vals.len() as u32 - 1);
                    vals.len() as u32 - 1
                }
            };
            if let Some(v) = first_var {
                by_id.insert(s[v], i);
            }
            idx.push(i);
        }
        let cmp0 = |a: &u32, b: &u32| {
            let o = func::order_cmp(vals[*a as usize].as_ref(), vals[*b as usize].as_ref());
            if *desc0 { o.reverse() } else { o }
        };
        // (with DISTINCT, rows past the limit may be needed after duplicates go)
        let distinct = matches!(
            &q.form,
            Form::Select { distinct: true, .. } | Form::Select { reduced: true, .. }
        );
        let mut rows: Vec<(u32, Sol)> = idx.into_iter().zip(sols).collect();
        let mut used: Vec<u32> = (0..vals.len() as u32).collect();
        if let Some(k) = q
            .limit
            .map(|l| q.offset.saturating_add(l))
            .filter(|_| !distinct)
        {
            if k == 0 {
                return Vec::new();
            }
            if k < used.len() {
                used.select_nth_unstable_by(k - 1, cmp0);
                used.truncate(k);
                let mut keep = vec![false; vals.len()];
                for &u in &used {
                    keep[u as usize] = true;
                }
                rows.retain(|(i, _)| keep[*i as usize]);
            }
        }
        // rank the distinct values left; equal values share a rank
        used.sort_by(cmp0);
        let mut rank = vec![0u32; vals.len()];
        let mut r = 0;
        for (n, &u) in used.iter().enumerate() {
            if n > 0 && cmp0(&used[n - 1], &u) != Ordering::Equal {
                r += 1;
            }
            rank[u as usize] = r;
        }
        let rest = &q.order_by[1..];
        let mut keyed: Vec<(u32, Vec<Option<Term>>, Sol)> = rows
            .into_iter()
            .map(|(i, s)| {
                (
                    rank[i as usize],
                    rest.iter()
                        .map(|(e, _)| self.expr(e, &s, Active::Default))
                        .collect(),
                    s,
                )
            })
            .collect();
        keyed.sort_by(|a, b| {
            a.0.cmp(&b.0).then_with(|| {
                for (i, (_, desc)) in rest.iter().enumerate() {
                    let o = func::order_cmp(a.1[i].as_ref(), b.1[i].as_ref());
                    let o = if *desc { o.reverse() } else { o };
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                Ordering::Equal
            })
        });
        keyed.into_iter().map(|(_, _, s)| s).collect()
    }

    /// Counting over a single triple pattern (`COUNT(*)`, `COUNT(?v)`, `COUNT(DISTINCT ?v)`,
    /// optionally grouped by pattern variables) straight from the index scan, without building
    /// a solution per matching triple. None when the query has another shape.
    fn count_one_pattern(&self, q: &Query, g: Active) -> R<Option<Vec<Sol>>> {
        // also with a filter on one of the pattern's variables, decided per row id
        let (pattern, filter) = match &q.pattern {
            Pattern::Filter(e, inner) => match single_var(e) {
                Some(v) => (&**inner, Some((e, v))),
                None => return Ok(None),
            },
            p => (p, None),
        };
        // a filter the spatial index can narrow is faster through `eval`
        let mut spatial = Vec::new();
        if let Some((e, _)) = filter {
            spatial_calls(e, &mut spatial);
        }
        if spatial
            .iter()
            .any(|(name, _, radius)| crate::geo::indexable(name, *radius))
        {
            return Ok(None);
        }
        let Pattern::Bgp(tps) = pattern else {
            return Ok(None);
        };
        let [tp] = tps.as_slice() else {
            return Ok(None);
        };
        if q.aggregates.is_empty() || q.aggregates.iter().any(|a| a.func != "COUNT") {
            return Ok(None);
        }
        // positions: a constant id, a variable, or (constant not in the store) no match at all
        enum Pos {
            Const(Option<Id>),
            Var(Var),
        }
        let pos = |vt: &VarOrTerm| match vt {
            VarOrTerm::Var(v) => Some(Pos::Var(*v)),
            VarOrTerm::Term(t) if !matches!(t, Term::Triple(_)) => Some(Pos::Const(self.lookup(t))),
            _ => None,
        };
        let p = match &tp.p {
            Predicate::Var(v) => Pos::Var(*v),
            Predicate::Path(Path::Iri(i))
                if i != ONTO_FTS
                    && !i.starts_with(RDF_RANK)
                    && !i.starts_with(KNN)
                    && !i.starts_with(crate::connector::NAMESPACE) =>
            {
                Pos::Const(self.lookup_iri(i))
            }
            _ => return Ok(None),
        };
        let (Some(s), Some(o)) = (pos(&tp.s), pos(&tp.o)) else {
            return Ok(None);
        };
        let positions = [s, p, o];
        let vars: Vec<(usize, Var)> = positions
            .iter()
            .enumerate()
            .filter_map(|(i, x)| {
                if let Pos::Var(v) = x {
                    Some((i, *v))
                } else {
                    None
                }
            })
            .collect();
        if (0..vars.len()).any(|i| vars[i + 1..].iter().any(|w| w.1 == vars[i].1)) {
            return Ok(None); // a repeated variable constrains the match
        }
        let slot = |v: Var| vars.iter().find(|x| x.1 == v).map(|x| x.0);
        let filter = match filter {
            Some((e, v)) => match slot(v) {
                Some(i) => Some((e, v, i)),
                None => return Ok(None),
            },
            None => None,
        };
        let e_slot = |e: &Expr| if let Expr::Var(v) = e { slot(*v) } else { None };
        let mut group_slots: Vec<(Var, usize)> = Vec::new();
        for (e, alias) in &q.group_by {
            match (e, alias, e_slot(e)) {
                (Expr::Var(v), None, Some(i)) => group_slots.push((*v, i)),
                _ => return Ok(None),
            }
        }
        let mut agg_slots = Vec::new();
        for a in &q.aggregates {
            agg_slots.push(match &a.expr {
                None => None,
                Some(Expr::Var(v)) => match slot(*v) {
                    Some(i) => Some(i),
                    None => return Ok(None),
                },
                Some(_) => return Ok(None),
            });
        }
        let id = |x: &Pos| match x {
            Pos::Const(c) => *c,
            Pos::Var(_) => None,
        };
        let missing = positions.iter().any(|x| matches!(x, Pos::Const(None)));
        // group key -> (rows, distinct values per aggregate)
        let mut order: Vec<Vec<Id>> = Vec::new();
        let mut groups: FastMap<Vec<Id>, (u64, Vec<FastSet<Id>>)> = FastMap::default();
        if !missing {
            let graphs = self.graphs_for(g);
            let rows: Box<dyn Iterator<Item = [Id; 4]>> = match &graphs {
                None => self.scan(
                    id(&positions[0]),
                    id(&positions[1]),
                    id(&positions[2]),
                    None,
                ),
                Some(_) => Box::new(
                    self.scan_graphs(
                        id(&positions[0]),
                        id(&positions[1]),
                        id(&positions[2]),
                        &graphs,
                    )
                    .into_iter(),
                ),
            };
            let mut probe = self.empty_sol();
            let mut memo: FastMap<Id, bool> = FastMap::default();
            let mut use_memo = true;
            // without grouping every row is in the one group: no key to build and hash per row
            let mut single: (u64, Vec<FastSet<Id>>) =
                (0, vec![FastSet::default(); agg_slots.len()]);
            // without the memo (values hardly repeat), the filtered column's terms are read from
            // the dictionary in bulk, a chunk ahead
            let bulk = &std::cell::Cell::new(false);
            let rows: Box<dyn Iterator<Item = [Id; 4]>> = match filter {
                Some((_, _, i)) => Box::new(prefetched(rows, move |chunk| {
                    if bulk.get() {
                        self.prefetch(chunk.iter().map(|r| r[i]))
                    }
                })),
                None => rows,
            };
            for (n, row) in rows.enumerate() {
                if n % 65_536 == 0 {
                    self.check_time()?;
                }
                if let Some((e, v, i)) = filter {
                    // the values of most filtered columns repeat: decide each once (unless the
                    // first rows show they hardly do)
                    if n == 512 && memo.len() * 4 > n * 3 {
                        use_memo = false;
                        bulk.set(true);
                    }
                    let mut test = |id: Id| {
                        probe[v] = Some(id);
                        self.filter(e, &probe, g)
                    };
                    let pass = if use_memo {
                        *memo.entry(row[i]).or_insert_with(|| test(row[i]))
                    } else {
                        test(row[i])
                    };
                    if !pass {
                        continue;
                    }
                }
                let entry = if group_slots.is_empty() {
                    &mut single
                } else {
                    let key: Vec<Id> = group_slots.iter().map(|&(_, i)| row[i]).collect();
                    match groups.get_mut(&key) {
                        Some(e) => e,
                        None => {
                            order.push(key.clone());
                            groups
                                .entry(key)
                                .or_insert_with(|| (0, vec![FastSet::default(); agg_slots.len()]))
                        }
                    }
                };
                entry.0 += 1;
                for (k, a) in q.aggregates.iter().enumerate() {
                    if a.distinct {
                        if let Some(i) = agg_slots[k] {
                            entry.1[k].insert(row[i]);
                        }
                    }
                }
            }
            if group_slots.is_empty() && single.0 > 0 {
                order.push(Vec::new());
                groups.insert(Vec::new(), single);
            }
        }
        if q.group_by.is_empty() && order.is_empty() {
            order.push(Vec::new());
            groups.insert(Vec::new(), (0, vec![FastSet::default(); agg_slots.len()]));
        }
        let mut out = Vec::with_capacity(order.len());
        for key in order {
            let (rows, distinct) = &groups[&key];
            let mut sol = self.empty_sol();
            for (k, (v, _)) in group_slots.iter().enumerate() {
                sol[*v] = Some(key[k]);
            }
            for (k, (a, &hv)) in q.aggregates.iter().zip(&q.agg_vars).enumerate() {
                // the matched triples are distinct, so COUNT(DISTINCT *) counts rows
                let n = if a.distinct && agg_slots[k].is_some() {
                    distinct[k].len() as u64
                } else {
                    *rows
                };
                sol[hv] = Some(self.intern(&Term::integer(n as i64)));
            }
            out.push(sol);
        }
        Ok(Some(out))
    }

    fn group(&self, q: &Query, sols: Vec<Sol>) -> R<Vec<Sol>> {
        let mut keys: Vec<Vec<Option<Id>>> = Vec::new();
        let mut groups: FastMap<Vec<Option<Id>>, Vec<Sol>> = FastMap::default();
        for s in sols {
            let key: Vec<Option<Id>> = q
                .group_by
                .iter()
                .map(|(e, _)| match e {
                    // plain variables group by id, without decoding terms
                    Expr::Var(v) => s[*v],
                    e => self.expr(e, &s, Active::Default).map(|t| self.intern(&t)),
                })
                .collect();
            if !groups.contains_key(&key) {
                keys.push(key.clone());
            }
            groups.entry(key).or_default().push(s);
        }
        if q.group_by.is_empty() && keys.is_empty() {
            keys.push(Vec::new());
            groups.insert(Vec::new(), Vec::new());
        }
        let mut out = Vec::new();
        for key in keys {
            let members = &groups[&key];
            let mut s = self.empty_sol();
            for (i, (e, alias)) in q.group_by.iter().enumerate() {
                if let Expr::Var(v) = e {
                    s[*v] = key[i];
                }
                if let Some(a) = alias {
                    s[*a] = key[i];
                }
            }
            for (agg, &hv) in q.aggregates.iter().zip(&q.agg_vars) {
                if let Some(t) = self.aggregate(agg, members) {
                    s[hv] = Some(self.intern(&t));
                }
            }
            out.push(s);
        }
        Ok(out)
    }

    fn aggregate(&self, agg: &Aggregate, members: &[Sol]) -> Option<Term> {
        let mut values: Vec<Term> = Vec::new();
        let mut errors = false;
        match &agg.expr {
            None => {
                let n = if agg.distinct {
                    members.iter().collect::<FastSet<_>>().len()
                } else {
                    members.len()
                };
                return Some(Term::integer(n as i64));
            }
            Some(Expr::Var(v)) if agg.func == "COUNT" => {
                // COUNT(?x) / COUNT(DISTINCT ?x) on ids, without decoding terms
                let n = if agg.distinct {
                    members
                        .iter()
                        .filter_map(|m| m[*v])
                        .collect::<FastSet<_>>()
                        .len()
                } else {
                    members.iter().filter(|m| m[*v].is_some()).count()
                };
                return Some(Term::integer(n as i64));
            }
            Some(Expr::Var(v)) if !agg.distinct && (agg.func == "SUM" || agg.func == "AVG") => {
                // decode each distinct value once
                let mut memo: FastMap<Id, Option<Num>> = FastMap::default();
                let mut acc = Num::Int(0);
                let mut n = 0i64;
                for m in members {
                    match m[*v] {
                        None if agg.func == "SUM" => return None,
                        None => {}
                        Some(id) => {
                            let x = (*memo.entry(id).or_insert_with(|| func::num(&self.term(id))))?;
                            acc = func::arith('+', acc, x)?;
                            n += 1;
                        }
                    }
                }
                if agg.func == "AVG" {
                    if n == 0 {
                        return Some(Term::integer(0));
                    }
                    acc = func::arith('/', acc, Num::Int(n))?;
                }
                return Some(acc.to_term());
            }
            Some(e) => {
                for m in members {
                    match self.expr(e, m, Active::Default) {
                        Some(t) => values.push(t),
                        None => errors = true,
                    }
                }
            }
        }
        if agg.distinct {
            let mut seen = HashSet::new();
            values.retain(|v| seen.insert(v.clone()));
        }
        match agg.func.as_str() {
            "COUNT" => Some(Term::integer(values.len() as i64)),
            "SUM" | "AVG" => {
                if errors && agg.func == "SUM" {
                    return None;
                }
                let mut acc = Num::Int(0);
                for v in &values {
                    acc = func::arith('+', acc, func::num(v)?)?;
                }
                if agg.func == "AVG" {
                    if values.is_empty() {
                        return Some(Term::integer(0));
                    }
                    acc = func::arith('/', acc, Num::Int(values.len() as i64))?;
                }
                Some(acc.to_term())
            }
            "MIN" | "MAX" => {
                let mut best: Option<Term> = None;
                for v in values {
                    best = Some(match best {
                        None => v,
                        Some(b) => {
                            let o = func::order_cmp(Some(&v), Some(&b));
                            if (agg.func == "MIN" && o.is_lt()) || (agg.func == "MAX" && o.is_gt())
                            {
                                v
                            } else {
                                b
                            }
                        }
                    });
                }
                best
            }
            "SAMPLE" => values.into_iter().next(),
            "GROUP_CONCAT" => {
                let parts: Option<Vec<String>> = values
                    .iter()
                    .map(|v| {
                        func::string_lit(v)
                            .map(|(s, _)| s.to_string())
                            .or_else(|| v.as_literal().map(|l| l.value.clone()))
                    })
                    .collect();
                Some(Term::string(parts?.join(&agg.separator)))
            }
            _ => None,
        }
    }

    /// Evaluate a pattern from the empty solution; graph `with` when given.
    pub fn query(&self, q: &Query) -> R<QueryResult> {
        self.prefetch_services(&q.pattern);
        match &q.form {
            Form::Select { .. } => {
                let (vars, sols) = self.select(q)?;
                let mut rows = Vec::with_capacity(sols.len());
                // a result too large to stay in the term cache has its terms read together,
                // a chunk at a time; a smaller one goes through the cache, which it warms for
                // the next query (bulk reads bypass the cache)
                let bulk = sols.len().saturating_mul(vars.len()) > BULK_RESULT_TERMS;
                for chunk in sols.chunks(PREFETCH) {
                    if bulk {
                        self.prefetch(chunk.iter().flat_map(|s| vars.iter().filter_map(|&v| s[v])));
                    }
                    rows.extend(chunk.iter().map(|s| {
                        vars.iter()
                            .map(|&v| s[v].map(|id| self.term(id)))
                            .collect::<Vec<_>>()
                    }));
                }
                Ok(QueryResult::Solutions {
                    vars: vars.iter().map(|&v| self.vars.names[v].clone()).collect(),
                    rows,
                })
            }
            Form::Ask => {
                // through select(): a trailing VALUES joins with the pattern
                let (_, sols) = self.select(&Query {
                    form: Form::Select {
                        distinct: false,
                        reduced: false,
                        projection: None,
                    },
                    limit: Some(1),
                    ..q.clone()
                })?;
                Ok(QueryResult::Boolean(!sols.is_empty()))
            }
            Form::Construct(template) => {
                // every variable, hidden ones included (reifiers and blank nodes of CONSTRUCT WHERE)
                let all = Some((0..self.nvars).map(|v| (None, v)).collect());
                let (_, sols) = self.select(&Query {
                    form: Form::Select {
                        distinct: false,
                        reduced: false,
                        projection: all,
                    },
                    ..q.clone()
                })?;
                let mut seen = HashSet::new();
                let mut out = Vec::new();
                for s in &sols {
                    let mut bnodes: HashMap<String, Term> = HashMap::new();
                    for tp in template {
                        let inst = |vt: &VarOrTerm, bnodes: &mut HashMap<String, Term>| {
                            self.inst(vt, s, bnodes)
                        };
                        let p = match &tp.p {
                            Predicate::Var(v) => s[*v].map(|id| self.term(id)),
                            Predicate::Path(Path::Iri(i)) => Some(Term::Iri(i.clone())),
                            _ => None,
                        };
                        let (Some(subj), Some(pred), Some(obj)) =
                            (inst(&tp.s, &mut bnodes), p, inst(&tp.o, &mut bnodes))
                        else {
                            continue;
                        };
                        if subj.is_literal() || !pred.is_iri() {
                            continue;
                        }
                        let quad = Quad {
                            s: subj,
                            p: pred,
                            o: obj,
                            g: None,
                        };
                        if seen.insert(quad.clone()) {
                            out.push(quad);
                        }
                    }
                }
                Ok(QueryResult::Graph(out))
            }
            Form::Describe(targets) => {
                let sols = if matches!(q.pattern, Pattern::Bgp(ref b) if b.is_empty()) {
                    vec![self.empty_sol()]
                } else {
                    self.select(&Query {
                        form: Form::Select {
                            distinct: false,
                            reduced: false,
                            projection: None,
                        },
                        ..q.clone()
                    })?
                    .1
                };
                let mut resources = Vec::new();
                for t in targets {
                    match t {
                        VarOrTerm::Term(t) => resources.push(t.clone()),
                        VarOrTerm::Triple(_) => {}
                        VarOrTerm::Var(v) => {
                            for s in &sols {
                                if let Some(id) = s[*v] {
                                    resources.push(self.term(id));
                                }
                            }
                        }
                    }
                }
                let mut seen_res = HashSet::new();
                let mut seen = HashSet::new();
                let mut out = Vec::new();
                let mut queue: VecDeque<Term> = resources.into_iter().collect();
                while let Some(r) = queue.pop_front() {
                    if !seen_res.insert(r.clone()) {
                        continue;
                    }
                    let Some(id) = self.lookup(&r) else { continue };
                    for qd in self.scan(Some(id), None, None, None) {
                        let quad = Quad {
                            s: self.term(qd[0]),
                            p: self.term(qd[1]),
                            o: self.term(qd[2]),
                            g: None,
                        };
                        if quad.o.is_blank() {
                            queue.push_back(quad.o.clone());
                        }
                        if seen.insert(quad.clone()) {
                            out.push(quad);
                        }
                    }
                }
                Ok(QueryResult::Graph(out))
            }
        }
    }

    // ------------------------------------------------------------ updates

    /// Instantiate quad templates for every solution.
    pub fn instantiate(
        &self,
        templates: &[QuadPattern],
        sols: &[Sol],
        with: Option<&Term>,
    ) -> Vec<Quad> {
        let mut out = Vec::new();
        for s in sols {
            let mut bnodes: HashMap<String, Term> = HashMap::new();
            for t in templates {
                let mut inst = |vt: &VarOrTerm| self.inst(vt, s, &mut bnodes);
                let (Some(subj), Some(pred), Some(obj)) = (inst(&t.s), inst(&t.p), inst(&t.o))
                else {
                    continue;
                };
                let g = match &t.g {
                    Some(g) => match inst(g) {
                        Some(g @ Term::Iri(_)) => Some(g),
                        _ => continue,
                    },
                    None => with.cloned(),
                };
                if subj.is_literal() || !pred.is_iri() {
                    continue;
                }
                out.push(Quad {
                    s: subj,
                    p: pred,
                    o: obj,
                    g,
                });
            }
        }
        out
    }

    /// Evaluate a pattern from the empty solution in the default graph.
    pub fn eval_where(&self, p: &Pattern, with: Option<&Term>) -> R<Vec<Sol>> {
        if let Some(w) = with {
            match self.lookup(w) {
                Some(id) => return self.eval(p, Active::Named(id), &self.empty_sol()),
                None => return self.eval(p, Active::Named(Id::MAX), &self.empty_sol()),
            }
        }
        self.eval(p, Active::Default, &self.empty_sol())
    }
}

/// The conjuncts of a filter that may narrow a geometry variable: `geof:` calls (with
/// `None`) and `geof:...(...) < r` comparisons (with `Some(r)`).
fn spatial_calls<'e>(e: &'e Expr, out: &mut Vec<(&'e str, &'e [Expr], Option<f64>)>) {
    let radius = |t: &Term| func::num(t).and_then(|_| t.value().parse::<f64>().ok());
    match e {
        Expr::And(a, b) => {
            spatial_calls(a, out);
            spatial_calls(b, out);
        }
        Expr::Call(name, args) if name.starts_with(crate::geo::GEOF) && args.len() >= 2 => {
            out.push((name, args, None))
        }
        Expr::Cmp(CmpOp::Lt | CmpOp::Le, call, r) | Expr::Cmp(CmpOp::Gt | CmpOp::Ge, r, call) => {
            if let (Expr::Call(name, args), Expr::Const(t)) = (&**call, &**r) {
                if name.starts_with(crate::geo::GEOF) && args.len() >= 2 {
                    if let Some(r) = radius(t) {
                        out.push((name, args, Some(r)));
                    }
                }
            }
        }
        _ => {}
    }
}

fn mentions(tp: &TriplePattern, v: Var) -> bool {
    let mut vars = Vec::new();
    tp.s.vars(&mut vars);
    tp.o.vars(&mut vars);
    if let Predicate::Var(p) = tp.p {
        vars.push(p);
    }
    vars.contains(&v)
}

fn bind_pred(s: &mut Sol, p: &Predicate, id: Id) -> bool {
    match p {
        Predicate::Var(v) => match s[*v] {
            Some(x) => x == id,
            None => {
                s[*v] = Some(id);
                true
            }
        },
        _ => true,
    }
}

fn merge(a: &Sol, b: &Sol) -> Option<Sol> {
    let mut out = a.clone();
    for (i, v) in b.iter().enumerate() {
        if let Some(v) = v {
            match out[i] {
                Some(x) if x != *v => return None,
                _ => out[i] = Some(*v),
            }
        }
    }
    Some(out)
}

fn join(left: Vec<Sol>, right: Vec<Sol>) -> Vec<Sol> {
    if left.is_empty() || right.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut index = Compatible::new(&right);
    for l in &left {
        index.each(l, false, |r| {
            out.push(merge(l, r).expect("compatible solutions merge"));
            true
        });
    }
    out
}

/// Solutions indexed for finding those compatible with another solution (equal wherever
/// both are bound) without comparing against each: they are grouped by the variables they
/// bind, and each group is hashed on the variables it shares with the probing solution (one
/// table per group and set of shared variables, built on first use).
struct Compatible<'a> {
    /// (bound variables, solutions binding exactly those)
    groups: Vec<(Vec<Var>, Vec<&'a Sol>)>,
    tables: FastMap<(usize, Vec<Var>), FastMap<Vec<Id>, Vec<usize>>>,
}

impl<'a> Compatible<'a> {
    fn new(sols: &'a [Sol]) -> Self {
        let mut by_vars: FastMap<Vec<Var>, usize> = FastMap::default();
        let mut groups: Vec<(Vec<Var>, Vec<&'a Sol>)> = Vec::new();
        for s in sols {
            let vars: Vec<Var> = (0..s.len()).filter(|&v| s[v].is_some()).collect();
            let i = *by_vars.entry(vars.clone()).or_insert_with(|| {
                groups.push((vars, Vec::new()));
                groups.len() - 1
            });
            groups[i].1.push(s);
        }
        Compatible {
            groups,
            tables: FastMap::default(),
        }
    }

    /// Call `f` with each solution compatible with `l`, until it returns false. With
    /// `sharing`, only those binding at least one variable `l` binds too (as MINUS needs).
    fn each(&mut self, l: &Sol, sharing: bool, mut f: impl FnMut(&Sol) -> bool) {
        for (gi, (vars, rows)) in self.groups.iter().enumerate() {
            let shared: Vec<Var> = vars.iter().copied().filter(|&v| l[v].is_some()).collect();
            if shared.is_empty() {
                if sharing {
                    continue;
                }
                for r in rows {
                    if !f(r) {
                        return;
                    }
                }
                continue;
            }
            let key: Vec<Id> = shared.iter().map(|&v| l[v].unwrap()).collect();
            let table = self
                .tables
                .entry((gi, shared))
                .or_insert_with_key(|(_, shared)| {
                    let mut t: FastMap<Vec<Id>, Vec<usize>> = FastMap::default();
                    for (i, r) in rows.iter().enumerate() {
                        t.entry(shared.iter().map(|&v| r[v].unwrap()).collect())
                            .or_default()
                            .push(i);
                    }
                    t
                });
            for &i in table.get(&key).into_iter().flatten() {
                if !f(rows[i]) {
                    return;
                }
            }
        }
    }
}

/// In-scope, user-visible variables of a pattern, in first-seen order.
pub fn visible_vars(p: &Pattern, vars: &VarTable) -> Vec<Var> {
    fn walk(p: &Pattern, out: &mut Vec<Var>) {
        let add = |v: Var, out: &mut Vec<Var>| {
            if !out.contains(&v) {
                out.push(v);
            }
        };
        match p {
            Pattern::Bgp(tps) => {
                for tp in tps {
                    let mut vs = Vec::new();
                    tp.s.vars(&mut vs);
                    if let Predicate::Var(v) = tp.p {
                        vs.push(v);
                    }
                    tp.o.vars(&mut vs);
                    for v in vs {
                        add(v, out);
                    }
                }
            }
            Pattern::Join(a, b) | Pattern::LeftJoin(a, b, _) | Pattern::Union(a, b) => {
                walk(a, out);
                walk(b, out);
            }
            Pattern::Minus(a, _) => walk(a, out),
            Pattern::Filter(_, a) | Pattern::Service(_, a, _, _) => walk(a, out),
            Pattern::Graph(g, a) => {
                if let VarOrTerm::Var(v) = g {
                    add(*v, out);
                }
                walk(a, out);
            }
            Pattern::Extend(a, v, _) => {
                walk(a, out);
                add(*v, out);
            }
            Pattern::Values(vs, _) => {
                for v in vs {
                    add(*v, out);
                }
            }
            Pattern::SubSelect(q) => match &q.form {
                Form::Select {
                    projection: Some(p),
                    ..
                } => {
                    for (_, v) in p {
                        add(*v, out);
                    }
                }
                _ => walk(&q.pattern, out),
            },
        }
    }
    let mut out = Vec::new();
    walk(p, &mut out);
    out.retain(|&v| !vars.is_hidden(v));
    out
}

/// Convert quad patterns into a WHERE pattern (for DELETE WHERE).
pub fn quads_to_pattern(quads: &[QuadPattern]) -> Pattern {
    let mut default = Vec::new();
    let mut graphs: Vec<(VarOrTerm, Vec<TriplePattern>)> = Vec::new();
    for q in quads {
        let tp = TriplePattern {
            s: q.s.clone(),
            p: match &q.p {
                VarOrTerm::Var(v) => Predicate::Var(*v),
                VarOrTerm::Term(t) => Predicate::Path(Path::Iri(t.value().to_string())),
                VarOrTerm::Triple(_) => Predicate::Path(Path::Iri(String::new())), // rejected by the parser
            },
            o: q.o.clone(),
        };
        match &q.g {
            None => default.push(tp),
            Some(g) => match graphs.iter_mut().find(|(x, _)| x == g) {
                Some((_, v)) => v.push(tp),
                None => graphs.push((g.clone(), vec![tp])),
            },
        }
    }
    let mut p = Pattern::Bgp(default);
    for (g, tps) in graphs {
        p = Pattern::Join(
            Box::new(p),
            Box::new(Pattern::Graph(g, Box::new(Pattern::Bgp(tps)))),
        );
    }
    p
}

// ------------------------------------------------------------------ explain

impl Ctx<'_> {
    /// A readable evaluation plan: the operator tree, each BGP in the order it will run, the
    /// estimated matches of every triple pattern (from its constants alone) and where a hash
    /// join will be tried.
    pub fn explain(&self, q: &Query) -> String {
        let mut out = String::new();
        let names = &self.vars.names;
        let v = |x: Var| format!("?{}", names.get(x).map(String::as_str).unwrap_or("_"));
        let head = match &q.form {
            Form::Select {
                distinct,
                reduced,
                projection,
            } => {
                let cols = match projection {
                    Some(p) => p
                        .iter()
                        .map(|(e, x)| match e {
                            // an aggregate's hidden slot: show the aggregate itself
                            Some(Expr::Var(h)) if q.agg_vars.contains(h) => {
                                let a =
                                    &q.aggregates[q.agg_vars.iter().position(|y| y == h).unwrap()];
                                format!(
                                    "({}({}{}) AS {})",
                                    a.func,
                                    if a.distinct { "distinct " } else { "" },
                                    a.expr.as_ref().map_or("*".into(), |e| self.expr_str(e)),
                                    v(*x)
                                )
                            }
                            Some(e) => format!("({} AS {})", self.expr_str(e), v(*x)),
                            None => v(*x),
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                    None => "*".into(),
                };
                format!(
                    "Select{} {cols}",
                    if *distinct {
                        " distinct"
                    } else if *reduced {
                        " reduced"
                    } else {
                        ""
                    }
                )
            }
            Form::Construct(t) => format!("Construct ({} template patterns)", t.len()),
            Form::Ask => "Ask".into(),
            Form::Describe(_) => "Describe".into(),
        };
        out.push_str(&head);
        out.push('\n');
        let mut depth = 1;
        let line = |out: &mut String, d: usize, s: String| {
            out.push_str(&"  ".repeat(d));
            out.push_str(&s);
            out.push('\n');
        };
        if q.limit.is_some() || q.offset > 0 {
            line(
                &mut out,
                depth,
                format!(
                    "Slice offset {} limit {}",
                    q.offset,
                    q.limit.map_or("none".into(), |l| l.to_string())
                ),
            );
            depth += 1;
        }
        if !q.order_by.is_empty() {
            let keys: Vec<String> = q
                .order_by
                .iter()
                .map(|(e, desc)| {
                    format!("{}{}", if *desc { "desc " } else { "" }, self.expr_str(e))
                })
                .collect();
            let topk = q
                .limit
                .map(|l| format!(" (partial sort: top {})", q.offset + l))
                .unwrap_or_default();
            line(
                &mut out,
                depth,
                format!("OrderBy {}{topk}", keys.join(", ")),
            );
            depth += 1;
        }
        for h in &q.having {
            line(&mut out, depth, format!("Having {}", self.expr_str(h)));
        }
        if !q.aggregates.is_empty() || !q.group_by.is_empty() {
            let keys: Vec<String> = q.group_by.iter().map(|(e, _)| self.expr_str(e)).collect();
            let aggs: Vec<String> = q
                .aggregates
                .iter()
                .map(|a| {
                    format!(
                        "{}({}{})",
                        a.func,
                        if a.distinct { "distinct " } else { "" },
                        a.expr.as_ref().map_or("*".into(), |e| self.expr_str(e))
                    )
                })
                .collect();
            let fast = if matches!(&q.pattern, Pattern::Bgp(t) if t.len() == 1)
                && q.aggregates.iter().all(|a| a.func == "COUNT")
            {
                " (counted from the index scan)"
            } else {
                ""
            };
            line(
                &mut out,
                depth,
                format!("Group by [{}] {}{fast}", keys.join(", "), aggs.join(", ")),
            );
            depth += 1;
        }
        self.explain_pattern(&q.pattern, depth, &FastSet::default(), &mut out);
        if let Some(values) = &q.values {
            line(&mut out, 1, "Join trailing VALUES".into());
            self.explain_pattern(values, 2, &FastSet::default(), &mut out);
        }
        out
    }

    fn explain_pattern(&self, p: &Pattern, d: usize, bound: &FastSet<Var>, out: &mut String) {
        let pad = "  ".repeat(d);
        let mut bound = bound.clone();
        match p {
            Pattern::Bgp(tps) if tps.is_empty() => out.push_str(&format!("{pad}Empty\n")),
            Pattern::Bgp(tps) => {
                out.push_str(&format!(
                    "{pad}BGP ({} patterns, in evaluation order)\n",
                    tps.len()
                ));
                let mut rows: Option<u64> = None;
                for tp in self.order_bgp(tps, &bound) {
                    let est = self.estimate(tp);
                    let join = match rows {
                        Some(r) if r as usize >= HASH_JOIN_MIN && est <= r.saturating_mul(16) => {
                            "  hash join"
                        }
                        Some(_) => "  index lookup per row",
                        None => "  scan",
                    };
                    out.push_str(&format!(
                        "{pad}  {}   ~{}{join}\n",
                        self.tp_str(tp),
                        if est > 1_000_000 {
                            "1M+".to_string()
                        } else {
                            est.to_string()
                        }
                    ));
                    rows = Some(rows.map_or(est, |r| r.max(est)));
                    let mut vs = Vec::new();
                    tp.s.vars(&mut vs);
                    tp.o.vars(&mut vs);
                    if let Predicate::Var(x) = &tp.p {
                        vs.push(*x);
                    }
                    bound.extend(vs);
                }
            }
            Pattern::Join(a, b) => {
                out.push_str(&format!("{pad}Join\n"));
                self.explain_pattern(a, d + 1, &bound, out);
                self.explain_pattern(b, d + 1, &bound, out);
            }
            Pattern::LeftJoin(a, b, f) => {
                out.push_str(&format!(
                    "{pad}Optional{}\n",
                    f.as_ref()
                        .map_or(String::new(), |f| format!(" filter {}", self.expr_str(f)))
                ));
                self.explain_pattern(a, d + 1, &bound, out);
                self.explain_pattern(b, d + 1, &bound, out);
            }
            Pattern::Union(a, b) => {
                out.push_str(&format!("{pad}Union\n"));
                self.explain_pattern(a, d + 1, &bound, out);
                self.explain_pattern(b, d + 1, &bound, out);
            }
            Pattern::Minus(a, b) => {
                out.push_str(&format!("{pad}Minus\n"));
                self.explain_pattern(a, d + 1, &bound, out);
                self.explain_pattern(b, d + 1, &bound, out);
            }
            Pattern::Filter(e, inner) => {
                out.push_str(&format!("{pad}Filter {}\n", self.expr_str(e)));
                self.explain_pattern(inner, d + 1, &bound, out);
            }
            Pattern::Graph(g, inner) => {
                out.push_str(&format!("{pad}Graph {}\n", self.vt_str(g)));
                self.explain_pattern(inner, d + 1, &bound, out);
            }
            Pattern::Extend(inner, x, e) => {
                self.explain_pattern(inner, d, &bound, out);
                out.push_str(&format!(
                    "{pad}Bind {} AS ?{}\n",
                    self.expr_str(e),
                    self.vars.names.get(*x).map(String::as_str).unwrap_or("_")
                ));
            }
            Pattern::Values(vs, rows) => out.push_str(&format!(
                "{pad}Values ({} variables, {} rows)\n",
                vs.len(),
                rows.len()
            )),
            Pattern::SubSelect(q) => {
                out.push_str(&format!("{pad}Subquery\n"));
                for l in self.explain(q).lines() {
                    out.push_str(&format!("{pad}  {l}\n"));
                }
            }
            Pattern::Service(ep, inner, silent, _) => {
                out.push_str(&format!(
                    "{pad}Service{} {} (bind join)\n",
                    if *silent { " silent" } else { "" },
                    self.vt_str(ep)
                ));
                self.explain_pattern(inner, d + 1, &bound, out);
            }
        }
    }

    /// Matches of a triple pattern counting its constants only (capped).
    fn estimate(&self, tp: &TriplePattern) -> u64 {
        let c = |vt: &VarOrTerm| match vt {
            VarOrTerm::Term(t) => Some(self.lookup(t)),
            _ => None,
        };
        let (s, o) = (c(&tp.s), c(&tp.o));
        let p = match &tp.p {
            Predicate::Path(Path::Iri(i)) => Some(self.lookup_iri(i)),
            Predicate::Var(_) => None,
            Predicate::Path(_) => return 0,
        };
        if [s, p, o].iter().any(|x| matches!(x, Some(None))) {
            return 0;
        }
        self.scan(s.flatten(), p.flatten(), o.flatten(), None)
            .take(1_000_001)
            .count() as u64
    }

    fn vt_str(&self, vt: &VarOrTerm) -> String {
        match vt {
            VarOrTerm::Var(x) => format!(
                "?{}",
                self.vars.names.get(*x).map(String::as_str).unwrap_or("_")
            ),
            VarOrTerm::Term(t) => t.to_string(),
            VarOrTerm::Triple(t) => format!(
                "<< {} {} {} >>",
                self.vt_str(&t[0]),
                self.vt_str(&t[1]),
                self.vt_str(&t[2])
            ),
        }
    }

    fn path_str(p: &Path) -> String {
        match p {
            Path::Iri(i) => format!("<{i}>"),
            Path::Inverse(x) => format!("^{}", Self::path_str(x)),
            Path::Seq(xs) => xs.iter().map(Self::path_str).collect::<Vec<_>>().join("/"),
            Path::Alt(xs) => format!(
                "({})",
                xs.iter().map(Self::path_str).collect::<Vec<_>>().join("|")
            ),
            Path::ZeroOrMore(x) => format!("{}*", Self::path_str(x)),
            Path::OneOrMore(x) => format!("{}+", Self::path_str(x)),
            Path::ZeroOrOne(x) => format!("{}?", Self::path_str(x)),
            Path::Negated(xs) => format!(
                "!({})",
                xs.iter()
                    .map(|(i, inv)| format!("{}<{i}>", if *inv { "^" } else { "" }))
                    .collect::<Vec<_>>()
                    .join("|")
            ),
        }
    }

    fn tp_str(&self, tp: &TriplePattern) -> String {
        let p = match &tp.p {
            Predicate::Var(x) => format!(
                "?{}",
                self.vars.names.get(*x).map(String::as_str).unwrap_or("_")
            ),
            Predicate::Path(p) => Self::path_str(p),
        };
        format!("{} {} {}", self.vt_str(&tp.s), p, self.vt_str(&tp.o))
    }

    fn expr_str(&self, e: &Expr) -> String {
        let s = |e: &Expr| self.expr_str(e);
        match e {
            Expr::Var(x) => format!(
                "?{}",
                self.vars.names.get(*x).map(String::as_str).unwrap_or("_")
            ),
            Expr::Const(t) => t.to_string(),
            Expr::Or(a, b) => format!("({} || {})", s(a), s(b)),
            Expr::And(a, b) => format!("({} && {})", s(a), s(b)),
            Expr::Not(a) => format!("!{}", s(a)),
            Expr::Cmp(op, a, b) => format!(
                "({} {} {})",
                s(a),
                match op {
                    CmpOp::Eq => "=",
                    CmpOp::Ne => "!=",
                    CmpOp::Lt => "<",
                    CmpOp::Le => "<=",
                    CmpOp::Gt => ">",
                    CmpOp::Ge => ">=",
                },
                s(b)
            ),
            Expr::In(a, list, neg) => format!(
                "({} {}IN ({}))",
                s(a),
                if *neg { "NOT " } else { "" },
                list.iter().map(s).collect::<Vec<_>>().join(", ")
            ),
            Expr::Arith(op, a, b) => format!("({} {op} {})", s(a), s(b)),
            Expr::Neg(a) => format!("-{}", s(a)),
            Expr::Bound(x) => format!(
                "BOUND(?{})",
                self.vars.names.get(*x).map(String::as_str).unwrap_or("_")
            ),
            Expr::Exists(_, neg) => format!("{}EXISTS {{...}}", if *neg { "NOT " } else { "" }),
            Expr::Call(name, args) => format!(
                "{}({})",
                name.rsplit(['/', '#']).next().unwrap_or(name),
                args.iter().map(s).collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

/// The one variable a filter expression reads, when its value depends on nothing else: no
/// EXISTS, and no function that answers differently each time (RAND, NOW, UUID, BNODE...).
pub(crate) fn single_var(e: &Expr) -> Option<Var> {
    fn walk(e: &Expr, var: &mut Option<Var>, ok: &mut bool) {
        match e {
            Expr::Var(v) | Expr::Bound(v) => match var {
                Some(x) if x != v => *ok = false,
                _ => *var = Some(*v),
            },
            Expr::Exists(..) => *ok = false,
            Expr::Call(name, _)
                if matches!(name.as_str(), "RAND" | "NOW" | "UUID" | "STRUUID" | "BNODE") =>
            {
                *ok = false
            }
            _ => e.for_each_child(&mut |c| walk(c, var, ok)),
        }
    }
    let (mut var, mut ok) = (None, true);
    walk(e, &mut var, &mut ok);
    var.filter(|_| ok)
}

/// A SERVICE answer: its variables and rows.
pub type ServiceRows = (Vec<String>, Vec<Vec<Option<Term>>>);

/// Seconds a SERVICE answer is reused by later queries (`serve --service-cache`; 0 = never).
pub static SERVICE_CACHE_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Time limit of one SERVICE call (`serve --service-timeout`; 0 = 300 s), within the query's own.
pub static SERVICE_TIMEOUT_SECS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Further attempts after a SERVICE call fails to connect or gets a 5xx answer
/// (`serve --service-retries`).
pub static SERVICE_RETRIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

type CacheEntry = (std::time::Instant, std::sync::Arc<ServiceRows>);
static SERVICE_CACHE: std::sync::Mutex<Option<HashMap<(String, String), CacheEntry>>> =
    std::sync::Mutex::new(None);

/// One SERVICE call (through the answer cache when it is on).
fn fetch_service(
    endpoint: &str,
    text: &str,
    timeout: std::time::Duration,
) -> R<std::sync::Arc<ServiceRows>> {
    use std::sync::atomic::Ordering::Relaxed;
    let ttl = SERVICE_CACHE_SECS.load(Relaxed);
    let key = (endpoint.to_string(), text.to_string());
    if ttl > 0 {
        let cache = SERVICE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, rows)) = cache.as_ref().and_then(|c| c.get(&key)) {
            if at.elapsed().as_secs() < ttl {
                return Ok(rows.clone());
            }
        }
    }
    let accept = "application/sparql-results+json";
    let enc = crate::http::percent_encode(text);
    let started = std::time::Instant::now();
    let mut attempt = 0;
    let body = loop {
        let left = timeout.saturating_sub(started.elapsed());
        let r = if enc.len() < 1500 {
            let sep = if endpoint.contains('?') { '&' } else { '?' };
            crate::http::fetch_with(
                &format!("{endpoint}{sep}query={enc}"),
                "GET",
                accept,
                None,
                left,
            )
        } else {
            let form = format!("query={enc}");
            crate::http::fetch_with(
                endpoint,
                "POST",
                accept,
                Some(("application/x-www-form-urlencoded", form.as_bytes())),
                left,
            )
        };
        match r {
            Ok((_, body)) => break body,
            Err(e) => {
                // the error starts with the request URL, which repeats the whole query
                let msg = e
                    .rsplit_once(": ")
                    .map_or(e.as_str(), |(_, m)| m)
                    .to_string();
                // a 4xx answer will not change; a refused connection or a 5xx may
                let transient = !e.contains(": HTTP 4");
                if attempt < SERVICE_RETRIES.load(Relaxed)
                    && transient
                    && started.elapsed() < timeout
                {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100 << attempt.min(5)));
                    continue;
                }
                return Err(format!("SERVICE <{endpoint}>: {msg}"));
            }
        }
    };
    let parsed = super::parse_results_json(&String::from_utf8_lossy(&body))
        .map_err(|e| format!("SERVICE <{endpoint}>: {e}"))?;
    let rows = std::sync::Arc::new(
        parsed.map_err(|_| format!("SERVICE <{endpoint}> answered a boolean"))?,
    );
    if ttl > 0 {
        let mut cache = SERVICE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let c = cache.get_or_insert_with(HashMap::new);
        c.retain(|_, (at, _)| at.elapsed().as_secs() < ttl);
        c.insert(key, (std::time::Instant::now(), rows.clone()));
    }
    Ok(rows)
}
