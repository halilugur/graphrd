//! Offline command-line tools: load, dump, query, update and convert, working directly on a
//! data directory (no server). They go through the same request handling as the HTTP API, so
//! formats, inference, SHACL validation and result serialization behave exactly as they do there.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use crate::http::{self, Request};
use crate::json::Json;
use crate::rdf::{self, Format, Quad, Term};
use crate::server::App;
use std::collections::HashMap;

/// Hold an exclusive lock on a data directory for the life of the process, so a server and an
/// offline tool (or two servers) never write the same files at once.
pub fn lock_data_dir(data: &Path) -> Result<File, String> {
    fs::create_dir_all(data).map_err(|e| format!("{}: {e}", data.display()))?;
    let f = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(data.join(".lock"))
        .map_err(|e| e.to_string())?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(fs::TryLockError::WouldBlock) => Err(format!(
            "{} is in use by another GraphRD process",
            data.display()
        )),
        Err(fs::TryLockError::Error(e)) => Err(format!("locking {}: {e}", data.display())),
    }
}

struct Args {
    data: PathBuf,
    repo: String,
    values: Vec<(String, String)>,
    files: Vec<String>,
}

impl Args {
    fn parse(args: &[String], flags: &[&str]) -> Result<Args, String> {
        let mut a = Args {
            data: PathBuf::from("graphrd-data"),
            repo: "default".into(),
            values: Vec::new(),
            files: Vec::new(),
        };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let mut value = || {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("{arg} needs a value"))
            };
            match arg.as_str() {
                "--data" => a.data = PathBuf::from(value()?),
                "--repo" => a.repo = value()?,
                // switches, without a value
                f @ ("--lenient" | "--incremental") if flags.contains(&f) => a
                    .values
                    .push((f.trim_start_matches("--").into(), "true".into())),
                f if flags.contains(&f) => {
                    let v = value()?;
                    a.values.push((f.trim_start_matches("--").to_string(), v));
                }
                f if f.starts_with("--") => return Err(format!("unknown option {f}")),
                file => a.files.push(file.to_string()),
            }
        }
        Ok(a)
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }
}

fn request(
    method: &str,
    path: &str,
    query: Vec<(String, String)>,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Request {
    Request {
        method: method.into(),
        path: path.into(),
        query,
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
            .collect(),
        body,
    }
}

fn check(r: http::Response, what: &str) -> Result<Vec<u8>, String> {
    if r.status >= 400 {
        return Err(format!(
            "{what}: {}",
            String::from_utf8_lossy(&r.body).trim()
        ));
    }
    Ok(r.body)
}

fn open(a: &Args) -> Result<(File, App), String> {
    crate::server::QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
    let lock = lock_data_dir(&a.data)?;
    let app =
        App::open(&a.data, None, None).map_err(|e| format!("opening {}: {e}", a.data.display()))?;
    Ok((lock, app))
}

fn ensure_repo(app: &App, a: &Args) -> Result<(), String> {
    let r = app.handle(&request(
        "GET",
        &format!("/rest/repositories/{}", a.repo),
        vec![],
        &[],
        vec![],
    ));
    if r.status == 200 {
        return Ok(());
    }
    let mut params = Json::obj();
    if let Some(rs) = a.get("ruleset") {
        params.set("ruleset", Json::obj().with("value", rs));
    }
    let cfg = Json::obj()
        .with("id", a.repo.as_str())
        .with("title", a.repo.as_str())
        .with("params", params);
    check(
        app.handle(&request(
            "POST",
            "/rest/repositories",
            vec![],
            &[("Content-Type", "application/json")],
            cfg.to_string().into_bytes(),
        )),
        "creating repository",
    )?;
    Ok(())
}

/// A format given by name (`ttl`, `nq`, `json`...) or media type.
fn rdf_format(name: &str) -> Result<Format, String> {
    Format::from_media_type(name)
        .or_else(|| Format::from_extension(&format!("x.{name}")))
        .ok_or_else(|| format!("unknown RDF format {name}"))
}

fn results_media_type(name: &str) -> String {
    match name {
        "json" | "srj" => "application/sparql-results+json".into(),
        "xml" | "srx" => "application/sparql-results+xml".into(),
        "csv" => "text/csv".into(),
        "tsv" => "text/tab-separated-values".into(),
        other => rdf_format(other)
            .map(|f| f.media_type().to_string())
            .unwrap_or_else(|_| other.to_string()),
    }
}

fn output(a: &Args) -> Result<Box<dyn Write>, String> {
    Ok(match a.get("output") {
        Some(p) if p != "-" => Box::new(io::BufWriter::new(
            File::create(p).map_err(|e| format!("{p}: {e}"))?,
        )),
        _ => Box::new(io::BufWriter::new(io::stdout())),
    })
}

fn text_arg(a: &Args, inline: &str, file: &str) -> Result<String, String> {
    if let Some(t) = a.get(inline) {
        return Ok(t.to_string());
    }
    if let Some(p) = a.get(file) {
        return fs::read_to_string(p).map_err(|e| format!("{p}: {e}"));
    }
    if let Some(t) = a.files.first() {
        return Ok(t.clone());
    }
    let mut s = String::new();
    io::stdin()
        .read_to_string(&mut s)
        .map_err(|e| e.to_string())?;
    Ok(s)
}

/// `graphrd load [--data DIR] [--repo ID] [--ruleset R] [--format F] [--graph IRI] FILE...`
///
/// The data directory is locked, so the statements are added in bulk: files are parsed on
/// all cores and each index gets them as one sorted run, without the write-ahead log (the
/// new manifest commits each batch at once). Repositories that validate SHACL take the
/// checked path of an upload instead.
pub fn load(args: &[String]) -> Result<(), String> {
    let a = Args::parse(
        args,
        &["--ruleset", "--format", "--graph", "--base", "--lenient"],
    )?;
    let (_lock, app) = open(&a)?;
    ensure_repo(&app, &a)?;
    if app.validates_shacl(&a.repo) {
        return load_checked(&a, &app);
    }
    let lenient = a.get("lenient").is_some();
    let files: Vec<String> = if a.files.is_empty() {
        vec!["-".into()]
    } else {
        a.files.clone()
    };
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let graph = a.get("graph").map(Term::iri);
    let base = a.get("base").unwrap_or("").to_string();
    let t0 = std::time::Instant::now();
    let (mut lines, mut added, mut skipped) = (0usize, 0usize, 0usize);
    let report = |file: &str, errors: Vec<String>, skipped: &mut usize| {
        for e in errors.iter().take(20usize.saturating_sub(*skipped)) {
            eprintln!("\r{file}: skipped: {e}");
        }
        *skipped += errors.len();
    };
    for file in &files {
        let format = match a.get("format") {
            Some(f) => rdf_format(f)?,
            None if file == "-" => {
                return Err("--format is needed when reading standard input".into());
            }
            None => Format::from_extension(file).ok_or_else(|| {
                format!("{file}: cannot tell the format from the extension; pass --format")
            })?,
        };
        let reader: Box<dyn Read> = if file == "-" {
            Box::new(io::stdin())
        } else {
            Box::new(File::open(file).map_err(|e| format!("{file}: {e}"))?)
        };
        let mut reader = BufReader::with_capacity(1 << 20, reader);
        // blank node labels mean the same node throughout one file
        let mut bnodes = HashMap::new();
        let mut insert = |chunks: Vec<Vec<Quad>>| -> Result<usize, String> {
            app.bulk_insert(&a.repo, chunks, &mut bnodes)?
                .ok_or_else(|| "the repository validates SHACL".to_string())
        };
        if matches!(format, Format::NTriples | Format::NQuads) {
            // line-based formats load in segments, so files larger than memory work
            const SEGMENT: u64 = 256 << 20;
            let mut first_line = 1usize;
            loop {
                let mut buf = Vec::with_capacity(SEGMENT as usize + (1 << 20));
                reader
                    .by_ref()
                    .take(SEGMENT)
                    .read_to_end(&mut buf)
                    .map_err(|e| format!("{file}: {e}"))?;
                if buf.is_empty() {
                    break;
                }
                if !buf.ends_with(b"\n") {
                    reader
                        .read_until(b'\n', &mut buf)
                        .map_err(|e| format!("{file}: {e}"))?;
                }
                let text = String::from_utf8_lossy(&buf);
                let parts = split_lines(&text, threads);
                let quads = format == Format::NQuads;
                let parsed: Vec<(usize, Result<Vec<Quad>, String>, Vec<String>)> =
                    std::thread::scope(|sc| {
                        let handles: Vec<_> = parts
                            .iter()
                            .map(|part| {
                                sc.spawn(move || {
                                    let n = part.bytes().filter(|&b| b == b'\n').count();
                                    let mut errors = Vec::new();
                                    let r = rdf::ntriples::parse_part(
                                        part,
                                        quads,
                                        lenient.then_some(&mut errors),
                                    );
                                    (n, r, errors)
                                })
                            })
                            .collect();
                        handles
                            .into_iter()
                            .map(|h| h.join().expect("parser thread"))
                            .collect()
                    });
                let mut chunks = Vec::with_capacity(parsed.len());
                let mut errors = Vec::new();
                for (n, r, errs) in parsed {
                    // line numbers in the messages count from the start of the file
                    let renumber =
                        |e: String| match e.strip_prefix("line ").and_then(|r| r.split_once(':')) {
                            Some((l, rest)) if l.parse::<usize>().is_ok() => format!(
                                "line {}:{rest}",
                                l.parse::<usize>().unwrap() + first_line - 1
                            ),
                            _ => e,
                        };
                    let mut qs = r.map_err(|e| format!("{file}: {}", renumber(e)))?;
                    if let Some(g) = &graph {
                        for q in qs.iter_mut().filter(|q| q.g.is_none()) {
                            q.g = Some(g.clone());
                        }
                    }
                    errors.extend(errs.into_iter().map(renumber));
                    chunks.push(qs);
                    first_line += n;
                }
                report(file, errors, &mut skipped);
                added += insert(chunks)?;
                eprint!("\r{file}: {} lines", first_line - 1);
                if (buf.len() as u64) < SEGMENT {
                    break;
                }
            }
            eprintln!();
            lines += first_line - 1;
        } else {
            let mut text = String::new();
            reader
                .read_to_string(&mut text)
                .map_err(|e| format!("{file}: {e}"))?;
            let mut errors = Vec::new();
            let quads = if lenient {
                rdf::parse_lenient(&text, format, &base, graph.as_ref(), &mut errors)
            } else {
                rdf::parse(&text, format, &base, graph.as_ref())
            }
            .map_err(|e| format!("{file}: {e}"))?;
            report(file, errors, &mut skipped);
            app.learn_prefixes(&a.repo, &text, format);
            let size = quads.len().div_ceil(threads).max(1);
            let mut chunks = Vec::new();
            let mut rest = quads;
            while rest.len() > size {
                let tail = rest.split_off(size);
                chunks.push(rest);
                rest = tail;
            }
            chunks.push(rest);
            added += insert(chunks)?;
            eprintln!("{file}: loaded");
        }
    }
    if lenient {
        eprintln!("{skipped} statements skipped");
    }
    let secs = t0.elapsed().as_secs_f64();
    let rate = if lines > 0 {
        format!(", {:.0} lines/s", lines as f64 / secs)
    } else {
        String::new()
    };
    let size = check(
        app.handle(&request(
            "GET",
            &format!("/repositories/{}/size", a.repo),
            vec![],
            &[],
            vec![],
        )),
        "size",
    )?;
    eprintln!(
        "repository {} now has {} statements ({added} added, {secs:.1} s{rate})",
        a.repo,
        String::from_utf8_lossy(&size)
    );
    Ok(())
}

/// `text` cut into about `n` pieces at line ends.
fn split_lines(text: &str, n: usize) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut parts = Vec::with_capacity(n);
    let mut start = 0;
    for i in 1..=n {
        let mut end = if i == n {
            bytes.len()
        } else {
            (bytes.len() * i / n).max(start)
        };
        while end < bytes.len() && end > 0 && bytes[end - 1] != b'\n' {
            end += 1;
        }
        if end > start {
            parts.push(&text[start..end]);
        }
        start = end;
    }
    parts
}

/// `load` into a repository that validates SHACL: every batch goes through the checked
/// path of an upload.
fn load_checked(a: &Args, app: &App) -> Result<(), String> {
    let lenient = a.get("lenient").is_some();
    let mut skipped = 0usize;
    let files: Vec<String> = if a.files.is_empty() {
        vec!["-".into()]
    } else {
        a.files.clone()
    };
    let t0 = std::time::Instant::now();
    let mut total = 0usize;
    for file in &files {
        let format = match a.get("format") {
            Some(f) => rdf_format(f)?,
            None if file == "-" => {
                return Err("--format is needed when reading standard input".into());
            }
            None => Format::from_extension(file).ok_or_else(|| {
                format!("{file}: cannot tell the format from the extension; pass --format")
            })?,
        };
        let reader: Box<dyn Read> = if file == "-" {
            Box::new(io::stdin())
        } else {
            Box::new(File::open(file).map_err(|e| format!("{file}: {e}"))?)
        };
        let mut query = Vec::new();
        if let Some(g) = a.get("graph") {
            query.push(("context".to_string(), format!("<{g}>")));
        }
        if let Some(b) = a.get("base") {
            query.push(("baseURI".to_string(), b.to_string()));
        }
        if lenient {
            query.push(("lenient".to_string(), "true".to_string()));
        }
        let mut send = |body: Vec<u8>| -> Result<(), String> {
            let r = app.handle(&request(
                "POST",
                &format!("/repositories/{}/statements", a.repo),
                query.clone(),
                &[("Content-Type", format.media_type())],
                body,
            ));
            let out = check(r, file)?;
            if let Some(report) = lenient
                .then(|| Json::parse(&String::from_utf8_lossy(&out)).ok())
                .flatten()
            {
                let n = report.get("skipped").and_then(Json::as_f64).unwrap_or(0.0) as usize;
                if n > 0 {
                    for e in report
                        .get("errors")
                        .and_then(Json::as_array)
                        .unwrap_or(&[])
                        .iter()
                        .filter_map(Json::as_str)
                        .take(5.min(20usize.saturating_sub(skipped)))
                    {
                        eprintln!("\r{file}: skipped: {e}");
                    }
                }
                skipped += n;
            }
            Ok(())
        };
        if matches!(format, Format::NTriples | Format::NQuads) {
            // line-based formats load in batches, so files larger than memory work
            let mut lines = BufReader::new(reader);
            let mut batch = Vec::with_capacity(64 << 20);
            let mut n = 0usize;
            loop {
                let len = lines
                    .read_until(b'\n', &mut batch)
                    .map_err(|e| format!("{file}: {e}"))?;
                if len > 0 {
                    n += 1;
                }
                if len == 0 || n % 500_000 == 0 {
                    if !batch.is_empty() {
                        send(std::mem::take(&mut batch))?;
                    }
                    eprint!("\r{file}: {n} lines");
                    if len == 0 {
                        break;
                    }
                }
            }
            eprintln!();
            total += n;
        } else {
            let mut body = Vec::new();
            BufReader::new(reader)
                .read_to_end(&mut body)
                .map_err(|e| format!("{file}: {e}"))?;
            send(body)?;
            eprintln!("{file}: loaded");
        }
    }
    if lenient {
        eprintln!("{skipped} statements skipped");
    }
    let size = check(
        app.handle(&request(
            "GET",
            &format!("/repositories/{}/size", a.repo),
            vec![],
            &[],
            vec![],
        )),
        "size",
    )?;
    eprintln!(
        "repository {} now has {} statements ({:.1} s{})",
        a.repo,
        String::from_utf8_lossy(&size),
        t0.elapsed().as_secs_f64(),
        if total > 0 {
            format!(", {:.0} lines/s", total as f64 / t0.elapsed().as_secs_f64())
        } else {
            String::new()
        }
    );
    Ok(())
}

/// `graphrd dump [--data DIR] [--repo ID] [--format F] [--graph IRI] [--output FILE]`
pub fn dump(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &["--format", "--graph", "--output"])?;
    let (_lock, app) = open(&a)?;
    let format = match (a.get("format"), a.get("output")) {
        (Some(f), _) => rdf_format(f)?,
        (None, Some(o)) if o != "-" => Format::from_extension(o).unwrap_or(Format::NQuads),
        _ => Format::NQuads,
    };
    let mut query = vec![("infer".to_string(), "false".to_string())];
    if let Some(g) = a.get("graph") {
        // "default" names the default graph (null in the repository protocol)
        query.push((
            "context".to_string(),
            if g == "default" {
                "null".to_string()
            } else {
                format!("<{g}>")
            },
        ));
    }
    let body = check(
        app.handle(&request(
            "GET",
            &format!("/repositories/{}/statements", a.repo),
            query,
            &[("Accept", format.media_type())],
            vec![],
        )),
        "dump",
    )?;
    output(&a)?.write_all(&body).map_err(|e| e.to_string())
}

/// `graphrd query [--data DIR] [--repo ID] (--query Q | --query-file F | Q) [--format F] [--output FILE] [--infer false] [--as-of T]`
pub fn query(args: &[String]) -> Result<(), String> {
    let a = Args::parse(
        args,
        &[
            "--query",
            "--query-file",
            "--format",
            "--output",
            "--infer",
            "--base",
            "--explain",
            "--as-of",
        ],
    )?;
    let q = text_arg(&a, "query", "query-file")?;
    let (_lock, app) = open(&a)?;
    if a.get("explain").is_some_and(|v| v != "false") {
        let plan = app.explain(
            &a.repo,
            &q,
            a.get("base").unwrap_or(""),
            a.get("infer") != Some("false"),
        )?;
        eprint!("{plan}");
    }
    let t0 = std::time::Instant::now();
    let accept = match a.get("format") {
        Some(f) => results_media_type(f),
        None => "application/sparql-results+json, text/turtle;q=0.9".into(),
    };
    let mut params = vec![("query".to_string(), q)];
    if let Some(i) = a.get("infer") {
        params.push(("infer".to_string(), i.to_string()));
    }
    if let Some(b) = a.get("base") {
        params.push(("baseURI".to_string(), b.to_string()));
    }
    if let Some(t) = a.get("as-of") {
        params.push(("as-of".to_string(), t.to_string()));
    }
    let body = http::encode_form(&params);
    let r = app.handle(&request(
        "POST",
        &format!("/repositories/{}", a.repo),
        vec![],
        &[
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Accept", &accept),
        ],
        body.into_bytes(),
    ));
    let body = check(r, "query")?;
    if a.get("explain").is_some_and(|v| v != "false") {
        eprintln!("evaluated in {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0);
    }
    let mut out = output(&a)?;
    out.write_all(&body).map_err(|e| e.to_string())?;
    if !body.ends_with(b"\n") {
        out.write_all(b"\n").map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `graphrd update [--data DIR] [--repo ID] (--update U | --update-file F | U)`
pub fn update(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &["--update", "--update-file", "--ruleset", "--base"])?;
    let u = text_arg(&a, "update", "update-file")?;
    let (_lock, app) = open(&a)?;
    ensure_repo(&app, &a)?;
    let mut params = vec![("update".to_string(), u)];
    if let Some(b) = a.get("base") {
        params.push(("baseURI".to_string(), b.to_string()));
    }
    let body = http::encode_form(&params);
    let r = app.handle(&request(
        "POST",
        &format!("/repositories/{}/statements", a.repo),
        vec![],
        &[("Content-Type", "application/x-www-form-urlencoded")],
        body.into_bytes(),
    ));
    check(r, "update").map(|_| ())
}

/// `graphrd convert [--from F] [--to F] [--base IRI] [--graph IRI] [INPUT] [--output FILE]`
pub fn convert(args: &[String]) -> Result<(), String> {
    let a = Args::parse(
        args,
        &[
            "--from",
            "--to",
            "--base",
            "--graph",
            "--output",
            "--hash",
            "--jsonld",
            "--n3",
            "--from-graph",
        ],
    )?;
    let input = a.files.first().cloned().unwrap_or_else(|| "-".into());
    let from = match a.get("from") {
        Some(f) => rdf_format(f)?,
        None if input == "-" => return Err("--from is needed when reading standard input".into()),
        None => Format::from_extension(&input).ok_or_else(|| format!("{input}: pass --from"))?,
    };
    // RDFC-1.0: canonical N-Quads, or the map of issued identifiers as JSON
    if let Some(mode @ ("rdfc" | "rdfc-map")) = a.get("to") {
        let mut text = String::new();
        if input == "-" {
            io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| e.to_string())?;
        } else {
            text = fs::read_to_string(&input).map_err(|e| format!("{input}: {e}"))?;
        }
        // N-Triples / N-Quads keep their blank node labels (the map refers to them)
        let quads = match from {
            Format::NTriples | Format::NQuads => rdf::ntriples::parse(&text, true, true)?,
            _ => rdf::parse(&text, from, a.get("base").unwrap_or(""), None)?,
        };
        let alg = a
            .get("hash")
            .unwrap_or("SHA256")
            .to_ascii_uppercase()
            .replace('-', "");
        let out = if mode == "rdfc" {
            rdf::canon::canonical_nquads_with(&quads, &alg)?
        } else {
            let mut map = Json::obj();
            for (from, to) in rdf::canon::issued_identifiers_with(&quads, &alg)? {
                map.set(&from, to.as_str());
            }
            map.to_string() + "\n"
        };
        return output(&a)?
            .write_all(out.as_bytes())
            .map_err(|e| e.to_string());
    }
    let to = match (a.get("to"), a.get("output")) {
        (Some(f), _) => rdf_format(f)?,
        (None, Some(o)) if o != "-" => {
            Format::from_extension(o).ok_or_else(|| format!("{o}: pass --to"))?
        }
        _ => return Err("--to is needed when writing to standard output".into()),
    };
    let mut text = String::new();
    if input == "-" {
        io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;
    } else {
        text = fs::read_to_string(&input).map_err(|e| format!("{input}: {e}"))?;
    }
    let graph = a.get("graph").map(|g| rdf::Term::iri(g));
    let mut quads = if from == Format::JsonLd {
        // JSON-LD processing options: --jsonld processingMode=json-ld-1.0, rdfDirection=...,
        // produceGeneralizedRdf=true, expandContext=<file>
        let mut o = rdf::jsonld::Options::new(a.get("base").unwrap_or(""));
        for (k, v) in a
            .values
            .iter()
            .filter(|(k, _)| k == "jsonld")
            .filter_map(|(_, v)| v.split_once('='))
        {
            match k {
                "processingMode" => o.processing_mode = v.to_string(),
                "rdfDirection" => o.rdf_direction = Some(v.to_string()),
                "produceGeneralizedRdf" => o.generalized_rdf = v == "true",
                "expandContext" => {
                    o.expand_context = Some(Json::parse(
                        &fs::read_to_string(v).map_err(|e| format!("{v}: {e}"))?,
                    )?)
                }
                other => return Err(format!("unknown JSON-LD option {other}")),
            }
        }
        rdf::jsonld::to_rdf(&text, &o)?
    } else if from == Format::N3 {
        // --n3 variables=true: keep N3 variables (as blank nodes) and literal subjects
        let vars = a
            .values
            .iter()
            .any(|(k, v)| k == "n3" && v == "variables=true");
        rdf::turtle::parse_n3(&text, a.get("base").unwrap_or(""), vars)?
    } else {
        rdf::parse(&text, from, a.get("base").unwrap_or(""), None)?
    };
    // --from-graph IRI|default: keep only that graph of the input
    if let Some(fg) = a.get("from-graph") {
        let keep = if fg == "default" {
            None
        } else {
            Some(rdf::Term::iri(fg))
        };
        quads.retain(|q| q.g == keep);
        for q in &mut quads {
            q.g = None;
        }
    }
    if let Some(g) = &graph {
        for q in &mut quads {
            if q.g.is_none() {
                q.g = Some(g.clone());
            }
        }
    }
    let prefixes = if matches!(from, Format::Turtle | Format::TriG | Format::N3) {
        rdf::turtle::prefixes(&text)
    } else {
        Vec::new()
    };
    let out = rdf::write::serialize(&quads, to, &prefixes)?;
    output(&a)?
        .write_all(out.as_bytes())
        .map_err(|e| e.to_string())?;
    eprintln!("{} statements converted", quads.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn data_dir_lock_is_exclusive() {
        let dir = std::env::temp_dir().join(format!("graphrd-lock-{}", std::process::id()));
        let first = super::lock_data_dir(&dir).unwrap();
        assert!(super::lock_data_dir(&dir).unwrap_err().contains("in use"));
        drop(first);
        // a process another test starts meanwhile holds a copy of the descriptor until it
        // execs, so the lock can stay taken for a moment after it is dropped
        let again = (0..200).find_map(|_| {
            super::lock_data_dir(&dir).ok().or_else(|| {
                std::thread::sleep(std::time::Duration::from_millis(10));
                None
            })
        });
        assert!(again.is_some(), "the lock is free again once dropped");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// `graphrd backup [--data DIR] DESTINATION`: a consistent copy of the whole data directory
/// (every repository, users, saved queries), usable as a data directory of its own. The
/// directory lock keeps a server from writing meanwhile; for a running server, use
/// `GET /rest/recovery/backup` instead.
pub fn backup(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &["--incremental"])?;
    let incremental = a.get("incremental").is_some();
    let dest = PathBuf::from(
        a.files
            .first()
            .ok_or("backup needs a destination directory")?,
    );
    let exists = dest.exists()
        && fs::read_dir(&dest)
            .map_err(|e| e.to_string())?
            .next()
            .is_some();
    if exists && !incremental {
        return Err(format!(
            "{} exists and is not empty (--incremental updates an earlier backup)",
            dest.display()
        ));
    }
    if incremental && exists && !dest.join(BACKUP_LOG).exists() {
        return Err(format!("{} holds no GraphRD backup", dest.display()));
    }
    let _lock = lock_data_dir(&a.data)?;
    let mut copied = (0u64, 0u64);
    mirror(&a.data, &dest, &mut copied)
        .map_err(|e| format!("backup to {}: {e}", dest.display()))?;
    let kind = if exists { "incremental" } else { "full" };
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dest.join(BACKUP_LOG))
        .map_err(|e| e.to_string())?;
    writeln!(
        log,
        "{} {kind} {} files {} bytes",
        now_millis(),
        copied.0,
        copied.1
    )
    .map_err(|e| e.to_string())?;
    log.sync_all().map_err(|e| e.to_string())?;
    eprintln!(
        "{kind} backup of {} to {}: {} files, {:.1} MB copied",
        a.data.display(),
        dest.display(),
        copied.0,
        copied.1 as f64 / 1e6
    );
    Ok(())
}

/// A backup directory's record of the backups made into it.
const BACKUP_LOG: &str = "backup.log";

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Make `to` a copy of `from`: files that differ in size or modification time are copied
/// (index runs never change, so after the first backup only new runs, the grown dictionary,
/// the manifest and logs are), files `from` no longer has are removed. Counts (files, bytes).
fn mirror(from: &Path, to: &Path, n: &mut (u64, u64)) -> io::Result<()> {
    fs::create_dir_all(to)?;
    let mut seen = std::collections::HashSet::new();
    for e in fs::read_dir(from)? {
        let e = e?;
        let name = e.file_name();
        if name == ".lock" || name == BACKUP_LOG {
            continue;
        }
        seen.insert(name.clone());
        let (src, dst) = (e.path(), to.join(&name));
        if e.file_type()?.is_dir() {
            mirror(&src, &dst, n)?;
            continue;
        }
        let m = e.metadata()?;
        let same = fs::metadata(&dst)
            .is_ok_and(|d| d.len() == m.len() && d.modified().ok() == m.modified().ok());
        if same {
            continue;
        }
        let tmp = dst.with_extension("backup-tmp");
        n.1 += fs::copy(&src, &tmp)?;
        let f = File::open(&tmp)?;
        f.sync_all()?;
        if let Ok(t) = m.modified() {
            f.set_modified(t)?;
        }
        drop(f);
        fs::rename(&tmp, &dst)?;
        n.0 += 1;
    }
    for e in fs::read_dir(to)? {
        let e = e?;
        let name = e.file_name();
        if name == BACKUP_LOG || seen.contains(&name) {
            continue;
        }
        if e.file_type()?.is_dir() {
            fs::remove_dir_all(e.path())?;
        } else {
            fs::remove_file(e.path())?;
        }
    }
    Ok(())
}

/// `graphrd restore --from BACKUP [--data DIR] [--to-time T]`
pub fn restore(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &["--from", "--to-time"])?;
    let from = PathBuf::from(a.get("from").ok_or("restore needs --from BACKUP")?);
    if !from.join(BACKUP_LOG).exists() && !from.join("repositories").exists() {
        return Err(format!("{} is not a GraphRD backup", from.display()));
    }
    if a.data.exists()
        && fs::read_dir(&a.data)
            .map_err(|e| e.to_string())?
            .next()
            .is_some()
    {
        return Err(format!("{} exists and is not empty", a.data.display()));
    }
    let at = a
        .get("to-time")
        .map(crate::server::parse_as_of)
        .transpose()
        .map_err(|e| e.replace("Protocol error: as-of", "--to-time"))?;
    let mut copied = (0u64, 0u64);
    mirror(&from, &a.data, &mut copied)
        .map_err(|e| format!("restore to {}: {e}", a.data.display()))?;
    eprintln!(
        "restored {} to {} ({:.1} MB)",
        from.display(),
        a.data.display(),
        copied.1 as f64 / 1e6
    );
    if let Some(at) = at {
        let (_lock, app) = open(&a)?;
        for id in app.repository_ids() {
            match app.roll_back(&id, at) {
                Ok((r, i)) => eprintln!("{id}: back to {at}: {r} statements removed, {i} added"),
                Err(e) => return Err(format!("{id}: {e}")),
            }
        }
    }
    Ok(())
}

/// `graphrd optimize [--data DIR]`: checkpoint every repository and merge each index into a
/// single run file (merging also happens on its own as data is written).
pub fn optimize(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &[])?;
    let (_lock, app) = open(&a)?;
    let t0 = std::time::Instant::now();
    let done = app.optimize()?;
    eprintln!(
        "optimized {} repositories in {:.1} s",
        done.len(),
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

/// `graphrd validate --shapes FILE [--format turtle|json] [--base IRI] [--output FILE] DATA...`:
/// validate RDF files against SHACL shapes in memory (no data directory). A file may hold both
/// (`--shapes` naming one of the data files validates the shapes' own triples too).
pub fn validate(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &["--shapes", "--format", "--base", "--output"])?;
    let read = |file: &str| -> Result<Vec<Quad>, String> {
        let format =
            Format::from_extension(file).ok_or_else(|| format!("{file}: unknown RDF format"))?;
        let text = fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
        // relative IRIs resolve against the file's own URL unless --base is given
        let base = match a.get("base") {
            Some(b) => b.to_string(),
            None => format!(
                "file://{}",
                fs::canonicalize(file)
                    .map_err(|e| format!("{file}: {e}"))?
                    .display()
            ),
        };
        rdf::parse(&text, format, &base, None)
    };
    let shapes_file = a.get("shapes").ok_or("--shapes FILE is needed")?;
    let mut store = crate::store::Store::memory();
    let mut quads = Vec::new();
    let mut shapes = None;
    for f in &a.files {
        let q = read(f)?;
        if f == shapes_file {
            shapes = Some(q.clone());
        }
        quads.extend(q);
    }
    let shapes = match shapes {
        Some(s) => s,
        None => read(shapes_file)?,
    };
    let sg = Term::iri(crate::shacl::SHAPES_GRAPH);
    quads.extend(shapes.into_iter().map(|q| Quad {
        g: Some(sg.clone()),
        ..q
    }));
    store.insert(&quads).map_err(|e| e.to_string())?;
    let report = match crate::shacl::Shapes::load(&store) {
        Some(s) => s.check(&store, None)?,
        None => Vec::new(),
    };
    let out = match a.get("format").unwrap_or("turtle") {
        "json" => crate::shacl::report_json(&report).to_string() + "\n",
        "turtle" | "ttl" => crate::shacl::report_turtle(&report),
        f => return Err(format!("unknown report format {f} (turtle or json)")),
    };
    output(&a)?
        .write_all(out.as_bytes())
        .map_err(|e| e.to_string())
}
