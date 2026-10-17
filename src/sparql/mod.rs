//! SPARQL 1.1 engine: parse, evaluate, update, and serialize results.

pub mod ast;
pub mod eval;
pub mod func;
pub mod hash;
pub mod parser;
pub mod reasoning;
pub mod regex;

use std::collections::HashMap;

use crate::json::{Json, escape_into};
use crate::rdf::{self, Format, Quad, Term, XSD_STRING};
use crate::store::{DEFAULT_GRAPH, Store};
use ast::*;
use eval::Ctx;
pub use eval::QueryResult;

/// Everything a query may be run with, besides its text.
pub struct QueryOptions<'a> {
    /// The base IRI for relative IRIs in the query.
    pub base: &'a str,
    /// Well-known prefixes (`PREFIX` declarations are parsed into this table).
    pub prefixes: &'a HashMap<String, String>,
    /// The dataset the query reads, overriding its own FROM/FROM NAMED.
    pub dataset: Option<Dataset>,
    /// A time limit in seconds.
    pub timeout_secs: Option<u64>,
    /// Whether inferred statements take part (the default).
    pub infer: bool,
    /// Query-time inference (`infer=at-query`, see `reasoning`).
    pub at_query: bool,
}

/// Parse and evaluate a query; the error text is a parser or evaluation message.
pub fn query(store: &Store, text: &str, opts: &QueryOptions) -> Result<QueryResult, String> {
    let parsed = parser::parse_query(text, opts.base, opts.prefixes)?;
    let mut ctx = Ctx::new(store, &parsed.vars, &parsed.base, opts.timeout_secs);
    ctx.set_infer(opts.infer);
    ctx.set_dataset(opts.dataset.as_ref().or(parsed.query.dataset.as_ref()));
    if opts.at_query {
        ctx.set_at_query();
    }
    ctx.query(&parsed.query)
}

/// A query with variables pre-bound to terms (SHACL-SPARQL's `$this`, `$value`, ...). Following
/// SHACL, the query may not use MINUS, VALUES or SERVICE, nor assign a pre-bound variable, and
/// its subqueries must project every pre-bound variable (and see them bound).
pub fn query_prebound(
    store: &Store,
    text: &str,
    opts: &QueryOptions,
    bindings: &[(&str, Term)],
) -> Result<QueryResult, String> {
    let parsed = parser::parse_query(text, opts.base, opts.prefixes)?;
    let bound: Vec<(Var, Term)> = bindings
        .iter()
        .filter_map(|(n, t)| Some((parsed.vars.names.iter().position(|v| v == n)?, t.clone())))
        .collect();
    let vars: Vec<Var> = bound.iter().map(|(v, _)| *v).collect();
    if parsed.query.values.is_some() {
        return Err("VALUES is not allowed in a query with pre-bound variables".into());
    }
    prebound_check(&parsed.query.pattern, &vars, &parsed.vars)?;
    let mut ctx = Ctx::new(store, &parsed.vars, &parsed.base, opts.timeout_secs);
    ctx.set_infer(opts.infer);
    ctx.set_dataset(opts.dataset.as_ref().or(parsed.query.dataset.as_ref()));
    ctx.set_seed(&bound);
    ctx.query(&parsed.query)
}

fn prebound_check(p: &Pattern, vars: &[Var], names: &VarTable) -> Result<(), String> {
    let expr = |e: &Expr| -> Result<(), String> {
        fn walk(e: &Expr, vars: &[Var], names: &VarTable) -> Result<(), String> {
            if let Expr::Exists(p, _) = e {
                prebound_check(p, vars, names)?;
            }
            let mut r = Ok(());
            e.for_each_child(&mut |c| {
                if r.is_ok() {
                    r = walk(c, vars, names);
                }
            });
            r
        }
        walk(e, vars, names)
    };
    match p {
        Pattern::Minus(..) => {
            Err("MINUS is not allowed in a query with pre-bound variables".into())
        }
        Pattern::Values(..) => {
            Err("VALUES is not allowed in a query with pre-bound variables".into())
        }
        Pattern::Service(..) => {
            Err("SERVICE is not allowed in a query with pre-bound variables".into())
        }
        Pattern::Extend(inner, v, e) => {
            if vars.contains(v) {
                return Err(format!(
                    "?{} is pre-bound and cannot be assigned",
                    names.names[*v]
                ));
            }
            expr(e)?;
            prebound_check(inner, vars, names)
        }
        Pattern::SubSelect(q) => {
            let projected: Vec<Var> = match &q.form {
                Form::Select {
                    projection: Some(proj),
                    ..
                } => proj.iter().map(|(_, v)| *v).collect(),
                _ => eval::visible_vars(&q.pattern, names),
            };
            if let Some(v) = vars.iter().find(|v| !projected.contains(v)) {
                return Err(format!(
                    "a subquery must project the pre-bound ?{}",
                    names.names[*v]
                ));
            }
            prebound_check(&q.pattern, vars, names)
        }
        Pattern::Join(a, b) | Pattern::Union(a, b) => {
            prebound_check(a, vars, names)?;
            prebound_check(b, vars, names)
        }
        Pattern::LeftJoin(a, b, c) => {
            if let Some(c) = c {
                expr(c)?;
            }
            prebound_check(a, vars, names)?;
            prebound_check(b, vars, names)
        }
        Pattern::Filter(e, inner) => {
            expr(e)?;
            prebound_check(inner, vars, names)
        }
        Pattern::Graph(_, inner) => prebound_check(inner, vars, names),
        Pattern::Bgp(_) => Ok(()),
    }
}

/// The evaluation plan of a query (see `Ctx::explain`).
/// The query's evaluation plan (see `Ctx::explain`).
pub fn explain(store: &Store, text: &str, opts: &QueryOptions) -> Result<String, String> {
    let parsed = parser::parse_query(text, opts.base, opts.prefixes)?;
    let mut ctx = Ctx::new(store, &parsed.vars, &parsed.base, opts.timeout_secs);
    ctx.set_infer(opts.infer);
    ctx.set_dataset(opts.dataset.as_ref().or(parsed.query.dataset.as_ref()));
    Ok(ctx.explain(&parsed.query))
}

/// Parse and apply an update: all of its operations or none.
pub fn update(
    store: &mut Store,
    text: &str,
    base: &str,
    prefixes: &HashMap<String, String>,
) -> Result<(), String> {
    update_with_dataset(store, text, base, prefixes, None)
}

/// An update whose WHERE clauses read the given dataset (the SPARQL protocol's
/// `using-graph-uri` / `using-named-graph-uri` parameters). It is an error for an
/// operation to name its own dataset with USING or WITH as well.
pub fn update_with_dataset(
    store: &mut Store,
    text: &str,
    base: &str,
    prefixes: &HashMap<String, String>,
    dataset: Option<Dataset>,
) -> Result<(), String> {
    let mut parsed = parser::parse_update(text, base, prefixes)?;
    if let Some(ds) = dataset {
        for op in &mut parsed.ops {
            if let ast::UpdateOp::Modify { with, using, .. } = op {
                if with.is_some() || using.is_some() {
                    return Err("Protocol error: using-graph-uri/using-named-graph-uri cannot be combined with USING or WITH".into());
                }
                *using = Some(ds.clone());
            }
        }
    }
    for op in &parsed.ops {
        execute(store, op, &parsed.vars, &parsed.base)?;
    }
    Ok(())
}

fn rename_blanks(quads: Vec<Quad>) -> Vec<Quad> {
    fn f(map: &mut HashMap<String, Term>, t: Term) -> Term {
        match t {
            Term::Blank(b) => map.entry(b).or_insert_with(Term::fresh_blank).clone(),
            Term::Triple(q) => {
                let [s, p, o] = *q;
                Term::triple(f(map, s), p, f(map, o))
            }
            t => t,
        }
    }
    let mut map: HashMap<String, Term> = HashMap::new();
    quads
        .into_iter()
        .map(|q| Quad {
            s: f(&mut map, q.s),
            p: q.p,
            o: f(&mut map, q.o),
            g: q.g,
        })
        .collect()
}

fn ground(quads: &[QuadPattern]) -> Result<Vec<Quad>, String> {
    let term = |vt: &VarOrTerm| match vt {
        VarOrTerm::Term(t) => Ok(t.clone()),
        VarOrTerm::Var(_) => {
            Err("variables are not allowed in INSERT DATA / DELETE DATA".to_string())
        }
        VarOrTerm::Triple(_) => {
            Err("variables are not allowed in INSERT DATA / DELETE DATA".to_string())
        }
    };
    quads
        .iter()
        .map(|q| {
            Ok(Quad {
                s: term(&q.s)?,
                p: term(&q.p)?,
                o: term(&q.o)?,
                g: q.g.as_ref().map(term).transpose()?,
            })
        })
        .collect()
}

/// Expand removals of default-graph quads to every graph holding the triple
/// (The default graph is the union of all contexts.)
fn expand_removals(store: &Store, quads: Vec<Quad>) -> Vec<Quad> {
    let mut out = Vec::new();
    for q in quads {
        // without GRAPH, a removal targets the default graph; when the default graph is the
        // union of all graphs, that means every graph holding the triple
        if q.g.is_some() || !store.union_default_graph {
            out.push(q);
            continue;
        }
        let (Some(s), Some(p), Some(o)) = (
            store.dict.lookup(&q.s),
            store.dict.lookup(&q.p),
            store.dict.lookup(&q.o),
        ) else {
            continue;
        };
        for ids in store.quads_matching(Some(s), Some(p), Some(o), None) {
            out.push(store.decode(ids));
        }
    }
    out
}

fn graph_quads(store: &Store, g: Option<&str>) -> Vec<Quad> {
    let gid = match g {
        None => Some(DEFAULT_GRAPH),
        Some(iri) => match store.dict.lookup(&Term::iri(iri)) {
            Some(id) => Some(id),
            None => return Vec::new(),
        },
    };
    store
        .quads_matching(None, None, None, gid)
        .map(|q| store.decode(q))
        .collect()
}

fn execute(store: &mut Store, op: &UpdateOp, vars: &VarTable, base: &str) -> Result<(), String> {
    let io = |e: std::io::Error| format!("storage error: {e}");
    match op {
        UpdateOp::InsertData(qs) => {
            let quads = rename_blanks(ground(qs)?);
            store.insert(&quads).map_err(io)?;
        }
        UpdateOp::DeleteData(qs) => {
            let quads = expand_removals(store, ground(qs)?);
            store.apply(&quads, &[]).map_err(io)?;
        }
        UpdateOp::DeleteWhere(qs) => {
            let pattern = eval::quads_to_pattern(qs);
            let removals = {
                let ctx = Ctx::new(store, vars, base, None);
                let sols = ctx.eval_where(&pattern, None)?;
                ctx.instantiate(qs, &sols, None)
            };
            let removals = expand_removals(store, removals);
            store.apply(&removals, &[]).map_err(io)?;
        }
        UpdateOp::Modify {
            with,
            delete,
            insert,
            using,
            pattern,
        } => {
            let with_term = with.as_ref().map(|w| Term::iri(w.clone()));
            let (removals, additions) = {
                let mut ctx = Ctx::new(store, vars, base, None);
                ctx.set_dataset(using.as_ref());
                let sols = ctx.eval_where(
                    pattern,
                    if using.is_some() {
                        None
                    } else {
                        with_term.as_ref()
                    },
                )?;
                (
                    ctx.instantiate(delete, &sols, with_term.as_ref()),
                    ctx.instantiate(insert, &sols, with_term.as_ref()),
                )
            };
            let removals = expand_removals(store, removals);
            store.apply(&removals, &additions).map_err(io)?;
        }
        UpdateOp::Clear(target, _) | UpdateOp::Drop(target, _) => match target {
            GraphTarget::Default => {
                store.clear_graph(Some(DEFAULT_GRAPH)).map_err(io)?;
            }
            GraphTarget::All => {
                store.clear_graph(None).map_err(io)?;
            }
            GraphTarget::Named => {
                for g in store.graphs() {
                    store.clear_graph(Some(g)).map_err(io)?;
                }
            }
            GraphTarget::Graph(iri) => {
                if let Some(id) = store.dict.lookup(&Term::iri(iri.clone())) {
                    store.clear_graph(Some(id)).map_err(io)?;
                }
            }
        },
        UpdateOp::Create(..) => {}
        UpdateOp::Load {
            source,
            into,
            silent,
        } => {
            let graph = into.as_ref().map(|g| Term::iri(g.clone()));
            match crate::rdf::fetch(source, graph.as_ref()) {
                Ok(quads) => {
                    store.insert(&rename_blanks(quads)).map_err(io)?;
                }
                Err(e) if !silent => return Err(format!("LOAD failed: {e}")),
                Err(_) => {}
            }
        }
        UpdateOp::Transfer(kind, from, to, _) => {
            if from == to {
                return Ok(());
            }
            let src = graph_quads(store, from.as_deref());
            let to_term = to.as_ref().map(|t| Term::iri(t.clone()));
            if kind != "ADD" {
                let dst = graph_quads(store, to.as_deref());
                store.apply(&dst, &[]).map_err(io)?;
            }
            let copies: Vec<Quad> = src
                .iter()
                .map(|q| Quad {
                    g: to_term.clone(),
                    ..q.clone()
                })
                .collect();
            let removals = if kind == "MOVE" { src } else { Vec::new() };
            store.apply(&removals, &copies).map_err(io)?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------ result formats

#[derive(Clone, Copy, PartialEq, Debug)]
/// The wire format of a tabular SPARQL result.
pub enum ResultFormat {
    /// SPARQL Results JSON.
    Json,
    /// SPARQL Results XML.
    Xml,
    /// CSV.
    Csv,
    /// TSV.
    Tsv,
    /// `text/boolean`: just `true` or `false`.
    Text,
}

impl ResultFormat {
    /// The format's canonical media type.
    pub fn media_type(self) -> &'static str {
        match self {
            ResultFormat::Json => "application/sparql-results+json",
            ResultFormat::Xml => "application/sparql-results+xml",
            ResultFormat::Csv => "text/csv",
            ResultFormat::Tsv => "text/tab-separated-values",
            ResultFormat::Text => "text/boolean",
        }
    }
}

/// Pick a tabular result format from an Accept header (first acceptable wins).
pub fn negotiate_results(accept: &str) -> ResultFormat {
    for part in accept.split(',') {
        let mt = part
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let f = match mt.as_str() {
            "application/sparql-results+json"
            | "application/json"
            | "application/x-sparqlstar-results+json" => ResultFormat::Json,
            "application/sparql-results+xml"
            | "application/xml"
            | "text/xml"
            | "application/x-sparqlstar-results+xml" => ResultFormat::Xml,
            "text/csv" => ResultFormat::Csv,
            "text/tab-separated-values" | "application/x-sparqlstar-results+tsv" => {
                ResultFormat::Tsv
            }
            "text/boolean" => ResultFormat::Text,
            _ => continue,
        };
        return f;
    }
    ResultFormat::Json
}

/// Pick an RDF graph format from an Accept header (first acceptable wins).
pub fn negotiate_rdf(accept: &str) -> Format {
    for part in accept.split(',') {
        if let Some(f) = Format::from_media_type(part) {
            if part.trim().starts_with("application/json") {
                continue;
            }
            return f;
        }
    }
    Format::Turtle
}

/// A term in SPARQL JSON results, written straight into `out` (no intermediate tree).
fn write_term_json(out: &mut String, t: &Term) {
    match t {
        Term::Triple(q) => {
            out.push_str("{\"type\":\"triple\",\"value\":{\"subject\":");
            write_term_json(out, &q[0]);
            out.push_str(",\"predicate\":");
            write_term_json(out, &q[1]);
            out.push_str(",\"object\":");
            write_term_json(out, &q[2]);
            out.push_str("}}");
        }
        Term::Iri(i) => {
            out.push_str("{\"type\":\"uri\",\"value\":");
            escape_into(out, i);
            out.push('}');
        }
        Term::Blank(b) => {
            out.push_str("{\"type\":\"bnode\",\"value\":");
            escape_into(out, b);
            out.push('}');
        }
        Term::Literal(l) => {
            out.push_str("{\"type\":\"literal\",\"value\":");
            escape_into(out, &l.value);
            if let Some(lang) = &l.lang {
                match lang.split_once("--") {
                    Some((lang, dir)) => {
                        out.push_str(",\"xml:lang\":");
                        escape_into(out, lang);
                        out.push_str(",\"its:dir\":");
                        escape_into(out, dir);
                    }
                    None => {
                        out.push_str(",\"xml:lang\":");
                        escape_into(out, lang);
                    }
                }
            } else if l.datatype != XSD_STRING {
                out.push_str(",\"datatype\":");
                escape_into(out, &l.datatype);
            }
            out.push('}');
        }
    }
}

fn xml_esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn xml_term(out: &mut String, t: &Term) {
    match t {
        Term::Iri(i) => out.push_str(&format!("<uri>{}</uri>", xml_esc(i))),
        Term::Blank(b) => out.push_str(&format!("<bnode>{}</bnode>", xml_esc(b))),
        Term::Literal(l) => {
            if let Some(lang) = &l.lang {
                match lang.split_once("--") {
                    Some((lang, dir)) => out.push_str(&format!(
                        "<literal xml:lang=\"{}\" its:dir=\"{}\">",
                        xml_esc(lang),
                        xml_esc(dir)
                    )),
                    None => out.push_str(&format!("<literal xml:lang=\"{}\">", xml_esc(lang))),
                }
            } else if l.datatype != XSD_STRING {
                out.push_str(&format!("<literal datatype=\"{}\">", xml_esc(&l.datatype)));
            } else {
                out.push_str("<literal>");
            }
            out.push_str(&xml_esc(&l.value));
            out.push_str("</literal>");
        }
        Term::Triple(q) => {
            out.push_str("<triple>");
            for (tag, part) in ["subject", "predicate", "object"].iter().zip(q.iter()) {
                out.push_str(&format!("<{tag}>"));
                xml_term(out, part);
                out.push_str(&format!("</{tag}>"));
            }
            out.push_str("</triple>");
        }
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Parse a SPARQL JSON results document: (variables, rows) for SELECT, or a boolean for ASK.
pub fn parse_results_json(
    text: &str,
) -> Result<Result<(Vec<String>, Vec<Vec<Option<Term>>>), bool>, String> {
    let doc = Json::parse(text).map_err(|e| format!("bad SPARQL JSON results: {e}"))?;
    if let Some(Json::Bool(b)) = doc.get("boolean") {
        return Ok(Err(*b));
    }
    let vars: Vec<String> = doc
        .get("head")
        .and_then(|h| h.get("vars"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let bindings = doc
        .get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(|b| b.as_array())
        .ok_or("SPARQL JSON results without results.bindings")?;
    fn term(v: &Json) -> Result<Term, String> {
        let value = v.str_at(&["value"]).to_string();
        Ok(match v.str_at(&["type"]) {
            "uri" => Term::iri(value),
            "bnode" => Term::Blank(value),
            "literal" | "typed-literal" => match (
                v.get("xml:lang").and_then(Json::as_str),
                v.get("datatype").and_then(Json::as_str),
            ) {
                (Some(l), _) => match v.get("its:dir").and_then(Json::as_str) {
                    Some(d) => Term::dir_lang(value, l, d),
                    None => Term::lang(value, l),
                },
                (None, Some(dt)) => Term::typed(value, dt),
                (None, None) => Term::string(value),
            },
            "triple" => {
                let t = v.get("value").ok_or("triple without value")?;
                let part = |k: &str| {
                    t.get(k)
                        .ok_or_else(|| format!("triple without {k}"))
                        .and_then(term)
                };
                Term::triple(part("subject")?, part("predicate")?, part("object")?)
            }
            other => return Err(format!("unknown RDF term type {other:?} in SPARQL results")),
        })
    }
    let mut rows = Vec::with_capacity(bindings.len());
    for b in bindings {
        let mut row = Vec::with_capacity(vars.len());
        for v in &vars {
            row.push(match b.get(v) {
                Some(t) => Some(term(t)?),
                None => None,
            });
        }
        rows.push(row);
    }
    Ok(Ok((vars, rows)))
}

/// Tabular solutions in one of the result formats.
pub fn write_solutions(vars: &[String], rows: &[Vec<Option<Term>>], f: ResultFormat) -> Vec<u8> {
    match f {
        ResultFormat::Json | ResultFormat::Text => {
            // Stream the bindings directly; building a Json tree for big results is wasteful.
            let mut out = String::with_capacity(64 + rows.len() * vars.len() * 64);
            out.push_str("{\"head\":{\"vars\":");
            out.push_str(&Json::Arr(vars.iter().map(Json::from).collect()).to_string());
            out.push_str("},\"results\":{\"bindings\":[");
            for (i, row) in rows.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('{');
                let mut first = true;
                for (v, t) in vars.iter().zip(row) {
                    if let Some(t) = t {
                        if !first {
                            out.push(',');
                        }
                        first = false;
                        escape_into(&mut out, v);
                        out.push(':');
                        write_term_json(&mut out, t);
                    }
                }
                out.push('}');
            }
            out.push_str("]}}");
            out.into_bytes()
        }
        ResultFormat::Xml => {
            let mut out = String::from(
                "<?xml version=\"1.0\"?>\n<sparql xmlns=\"http://www.w3.org/2005/sparql-results#\" xmlns:its=\"http://www.w3.org/2005/11/its\">\n<head>",
            );
            for v in vars {
                out.push_str(&format!("<variable name=\"{}\"/>", xml_esc(v)));
            }
            out.push_str("</head>\n<results>\n");
            for row in rows {
                out.push_str("<result>");
                for (v, t) in vars.iter().zip(row) {
                    let Some(t) = t else { continue };
                    out.push_str(&format!("<binding name=\"{}\">", xml_esc(v)));
                    xml_term(&mut out, t);
                    out.push_str("</binding>");
                }
                out.push_str("</result>\n");
            }
            out.push_str("</results>\n</sparql>\n");
            out.into_bytes()
        }
        ResultFormat::Csv => {
            let mut out = vars.join(",");
            out.push_str("\r\n");
            for row in rows {
                let cells: Vec<String> = row
                    .iter()
                    .map(|t| match t {
                        None => String::new(),
                        Some(Term::Blank(b)) => format!("_:{b}"),
                        Some(t @ Term::Triple(_)) => csv_field(&t.to_string()),
                        Some(t) => csv_field(t.value()),
                    })
                    .collect();
                out.push_str(&cells.join(","));
                out.push_str("\r\n");
            }
            out.into_bytes()
        }
        ResultFormat::Tsv => {
            let mut out = vars
                .iter()
                .map(|v| format!("?{v}"))
                .collect::<Vec<_>>()
                .join("\t");
            out.push('\n');
            for row in rows {
                let cells: Vec<String> = row
                    .iter()
                    .map(|t| t.as_ref().map_or(String::new(), tsv_cell))
                    .collect();
                out.push_str(&cells.join("\t"));
                out.push('\n');
            }
            out.into_bytes()
        }
    }
}

/// A TSV cell: numbers whose lexical form is valid Turtle are written bare (`4`, `5.5`, `1.0e6`).
fn tsv_cell(t: &Term) -> String {
    if let Term::Literal(l) = t {
        let v = l.value.as_str();
        let body = v.strip_prefix(['+', '-']).unwrap_or(v);
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let bare = match l.datatype.as_str() {
            "http://www.w3.org/2001/XMLSchema#integer" => digits(body),
            "http://www.w3.org/2001/XMLSchema#decimal" => body
                .split_once('.')
                .is_some_and(|(i, f)| (i.is_empty() || digits(i)) && digits(f)),
            "http://www.w3.org/2001/XMLSchema#double" => {
                body.split_once(['e', 'E']).is_some_and(|(m, e)| {
                    let e = e.strip_prefix(['+', '-']).unwrap_or(e);
                    let m_ok = match m.split_once('.') {
                        Some((i, f)) => {
                            (digits(i) || i.is_empty())
                                && (digits(f) || f.is_empty())
                                && !(i.is_empty() && f.is_empty())
                        }
                        None => digits(m),
                    };
                    m_ok && digits(e)
                })
            }
            _ => false,
        };
        if bare {
            return v.to_string();
        }
    }
    t.to_string()
}

/// An ASK result in one of the result formats.
pub fn write_boolean(b: bool, f: ResultFormat) -> Vec<u8> {
    match f {
        ResultFormat::Xml => format!(
            "<?xml version=\"1.0\"?>\n<sparql xmlns=\"http://www.w3.org/2005/sparql-results#\"><head></head><boolean>{b}</boolean></sparql>\n"
        )
        .into_bytes(),
        ResultFormat::Text | ResultFormat::Csv | ResultFormat::Tsv => b.to_string().into_bytes(),
        ResultFormat::Json => Json::obj().with("head", Json::obj()).with("boolean", b).to_string().into_bytes(),
    }
}

/// A graph result (CONSTRUCT/DESCRIBE) in one of the RDF formats.
pub fn write_graph(
    quads: &[Quad],
    f: Format,
    prefixes: &[(String, String)],
) -> Result<Vec<u8>, String> {
    rdf::write::serialize(quads, f, prefixes).map(String::into_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(store: &Store, q: &str) -> Vec<Vec<Option<Term>>> {
        let pf = HashMap::new();
        match query(
            store,
            q,
            &QueryOptions {
                base: "",
                prefixes: &pf,
                dataset: None,
                timeout_secs: None,
                infer: true,
                at_query: false,
            },
        )
        .unwrap()
        {
            QueryResult::Solutions { rows, .. } => rows,
            _ => panic!("not a select"),
        }
    }

    #[test]
    fn service_calls_run_in_parallel_with_cache_and_retries() {
        use std::io::{Read, Write};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        // endpoints that answer after 300 ms; the third fails once with 503
        let endpoint = |name: &'static str,
                        var: &'static str,
                        fail_first: bool|
         -> (String, Arc<AtomicUsize>) {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/sparql", listener.local_addr().unwrap());
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = calls.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let n = counter.fetch_add(1, SeqCst);
                    std::thread::spawn(move || {
                        let mut s = stream;
                        let mut buf = [0u8; 8192];
                        let _ = s.read(&mut buf);
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        let body = format!(
                            r#"{{"head":{{"vars":["{var}"]}},"results":{{"bindings":[{{"{var}":{{"type":"uri","value":"http://e/{name}"}}}}]}}}}"#
                        );
                        let resp = if fail_first && n == 0 {
                            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                        } else {
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/sparql-results+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                        };
                        let _ = s.write_all(resp.as_bytes());
                    });
                }
            });
            (url, calls)
        };
        let (a, ca) = endpoint("a", "x", false);
        let (b, _) = endpoint("b", "y", false);
        let (c, cc) = endpoint("c", "z", true);
        let st = Store::memory();
        let q = format!(
            "SELECT ?x ?y ?z WHERE {{ SERVICE <{a}> {{ ?x ?p1 ?o1 }} SERVICE <{b}> {{ ?y ?p2 ?o2 }} SERVICE <{c}> {{ ?z ?p3 ?o3 }} }}"
        );
        let ev = eval::SERVICE_RETRIES.load(SeqCst);
        eval::SERVICE_RETRIES.store(1, SeqCst);
        eval::SERVICE_CACHE_SECS.store(60, SeqCst);
        let t0 = std::time::Instant::now();
        let rows = run(&st, &q);
        let took = t0.elapsed();
        assert_eq!(rows.len(), 1);
        // in parallel: about one endpoint's 300 ms, plus the retry of the failing
        // one; generous for slow CI machines, still far below the serial ~1.8 s
        assert!(took < std::time::Duration::from_millis(1500), "{took:?}");
        assert_eq!(cc.load(SeqCst), 2, "the 503 was retried");
        let t0 = std::time::Instant::now();
        assert_eq!(run(&st, &q).len(), 1);
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(100),
            "answers reused"
        );
        assert_eq!(ca.load(SeqCst), 1);
        eval::SERVICE_CACHE_SECS.store(0, SeqCst);
        eval::SERVICE_RETRIES.store(ev, SeqCst);
    }

    #[test]
    fn query_time_inference_matches_materialized_inference() {
        let schema = "@prefix ex: <http://e/> . @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> . @prefix owl: <http://www.w3.org/2002/07/owl#> .
            ex:Dog rdfs:subClassOf ex:Mammal . ex:Mammal rdfs:subClassOf ex:Animal . ex:Pet owl:equivalentClass ex:Companion .
            ex:owns rdfs:domain ex:Person ; rdfs:range ex:Pet . ex:hasDog rdfs:subPropertyOf ex:owns .
            ex:ownedBy owl:inverseOf ex:owns . ex:friend a owl:SymmetricProperty . ex:ancestor a owl:TransitiveProperty .
            ex:parent rdfs:subPropertyOf ex:ancestor .
            ex:rex a ex:Dog . ex:ann ex:hasDog ex:rex . ex:bob ex:friend ex:ann . ex:cy ex:parent ex:dee . ex:dee ex:parent ex:eve . ex:eve ex:ancestor ex:fay .
            ex:kit a ex:Companion .";
        let quads =
            crate::rdf::turtle::parse(schema, "", crate::rdf::turtle::Mode::Turtle).unwrap();
        let mut mat = Store::memory();
        mat.set_ruleset("rdfsplus-optimized", false);
        mat.insert(&quads).unwrap();
        let mut plain = Store::memory();
        plain.insert(&quads).unwrap();
        let pf = HashMap::new();
        let run_with = |st: &Store, q: &str, at_query: bool| -> Vec<Vec<Option<Term>>> {
            let opts = QueryOptions {
                base: "",
                prefixes: &pf,
                dataset: None,
                timeout_secs: None,
                infer: true,
                at_query,
            };
            match query(st, q, &opts).unwrap() {
                QueryResult::Solutions { mut rows, .. } => {
                    rows.sort_by_key(|r| format!("{r:?}"));
                    rows
                }
                _ => panic!(),
            }
        };
        for q in [
            "SELECT ?x WHERE { ?x a <http://e/Animal> }",
            "SELECT ?x WHERE { ?x a <http://e/Person> }",
            "SELECT ?x WHERE { ?x a <http://e/Pet> }",
            "SELECT ?x WHERE { ?x a <http://e/Companion> }",
            "SELECT ?t WHERE { <http://e/rex> a ?t }",
            "SELECT ?t WHERE { <http://e/ann> a ?t }",
            "SELECT ?x ?y WHERE { ?x <http://e/owns> ?y }",
            "SELECT ?x WHERE { <http://e/rex> <http://e/ownedBy> ?x }",
            "SELECT ?x WHERE { ?x <http://e/friend> <http://e/bob> }",
            "SELECT ?y WHERE { <http://e/cy> <http://e/ancestor> ?y }",
            "SELECT ?x ?t WHERE { ?x a ?t }",
        ] {
            let expected = run_with(&mat, q, false);
            assert!(!expected.is_empty(), "{q}");
            assert_eq!(run_with(&plain, q, true), expected, "{q}");
            assert_ne!(
                run_with(&plain, q, false),
                expected,
                "{q}: nothing is materialized"
            );
        }
    }

    #[test]
    fn single_variable_filters_are_decided_once_per_value() {
        let mut st = Store::memory();
        let mut data = String::from("INSERT DATA {");
        for i in 0..2000 {
            data.push_str(&format!(
                " <http://e/p{i}> <http://e/age> {} . <http://e/p{i}> <http://e/n> \"{}\" .",
                i % 50,
                i
            ));
        }
        data.push_str(" }");
        update(&mut st, &data, "", &HashMap::new()).unwrap();
        let count = |q: &str| run(&st, q)[0][0].as_ref().unwrap().value().to_string();
        assert_eq!(
            count("SELECT (COUNT(*) AS ?c) { ?p <http://e/age> ?a FILTER(?a >= 40) }"),
            "400"
        );
        assert_eq!(
            count(
                "SELECT (COUNT(*) AS ?c) { ?p <http://e/age> ?a FILTER(?a = 7 || STR(?a) = \"8\") }"
            ),
            "80"
        );
        // two variables: evaluated per row
        assert_eq!(count("SELECT (COUNT(*) AS ?c) { ?p <http://e/age> ?a ; <http://e/n> ?n FILTER(?a = xsd:integer(?n)) }".replace("xsd:", "<http://www.w3.org/2001/XMLSchema#>").replace("<http://www.w3.org/2001/XMLSchema#>integer", "<http://www.w3.org/2001/XMLSchema#integer>").as_str()), "50");
        // RAND answers differently each time: never memoized, so about half the rows pass
        let n: usize =
            count("SELECT (COUNT(*) AS ?c) { ?p <http://e/age> ?a FILTER(RAND() + ?a * 0 < 0.5) }")
                .parse()
                .unwrap();
        assert!(n > 700 && n < 1300, "{n}");
        assert!(eval::single_var(&ast::Expr::Var(3)).is_some());
    }

    #[test]
    fn long_property_path_walks_read_edges_in_one_scan() {
        // more nodes than the walk looks up one by one, so it switches to scanning `k` once
        const N: usize = 50_000;
        let mut st = Store::memory();
        let mut edges: Vec<(usize, usize)> = (0..N - 1).map(|i| (i, i + 1)).collect();
        edges.extend((0..N).step_by(10).map(|i| (i, i * 7 % N)));
        let mut data = String::from("INSERT DATA { GRAPH <http://e/g> {");
        for &(a, b) in &edges {
            data.push_str(&format!(" <http://e/n{a}> <http://e/k> <http://e/n{b}> ."));
        }
        data.push_str(" } }");
        update(&mut st, &data, "", &HashMap::new()).unwrap();
        let mut adj = [vec![Vec::new(); N], vec![Vec::new(); N]];
        for &(a, b) in &edges {
            adj[0][a].push(b);
            adj[1][b].push(a);
        }
        // the nodes `from` reaches in one step or more
        let reach = |from: usize, back: bool| {
            let mut seen = vec![false; N];
            let mut stack = vec![from];
            while let Some(n) = stack.pop() {
                for &m in &adj[back as usize][n] {
                    if !seen[m] {
                        seen[m] = true;
                        stack.push(m);
                    }
                }
            }
            seen
        };
        let size = |seen: Vec<bool>| seen.iter().filter(|&&s| s).count().to_string();
        let count = |q: &str| run(&st, q)[0][0].as_ref().unwrap().value().to_string();
        assert_eq!(
            count("SELECT (COUNT(*) AS ?c) { <http://e/n3> <http://e/k>+ ?x }"),
            size(reach(3, false))
        );
        assert_eq!(
            count(&format!(
                "SELECT (COUNT(*) AS ?c) {{ ?x <http://e/k>+ <http://e/n{}> }}",
                N - 2
            )),
            size(reach(N - 2, true))
        );
        let mut star = reach(3, false);
        star[3] = true;
        assert_eq!(
            count("SELECT (COUNT(*) AS ?c) FROM <http://e/g> { <http://e/n3> <http://e/k>* ?x }"),
            size(star)
        );
        assert_eq!(
            count(
                "SELECT (COUNT(*) AS ?c) FROM <http://e/none> { <http://e/n3> <http://e/k>+ ?x }"
            ),
            "0"
        );
    }

    #[test]
    fn limit_without_order_stops_early_but_answers_correctly() {
        let mut st = Store::memory();
        let mut data = String::new();
        for i in 0..3000 {
            data.push_str(&format!("INSERT DATA {{ <http://e/p{i}> a <http://e/P> . <http://e/p{i}> <http://e/age> {} }} ;\n", i % 90));
            if i % 3 == 0 {
                data.push_str(&format!("INSERT DATA {{ <http://e/p{i}> <http://e/works> <http://e/c{}> . <http://e/c{}> <http://e/name> \"c{}\" }} ;\n", i % 7, i % 7, i % 7));
            }
        }
        update(
            &mut st,
            data.trim_end().trim_end_matches(';'),
            "",
            &HashMap::new(),
        )
        .unwrap();
        let shapes = [
            "SELECT * { ?p a <http://e/P> OPTIONAL { ?p <http://e/works> ?c . ?c <http://e/name> ?n } }",
            "SELECT * { ?p a <http://e/P> . ?p <http://e/age> ?a }",
            "SELECT * { { ?p <http://e/works> ?c } UNION { ?p <http://e/age> 5 } }",
            "SELECT * { ?p a <http://e/P> . ?p <http://e/age> ?a . ?p <http://e/works> ?c }",
            "SELECT * { ?p <http://e/age> 89 . ?p <http://e/works> ?c BIND(STR(?c) AS ?s) }",
        ];
        for q in shapes {
            let all = run(&st, q);
            for (limit, offset) in [(1, 0), (7, 0), (200, 0), (50, 30), (100_000, 0)] {
                let part = run(&st, &format!("{q} LIMIT {limit} OFFSET {offset}"));
                assert_eq!(
                    part.len(),
                    all.len().saturating_sub(offset).min(limit),
                    "{q} LIMIT {limit} OFFSET {offset}"
                );
                assert!(
                    part.iter().all(|r| all.contains(r)),
                    "{q}: every row is a real solution"
                );
            }
        }
    }

    #[test]
    fn sparql_rdf12() {
        let mut st = Store::memory();
        let pf = HashMap::new();
        update(
            &mut st,
            r#"PREFIX ex: <http://ex.org/>
            INSERT DATA {
              << ex:alice ex:knows ex:bob ~ ex:r1 >> ex:since 2019 ; ex:source ex:hr .
              ex:bob ex:knows ex:carol {| ex:since 2021 |} .
            }"#,
            "",
            &pf,
        )
        .unwrap();
        // r1 reifies + 2 about r1; asserted triple + reifier + 1 annotation
        assert_eq!(st.len(), 6);
        let rows = |q: &str| match query(
            &st,
            q,
            &QueryOptions {
                base: "",
                prefixes: &pf,
                dataset: None,
                timeout_secs: None,
                infer: true,
                at_query: false,
            },
        )
        .unwrap()
        {
            QueryResult::Solutions { rows, .. } => rows,
            _ => panic!(),
        };
        // reified triple patterns with variables
        let r = rows(
            "PREFIX ex: <http://ex.org/> SELECT ?a ?b ?y WHERE { << ?a ex:knows ?b >> ex:since ?y } ORDER BY ?y",
        );
        assert_eq!(r.len(), 2);
        assert_eq!(r[0][0], Some(Term::iri("http://ex.org/alice")));
        assert_eq!(r[1][2], Some(Term::integer(2021)));
        // the reifier can be named in a pattern
        let r = rows(
            "PREFIX ex: <http://ex.org/> SELECT ?r WHERE { << ex:alice ex:knows ex:bob ~ ?r >> ex:source ex:hr }",
        );
        assert_eq!(r, vec![vec![Some(Term::iri("http://ex.org/r1"))]]);
        // annotation syntax in a query; a reified triple is not asserted
        assert_eq!(rows("PREFIX ex: <http://ex.org/> SELECT ?x WHERE { ?x ex:knows ex:carol {| ex:since ?y |} }").len(), 1);
        assert!(
            rows("PREFIX ex: <http://ex.org/> SELECT * WHERE { ex:alice ex:knows ex:bob }")
                .is_empty()
        );
        // triple terms in patterns, expressions and functions
        let r = rows(
            "PREFIX ex: <http://ex.org/> PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> SELECT ?t ?s (isTRIPLE(?t) AS ?is) WHERE { ex:r1 rdf:reifies ?t BIND(SUBJECT(?t) AS ?s) }",
        );
        assert!(r[0][0].as_ref().unwrap().is_triple());
        assert_eq!(r[0][1], Some(Term::iri("http://ex.org/alice")));
        assert_eq!(r[0][2], Some(Term::boolean(true)));
        let r = rows(
            "PREFIX ex: <http://ex.org/> SELECT ?t WHERE { VALUES ?o { 1 } BIND(<<( ex:a ex:b ?o )>> AS ?t) }",
        );
        assert_eq!(
            r[0][0],
            Some(Term::triple(
                Term::iri("http://ex.org/a"),
                Term::iri("http://ex.org/b"),
                Term::integer(1)
            ))
        );
        let json =
            String::from_utf8(write_solutions(&["t".into()], &r, ResultFormat::Json)).unwrap();
        assert!(json.contains("\"type\":\"triple\""), "{json}");
        // base direction
        let r = rows(
            r#"SELECT (LANG(?v) AS ?l) (LANGDIR(?v) AS ?d) WHERE { BIND("x"@ar--rtl AS ?v) }"#,
        );
        assert_eq!(
            r[0],
            vec![Some(Term::string("ar")), Some(Term::string("rtl"))]
        );
        // CONSTRUCT with a reified template, then round-trip through Turtle 1.2
        match query(&st, "PREFIX ex: <http://ex.org/> CONSTRUCT { << ?a ex:knows ?b >> ex:year ?y } WHERE { << ?a ex:knows ?b >> ex:since ?y }",
            &QueryOptions { base: "", prefixes: &pf, dataset: None, timeout_secs: None, infer: true, at_query: false }).unwrap() {
            QueryResult::Graph(g) => {
                assert_eq!(g.len(), 4, "{g:?}"); // per solution: reifier rdf:reifies + ex:year
                let ttl = String::from_utf8(write_graph(&g, Format::Turtle, &[("ex".into(), "http://ex.org/".into())]).unwrap()).unwrap();
                assert_eq!(crate::rdf::parse(&ttl, Format::Turtle, "", None).unwrap().len(), 4, "{ttl}");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn default_graph_is_a_set() {
        let mut st = Store::memory();
        let pf = HashMap::new();
        update(&mut st, "PREFIX ex: <http://ex.org/> INSERT DATA { ex:a ex:p ex:b . GRAPH ex:g1 { ex:a ex:p ex:b } GRAPH ex:g2 { ex:a ex:p ex:b . ex:b ex:p ex:c } }", "", &pf).unwrap();
        let count = |q: &str| match query(
            &st,
            q,
            &QueryOptions {
                base: "",
                prefixes: &pf,
                dataset: None,
                timeout_secs: None,
                infer: true,
                at_query: false,
            },
        )
        .unwrap()
        {
            QueryResult::Solutions { rows, .. } => rows.len(),
            _ => panic!(),
        };
        // the same triple in three graphs is one triple of the default graph (their RDF merge)
        assert_eq!(count("SELECT * { ?s ?p ?o }"), 2);
        assert_eq!(
            count("PREFIX ex: <http://ex.org/> SELECT * { ex:a ex:p ?o }"),
            1
        );
        assert_eq!(
            count("PREFIX ex: <http://ex.org/> SELECT * { ex:a ex:p/ex:p ?o }"),
            1
        );
        assert_eq!(
            count("PREFIX ex: <http://ex.org/> SELECT * FROM ex:g1 FROM ex:g2 { ?s ?p ?o }"),
            2
        );
        assert_eq!(count("SELECT * { GRAPH ?g { ?s ?p ?o } }"), 3);
    }

    #[test]
    fn full_text_search() {
        let mut st = Store::memory();
        let pf = HashMap::new();
        update(&mut st, r#"PREFIX ex: <http://ex.org/> PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
            INSERT DATA { ex:a rdfs:label "Graph databases and linked data" . ex:b rdfs:label "Relational databases" . }"#, "", &pf).unwrap();
        st.set_fts(true); // indexes existing literals
        update(
            &mut st,
            r#"PREFIX ex: <http://ex.org/> PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
            INSERT DATA { ex:c rdfs:label "A database of graphs, linked to the web of data" . }"#,
            "",
            &pf,
        )
        .unwrap();
        let rows = |st: &Store, q: &str| match query(
            st,
            q,
            &QueryOptions {
                base: "",
                prefixes: &pf,
                dataset: None,
                timeout_secs: None,
                infer: true,
                at_query: false,
            },
        )
        .unwrap()
        {
            QueryResult::Solutions { rows, .. } => rows,
            _ => panic!(),
        };
        let q = r#"PREFIX g: <http://graphrd.org/fn#>
            SELECT ?s ?score WHERE { ?l g:fts "database* linked" . ?s ?p ?l BIND(g:ftsScore(?l, "database* linked") AS ?score) } ORDER BY DESC(?score)"#;
        let r = rows(&st, q);
        assert_eq!(r.len(), 2, "{r:?}");
        assert!(
            r.iter()
                .all(|row| row[0] != Some(Term::iri("http://ex.org/b")))
        );
        assert_eq!(rows(&st, r#"PREFIX g: <http://graphrd.org/fn#> SELECT ?l { ?l g:fts "\"relational databases\"" }"#).len(), 1);
        // "databases" also finds "database" (stemming)
        assert_eq!(rows(&st, r#"PREFIX g: <http://graphrd.org/fn#> SELECT ?l { ?l g:fts "databases -relational" }"#).len(), 2);
        // a misspelled query finds what the right spelling would (typo-tolerant fallback)
        assert_eq!(
            rows(
                &st,
                r#"PREFIX g: <http://graphrd.org/fn#> SELECT ?l { ?l g:fts "relatonal databses" }"#
            )
            .len(),
            1
        );
        assert_eq!(rows(&st, r#"PREFIX g: <http://graphrd.org/fn#> SELECT ?l { ?l g:fts "relatonal~ databases" }"#).len(), 1);
        let hl = rows(
            &st,
            r#"PREFIX g: <http://graphrd.org/fn#> PREFIX ex: <http://ex.org/>
            SELECT (g:ftsHighlight(?l, "relatonal", "[", "]") AS ?h) { ex:b ?p ?l }"#,
        );
        assert_eq!(hl[0][0], Some(Term::string("[Relational] databases")));
        update(&mut st, r#"PREFIX ex: <http://ex.org/> INSERT DATA { ex:t <http://ex.org/title> "Kitaplarımızdan biri"@tr }"#, "", &pf).unwrap();
        assert_eq!(
            rows(
                &st,
                r#"PREFIX g: <http://graphrd.org/fn#> SELECT ?l { ?l g:fts "kitap" }"#
            )
            .len(),
            1
        );
        // deleted statements disappear from results
        update(
            &mut st,
            r#"PREFIX ex: <http://ex.org/> DELETE WHERE { ex:c ?p ?o }"#,
            "",
            &pf,
        )
        .unwrap();
        assert_eq!(rows(&st, q).len(), 1);
    }

    #[test]
    fn service_federation() {
        use std::sync::{Arc, Mutex};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let (serve_addr, log) = (addr.clone(), seen.clone());
        std::thread::spawn(move || {
            let _ = crate::http::serve(
                &serve_addr,
                Arc::new(move |req: &crate::http::Request| {
                    let q = req.param("query").unwrap_or("").to_string();
                    log.lock().unwrap().push(q);
                    let body = r#"{"head":{"vars":["p","name","extra"]},"results":{"bindings":[
                    {"p":{"type":"uri","value":"http://ex.org/b"},"name":{"type":"literal","value":"Bob"}},
                    {"p":{"type":"uri","value":"http://ex.org/c"},"name":{"type":"literal","value":"Cleo","xml:lang":"en"}},
                    {"p":{"type":"uri","value":"http://ex.org/z"},"name":{"type":"literal","value":"9","datatype":"http://www.w3.org/2001/XMLSchema#integer"}}]}}"#;
                    crate::http::Response::new(200, "application/sparql-results+json", body)
                }),
            );
        });
        for _ in 0..100 {
            if std::net::TcpStream::connect(&addr).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut st = Store::memory();
        let pf = HashMap::new();
        update(
            &mut st,
            "PREFIX ex: <http://ex.org/> INSERT DATA { ex:a ex:knows ex:b , ex:c , ex:d }",
            "",
            &pf,
        )
        .unwrap();
        let rows = |q: &str| match query(
            &st,
            q,
            &QueryOptions {
                base: "",
                prefixes: &pf,
                dataset: None,
                timeout_secs: None,
                infer: true,
                at_query: false,
            },
        ) {
            Ok(QueryResult::Solutions { rows, .. }) => Ok(rows),
            Ok(_) => panic!(),
            Err(e) => Err(e),
        };
        let q = format!(
            "PREFIX ex: <http://ex.org/> PREFIX unused: <http://u/> SELECT ?p ?name WHERE {{ ex:a ex:knows ?p SERVICE <http://{addr}/sparql> {{ ?p ex:name ?name }} }} ORDER BY ?p"
        );
        let r = rows(&q).unwrap();
        assert_eq!(
            r.len(),
            2,
            "joined on ?p: b and c, not d (no remote row) or z (no local row)"
        );
        assert_eq!(r[1][1], Some(Term::lang("Cleo", "en")));
        let sent = seen.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "one call for all three local solutions");
        assert!(
            sent[0].contains("PREFIX ex: <http://ex.org/>") && !sent[0].contains("unused:"),
            "{}",
            sent[0]
        );
        assert!(
            sent[0].ends_with("SELECT * WHERE { ?p ex:name ?name }"),
            "{}",
            sent[0]
        );
        assert!(
            rows("SELECT * WHERE { SERVICE <http://127.0.0.1:9/x> { ?s ?p ?o } }")
                .unwrap_err()
                .contains("SERVICE <http://127.0.0.1:9/x>")
        );
        assert_eq!(
            rows("SELECT * WHERE { SERVICE SILENT <http://127.0.0.1:9/x> { ?s ?p ?o } }")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn geosparql_functions() {
        let mut st = Store::memory();
        let pf = HashMap::new();
        let ttl = "PREFIX ex: <http://ex.org/> PREFIX geo: <http://www.opengis.net/ont/geosparql#> INSERT DATA {
            ex:park geo:asWKT \"POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))\"^^geo:wktLiteral .
            ex:kiosk geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral .
            ex:road geo:asWKT \"LINESTRING(-1 2, 5 2)\"^^geo:wktLiteral .
            ex:lake geo:asGeoJSON \"{\\\"type\\\":\\\"Polygon\\\",\\\"coordinates\\\":[[[10,10],[11,10],[11,11],[10,11],[10,10]]]}\"^^geo:geoJSONLiteral }";
        update(&mut st, ttl, "", &pf).unwrap();
        let rows = |q: &str| match query(
            &st,
            &format!(
                "PREFIX ex: <http://ex.org/> PREFIX geo: <http://www.opengis.net/ont/geosparql#> PREFIX geof: <http://www.opengis.net/def/function/geosparql/> PREFIX uom: <http://www.opengis.net/def/uom/OGC/1.0/> {q}"
            ),
            &QueryOptions {
                base: "",
                prefixes: &pf,
                dataset: None,
                timeout_secs: None,
                infer: true,
                at_query: false,
            },
        ) {
            Ok(QueryResult::Solutions { rows, .. }) => rows,
            other => panic!("{:?}", other.err()),
        };
        let r = rows(
            "SELECT ?x WHERE { ex:park geo:asWKT ?p . ?x geo:asWKT ?g FILTER(?x != ex:park && geof:sfIntersects(?p, ?g)) } ORDER BY ?x",
        );
        assert_eq!(
            r.iter().map(|r| r[0].clone().unwrap()).collect::<Vec<_>>(),
            vec![
                Term::iri("http://ex.org/kiosk"),
                Term::iri("http://ex.org/road")
            ]
        );
        let r = rows(
            "SELECT (geof:sfWithin(?k, ?p) AS ?w) (geof:sfCrosses(?r, ?p) AS ?c) (geof:relate(?k, ?p, \"0FFFFF212\") AS ?m) (geof:envelope(?r) AS ?e) (geof:dimension(?p) AS ?d) WHERE { ex:park geo:asWKT ?p . ex:kiosk geo:asWKT ?k . ex:road geo:asWKT ?r }",
        );
        assert_eq!(r[0][0], Some(Term::boolean(true)));
        assert_eq!(r[0][1], Some(Term::boolean(true)));
        assert_eq!(r[0][2], Some(Term::boolean(true)));
        assert_eq!(
            r[0][3],
            Some(Term::typed(
                "LINESTRING(-1 2, 5 2)",
                crate::geo::WKT_LITERAL
            ))
        );
        assert_eq!(r[0][4], Some(Term::integer(2)));
        let r = rows(
            "SELECT (geof:distance(?p, ?l, uom:metre) AS ?d) (geof:sfDisjoint(?p, ?l) AS ?j) WHERE { ex:park geo:asWKT ?p . ex:lake geo:asGeoJSON ?l }",
        );
        let d: f64 = r[0][0].as_ref().unwrap().value().parse().unwrap();
        assert!((d / 1000.0 - 943.0).abs() < 10.0, "{d}");
        assert_eq!(r[0][1], Some(Term::boolean(true)));
    }

    /// Filters the spatial index answers give the same rows as the plain scan (forced by
    /// hiding the call under `|| false`), in every shape the rewrite knows.
    #[test]
    fn spatial_index_matches_scan() {
        let mut st = Store::memory();
        let pf = HashMap::new();
        let mut data = String::from(
            "PREFIX ex: <http://ex.org/> PREFIX geo: <http://www.opengis.net/ont/geosparql#> INSERT DATA {\n",
        );
        let mut seed = 7u64;
        let mut rnd = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        for i in 0..600 {
            let (x, y) = (28.0 + rnd() * 2.0, 40.0 + rnd() * 2.0);
            let wkt = match i % 4 {
                0 | 1 => format!("POINT({x} {y})"),
                2 => format!(
                    "POLYGON(({x} {y}, {} {y}, {} {}, {x} {}, {x} {y}))",
                    x + 0.05,
                    x + 0.05,
                    y + 0.05,
                    y + 0.05
                ),
                // the same kind of point, written in UTM zone 35N
                _ => format!(
                    "<http://www.opengis.net/def/crs/EPSG/0/32635> POINT({} {})",
                    500000.0 + (x - 27.0) * 84000.0,
                    4430000.0 + (y - 40.0) * 111000.0
                ),
            };
            data.push_str(&format!(
                "ex:f{i} geo:hasGeometry ex:g{i} . ex:g{i} geo:asWKT \"{wkt}\"^^geo:wktLiteral .\n"
            ));
        }
        data.push_str("ex:area geo:asWKT \"POLYGON((28.5 40.5, 29.2 40.5, 29.2 41.1, 28.5 41.1, 28.5 40.5))\"^^geo:wktLiteral }");
        update(&mut st, &data, "", &pf).unwrap();
        let rows = |q: &str| -> Vec<String> {
            let text = format!(
                "PREFIX ex: <http://ex.org/> PREFIX geo: <http://www.opengis.net/ont/geosparql#> PREFIX geof: <http://www.opengis.net/def/function/geosparql/> PREFIX uom: <http://www.opengis.net/def/uom/OGC/1.0/> {q}"
            );
            match query(
                &st,
                &text,
                &QueryOptions {
                    base: "",
                    prefixes: &pf,
                    dataset: None,
                    timeout_secs: None,
                    infer: true,
                    at_query: false,
                },
            ) {
                Ok(QueryResult::Solutions { rows, .. }) => {
                    let mut v: Vec<String> = rows.iter().map(|r| format!("{:?}", r)).collect();
                    v.sort();
                    v
                }
                other => panic!("{:?}", other.err()),
            }
        };
        let area =
            "\"POLYGON((28.5 40.5, 29.2 40.5, 29.2 41.1, 28.5 41.1, 28.5 40.5))\"^^geo:wktLiteral";
        for (filter, at_least) in [
            (format!("geof:sfWithin(?w, {area})"), 40),
            (format!("geof:sfIntersects({area}, ?w)"), 40),
            (format!("geof:ehOverlap(?w, {area})"), 1),
            (
                "geof:distance(?w, \"POINT(29 41)\"^^geo:wktLiteral, uom:kilometre) < 25"
                    .to_string(),
                10,
            ),
            (
                "30000 >= geof:distance(\"POINT(29 41)\"^^geo:wktLiteral, ?w)".to_string(),
                10,
            ),
            (
                "geof:sfContains(?w, \"POINT(29 41)\"^^geo:wktLiteral)".to_string(),
                0,
            ),
        ] {
            let fast = rows(&format!(
                "SELECT ?f WHERE {{ ?f geo:hasGeometry ?g . ?g geo:asWKT ?w FILTER({filter}) }}"
            ));
            let slow = rows(&format!(
                "SELECT ?f WHERE {{ ?f geo:hasGeometry ?g . ?g geo:asWKT ?w FILTER(({filter}) || false) }}"
            ));
            assert_eq!(fast, slow, "{filter}");
            assert!(fast.len() >= at_least, "{filter}: {}", fast.len());
        }
        // through a property path
        let fast = rows(&format!(
            "SELECT ?f WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w FILTER(geof:sfWithin(?w, {area})) }}"
        ));
        let slow = rows(&format!(
            "SELECT ?f WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w FILTER(geof:sfWithin(?w, {area}) || false) }}"
        ));
        assert_eq!(fast, slow);
        assert!(fast.len() >= 40);
        // the region bound by another pattern of the same BGP
        let fast = rows(
            "SELECT ?f WHERE { ex:area geo:asWKT ?a . ?f geo:hasGeometry ?g . ?g geo:asWKT ?w FILTER(geof:sfWithin(?w, ?a)) }",
        );
        let slow = rows(
            "SELECT ?f WHERE { ex:area geo:asWKT ?a . ?f geo:hasGeometry ?g . ?g geo:asWKT ?w FILTER(geof:sfWithin(?w, ?a) || false) }",
        );
        assert_eq!(fast, slow);
        assert!(fast.len() >= 40);
        // and the index is kept across a restart, and built for a store from before it
        let dir = std::env::temp_dir().join(format!("graphrd-geo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut disk = Store::open(&dir).unwrap();
            update(&mut disk, &data, "", &pf).unwrap();
            disk.checkpoint().unwrap();
        }
        let count = |st: &Store| st.geo_candidates((28.5, 40.5, 29.2, 41.1)).len();
        let n = count(&Store::open(&dir).unwrap());
        assert!(n >= 40);
        let manifest = dir.join("MANIFEST");
        let old: String = std::fs::read_to_string(&manifest)
            .unwrap()
            .lines()
            .filter(|l| !l.starts_with("geo") && !l.starts_with("index geo"))
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(&manifest, old).unwrap();
        assert_eq!(count(&Store::open(&dir).unwrap()), n);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let addr = format!("127.0.0.1:{port}");
        let serve_addr = addr.clone();
        std::thread::spawn(move || {
            let _ = crate::http::serve(
                &serve_addr,
                std::sync::Arc::new(|req: &crate::http::Request| match req.path.as_str() {
                    "/old.ttl" => {
                        crate::http::Response::empty(301).with_header("Location", "/data.ttl")
                    }
                    "/data.ttl" => crate::http::Response::new(
                        200,
                        "text/turtle",
                        "@prefix ex: <http://ex.org/> . ex:a ex:b ex:c , [ ex:d 1 ] .",
                    ),
                    _ => crate::http::Response::text(404, "no"),
                }),
            );
        });
        for _ in 0..100 {
            if std::net::TcpStream::connect(&addr).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut st = Store::memory();
        let pf = HashMap::new();
        update(
            &mut st,
            &format!("LOAD <http://{addr}/old.ttl> INTO GRAPH <http://g>"),
            "",
            &pf,
        )
        .unwrap();
        assert_eq!(st.len(), 3);
        let g = st.dict.lookup(&Term::iri("http://g"));
        assert_eq!(st.count_graph(g), 3);
        assert!(
            update(
                &mut st,
                &format!("LOAD <http://{addr}/missing.ttl>"),
                "",
                &pf
            )
            .unwrap_err()
            .contains("404")
        );
        update(
            &mut st,
            &format!("LOAD SILENT <http://{addr}/missing.ttl>"),
            "",
            &pf,
        )
        .unwrap();
    }

    #[test]
    fn inference_visibility() {
        let mut st = Store::memory();
        st.set_ruleset("rdfs-optimized", true);
        let pf = HashMap::new();
        update(&mut st, "PREFIX ex: <http://ex.org/> PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> \
            INSERT DATA { ex:Dog rdfs:subClassOf ex:Animal . ex:rex a ex:Dog . ex:owns rdfs:range ex:Pet . ex:ann ex:owns ex:rex }", "", &pf).unwrap();
        let count = |st: &Store, q: &str, infer: bool| match query(
            st,
            q,
            &QueryOptions {
                base: "",
                prefixes: &pf,
                dataset: None,
                timeout_secs: None,
                infer,
                at_query: false,
            },
        )
        .unwrap()
        {
            QueryResult::Solutions { rows, .. } => rows.len(),
            _ => panic!(),
        };
        let q = "PREFIX ex: <http://ex.org/> SELECT ?t WHERE { ex:rex a ?t }";
        assert_eq!(count(&st, q, true), 3); // Dog, Animal, Pet
        assert_eq!(count(&st, q, false), 1);
        assert_eq!(
            count(
                &st,
                "PREFIX ex: <http://ex.org/> SELECT ?t FROM <http://graphrd.org/graph#explicit> WHERE { ex:rex a ?t }",
                true
            ),
            1
        );
        assert_eq!(
            count(
                &st,
                "PREFIX ex: <http://ex.org/> SELECT ?t FROM <http://graphrd.org/graph#implicit> WHERE { ex:rex a ?t }",
                true
            ),
            2
        );
        // deleting the explicit fact retracts what was derived from it
        update(
            &mut st,
            "PREFIX ex: <http://ex.org/> DELETE DATA { ex:ann ex:owns ex:rex }",
            "",
            &pf,
        )
        .unwrap();
        assert_eq!(count(&st, q, true), 2);
    }

    #[test]
    fn end_to_end() {
        let mut st = Store::memory();
        let pf = HashMap::new();
        update(
            &mut st,
            r#"PREFIX ex: <http://ex.org/>
               INSERT DATA {
                 ex:alice a ex:Person ; ex:name "Alice" ; ex:age 30 ; ex:knows ex:bob .
                 ex:bob a ex:Person ; ex:name "Bob" ; ex:age 25 ; ex:knows ex:carol .
                 ex:carol a ex:Person ; ex:name "Carol"@en ; ex:age 35 .
                 GRAPH ex:g { ex:dave a ex:Person ; ex:age 40 }
               }"#,
            "",
            &pf,
        )
        .unwrap();
        assert_eq!(st.len(), 13);

        let r = run(
            &st,
            "PREFIX ex: <http://ex.org/> SELECT ?n WHERE { ?p ex:name ?n ; ex:age ?a FILTER(?a > 26) } ORDER BY ?n",
        );
        assert_eq!(r.len(), 2);

        let r = run(
            &st,
            "PREFIX ex: <http://ex.org/> SELECT (COUNT(*) AS ?c) (AVG(?a) AS ?avg) WHERE { ?p ex:age ?a }",
        );
        assert_eq!(r[0][0], Some(Term::integer(4)));

        let r = run(
            &st,
            "PREFIX ex: <http://ex.org/> SELECT ?x WHERE { ex:alice ex:knows+ ?x }",
        );
        assert_eq!(r.len(), 2);

        let r = run(
            &st,
            "PREFIX ex: <http://ex.org/> SELECT ?p ?n WHERE { ?p a ex:Person OPTIONAL { ?p ex:name ?n } } ORDER BY ?p",
        );
        assert_eq!(r.len(), 4);
        assert_eq!(r.iter().filter(|row| row[1].is_none()).count(), 1);

        let r = run(
            &st,
            "PREFIX ex: <http://ex.org/> SELECT ?g ?s WHERE { GRAPH ?g { ?s ?p ?o } }",
        );
        assert_eq!(r.len(), 2);

        let r = run(
            &st,
            "PREFIX ex: <http://ex.org/> SELECT ?p (UCASE(STR(?n)) AS ?u) WHERE { ?p ex:name ?n FILTER(REGEX(?n, \"^b\", \"i\")) }",
        );
        assert_eq!(r[0][1], Some(Term::string("BOB")));

        let r = run(
            &st,
            "PREFIX ex: <http://ex.org/> SELECT ?p WHERE { ?p a ex:Person MINUS { ?p ex:knows ?x } }",
        );
        assert_eq!(r.len(), 2);

        update(&mut st, "PREFIX ex: <http://ex.org/> DELETE { ?p ex:age ?a } INSERT { ?p ex:age ?b } WHERE { ?p ex:age ?a BIND(?a + 1 AS ?b) }", "", &pf).unwrap();
        let r = run(
            &st,
            "PREFIX ex: <http://ex.org/> SELECT (SUM(?a) AS ?s) WHERE { ?p ex:age ?a }",
        );
        assert_eq!(r[0][0], Some(Term::integer(134)));

        // dave's re-inserted age went to the default graph, so only his type is dropped
        update(
            &mut st,
            "PREFIX ex: <http://ex.org/> DROP GRAPH ex:g",
            "",
            &pf,
        )
        .unwrap();
        assert_eq!(st.len(), 12);

        let opts = QueryOptions {
            base: "",
            prefixes: &pf,
            dataset: None,
            timeout_secs: None,
            infer: true,
            at_query: false,
        };
        match query(
            &st,
            "PREFIX ex: <http://ex.org/> CONSTRUCT { ?a ex:friendOf ?b } WHERE { ?b ex:knows ?a }",
            &opts,
        )
        .unwrap()
        {
            QueryResult::Graph(g) => assert_eq!(g.len(), 2),
            _ => panic!(),
        }
        match query(
            &st,
            "PREFIX ex: <http://ex.org/> ASK { ex:alice ex:knows ex:bob }",
            &opts,
        )
        .unwrap()
        {
            QueryResult::Boolean(b) => assert!(b),
            _ => panic!(),
        }
    }
}
