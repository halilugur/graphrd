//! `graphrd bench`: end-to-end benchmark against one or more running
//! servers over HTTP. Generates a synthetic social/organisation graph,
//! loads it, then measures query latency, concurrent throughput and write
//! latency (with inference and full-text indexing switched on).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::http::percent_encode;

struct Client {
    host: String,
    stream: Option<BufReader<TcpStream>>,
}

impl Client {
    fn new(url: &str) -> Client {
        Client {
            host: url
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_string(),
            stream: None,
        }
    }

    fn request(
        &mut self,
        method: &str,
        path: &str,
        ctype: &str,
        accept: &str,
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        for attempt in 0..2 {
            if self.stream.is_none() {
                let s = TcpStream::connect(&self.host)
                    .map_err(|e| format!("connect {}: {e}", self.host))?;
                s.set_nodelay(true).ok();
                self.stream = Some(BufReader::new(s));
            }
            match self.try_request(method, path, ctype, accept, body) {
                Ok(r) => return Ok(r),
                Err(e) => {
                    self.stream = None;
                    if attempt == 1 {
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
        ctype: &str,
        accept: &str,
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        let r = self.stream.as_mut().unwrap();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: {ctype}\r\nAccept: {accept}\r\nContent-Length: {}\r\n\r\n",
            self.host,
            body.len()
        );
        let w = r.get_mut();
        w.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
        w.write_all(body).map_err(|e| e.to_string())?;
        let mut line = String::new();
        r.read_line(&mut line).map_err(|e| e.to_string())?;
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or("bad status line")?;
        let mut len = 0usize;
        loop {
            let mut h = String::new();
            r.read_line(&mut h).map_err(|e| e.to_string())?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).map_err(|e| e.to_string())?;
        Ok((status, buf))
    }

    fn query(&mut self, repo: &str, q: &str) -> Result<usize, String> {
        let body = format!("query={}", percent_encode(q));
        let (st, b) = self.request(
            "POST",
            &format!("/repositories/{repo}"),
            "application/x-www-form-urlencoded",
            "text/csv",
            body.as_bytes(),
        )?;
        if st != 200 {
            return Err(format!("{st}: {}", String::from_utf8_lossy(&b)));
        }
        Ok(b.iter().filter(|&&c| c == b'\n').count().saturating_sub(1))
    }

    fn update(&mut self, repo: &str, u: &str) -> Result<(), String> {
        let body = format!("update={}", percent_encode(u));
        let (st, b) = self.request(
            "POST",
            &format!("/repositories/{repo}/statements"),
            "application/x-www-form-urlencoded",
            "*/*",
            body.as_bytes(),
        )?;
        if st != 204 {
            return Err(format!("{st}: {}", String::from_utf8_lossy(&b)));
        }
        Ok(())
    }
}

pub(crate) const EX: &str = "http://bench.graphrd.org/";

/// Synthetic data: people in organisations, typed by one of 50 classes that
/// all specialise ex:Agent, with names, ages, bios and `knows` links.
pub(crate) fn generate(from: usize, to: usize, persons: usize, seed: &mut u64) -> String {
    let mut rnd = || {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    };
    let words = [
        "graph", "data", "music", "travel", "science", "history", "chess", "cooking", "football",
        "poetry", "robotics", "finance",
    ];
    let mut s = String::with_capacity((to - from) * 420);
    for i in from..to {
        let p = format!("<{EX}p{i}>");
        s.push_str(&format!(
            "{p} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{EX}C{}> .\n",
            i % 50
        ));
        s.push_str(&format!("{p} <{EX}name> \"Person {i}\" .\n"));
        s.push_str(&format!(
            "{p} <{EX}age> \"{}\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n",
            rnd() % 90 + 10
        ));
        s.push_str(&format!(
            "{p} <{EX}worksFor> <{EX}org{}> .\n",
            rnd() as usize % (persons / 100 + 1)
        ));
        s.push_str(&format!(
            "{p} <{EX}knows> <{EX}p{}> .\n",
            rnd() as usize % persons
        ));
        s.push_str(&format!(
            "{p} <{EX}bio> \"Likes {} and {}, works on {}\" .\n",
            words[rnd() as usize % words.len()],
            words[rnd() as usize % words.len()],
            words[rnd() as usize % words.len()]
        ));
    }
    s
}

fn schema() -> String {
    let mut s = String::new();
    for c in 0..50 {
        s.push_str(&format!(
            "<{EX}C{c}> <http://www.w3.org/2000/01/rdf-schema#subClassOf> <{EX}Agent> .\n"
        ));
    }
    s.push_str(&format!(
        "<{EX}knows> <http://www.w3.org/2002/07/owl#inverseOf> <{EX}knownBy> .\n"
    ));
    s.push_str(&format!(
        "<{EX}worksFor> <http://www.w3.org/2000/01/rdf-schema#range> <{EX}Organisation> .\n"
    ));
    s
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn ms(d: f64) -> String {
    if d < 10.0 {
        format!("{d:.2} ms")
    } else {
        format!("{d:.0} ms")
    }
}

struct Opts {
    urls: Vec<String>,
    persons: usize,
    clients: usize,
    seconds: u64,
    repo: String,
    load: bool,
}

fn queries(persons: usize) -> Vec<(&'static str, String)> {
    let p = persons / 2 + 7;
    vec![
        ("Point lookup (all properties of one person)", format!("SELECT ?p ?o WHERE {{ <{EX}p{p}> ?p ?o }}")),
        ("Two-hop: names of people a person knows", format!("SELECT ?n WHERE {{ <{EX}p{p}> <{EX}knows> ?f . ?f <{EX}name> ?n }}")),
        ("Inferred inverse (owl:inverseOf)", format!("SELECT ?x WHERE {{ <{EX}p{p}> <{EX}knownBy> ?x }}")),
        ("Count instances of an inferred superclass", format!("SELECT (COUNT(*) AS ?n) WHERE {{ ?s a <{EX}Agent> }}")),
        ("Filter + order: oldest 10 in one class", format!("SELECT ?s ?a WHERE {{ ?s a <{EX}C7> ; <{EX}age> ?a FILTER(?a > 95) }} ORDER BY DESC(?a) LIMIT 10")),
        ("Aggregate: people per organisation, top 5", format!("SELECT ?o (COUNT(?s) AS ?n) WHERE {{ ?s <{EX}worksFor> ?o }} GROUP BY ?o ORDER BY DESC(?n) LIMIT 5")),
        ("Full-text: phrase", "PREFIX g: <http://graphrd.org/fn#> SELECT ?l WHERE { ?l g:fts \"\\\"Person 4242\\\"\" }".to_string()),
        ("Full-text: words + prefix", "PREFIX g: <http://graphrd.org/fn#> SELECT (COUNT(*) AS ?n) WHERE { ?l g:fts \"chess robot*\" }".to_string()),
        ("Property path: knows{1,3} reachable", format!("SELECT (COUNT(DISTINCT ?x) AS ?n) WHERE {{ <{EX}p{p}> <{EX}knows>/<{EX}knows>?/<{EX}knows>? ?x }}")),
        ("Join: colleagues of a person", format!("SELECT ?c WHERE {{ <{EX}p{p}> <{EX}worksFor> ?o . ?c <{EX}worksFor> ?o }} LIMIT 100")),
    ]
}

/// The `graphrd bench` command: load data and measure against running servers.
pub fn run(args: &[String]) -> Result<(), String> {
    let mut o = Opts {
        urls: vec!["http://127.0.0.1:7200".into()],
        persons: 100_000,
        clients: 8,
        seconds: 5,
        repo: "bench".into(),
        load: true,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut v = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--url" | "--urls" => o.urls = v()?.split(',').map(String::from).collect(),
            "--persons" => o.persons = v()?.parse().map_err(|_| "bad --persons")?,
            "--clients" => o.clients = v()?.parse().map_err(|_| "bad --clients")?,
            "--seconds" => o.seconds = v()?.parse().map_err(|_| "bad --seconds")?,
            "--repo" => o.repo = v()?,
            "--no-load" => o.load = false,
            other => return Err(format!("unknown option {other}")),
        }
    }
    let triples = o.persons * 6 + 52;
    println!("# GraphRD benchmark\n");
    println!(
        "{} server(s): {} · {} persons (~{} triples per server) · {} concurrent clients per server\n",
        o.urls.len(),
        o.urls.join(", "),
        o.persons,
        triples,
        o.clients
    );

    // Load all servers in parallel.
    let started = Instant::now();
    let handles: Vec<_> = o
        .urls
        .iter()
        .filter(|_| o.load)
        .map(|url| {
            let (url, repo, persons) = (url.clone(), o.repo.clone(), o.persons);
            std::thread::spawn(move || load(&url, &repo, persons))
        })
        .collect();
    let mut loads = Vec::new();
    for (h, url) in handles.into_iter().zip(&o.urls) {
        let r = h.join().map_err(|_| "loader panicked")??;
        loads.push((url.clone(), r));
    }
    let wall = started.elapsed().as_secs_f64();
    if o.load {
        println!(
            "## Load (N-Triples over HTTP, rdfsplus-optimized inference, full-text index on)\n"
        );
        println!("| Server | Explicit | Inferred | Time | Throughput |\n|---|---:|---:|---:|---:|");
        for (url, (explicit, inferred, secs)) in &loads {
            println!(
                "| {url} | {explicit} | {inferred} | {secs:.1} s | {:.0} triples/s |",
                *explicit as f64 / secs
            );
        }
        if o.urls.len() > 1 {
            let total: usize = loads.iter().map(|(_, l)| l.0).sum();
            println!(
                "| **all, in parallel** | {total} | | {wall:.1} s | **{:.0} triples/s** |",
                total as f64 / wall
            );
        }
    }

    // Single-client latency, on the first server.
    println!(
        "\n## Query latency (one client, {} runs each, server 1)\n",
        20
    );
    println!("| Query | Rows | p50 | p95 |\n|---|---:|---:|---:|");
    let mut c = Client::new(&o.urls[0]);
    for (name, q) in queries(o.persons) {
        let mut times = Vec::new();
        let mut rows = 0;
        for _ in 0..20 {
            let t = Instant::now();
            rows = c.query(&o.repo, &q).map_err(|e| format!("{name}: {e}"))?;
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "| {name} | {rows} | {} | {} |",
            ms(percentile(&times, 0.5)),
            ms(percentile(&times, 0.95))
        );
    }

    // Writes.
    println!("\n## Writes (one client, server 1)\n");
    println!("| Operation | Count | p50 | p95 | Total |\n|---|---:|---:|---:|---:|");
    let ops: Vec<(&str, Box<dyn Fn(usize) -> String>)> = vec![
        (
            "INSERT DATA, 1 new person (6 triples)",
            Box::new(|i| {
                format!(
                    "INSERT DATA {{ {} }}",
                    generate(10_000_000 + i, 10_000_001 + i, 1000, &mut (i as u64 + 1))
                        .replace('\n', " ")
                )
            }),
        ),
        (
            "DELETE DATA, 1 type statement (DRed retraction)",
            Box::new(|i| format!("DELETE DATA {{ <{EX}p{i}> a <{EX}C{}> }}", i % 50)),
        ),
        (
            "DELETE/INSERT WHERE: change an age",
            Box::new(|i| {
                format!(
                    "DELETE {{ <{EX}p{n}> <{EX}age> ?a }} INSERT {{ <{EX}p{n}> <{EX}age> 42 }} WHERE {{ <{EX}p{n}> <{EX}age> ?a }}",
                    n = i + 1000
                )
            }),
        ),
    ];
    for (name, make) in &ops {
        let mut times = Vec::new();
        let t0 = Instant::now();
        for i in 0..200 {
            let t = Instant::now();
            c.update(&o.repo, &make(i))
                .map_err(|e| format!("{name}: {e}"))?;
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "| {name} | 200 | {} | {} | {:.2} s |",
            ms(percentile(&times, 0.5)),
            ms(percentile(&times, 0.95)),
            t0.elapsed().as_secs_f64()
        );
    }

    // Concurrent mixed read workload on every server at once.
    println!(
        "\n## Concurrent reads ({} clients per server for {} s, mixed query suite)\n",
        o.clients, o.seconds
    );
    println!("| Server | Queries | QPS | p50 | p99 | Errors |\n|---|---:|---:|---:|---:|---:|");
    let mut total_q = 0u64;
    let results: Vec<_> = o
        .urls
        .iter()
        .map(|url| {
            let (url, repo, persons, clients, secs) =
                (url.clone(), o.repo.clone(), o.persons, o.clients, o.seconds);
            std::thread::spawn(move || concurrent(&url, &repo, persons, clients, secs, false))
        })
        .collect();
    for (h, url) in results.into_iter().zip(&o.urls) {
        let (n, qps, p50, p99, errs) = h.join().map_err(|_| "worker panicked")?;
        total_q += n;
        println!(
            "| {url} | {n} | {qps:.0} | {} | {} | {errs} |",
            ms(p50),
            ms(p99)
        );
    }
    if o.urls.len() > 1 {
        println!(
            "| **all servers** | {total_q} | **{:.0}** | | | |",
            total_q as f64 / o.seconds as f64
        );
    }

    println!(
        "\n## Concurrent reads while writing (server 1: {} readers + 1 writer for {} s)\n",
        o.clients, o.seconds
    );
    let (n, qps, p50, p99, errs) =
        concurrent(&o.urls[0], &o.repo, o.persons, o.clients, o.seconds, true);
    println!(
        "| Reads | QPS | p50 | p99 | Errors |\n|---:|---:|---:|---:|---:|\n| {n} | {qps:.0} | {} | {} | {errs} |",
        ms(p50),
        ms(p99)
    );
    Ok(())
}

fn load(url: &str, repo: &str, persons: usize) -> Result<(usize, usize, f64), String> {
    let mut c = Client::new(url);
    c.request(
        "DELETE",
        &format!("/rest/repositories/{repo}"),
        "application/json",
        "*/*",
        b"",
    )?;
    let cfg = format!(
        r#"{{"id":"{repo}","title":"Benchmark","params":{{"ruleset":{{"value":"rdfsplus-optimized"}},"enableFtsIndex":{{"value":"true"}}}}}}"#
    );
    let (st, b) = c.request(
        "POST",
        "/rest/repositories",
        "application/json",
        "*/*",
        cfg.as_bytes(),
    )?;
    if st != 201 {
        return Err(format!(
            "create repository on {url}: {st} {}",
            String::from_utf8_lossy(&b)
        ));
    }
    let t = Instant::now();
    c.request(
        "POST",
        &format!("/repositories/{repo}/statements"),
        "application/n-triples",
        "*/*",
        schema().as_bytes(),
    )?;
    let mut seed = 0x9E3779B97F4A7C15u64;
    let chunk = 25_000;
    let mut from = 0;
    while from < persons {
        let to = (from + chunk).min(persons);
        let data = generate(from, to, persons, &mut seed);
        let (st, b) = c.request(
            "POST",
            &format!("/repositories/{repo}/statements"),
            "application/n-triples",
            "*/*",
            data.as_bytes(),
        )?;
        if st != 204 {
            return Err(format!(
                "load on {url}: {st} {}",
                String::from_utf8_lossy(&b)
            ));
        }
        from = to;
    }
    let secs = t.elapsed().as_secs_f64();
    let (_, size) = c.request(
        "GET",
        &format!("/rest/repositories/{repo}/size"),
        "",
        "application/json",
        b"",
    )?;
    let size =
        crate::json::Json::parse(&String::from_utf8_lossy(&size)).map_err(|e| e.to_string())?;
    let num = |k: &str| {
        size.get(k)
            .and_then(|v| {
                if let crate::json::Json::Num(n) = v {
                    Some(*n as usize)
                } else {
                    None
                }
            })
            .unwrap_or(0)
    };
    Ok((num("explicit"), num("inferred"), secs))
}

fn concurrent(
    url: &str,
    repo: &str,
    persons: usize,
    clients: usize,
    secs: u64,
    with_writer: bool,
) -> (u64, f64, f64, f64, u64) {
    let stop = Arc::new(AtomicBool::new(false));
    let errors = Arc::new(AtomicU64::new(0));
    let lat = Arc::new(Mutex::new(Vec::<f64>::new()));
    let suite = Arc::new(queries(persons));
    let mut hs = Vec::new();
    for w in 0..clients {
        let (stop, errors, lat, suite, url, repo) = (
            stop.clone(),
            errors.clone(),
            lat.clone(),
            suite.clone(),
            url.to_string(),
            repo.to_string(),
        );
        hs.push(std::thread::spawn(move || {
            let mut c = Client::new(&url);
            let mut mine = Vec::new();
            let mut i = w;
            while !stop.load(Ordering::Relaxed) {
                let (_, q) = &suite[i % suite.len()];
                i += 1;
                let t = Instant::now();
                match c.query(&repo, q) {
                    Ok(_) => mine.push(t.elapsed().as_secs_f64() * 1000.0),
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            lat.lock().unwrap().extend(mine);
        }));
    }
    if with_writer {
        let (stop, url, repo) = (stop.clone(), url.to_string(), repo.to_string());
        hs.push(std::thread::spawn(move || {
            let mut c = Client::new(&url);
            let mut i = 0;
            while !stop.load(Ordering::Relaxed) {
                let data = generate(20_000_000 + i, 20_000_001 + i, 1000, &mut (i as u64 + 7))
                    .replace('\n', " ");
                let _ = c.update(&repo, &format!("INSERT DATA {{ {data} }}"));
                i += 1;
            }
        }));
    }
    std::thread::sleep(Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    for h in hs {
        let _ = h.join();
    }
    let mut l = lat.lock().unwrap().clone();
    l.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = l.len() as u64;
    (
        n,
        n as f64 / secs as f64,
        percentile(&l, 0.5),
        percentile(&l, 0.99),
        errors.load(Ordering::Relaxed),
    )
}
