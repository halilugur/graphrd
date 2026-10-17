//! `graphrd compare`: a portable benchmark for any SPARQL 1.1 store, so GraphRD can be measured
//! side by side with other databases on the same machine and the same data.
//!
//! Only standard protocols are used: SPARQL 1.1 Query and Update over HTTP (form-encoded POST)
//! and an N-Triples POST for bulk loading (Graph Store protocol or the store's equivalent). The
//! query suite is plain SPARQL 1.1 with no inference or extensions, and every query's row
//! count is recorded so the report can flag stores that disagree.
//!
//!   graphrd compare --name X --query URL --update URL --load URL [--load-type T] [--graph IRI]
//!                   [--persons 100000] [--clients 8] [--seconds 10] [--out X.json]
//!   graphrd compare-report a.json b.json ...   (markdown tables, one column per store)

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bench::{EX, generate};
use crate::http::percent_encode;
use crate::json::Json;

const TIMEOUT: Duration = Duration::from_secs(300);

/// A keep-alive HTTP/1.1 client for one `http://host:port`, with chunked response decoding.
struct Client {
    host: String,
    stream: Option<BufReader<TcpStream>>,
}

struct Url {
    host: String,
    path: String,
}

fn split_url(url: &str) -> Result<Url, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// URLs are supported: {url}"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:80")
    };
    Ok(Url {
        host,
        path: path.to_string(),
    })
}

impl Client {
    fn new(host: &str) -> Client {
        Client {
            host: host.to_string(),
            stream: None,
        }
    }

    fn request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        for attempt in 0..2 {
            if self.stream.is_none() {
                let s = TcpStream::connect(&self.host)
                    .map_err(|e| format!("connect {}: {e}", self.host))?;
                s.set_nodelay(true).ok();
                s.set_read_timeout(Some(TIMEOUT)).ok();
                self.stream = Some(BufReader::with_capacity(1 << 16, s));
            }
            match self.try_request(method, path, headers, body) {
                Ok((status, body, close)) => {
                    if close {
                        self.stream = None;
                    }
                    return Ok((status, body));
                }
                Err(e) => {
                    self.stream = None;
                    // a timeout is an answer, not a reason to resend
                    if attempt == 1
                        || e.contains("timed out")
                        || e.contains("WouldBlock")
                        || e.contains("Resource temporarily")
                    {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!()
    }

    fn try_request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<(u16, Vec<u8>, bool), String> {
        let r = self.stream.as_mut().unwrap();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\n",
            self.host,
            body.len()
        );
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let w = r.get_mut();
        w.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
        w.write_all(body).map_err(|e| e.to_string())?;
        let mut line = String::new();
        r.read_line(&mut line).map_err(|e| e.to_string())?;
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("bad status line {line:?}"))?;
        let (mut len, mut chunked, mut close) = (None, false, line.starts_with("HTTP/1.0"));
        loop {
            let mut h = String::new();
            r.read_line(&mut h).map_err(|e| e.to_string())?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                let v = v.trim();
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.parse().ok();
                } else if k.eq_ignore_ascii_case("transfer-encoding")
                    && v.eq_ignore_ascii_case("chunked")
                {
                    chunked = true;
                } else if k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close") {
                    close = true;
                }
            }
        }
        let mut buf = Vec::new();
        if chunked {
            loop {
                let mut size = String::new();
                r.read_line(&mut size).map_err(|e| e.to_string())?;
                let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or(""), 16)
                    .map_err(|_| format!("bad chunk size {size:?}"))?;
                if n == 0 {
                    // trailers, then the empty line
                    loop {
                        let mut t = String::new();
                        r.read_line(&mut t).map_err(|e| e.to_string())?;
                        if t.trim().is_empty() {
                            break;
                        }
                    }
                    break;
                }
                let start = buf.len();
                buf.resize(start + n, 0);
                r.read_exact(&mut buf[start..]).map_err(|e| e.to_string())?;
                let mut crlf = [0u8; 2];
                r.read_exact(&mut crlf).map_err(|e| e.to_string())?;
            }
        } else if let Some(n) = len {
            buf.resize(n, 0);
            r.read_exact(&mut buf).map_err(|e| e.to_string())?;
        } else if status != 204 && status != 304 && !(100..200).contains(&status) {
            r.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            close = true;
        }
        Ok((status, buf, close))
    }
}

/// The store under test: endpoints and how it wants data.
#[derive(Clone)]
struct Target {
    query: Url,
    update: Url,
    load: Url,
    load_type: String,
    /// Load and update into this named graph instead of the default graph (for stores whose
    /// default graph is the union of all graphs and which refuse default-graph writes).
    graph: Option<String>,
}

impl Clone for Url {
    fn clone(&self) -> Url {
        Url {
            host: self.host.clone(),
            path: self.path.clone(),
        }
    }
}

enum Answer {
    Rows(usize),
    Bool(bool),
}

impl Answer {
    fn count(&self) -> usize {
        match self {
            Answer::Rows(n) => *n,
            Answer::Bool(b) => *b as usize,
        }
    }
}

struct Session {
    t: Target,
    q: Client,
    u: Client,
}

impl Session {
    fn new(t: &Target) -> Session {
        Session {
            q: Client::new(&t.query.host),
            u: Client::new(&t.update.host),
            t: t.clone(),
        }
    }

    fn query(&mut self, kind: Kind, q: &str) -> Result<Answer, String> {
        let body = format!("query={}", percent_encode(q));
        let accept = match kind {
            Kind::Graph => "application/n-triples",
            _ => "application/sparql-results+json",
        };
        let (st, b) = self.q.request(
            "POST",
            &self.t.query.path,
            &[
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Accept", accept),
            ],
            body.as_bytes(),
        )?;
        if st != 200 {
            return Err(format!(
                "HTTP {st}: {}",
                String::from_utf8_lossy(&b[..b.len().min(300)])
            ));
        }
        let text = String::from_utf8_lossy(&b);
        match kind {
            Kind::Graph => Ok(Answer::Rows(
                text.lines()
                    .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
                    .count(),
            )),
            _ => {
                let j = Json::parse(&text).map_err(|e| format!("bad JSON results: {e}"))?;
                if let Some(Json::Bool(b)) = j.get("boolean") {
                    return Ok(Answer::Bool(*b));
                }
                let rows = j
                    .get("results")
                    .and_then(|r| r.get("bindings"))
                    .and_then(Json::as_array)
                    .ok_or("no bindings in results")?;
                Ok(Answer::Rows(rows.len()))
            }
        }
    }

    /// The value of `COUNT(*)` over the default graph (or the target graph).
    fn count_triples(&mut self) -> Result<f64, String> {
        let q = match &self.t.graph {
            Some(g) => format!("SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{g}> {{ ?s ?p ?o }} }}"),
            None => "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }".to_string(),
        };
        let body = format!("query={}", percent_encode(&q));
        let (st, b) = self.q.request(
            "POST",
            &self.t.query.path,
            &[
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Accept", "application/sparql-results+json"),
            ],
            body.as_bytes(),
        )?;
        if st != 200 {
            return Err(format!("HTTP {st}"));
        }
        let j = Json::parse(&String::from_utf8_lossy(&b))?;
        let row = j
            .get("results")
            .and_then(|r| r.get("bindings"))
            .and_then(Json::as_array)
            .and_then(|a| a.first().cloned())
            .ok_or("no count")?;
        row.get("n")
            .and_then(|n| n.get("value"))
            .and_then(Json::as_str)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| "no count".to_string())
    }

    fn update(&mut self, u: &str) -> Result<(), String> {
        let body = format!("update={}", percent_encode(u));
        let (st, b) = self.u.request(
            "POST",
            &self.t.update.path,
            &[
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Accept", "*/*"),
            ],
            body.as_bytes(),
        )?;
        if !(200..300).contains(&st) {
            return Err(format!(
                "HTTP {st}: {}",
                String::from_utf8_lossy(&b[..b.len().min(300)])
            ));
        }
        Ok(())
    }

    /// `INSERT DATA` / `DELETE DATA` body, in the target graph if there is one.
    fn data(&self, op: &str, triples: &str) -> String {
        match &self.t.graph {
            Some(g) => format!("{op} DATA {{ GRAPH <{g}> {{ {triples} }} }}"),
            None => format!("{op} DATA {{ {triples} }}"),
        }
    }

    fn with_graph(&self, u: &str) -> String {
        match &self.t.graph {
            Some(g) => format!("WITH <{g}> {u}"),
            None => u.to_string(),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Select,
    Ask,
    Graph,
}

/// The query suite: plain SPARQL 1.1 over the generated graph (no inference, no extensions).
fn suite(persons: usize) -> Vec<(&'static str, &'static str, Kind, String)> {
    let p = persons / 2 + 7;
    let x = |s: &str| s.replace("ex:", EX).replace("@P", &format!("{EX}p{p}"));
    use Kind::*;
    vec![
        (
            "Q1",
            "Point lookup: all properties of one person",
            Select,
            x("SELECT ?p ?o WHERE { <@P> ?p ?o }"),
        ),
        (
            "Q2",
            "Two hops: names of the people a person knows",
            Select,
            x("SELECT ?n WHERE { <@P> <ex:knows> ?f . ?f <ex:name> ?n }"),
        ),
        (
            "Q3",
            "Star join: one person's name, age and employer",
            Select,
            x("SELECT ?n ?a ?o WHERE { <@P> <ex:name> ?n ; <ex:age> ?a ; <ex:worksFor> ?o }"),
        ),
        (
            "Q4",
            "Count all triples",
            Select,
            "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }".into(),
        ),
        (
            "Q5",
            "Count instances of one class",
            Select,
            x("SELECT (COUNT(?s) AS ?n) WHERE { ?s a <ex:C7> }"),
        ),
        (
            "Q6",
            "Range filter, ORDER BY, LIMIT",
            Select,
            x(
                "SELECT ?s ?a WHERE { ?s a <ex:C7> ; <ex:age> ?a FILTER(?a > 95) } ORDER BY DESC(?a) ?s LIMIT 10",
            ),
        ),
        (
            "Q7",
            "GROUP BY: people per organisation, top 5",
            Select,
            x(
                "SELECT ?o (COUNT(?s) AS ?n) WHERE { ?s <ex:worksFor> ?o } GROUP BY ?o ORDER BY DESC(?n) ?o LIMIT 5",
            ),
        ),
        (
            "Q8",
            "GROUP BY with AVG over all people",
            Select,
            x("SELECT ?c (AVG(?a) AS ?avg) WHERE { ?s a ?c ; <ex:age> ?a } GROUP BY ?c"),
        ),
        (
            "Q9",
            "COUNT DISTINCT",
            Select,
            x("SELECT (COUNT(DISTINCT ?o) AS ?n) WHERE { ?s <ex:worksFor> ?o }"),
        ),
        (
            "Q10",
            "Join: colleagues of a person",
            Select,
            x("SELECT ?c WHERE { <@P> <ex:worksFor> ?o . ?c <ex:worksFor> ?o }"),
        ),
        (
            "Q11",
            "Property path: knows/knows?/knows?",
            Select,
            x(
                "SELECT (COUNT(DISTINCT ?x) AS ?n) WHERE { <@P> <ex:knows>/<ex:knows>?/<ex:knows>? ?x }",
            ),
        ),
        (
            "Q12",
            "Property path: transitive knows+",
            Select,
            x("SELECT (COUNT(DISTINCT ?x) AS ?n) WHERE { <@P> <ex:knows>+ ?x }"),
        ),
        (
            "Q13",
            "Inverse path: who knows a person",
            Select,
            x("SELECT ?x WHERE { <@P> ^<ex:knows> ?x }"),
        ),
        (
            "Q14",
            "OPTIONAL over one class",
            Select,
            x("SELECT ?s ?f WHERE { ?s a <ex:C3> OPTIONAL { ?s <ex:knows> ?f . ?f a <ex:C3> } }"),
        ),
        (
            "Q15",
            "UNION of two classes",
            Select,
            x("SELECT (COUNT(*) AS ?n) WHERE { { ?s a <ex:C1> } UNION { ?s a <ex:C2> } }"),
        ),
        (
            "Q16",
            "FILTER NOT EXISTS",
            Select,
            x(
                "SELECT (COUNT(?s) AS ?n) WHERE { ?s a <ex:C4> FILTER NOT EXISTS { ?s <ex:age> ?a FILTER(?a < 50) } }",
            ),
        ),
        (
            "Q17",
            "MINUS",
            Select,
            x(
                "SELECT (COUNT(?s) AS ?n) WHERE { ?s a <ex:C5> MINUS { ?s <ex:worksFor> <ex:org1> } }",
            ),
        ),
        (
            "Q18",
            "String filter: CONTAINS over all bios",
            Select,
            x("SELECT (COUNT(?s) AS ?n) WHERE { ?s <ex:bio> ?b FILTER(CONTAINS(?b, \"chess\")) }"),
        ),
        (
            "Q19",
            "Regex filter over all names",
            Select,
            x("SELECT ?s WHERE { ?s <ex:name> ?n FILTER(REGEX(?n, \"^Person 42[0-9]$\")) }"),
        ),
        (
            "Q20",
            "Deep pagination: ORDER BY name OFFSET 50%",
            Select,
            format!(
                "SELECT ?s ?n WHERE {{ ?s <{EX}name> ?n }} ORDER BY ?n OFFSET {} LIMIT 10",
                persons / 2
            ),
        ),
        (
            "Q21",
            "Subquery: members of the 3 largest organisations",
            Select,
            x(
                "SELECT (COUNT(?s) AS ?n) WHERE { { SELECT ?o WHERE { ?x <ex:worksFor> ?o } GROUP BY ?o ORDER BY DESC(COUNT(?x)) ?o LIMIT 3 } ?s <ex:worksFor> ?o }",
            ),
        ),
        (
            "Q22",
            "VALUES join",
            Select,
            x(
                "SELECT ?s ?n WHERE { VALUES ?s { <ex:p1> <ex:p10> <ex:p100> <ex:p1000> <ex:p10000> } ?s <ex:name> ?n }",
            ),
        ),
        (
            "Q23",
            "BIND and string functions",
            Select,
            x(
                "SELECT ?s ?u WHERE { ?s a <ex:C9> ; <ex:name> ?n BIND(UCASE(SUBSTR(?n, 8)) AS ?u) } LIMIT 1000",
            ),
        ),
        (
            "Q24",
            "ASK",
            Ask,
            x("ASK { <@P> <ex:knows> ?f . ?f <ex:worksFor> ?o }"),
        ),
        (
            "Q25",
            "CONSTRUCT: a person's neighbourhood",
            Graph,
            x("CONSTRUCT { <@P> ?p ?o . ?o ?p2 ?o2 } WHERE { <@P> ?p ?o OPTIONAL { ?o ?p2 ?o2 } }"),
        ),
        (
            "Q26",
            "Large result: every name",
            Select,
            x("SELECT ?s ?n WHERE { ?s <ex:name> ?n }"),
        ),
    ]
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn stats(mut v: Vec<f64>) -> Json {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    };
    Json::obj()
        .with("n", v.len() as f64)
        .with("p50", percentile(&v, 0.5))
        .with("p95", percentile(&v, 0.95))
        .with("p99", percentile(&v, 0.99))
        .with("mean", mean)
}

struct Opts {
    name: String,
    target: Target,
    persons: usize,
    clients: usize,
    seconds: u64,
    out: Option<String>,
    load: bool,
}

/// The `graphrd compare` command: one query or update against a server, with timing.
pub fn run(args: &[String]) -> Result<(), String> {
    let (mut name, mut query, mut update, mut load, mut load_type, mut graph) = (
        None,
        None,
        None,
        None,
        "application/n-triples".to_string(),
        None,
    );
    let (mut persons, mut clients, mut seconds, mut out, mut do_load) =
        (100_000usize, 8usize, 10u64, None, true);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut v = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--name" => name = Some(v()?),
            "--query" => query = Some(v()?),
            "--update" => update = Some(v()?),
            "--load" => load = Some(v()?),
            "--load-type" => load_type = v()?,
            "--graph" => graph = Some(v()?),
            "--persons" => persons = v()?.parse().map_err(|_| "bad --persons")?,
            "--clients" => clients = v()?.parse().map_err(|_| "bad --clients")?,
            "--seconds" => seconds = v()?.parse().map_err(|_| "bad --seconds")?,
            "--out" => out = Some(v()?),
            "--no-load" => do_load = false,
            other => return Err(format!("unknown option {other}")),
        }
    }
    let query = query.ok_or("--query URL is required")?;
    let o = Opts {
        name: name.unwrap_or_else(|| query.clone()),
        target: Target {
            update: split_url(update.as_deref().unwrap_or(&query))?,
            load: split_url(load.as_deref().ok_or("--load URL is required")?)?,
            query: split_url(&query)?,
            load_type,
            graph,
        },
        persons,
        clients,
        seconds,
        out,
        load: do_load,
    };
    let result = measure(&o)?;
    let text = result.to_string();
    match &o.out {
        Some(path) => std::fs::write(path, &text).map_err(|e| format!("{path}: {e}"))?,
        None => println!("{text}"),
    }
    Ok(())
}

fn log(s: &str) {
    eprintln!("[compare] {s}");
}

fn measure(o: &Opts) -> Result<Json, String> {
    let mut s = Session::new(&o.target);
    let mut result = Json::obj()
        .with("name", o.name.as_str())
        .with("persons", o.persons as f64)
        .with("clients", o.clients as f64)
        .with("seconds", o.seconds as f64);

    // ---- bulk load
    if o.load {
        let chunk = 25_000;
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut loader = Client::new(&o.target.load.host);
        let mut lat = Vec::new();
        let started = Instant::now();
        let mut from = 0;
        let mut triples = 0usize;
        while from < o.persons {
            let to = (from + chunk).min(o.persons);
            let data = generate(from, to, o.persons, &mut seed);
            triples += (to - from) * 6;
            let t = Instant::now();
            let (st, b) = loader.request(
                "POST",
                &o.target.load.path,
                &[("Content-Type", &o.target.load_type), ("Accept", "*/*")],
                data.as_bytes(),
            )?;
            if !(200..300).contains(&st) {
                return Err(format!(
                    "load: HTTP {st}: {}",
                    String::from_utf8_lossy(&b[..b.len().min(300)])
                ));
            }
            lat.push(t.elapsed().as_secs_f64());
            from = to;
            log(&format!("{}: loaded {from}/{} persons", o.name, o.persons));
        }
        let secs = started.elapsed().as_secs_f64();
        result.set(
            "load",
            Json::obj()
                .with("triples", triples as f64)
                .with("seconds", secs)
                .with("triples_per_second", triples as f64 / secs),
        );
    }
    let count = s.count_triples();
    log(&format!("{}: {count:?} triples stored", o.name));

    // ---- single-client query latency
    let mut qs = Vec::new();
    for (id, title, kind, q) in suite(o.persons) {
        let mut entry = Json::obj().with("id", id).with("title", title);
        let mut times = Vec::new();
        let mut rows = None;
        let mut error = None;
        let budget = Instant::now();
        // two warm-up runs, then up to 20 timed runs within a 30 s budget
        for run in 0..22 {
            let t = Instant::now();
            match s.query(kind, &q) {
                Ok(a) => {
                    if run >= 2 {
                        times.push(t.elapsed().as_secs_f64() * 1000.0);
                    }
                    rows = Some(a.count());
                }
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
            if budget.elapsed() > Duration::from_secs(30) && !times.is_empty() {
                break;
            }
        }
        if let Some(n) = rows {
            entry.set("rows", n as f64);
        }
        match error {
            Some(e) => {
                log(&format!("{}: {id} failed: {e}", o.name));
                entry.set("error", e.as_str());
            }
            None => entry.set("latency_ms", stats(times)),
        }
        qs.push(entry);
    }
    result.set("queries", Json::Arr(qs));

    // ---- writes, one client
    let mut ws = Vec::new();
    type Op = Box<dyn Fn(&Session, usize) -> String>;
    let ops: Vec<(&str, &str, Op)> = vec![
        (
            "W1",
            "INSERT DATA: one new person (6 triples)",
            Box::new(|s, i| {
                s.data(
                    "INSERT",
                    &generate(10_000_000 + i, 10_000_001 + i, 1000, &mut (i as u64 + 1))
                        .replace('\n', " "),
                )
            }),
        ),
        (
            "W2",
            "DELETE DATA: one existing triple",
            Box::new(|s, i| s.data("DELETE", &format!("<{EX}p{i}> <{EX}name> \"Person {i}\""))),
        ),
        (
            "W3",
            "DELETE/INSERT WHERE: change an age",
            Box::new(|s, i| {
                s.with_graph(&format!("DELETE {{ <{EX}p{n}> <{EX}age> ?a }} INSERT {{ <{EX}p{n}> <{EX}age> 42 }} WHERE {{ <{EX}p{n}> <{EX}age> ?a }}", n = i + 1000))
            }),
        ),
        (
            "W4",
            "INSERT DATA: batch of 100 persons (600 triples)",
            Box::new(|s, i| {
                s.data(
                    "INSERT",
                    &generate(
                        11_000_000 + i * 100,
                        11_000_100 + i * 100,
                        1000,
                        &mut (i as u64 + 3),
                    )
                    .replace('\n', " "),
                )
            }),
        ),
    ];
    for (id, title, make) in &ops {
        let mut times = Vec::new();
        let mut error = None;
        let n = if *id == "W4" { 50 } else { 200 };
        for i in 0..n {
            let u = make(&s, i);
            let t = Instant::now();
            if let Err(e) = s.update(&u) {
                error = Some(e);
                break;
            }
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let mut entry = Json::obj().with("id", *id).with("title", *title);
        match error {
            Some(e) => {
                log(&format!("{}: {id} failed: {e}", o.name));
                entry.set("error", e.as_str());
            }
            None => entry.set("latency_ms", stats(times)),
        }
        ws.push(entry);
    }
    result.set("writes", Json::Arr(ws));

    // ---- concurrent reads, then reads alongside a writer
    log(&format!("{}: concurrent reads", o.name));
    result.set("concurrent", concurrent(o, false));
    log(&format!("{}: concurrent reads with a writer", o.name));
    result.set("mixed", concurrent(o, true));
    if let Ok(n) = count {
        result.set("count_after_load", n);
    }
    Ok(result)
}

/// The short interactive queries (Q1-Q3, Q6, Q10, Q13, Q22, Q24) round-robin from `clients`
/// threads for `seconds`; optionally one more thread inserts a person per update meanwhile.
fn concurrent(o: &Opts, with_writer: bool) -> Json {
    let stop = Arc::new(AtomicBool::new(false));
    let errors = Arc::new(AtomicU64::new(0));
    let writes = Arc::new(AtomicU64::new(0));
    let lat = Arc::new(Mutex::new(Vec::<f64>::new()));
    let interactive = ["Q1", "Q2", "Q3", "Q6", "Q10", "Q13", "Q22", "Q24"];
    let mix: Arc<Vec<(Kind, String)>> = Arc::new(
        suite(o.persons)
            .into_iter()
            .filter(|q| interactive.contains(&q.0))
            .map(|q| (q.2, q.3))
            .collect(),
    );
    let mut hs = Vec::new();
    for w in 0..o.clients {
        let (stop, errors, lat, mix, t) = (
            stop.clone(),
            errors.clone(),
            lat.clone(),
            mix.clone(),
            o.target.clone(),
        );
        hs.push(std::thread::spawn(move || {
            let mut s = Session::new(&t);
            let mut mine = Vec::new();
            let mut i = w;
            while !stop.load(Ordering::Relaxed) {
                let (kind, q) = &mix[i % mix.len()];
                i += 1;
                let t = Instant::now();
                match s.query(*kind, q) {
                    Ok(_) => mine.push(t.elapsed().as_secs_f64() * 1000.0),
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
            lat.lock().unwrap().extend(mine);
        }));
    }
    if with_writer {
        let (stop, writes, t) = (stop.clone(), writes.clone(), o.target.clone());
        hs.push(std::thread::spawn(move || {
            let mut s = Session::new(&t);
            let mut i = 0;
            while !stop.load(Ordering::Relaxed) {
                let u = s.data(
                    "INSERT",
                    &generate(20_000_000 + i, 20_000_001 + i, 1000, &mut (i as u64 + 7))
                        .replace('\n', " "),
                );
                if s.update(&u).is_ok() {
                    writes.fetch_add(1, Ordering::Relaxed);
                }
                i += 1;
            }
        }));
    }
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(o.seconds));
    stop.store(true, Ordering::Relaxed);
    for h in hs {
        let _ = h.join();
    }
    let secs = started.elapsed().as_secs_f64();
    let l = lat.lock().unwrap().clone();
    let n = l.len();
    let mut j = Json::obj()
        .with("queries", n as f64)
        .with("qps", n as f64 / secs)
        .with("latency_ms", stats(l))
        .with("errors", errors.load(Ordering::Relaxed) as f64);
    if with_writer {
        j.set("writes", writes.load(Ordering::Relaxed) as f64);
        j.set(
            "writes_per_second",
            writes.load(Ordering::Relaxed) as f64 / secs,
        );
    }
    j
}

// ------------------------------------------------------------------ report

fn num(j: &Json, path: &[&str]) -> Option<f64> {
    let mut cur = j;
    for k in path {
        cur = cur.get(k)?;
    }
    match cur {
        Json::Num(n) => Some(*n),
        _ => None,
    }
}

fn fmt_ms(v: f64) -> String {
    if v < 1.0 {
        format!("{v:.2}")
    } else if v < 100.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.0}")
    }
}

fn fmt_int(v: f64) -> String {
    let s = format!("{:.0}", v);
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Markdown tables comparing result files; the best value in each row is bold, and a row
/// count that differs from the most common one is marked.
pub fn report(files: &[String]) -> Result<(), String> {
    let mut rs = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(f).map_err(|e| format!("{f}: {e}"))?;
        rs.push(Json::parse(&text).map_err(|e| format!("{f}: {e}"))?);
    }
    let names: Vec<String> = rs.iter().map(|r| r.str_at(&["name"]).to_string()).collect();
    let header = |first: &str| {
        format!(
            "| {first} | {} |\n|---|{}\n",
            names.join(" | "),
            "---:|".repeat(names.len())
        )
    };
    let bold_best =
        |vals: &[Option<f64>], lower: bool, fmt: &dyn Fn(f64) -> String| -> Vec<String> {
            let best = vals
                .iter()
                .flatten()
                .copied()
                .fold(None, |b: Option<f64>, v| {
                    Some(match b {
                        None => v,
                        Some(b) => {
                            if lower {
                                b.min(v)
                            } else {
                                b.max(v)
                            }
                        }
                    })
                });
            vals.iter()
                .map(|v| match v {
                    Some(v) if Some(*v) == best && vals.iter().flatten().count() > 1 => {
                        format!("**{}**", fmt(*v))
                    }
                    Some(v) => fmt(*v),
                    None => "–".into(),
                })
                .collect()
        };
    let mut out = String::new();
    if let Some(r) = rs.first() {
        out.push_str(&format!(
            "{} persons (~{} triples), {} concurrent clients for {} s.\n\n",
            fmt_int(num(r, &["persons"]).unwrap_or(0.0)),
            fmt_int(num(r, &["persons"]).unwrap_or(0.0) * 6.0),
            num(r, &["clients"]).unwrap_or(0.0),
            num(r, &["seconds"]).unwrap_or(0.0)
        ));
    }
    out.push_str("### Bulk load and footprint\n\n");
    out.push_str(&header("Metric"));
    let tps: Vec<Option<f64>> = rs
        .iter()
        .map(|r| num(r, &["load", "triples_per_second"]))
        .collect();
    out.push_str(&format!(
        "| Triples per second | {} |\n",
        bold_best(&tps, false, &fmt_int).join(" | ")
    ));
    let secs: Vec<Option<f64>> = rs.iter().map(|r| num(r, &["load", "seconds"])).collect();
    out.push_str(&format!(
        "| Load time (s) | {} |\n",
        bold_best(&secs, true, &|v| format!("{v:.1}")).join(" | ")
    ));
    let stored: Vec<String> = rs
        .iter()
        .map(|r| {
            num(r, &["count_after_load"])
                .map(fmt_int)
                .unwrap_or("–".into())
        })
        .collect();
    out.push_str(&format!(
        "| Triples stored after load | {} |\n",
        stored.join(" | ")
    ));

    for (label, key) in [
        ("Peak memory (MB)", "memory_peak_mb"),
        ("Data on disk (MB)", "disk_mb"),
    ] {
        let vals: Vec<Option<f64>> = rs.iter().map(|r| num(r, &[key])).collect();
        if vals.iter().any(Option::is_some) {
            out.push_str(&format!(
                "| {label} | {} |\n",
                bold_best(&vals, true, &fmt_int).join(" | ")
            ));
        }
    }

    out.push_str("\n### Query latency, one client (median ms; lower is better)\n\n");
    out.push_str(&header("Query"));
    let queries = rs[0]
        .get("queries")
        .and_then(Json::as_array)
        .map(|a| a.to_vec())
        .unwrap_or_default();
    let mut mismatches = Vec::new();
    for (i, q) in queries.iter().enumerate() {
        let id = q.str_at(&["id"]);
        let entries: Vec<Option<&Json>> = rs
            .iter()
            .map(|r| {
                r.get("queries")
                    .and_then(Json::as_array)
                    .and_then(|a| a.get(i))
            })
            .collect();
        let vals: Vec<Option<f64>> = entries
            .iter()
            .map(|e| e.and_then(|e| num(e, &["latency_ms", "p50"])))
            .collect();
        let mut cells = bold_best(&vals, true, &fmt_ms);
        // row counts: the most common one is taken as the expected answer
        let rows: Vec<Option<f64>> = entries
            .iter()
            .map(|e| e.and_then(|e| num(e, &["rows"])))
            .collect();
        let mut tally: Vec<(f64, usize)> = Vec::new();
        for r in rows.iter().flatten() {
            match tally.iter_mut().find(|(v, _)| v == r) {
                Some(t) => t.1 += 1,
                None => tally.push((*r, 1)),
            }
        }
        let expected = tally.iter().max_by_key(|t| t.1).map(|t| t.0);
        for (k, e) in entries.iter().enumerate() {
            if e.is_some_and(|e| e.get("error").is_some()) {
                cells[k] = "error".into();
                mismatches.push(format!(
                    "{} {id}: {}",
                    names[k],
                    e.unwrap()
                        .str_at(&["error"])
                        .chars()
                        .take(160)
                        .collect::<String>()
                ));
            } else if let (Some(r), Some(x)) = (rows[k], expected) {
                if r != x {
                    cells[k].push_str(" ≠");
                    mismatches.push(format!("{} {id}: {} rows, the others {}", names[k], r, x));
                }
            }
        }
        out.push_str(&format!(
            "| {id} {} ({} rows) | {} |\n",
            q.str_at(&["title"]),
            expected.map(fmt_int).unwrap_or_default(),
            cells.join(" | ")
        ));
    }

    out.push_str("\n### Updates, one client (median ms; lower is better)\n\n");
    out.push_str(&header("Operation"));
    let writes = rs[0]
        .get("writes")
        .and_then(Json::as_array)
        .map(|a| a.to_vec())
        .unwrap_or_default();
    for (i, w) in writes.iter().enumerate() {
        let entries: Vec<Option<&Json>> = rs
            .iter()
            .map(|r| {
                r.get("writes")
                    .and_then(Json::as_array)
                    .and_then(|a| a.get(i))
            })
            .collect();
        let vals: Vec<Option<f64>> = entries
            .iter()
            .map(|e| e.and_then(|e| num(e, &["latency_ms", "p50"])))
            .collect();
        let mut cells = bold_best(&vals, true, &fmt_ms);
        for (k, e) in entries.iter().enumerate() {
            if let Some(err) = e.and_then(|e| e.get("error")) {
                cells[k] = "error".into();
                mismatches.push(format!(
                    "{} {}: {}",
                    names[k],
                    w.str_at(&["id"]),
                    err.as_str()
                        .unwrap_or("")
                        .chars()
                        .take(160)
                        .collect::<String>()
                ));
            }
        }
        out.push_str(&format!(
            "| {} {} | {} |\n",
            w.str_at(&["id"]),
            w.str_at(&["title"]),
            cells.join(" | ")
        ));
    }

    out.push_str("\n### Concurrent interactive queries\n\n");
    out.push_str(&header("Metric"));
    for (label, path, lower, f) in [
        (
            "Queries per second",
            vec!["concurrent", "qps"],
            false,
            &fmt_int as &dyn Fn(f64) -> String,
        ),
        (
            "p50 latency (ms)",
            vec!["concurrent", "latency_ms", "p50"],
            true,
            &fmt_ms,
        ),
        (
            "p99 latency (ms)",
            vec!["concurrent", "latency_ms", "p99"],
            true,
            &fmt_ms,
        ),
        (
            "With one writer: queries per second",
            vec!["mixed", "qps"],
            false,
            &fmt_int,
        ),
        (
            "With one writer: p99 latency (ms)",
            vec!["mixed", "latency_ms", "p99"],
            true,
            &fmt_ms,
        ),
        (
            "With one writer: writes per second",
            vec!["mixed", "writes_per_second"],
            false,
            &fmt_int,
        ),
    ] {
        let vals: Vec<Option<f64>> = rs.iter().map(|r| num(r, &path)).collect();
        out.push_str(&format!(
            "| {label} | {} |\n",
            bold_best(&vals, lower, f).join(" | ")
        ));
    }
    let errs: Vec<String> = rs
        .iter()
        .map(|r| {
            fmt_int(
                num(r, &["concurrent", "errors"]).unwrap_or(0.0)
                    + num(r, &["mixed", "errors"]).unwrap_or(0.0),
            )
        })
        .collect();
    out.push_str(&format!("| Errors | {} |\n", errs.join(" | ")));
    if !mismatches.is_empty() {
        out.push_str("\n### Errors and differing answers\n\n");
        for m in mismatches {
            out.push_str(&format!("- {m}\n"));
        }
    }
    print!("{out}");
    Ok(())
}
