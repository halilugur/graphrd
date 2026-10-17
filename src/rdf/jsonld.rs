//! JSON-LD 1.1 to RDF: the W3C "JSON-LD 1.1 Processing Algorithms and API" context processing,
//! expansion, node map generation and RDF serialization algorithms, followed step by step.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use super::{resolve_iri, Quad, Term, RDF_FIRST, RDF_NIL, RDF_REIFIES, RDF_REST, RDF_TYPE};
use crate::json::Json;

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const MAX_REMOTE_CONTEXTS: usize = 32;
const MAX_EXPANSION_DEPTH: usize = 256;

type R<T> = Result<T, String>;

fn err<T>(code: &str) -> R<T> {
    Err(format!("JSON-LD error: {code}"))
}

/// Fetches a remote document (context or document): returns the parsed JSON and its final URL.
pub type Loader = dyn Fn(&str) -> R<(Json, String)>;

/// Everything a JSON-LD run may be told: mode, direction, safety, loader.
pub struct Options {
/// The base IRI relative IRIs resolve against.
    pub base: String,
    /// `json-ld-1.0` or `json-ld-1.1` (default).
    pub processing_mode: String,
    /// A context applied before the document's own (`expandContext`).
    pub expand_context: Option<Json>,
    /// `i18n-datatype` or `compound-literal`: how @direction becomes RDF.
    pub rdf_direction: Option<String>,
    /// Allow blank nodes as predicates.
    pub generalized_rdf: bool,
    /// Safe mode: a key that does not expand to an IRI, or a type that is not an absolute or
    /// compact IRI or a defined term, is an error instead of being dropped (or taken as a
    /// relative IRI), so nothing a sender wrote is lost without a word.
    pub safe_mode: bool,
/// How remote contexts (and documents) are fetched.
    pub loader: Rc<Loader>,
}

impl Options {
/// Default options: JSON-LD 1.1, no direction, unsafe, HTTP(S) loader.
    pub fn new(base: &str) -> Options {
        Options { base: base.to_string(), processing_mode: "json-ld-1.1".into(), expand_context: None, rdf_direction: None, generalized_rdf: false, safe_mode: false, loader: Rc::new(default_loader) }
    }
}

/// HTTP(S) fetch; `GRAPHRD_JSONLD_MAP=prefix=dir` serves URLs under `prefix` from local files.
pub fn default_loader(url: &str) -> R<(Json, String)> {
    if let Ok(map) = std::env::var("GRAPHRD_JSONLD_MAP") {
        if let Some((prefix, dir)) = map.split_once('=') {
            if let Some(rest) = url.strip_prefix(prefix) {
                let path = std::path::Path::new(dir).join(rest.split(['#', '?']).next().unwrap_or(""));
                let text = std::fs::read_to_string(&path).map_err(|_| "JSON-LD error: loading document failed".to_string())?;
                let json = Json::parse(&text).map_err(|_| "JSON-LD error: loading document failed".to_string())?;
                return Ok((json, url.to_string()));
            }
        }
    }
    let (_, body) = crate::http::fetch(url, "application/ld+json, application/json;q=0.9", std::time::Duration::from_secs(30)).map_err(|_| "JSON-LD error: loading remote context failed".to_string())?;
    let json = Json::parse(&String::from_utf8_lossy(&body)).map_err(|_| "JSON-LD error: loading remote context failed".to_string())?;
    Ok((json, url.to_string()))
}

// ------------------------------------------------------------------ helpers

fn is_keyword(s: &str) -> bool {
    matches!(
        s,
        "@base" | "@container" | "@context" | "@direction" | "@graph" | "@id" | "@import" | "@included" | "@index" | "@json" | "@language" | "@list"
            | "@nest" | "@none" | "@prefix" | "@propagate" | "@protected" | "@reverse" | "@set" | "@type" | "@value" | "@version" | "@vocab"
    )
}

fn keyword_form(s: &str) -> bool {
    s.len() > 1 && s.starts_with('@') && s[1..].bytes().all(|b| b.is_ascii_alphabetic())
}

fn is_absolute_iri(s: &str) -> bool {
    match s.find(':') {
        Some(i) if i > 0 => s.as_bytes()[0].is_ascii_alphabetic() && s[..i].bytes().all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b)),
        _ => false,
    }
}

fn is_blank(s: &str) -> bool {
    s.starts_with("_:")
}

/// An IRI usable in RDF: absolute, and free of characters IRIs cannot contain.
fn well_formed_iri(s: &str) -> bool {
    // (a fragment cannot contain another '#')
    is_absolute_iri(s) && !s.chars().any(|c| c.is_whitespace() || "<>\"{}|\\^`".contains(c)) && s.matches('#').count() <= 1
}

fn as_array(v: Json) -> Vec<Json> {
    match v {
        Json::Arr(a) => a,
        other => vec![other],
    }
}

fn get<'a>(o: &'a Json, k: &str) -> Option<&'a Json> {
    o.get(k)
}

fn is_map(v: &Json) -> bool {
    matches!(v, Json::Obj(_))
}

fn keys(v: &Json) -> Vec<String> {
    match v {
        Json::Obj(f) => f.iter().map(|(k, _)| k.clone()).collect(),
        _ => vec![],
    }
}

fn remove(v: &mut Json, k: &str) -> Option<Json> {
    if let Json::Obj(f) = v {
        if let Some(i) = f.iter().position(|(n, _)| n == k) {
            return Some(f.remove(i).1);
        }
    }
    None
}

/// Append `value` (array or single) to the array at `key` of `obj`.
fn add_value(obj: &mut Json, key: &str, value: Json, as_list: bool) {
    let items = if as_list { vec![value] } else { as_array(value) };
    if let Json::Obj(f) = obj {
        match f.iter_mut().find(|(k, _)| k == key) {
            Some((_, Json::Arr(a))) => a.extend(items),
            Some((_, other)) => {
                let mut a = vec![std::mem::replace(other, Json::Null)];
                a.extend(items);
                *other = Json::Arr(a);
            }
            None => f.push((key.to_string(), Json::Arr(items))),
        }
    }
}

fn is_value_object(v: &Json) -> bool {
    get(v, "@value").is_some()
}

fn is_list_object(v: &Json) -> bool {
    get(v, "@list").is_some()
}

fn is_graph_object(v: &Json) -> bool {
    is_map(v) && get(v, "@graph").is_some() && keys(v).iter().all(|k| matches!(k.as_str(), "@graph" | "@id" | "@index" | "@context"))
}

// ------------------------------------------------------------------ contexts

#[derive(Clone, Debug, PartialEq, Default)]
struct TermDef {
    iri: Option<String>,
    prefix: bool,
    protected: bool,
    reverse: bool,
    base_url: Option<String>,
    context: Option<Json>,
    container: Vec<String>,
    /// Some(None) = explicitly null.
    direction: Option<Option<String>>,
    index: Option<String>,
    language: Option<Option<String>>,
    nest: Option<String>,
    type_mapping: Option<String>,
}

impl TermDef {
    fn same_except_protected(&self, o: &TermDef) -> bool {
        let mut a = self.clone();
        let mut b = o.clone();
        a.protected = false;
        b.protected = false;
        a.base_url = None;
        b.base_url = None;
        a == b
    }
}

#[derive(Clone)]
struct Context {
    terms: HashMap<String, TermDef>,
    base: Option<String>,
    original_base: Option<String>,
    vocab: Option<String>,
    language: Option<String>,
    direction: Option<String>,
    previous: Option<Box<Context>>,
}

struct Processor<'a> {
    opts: &'a Options,
    contexts: HashMap<String, (Json, String)>,
    /// JSON-LD-star (the triple terms of RDF 1.2): on with `processingMode=json-ld-1.2`
    /// or `@version: 1.2`.
    star: bool,
}

impl Processor<'_> {
    fn mode10(&self) -> bool {
        self.opts.processing_mode == "json-ld-1.0"
    }

    fn load(&mut self, url: &str) -> R<(Json, String)> {
        if let Some(c) = self.contexts.get(url) {
            return Ok(c.clone());
        }
        let r = (self.opts.loader)(url).map_err(|e| if e.contains("loading document failed") { "JSON-LD error: loading remote context failed".into() } else { e })?;
        self.contexts.insert(url.to_string(), r.clone());
        Ok(r)
    }

    /// 4.1.2 Context Processing.
    #[allow(clippy::too_many_arguments)]
    fn process_context(&mut self, active: &Context, local: &Json, base_url: &str, remote: &mut Vec<String>, override_protected: bool, mut propagate: bool, validate_scoped: bool) -> R<Context> {
        let mut result = active.clone();
        if let Some(p) = get(local, "@propagate") {
            match p {
                Json::Bool(b) => propagate = *b,
                _ => return err("invalid @propagate value"),
            }
        }
        if !propagate && result.previous.is_none() {
            result.previous = Some(Box::new(active.clone()));
        }
        for context in as_array(local.clone()) {
            match &context {
                Json::Null => {
                    if !override_protected && result.terms.values().any(|t| t.protected) {
                        return err("invalid context nullification");
                    }
                    let prev = result.clone();
                    result = Context { terms: HashMap::new(), base: active.original_base.clone(), original_base: active.original_base.clone(), vocab: None, language: None, direction: None, previous: None };
                    if !propagate {
                        result.previous = Some(Box::new(prev));
                    }
                    continue;
                }
                Json::Str(s) => {
                    let url = resolve_iri(base_url, s);
                    if !validate_scoped && remote.contains(&url) {
                        continue;
                    }
                    if remote.len() > MAX_REMOTE_CONTEXTS {
                        return err("context overflow");
                    }
                    remote.push(url.clone());
                    let (doc, doc_url) = self.load(&url)?;
                    let Some(ctx) = get(&doc, "@context").cloned() else { return err("invalid remote context") };
                    result = self.process_context(&result, &ctx, &doc_url, &mut remote.clone(), false, true, validate_scoped)?;
                    continue;
                }
                Json::Obj(_) => {}
                _ => return err("invalid local context"),
            }
            let mut context = context;
            if let Some(v) = get(&context, "@version") {
                if !matches!(v, Json::Num(n) if *n == 1.1 || *n == 1.2) {
                    return err("invalid @version value");
                }
                if matches!(v, Json::Num(n) if *n == 1.2) {
                    self.star = true;
                }
                if self.mode10() {
                    return err("processing mode conflict");
                }
            }
            if let Some(imp) = get(&context, "@import").cloned() {
                if self.mode10() {
                    return err("invalid context entry");
                }
                let Json::Str(i) = imp else { return err("invalid @import value") };
                let url = resolve_iri(base_url, &i);
                let (doc, _) = self.load(&url)?;
                let Some(ictx) = get(&doc, "@context").cloned() else { return err("invalid remote context") };
                if !is_map(&ictx) {
                    return err("invalid remote context");
                }
                if get(&ictx, "@import").is_some() {
                    return err("invalid context entry");
                }
                // the importing context's entries override the imported ones
                let mut merged = ictx;
                for k in keys(&context) {
                    let v = get(&context, &k).cloned().unwrap_or(Json::Null);
                    merged.set(&k, v);
                }
                context = merged;
            }
            if let Some(b) = get(&context, "@base") {
                if remote.is_empty() {
                    match b {
                        Json::Null => result.base = None,
                        Json::Str(s) if is_absolute_iri(s) => result.base = Some(s.clone()),
                        Json::Str(s) => match &result.base {
                            Some(base) => result.base = Some(resolve_iri(base, s)),
                            None => return err("invalid base IRI"),
                        },
                        _ => return err("invalid base IRI"),
                    }
                }
            }
            if let Some(v) = get(&context, "@vocab") {
                match v {
                    Json::Null => result.vocab = None,
                    Json::Str(s) => {
                        let mut d = HashMap::new();
                        match self.expand_iri(&mut result, s, true, true, None, &mut d)? {
                            Some(iri) if is_absolute_iri(&iri) || is_blank(&iri) || iri.is_empty() || !keyword_form(&iri) => result.vocab = Some(iri),
                            _ => return err("invalid vocab mapping"),
                        }
                    }
                    _ => return err("invalid vocab mapping"),
                }
            }
            if let Some(v) = get(&context, "@language") {
                match v {
                    Json::Null => result.language = None,
                    Json::Str(s) => result.language = Some(s.to_ascii_lowercase()),
                    _ => return err("invalid default language"),
                }
            }
            if let Some(v) = get(&context, "@direction") {
                if self.mode10() {
                    return err("invalid context entry");
                }
                match v {
                    Json::Null => result.direction = None,
                    Json::Str(s) if s == "ltr" || s == "rtl" => result.direction = Some(s.clone()),
                    _ => return err("invalid base direction"),
                }
            }
            if let Some(v) = get(&context, "@propagate") {
                if self.mode10() {
                    return err("invalid context entry");
                }
                if !matches!(v, Json::Bool(_)) {
                    return err("invalid @propagate value");
                }
            }
            let protected = match get(&context, "@protected") {
                Some(Json::Bool(b)) => *b,
                Some(_) => return err("invalid @protected value"),
                None => false,
            };
            let mut defined: HashMap<String, bool> = HashMap::new();
            for k in keys(&context) {
                if matches!(k.as_str(), "@base" | "@direction" | "@import" | "@language" | "@propagate" | "@protected" | "@version" | "@vocab") {
                    continue;
                }
                self.create_term(&mut result, &context, &k, &mut defined, base_url, protected, override_protected, remote)?;
            }
        }
        Ok(result)
    }

    /// 4.2.2 Create Term Definition.
    #[allow(clippy::too_many_arguments)]
    fn create_term(&mut self, active: &mut Context, local: &Json, term: &str, defined: &mut HashMap<String, bool>, base_url: &str, protected: bool, override_protected: bool, remote: &[String]) -> R<()> {
        match defined.get(term) {
            Some(true) => return Ok(()),
            Some(false) => return err("cyclic IRI mapping"),
            None => {}
        }
        if term.is_empty() {
            return err("invalid term definition");
        }
        defined.insert(term.to_string(), false);
        let mut value = get(local, term).cloned().unwrap_or(Json::Null);
        if term == "@type" && !self.mode10() {
            let ok = match &value {
                Json::Obj(f) => !f.is_empty() && f.iter().all(|(k, v)| (k == "@container" && v == &Json::Str("@set".into())) || (k == "@protected" && matches!(v, Json::Bool(_)))),
                _ => false,
            };
            if !ok {
                return err("keyword redefinition");
            }
        } else if is_keyword(term) {
            return err("keyword redefinition");
        } else if keyword_form(term) {
            defined.insert(term.to_string(), true);
            return Ok(()); // reserved for future keywords: ignored
        }
        let previous = active.terms.remove(term);
        let simple = matches!(value, Json::Str(_));
        value = match value {
            Json::Null => Json::obj().with("@id", Json::Null),
            Json::Str(s) => Json::obj().with("@id", s),
            v @ Json::Obj(_) => v,
            _ => return err("invalid term definition"),
        };
        let mut def = TermDef::default();
        if let Some(p) = get(&value, "@protected") {
            if self.mode10() {
                return err("invalid term definition");
            }
            match p {
                Json::Bool(b) => def.protected = *b,
                _ => return err("invalid @protected value"),
            }
        } else {
            def.protected = protected;
        }
        if let Some(t) = get(&value, "@type") {
            let Json::Str(t) = t else { return err("invalid type mapping") };
            let t = self.expand_iri(active, t, false, true, Some(local), defined)?.unwrap_or_default();
            if (t == "@json" || t == "@none") && self.mode10() {
                return err("invalid type mapping");
            }
            if !matches!(t.as_str(), "@id" | "@json" | "@none" | "@vocab") && !is_absolute_iri(&t) {
                return err("invalid type mapping");
            }
            def.type_mapping = Some(t);
        }
        if let Some(r) = get(&value, "@reverse") {
            if get(&value, "@id").is_some() || get(&value, "@nest").is_some() {
                return err("invalid reverse property");
            }
            let Json::Str(r) = r else { return err("invalid IRI mapping") };
            if keyword_form(r) {
                defined.insert(term.to_string(), true);
                return Ok(());
            }
            let iri = self.expand_iri(active, r, false, true, Some(local), defined)?.unwrap_or_default();
            if !iri.contains(':') {
                return err("invalid IRI mapping");
            }
            def.iri = Some(iri);
            if let Some(c) = get(&value, "@container") {
                match c {
                    Json::Null => {}
                    Json::Str(s) if s == "@set" || s == "@index" => def.container = vec![s.clone()],
                    _ => return err("invalid reverse property"),
                }
            }
            def.reverse = true;
            active.terms.insert(term.to_string(), def);
            defined.insert(term.to_string(), true);
            return Ok(());
        }
        let id = get(&value, "@id").cloned();
        if id.is_some() && id != Some(Json::Str(term.to_string())) {
            match id.unwrap() {
                Json::Null => def.iri = None,
                Json::Str(i) => {
                    if !is_keyword(&i) && keyword_form(&i) {
                        defined.insert(term.to_string(), true);
                        return Ok(());
                    }
                    let iri = self.expand_iri(active, &i, false, true, Some(local), defined)?;
                    let iri = match iri {
                        Some(x) if is_keyword(&x) || is_absolute_iri(&x) || is_blank(&x) || x.contains(':') => x,
                        _ => return err("invalid IRI mapping"),
                    };
                    if iri == "@context" {
                        return err("invalid keyword alias");
                    }
                    def.iri = Some(iri.clone());
                    let inner_colon = term.find(':').is_some_and(|p| p > 0 && p < term.len() - 1);
                    if inner_colon || term.contains('/') {
                        defined.insert(term.to_string(), true);
                        let expanded = self.expand_iri(active, term, false, true, Some(local), defined)?;
                        if expanded.as_deref() != Some(iri.as_str()) {
                            return err("invalid IRI mapping");
                        }
                    }
                    if !term.contains(':') && !term.contains('/') && simple && (iri.ends_with([':', '/', '?', '#', '[', ']', '@']) || is_blank(&iri)) {
                        def.prefix = true;
                    }
                }
                _ => return err("invalid IRI mapping"),
            }
        } else if term[1..].contains(':') {
            let (prefix, suffix) = term.split_once(':').unwrap();
            if get(local, prefix).is_some() {
                self.create_term(active, local, prefix, defined, base_url, protected, override_protected, remote)?;
            }
            def.iri = Some(match active.terms.get(prefix).and_then(|d| d.iri.clone()) {
                Some(p) => format!("{p}{suffix}"),
                None => term.to_string(),
            });
        } else if term.contains('/') {
            let iri = self.expand_iri(active, term, false, true, Some(local), defined)?.unwrap_or_default();
            if !is_absolute_iri(&iri) {
                return err("invalid IRI mapping");
            }
            def.iri = Some(iri);
        } else if term == "@type" {
            def.iri = Some("@type".into());
        } else {
            match &active.vocab {
                Some(v) => def.iri = Some(format!("{v}{term}")),
                None => return err("invalid IRI mapping"),
            }
        }
        if let Some(c) = get(&value, "@container") {
            let list: Vec<String> = match c {
                Json::Str(s) => vec![s.clone()],
                Json::Arr(a) if !self.mode10() => {
                    let mut out = Vec::new();
                    for x in a {
                        match x {
                            Json::Str(s) => out.push(s.clone()),
                            _ => return err("invalid container mapping"),
                        }
                    }
                    out
                }
                _ => return err("invalid container mapping"),
            };
            if !valid_container(&list, self.mode10()) {
                return err("invalid container mapping");
            }
            if list.iter().any(|c| c == "@type") {
                match def.type_mapping.as_deref() {
                    None => def.type_mapping = Some("@id".into()),
                    Some("@id" | "@vocab") => {}
                    _ => return err("invalid type mapping"),
                }
            }
            def.container = list;
        }
        if let Some(i) = get(&value, "@index") {
            if self.mode10() || !def.container.iter().any(|c| c == "@index") {
                return err("invalid term definition");
            }
            let Json::Str(i) = i else { return err("invalid term definition") };
            let e = self.expand_iri(active, i, false, true, Some(local), defined)?.unwrap_or_default();
            if is_keyword(&e) || !is_absolute_iri(&e) {
                return err("invalid term definition");
            }
            def.index = Some(i.clone());
        }
        if let Some(c) = get(&value, "@context") {
            if self.mode10() {
                return err("invalid term definition");
            }
            let mut rc = remote.to_vec();
            if self.process_context(active, c, base_url, &mut rc, true, true, false).is_err() {
                return err("invalid scoped context");
            }
            def.context = Some(c.clone());
            def.base_url = Some(base_url.to_string());
        }
        if get(&value, "@type").is_none() {
            if let Some(l) = get(&value, "@language") {
                match l {
                    Json::Null => def.language = Some(None),
                    Json::Str(s) => def.language = Some(Some(s.to_ascii_lowercase())),
                    _ => return err("invalid language mapping"),
                }
            }
            if let Some(d) = get(&value, "@direction") {
                match d {
                    Json::Null => def.direction = Some(None),
                    Json::Str(s) if s == "ltr" || s == "rtl" => def.direction = Some(Some(s.clone())),
                    _ => return err("invalid base direction"),
                }
            }
        }
        if let Some(n) = get(&value, "@nest") {
            if self.mode10() {
                return err("invalid term definition");
            }
            match n {
                Json::Str(s) if s == "@nest" || !is_keyword(s) => def.nest = Some(s.clone()),
                _ => return err("invalid @nest value"),
            }
        }
        if let Some(p) = get(&value, "@prefix") {
            if self.mode10() || term.contains(':') || term.contains('/') {
                return err("invalid term definition");
            }
            match p {
                Json::Bool(b) => def.prefix = *b,
                _ => return err("invalid @prefix value"),
            }
            if def.prefix && def.iri.as_deref().is_some_and(is_keyword) {
                return err("invalid term definition");
            }
        }
        for k in keys(&value) {
            if !matches!(k.as_str(), "@id" | "@reverse" | "@container" | "@context" | "@direction" | "@index" | "@language" | "@nest" | "@prefix" | "@protected" | "@type") {
                return err("invalid term definition");
            }
        }
        if let Some(prev) = previous {
            if !override_protected && prev.protected {
                if !def.same_except_protected(&prev) {
                    return err("protected term redefinition");
                }
                def = prev;
            }
        }
        active.terms.insert(term.to_string(), def);
        defined.insert(term.to_string(), true);
        Ok(())
    }

    /// 4.3.2 IRI Expansion. `None` stands for a null mapping.
    fn expand_iri(&mut self, active: &mut Context, value: &str, document_relative: bool, vocab: bool, local: Option<&Json>, defined: &mut HashMap<String, bool>) -> R<Option<String>> {
        if is_keyword(value) {
            return Ok(Some(value.to_string()));
        }
        if keyword_form(value) {
            return Ok(None);
        }
        if let Some(l) = local {
            if get(l, value).is_some() && defined.get(value) != Some(&true) {
                let base = active.base.clone().unwrap_or_default();
                self.create_term(active, l, value, defined, &base, false, false, &[])?;
            }
        }
        if vocab {
            if let Some(d) = active.terms.get(value) {
                return Ok(d.iri.clone());
            }
        }
        if let Some(i) = value[1.min(value.len())..].find(':').map(|i| i + 1) {
            let (prefix, suffix) = (&value[..i], &value[i + 1..]);
            if prefix == "_" || suffix.starts_with("//") {
                return Ok(Some(value.to_string()));
            }
            if let Some(l) = local {
                if get(l, prefix).is_some() && defined.get(prefix) != Some(&true) {
                    let base = active.base.clone().unwrap_or_default();
                    self.create_term(active, l, prefix, defined, &base, false, false, &[])?;
                }
            }
            if let Some(d) = active.terms.get(prefix) {
                if let (Some(iri), true) = (&d.iri, d.prefix) {
                    return Ok(Some(format!("{iri}{suffix}")));
                }
            }
            if is_absolute_iri(value) {
                return Ok(Some(value.to_string()));
            }
        }
        if vocab {
            if let Some(v) = &active.vocab {
                return Ok(Some(format!("{v}{value}")));
            }
        }
        if document_relative {
            return Ok(Some(match &active.base {
                Some(b) => resolve_iri(b, value),
                None => value.to_string(),
            }));
        }
        Ok(Some(value.to_string()))
    }

    fn expand_iri_simple(&mut self, active: &Context, value: &str, document_relative: bool, vocab: bool) -> R<Option<String>> {
        let mut a = active.clone();
        let mut d = HashMap::new();
        self.expand_iri(&mut a, value, document_relative, vocab, None, &mut d)
    }

    // -------------------------------------------------------------- expansion

    /// 5.3.2 Value Expansion.
    fn value_expand(&mut self, active: &Context, prop: Option<&str>, value: &Json) -> R<Json> {
        let def = prop.and_then(|p| active.terms.get(p)).cloned();
        let tm = def.as_ref().and_then(|d| d.type_mapping.clone());
        if let Json::Str(s) = value {
            if tm.as_deref() == Some("@id") {
                return Ok(Json::obj().with("@id", self.expand_iri_simple(active, s, true, false)?.map(Json::Str).unwrap_or(Json::Null)));
            }
            if tm.as_deref() == Some("@vocab") {
                return Ok(Json::obj().with("@id", self.expand_iri_simple(active, s, true, true)?.map(Json::Str).unwrap_or(Json::Null)));
            }
        }
        let mut result = Json::obj().with("@value", value.clone());
        match tm.as_deref() {
            Some(t) if t != "@id" && t != "@vocab" && t != "@none" => result.set("@type", t),
            _ => {
                if matches!(value, Json::Str(_)) {
                    let lang = match def.as_ref().and_then(|d| d.language.clone()) {
                        Some(l) => l,
                        None => active.language.clone(),
                    };
                    let dir = match def.as_ref().and_then(|d| d.direction.clone()) {
                        Some(d) => d,
                        None => active.direction.clone(),
                    };
                    if let Some(l) = lang {
                        result.set("@language", l);
                    }
                    if let Some(d) = dir {
                        result.set("@direction", d);
                    }
                }
            }
        }
        Ok(result)
    }

    /// 5.1.2 Expansion Algorithm.
    fn expand(&mut self, active: &Context, prop: Option<&str>, element: &Json, base_url: &str, from_map: bool, depth: usize) -> R<Json> {
        if depth > MAX_EXPANSION_DEPTH {
            return err("expansion too deep");
        }
        if matches!(element, Json::Null) {
            return Ok(Json::Null);
        }
        let prop_def = prop.and_then(|p| active.terms.get(p)).cloned();
        let property_scoped = prop_def.as_ref().and_then(|d| d.context.clone());
        match element {
            Json::Arr(items) => {
                let mut result = Vec::new();
                for item in items {
                    let mut e = self.expand(active, prop, item, base_url, from_map, depth + 1)?;
                    if prop_def.as_ref().is_some_and(|d| d.container.iter().any(|c| c == "@list")) {
                        if let Json::Arr(a) = e {
                            e = Json::obj().with("@list", Json::Arr(a));
                        }
                    }
                    match e {
                        Json::Arr(a) => result.extend(a),
                        Json::Null => {}
                        other => result.push(other),
                    }
                }
                return Ok(Json::Arr(result));
            }
            Json::Obj(_) => {}
            scalar => {
                if prop.is_none() || prop == Some("@graph") {
                    return Ok(Json::Null);
                }
                if let Some(psc) = &property_scoped {
                    let bu = prop_def.as_ref().and_then(|d| d.base_url.clone()).unwrap_or_else(|| base_url.to_string());
                    let a = self.process_context(active, psc, &bu, &mut vec![], false, true, true)?;
                    return self.value_expand(&a, prop, scalar);
                }
                return self.value_expand(active, prop, scalar);
            }
        }
        let mut active = active.clone();
        // 7: revert a non-propagated context
        if active.previous.is_some() && !from_map {
            let mut has_value = false;
            let mut only_id = true;
            for k in keys(element) {
                let e = self.expand_iri_simple(&active, &k, false, true)?;
                if e.as_deref() == Some("@value") {
                    has_value = true;
                }
                if e.as_deref() != Some("@id") {
                    only_id = false;
                }
            }
            let single_id = only_id && keys(element).len() == 1;
            if !has_value && !single_id {
                active = *active.previous.clone().unwrap();
            }
        }
        if let Some(psc) = &property_scoped {
            let bu = prop_def.as_ref().and_then(|d| d.base_url.clone()).unwrap_or_else(|| base_url.to_string());
            active = self.process_context(&active, psc, &bu, &mut vec![], true, true, true)?;
        }
        if let Some(c) = get(element, "@context") {
            active = self.process_context(&active, c, base_url, &mut vec![], false, true, true)?;
        }
        let type_scoped = active.clone();
        let mut input_type: Option<String> = None;
        let mut sorted_keys = keys(element);
        sorted_keys.sort();
        for k in &sorted_keys {
            if self.expand_iri_simple(&active, k, false, true)?.as_deref() == Some("@type") {
                let mut types: Vec<String> = as_array(get(element, k).cloned().unwrap_or(Json::Null)).into_iter().filter_map(|t| t.as_str().map(String::from)).collect();
                types.sort();
                for t in &types {
                    if let Some(c) = type_scoped.terms.get(t).and_then(|d| d.context.clone()) {
                        let bu = type_scoped.terms.get(t).and_then(|d| d.base_url.clone()).unwrap_or_else(|| base_url.to_string());
                        active = self.process_context(&active, &c, &bu, &mut vec![], false, false, true)?;
                    }
                }
                if let Some(last) = types.last() {
                    input_type = self.expand_iri_simple(&active, last, false, true)?;
                }
                break;
            }
        }
        let mut result = Json::obj();
        let mut nests: Vec<String> = Vec::new();
        self.expand_entries(&active, &type_scoped, prop, element, base_url, &input_type, &mut result, &mut nests, depth)?;
        // 14: nested properties, each with the scoped context of its nesting term
        self.expand_nests(&active, &type_scoped, prop, element, base_url, &input_type, &mut result, &nests, depth)?;
        // 15: value objects
        if get(&result, "@value").is_some() {
            for k in keys(&result) {
                if !matches!(k.as_str(), "@annotation" | "@direction" | "@index" | "@language" | "@type" | "@value") {
                    return err("invalid value object");
                }
            }
            if get(&result, "@type").is_some() && (get(&result, "@language").is_some() || get(&result, "@direction").is_some()) {
                return err("invalid value object");
            }
            if get(&result, "@type") == Some(&Json::Str("@json".into())) {
                // any JSON value
            } else {
                let v = get(&result, "@value").unwrap();
                if matches!(v, Json::Null) || v == &Json::Arr(vec![]) {
                    return Ok(Json::Null);
                }
                if !matches!(v, Json::Str(_)) && get(&result, "@language").is_some() {
                    return err("invalid language-tagged value");
                }
                if matches!(v, Json::Arr(_) | Json::Obj(_)) {
                    return err("invalid value object value");
                }
                if let Some(t) = get(&result, "@type") {
                    match t {
                        Json::Str(t) if well_formed_iri(t) => {}
                        _ => return err("invalid typed value"),
                    }
                }
            }
        } else if let Some(t) = get(&result, "@type").cloned() {
            if !matches!(t, Json::Arr(_)) {
                result.set("@type", Json::Arr(vec![t]));
            }
        } else if get(&result, "@set").is_some() || get(&result, "@list").is_some() {
            if keys(&result).len() > 2 || keys(&result).iter().any(|k| !matches!(k.as_str(), "@set" | "@list" | "@index")) {
                return err("invalid set or list object");
            }
            if let Some(s) = remove(&mut result, "@set") {
                result = s;
            }
        }
        if keys(&result) == ["@language"] {
            return Ok(Json::Null);
        }
        if prop.is_none() || prop == Some("@graph") {
            if is_map(&result) && (keys(&result).is_empty() || get(&result, "@value").is_some() || get(&result, "@list").is_some()) {
                return Ok(Json::Null);
            }
            if is_map(&result) && keys(&result) == ["@id"] {
                return Ok(Json::Null);
            }
        }
        Ok(result)
    }

    /// Step 14: expand the maps under `nests` (keys of `element` aliased to @nest) into `result`.
    #[allow(clippy::too_many_arguments)]
    fn expand_nests(&mut self, active: &Context, type_scoped: &Context, prop: Option<&str>, element: &Json, base_url: &str, input_type: &Option<String>, result: &mut Json, nests: &[String], depth: usize) -> R<()> {
        let mut sorted = nests.to_vec();
        sorted.sort();
        for key in sorted {
            let mut nest_ctx = active.clone();
            if let Some(d) = active.terms.get(&key) {
                if let Some(c) = d.context.clone() {
                    let bu = d.base_url.clone().unwrap_or_else(|| base_url.to_string());
                    nest_ctx = self.process_context(active, &c, &bu, &mut vec![], true, true, true)?;
                }
            }
            for nested in as_array(get(element, &key).cloned().unwrap_or(Json::Null)) {
                if !is_map(&nested) {
                    return err("invalid @nest value");
                }
                for k in keys(&nested) {
                    if self.expand_iri_simple(&nest_ctx, &k, false, true)?.as_deref() == Some("@value") {
                        return err("invalid @nest value");
                    }
                }
                let mut inner = Vec::new();
                self.expand_entries(&nest_ctx, type_scoped, prop, &nested, base_url, input_type, result, &mut inner, depth)?;
                self.expand_nests(&nest_ctx, type_scoped, prop, &nested, base_url, input_type, result, &inner, depth)?;
            }
        }
        Ok(())
    }

    /// Step 13 of the Expansion Algorithm, for one (possibly nested) map.
    #[allow(clippy::too_many_arguments)]
    fn expand_entries(&mut self, active: &Context, type_scoped: &Context, prop: Option<&str>, element: &Json, base_url: &str, input_type: &Option<String>, result: &mut Json, nests: &mut Vec<String>, depth: usize) -> R<()> {
        let mut entries = keys(element);
        entries.sort();
        for key in entries {
            let value = get(element, &key).cloned().unwrap_or(Json::Null);
            if key == "@context" {
                continue;
            }
            if self.star && key == "@annotation" {
                if prop.is_none() {
                    return err("invalid @annotation placement");
                }
                if !is_map(&value) {
                    return err("invalid @annotation value");
                }
                let expanded = self.expand(active, None, &value, base_url, false, depth + 1)?;
                if is_value_object(&expanded) || get(&expanded, "@id").is_some() {
                    return err("invalid @annotation value");
                }
                result.set("@annotation", expanded);
                continue;
            }
            let Some(eprop) = self.expand_iri_simple(active, &key, false, true)? else {
                if self.opts.safe_mode {
                    return Err(format!("JSON-LD error: \"{key}\" is not defined by the context, so it would be dropped"));
                }
                continue;
            };
            if !eprop.contains(':') && !is_keyword(&eprop) {
                if self.opts.safe_mode {
                    return Err(format!("JSON-LD error: \"{key}\" is not defined by the context, so it would be dropped"));
                }
                continue;
            }
            if is_keyword(&eprop) {
                if prop == Some("@reverse") {
                    return err("invalid reverse property map");
                }
                if get(result, &eprop).is_some() && !(eprop == "@included" || eprop == "@type") {
                    return err("colliding keywords");
                }
                let expanded: Json = match eprop.as_str() {
                    "@id" => match &value {
                        Json::Str(s) => self.expand_iri_simple(active, s, true, false)?.map(Json::Str).unwrap_or(Json::Null),
                        Json::Obj(_) if self.star => {
                            // JSON-LD-star: a node object as @id denotes the triple it
                            // describes (an RDF 1.2 triple term)
                            self.expand(active, None, &value, base_url, false, depth + 1)?
                        }
                        _ => return err("invalid @id value"),
                    },
                    "@type" => {
                        let mut out = Vec::new();
                        match &value {
                            Json::Str(s) => out.push(s.clone()),
                            Json::Arr(a) => {
                                for x in a {
                                    match x {
                                        Json::Str(s) => out.push(s.clone()),
                                        _ => return err("invalid type value"),
                                    }
                                }
                            }
                            _ => return err("invalid type value"),
                        }
                        let mut ex = Vec::new();
                        for t in out {
                            if self.opts.safe_mode && !self.expand_iri_simple(type_scoped, &t, false, true)?.is_some_and(|i| i.contains(':')) {
                                return Err(format!("JSON-LD error: the type \"{t}\" is not defined by the context"));
                            }
                            if let Some(i) = self.expand_iri_simple(type_scoped, &t, true, true)? {
                                ex.push(Json::Str(i));
                            }
                        }
                        let ex = if matches!(value, Json::Str(_)) && ex.len() == 1 { ex.pop().unwrap() } else { Json::Arr(ex) };
                        if let Some(existing) = get(result, "@type").cloned() {
                            let mut all = as_array(existing);
                            all.extend(as_array(ex));
                            Json::Arr(all)
                        } else {
                            ex
                        }
                    }
                    "@graph" => Json::Arr(as_array(self.expand(active, Some("@graph"), &value, base_url, false, depth + 1)?)),
                    "@included" => {
                        if self.mode10() {
                            continue;
                        }
                        let items = as_array(self.expand(active, None, &value, base_url, false, depth + 1)?);
                        for i in &items {
                            if !is_map(i) || is_value_object(i) || is_list_object(i) || get(i, "@set").is_some() {
                                return err("invalid @included value");
                            }
                        }
                        let mut all = get(result, "@included").cloned().map(as_array).unwrap_or_default();
                        all.extend(items);
                        Json::Arr(all)
                    }
                    "@value" => {
                        if input_type.as_deref() == Some("@json") {
                            if self.mode10() {
                                return err("invalid value object value");
                            }
                            value.clone()
                        } else {
                            if matches!(value, Json::Arr(_) | Json::Obj(_)) {
                                return err("invalid value object value");
                            }
                            if matches!(value, Json::Null) {
                                result.set("@value", Json::Null);
                                continue;
                            }
                            value.clone()
                        }
                    }
                    "@language" => match &value {
                        Json::Str(s) => Json::Str(s.to_ascii_lowercase()),
                        _ => return err("invalid language-tagged string"),
                    },
                    "@direction" => {
                        if self.mode10() {
                            continue;
                        }
                        match &value {
                            Json::Str(s) if s == "ltr" || s == "rtl" => value.clone(),
                            _ => return err("invalid base direction"),
                        }
                    }
                    "@index" => match &value {
                        Json::Str(_) => value.clone(),
                        _ => return err("invalid @index value"),
                    },
                    "@list" => {
                        if prop.is_none() || prop == Some("@graph") {
                            continue;
                        }
                        Json::Arr(as_array(self.expand(active, prop, &value, base_url, false, depth + 1)?))
                    }
                    "@set" => self.expand(active, prop, &value, base_url, false, depth + 1)?,
                    "@reverse" => {
                        if !is_map(&value) {
                            return err("invalid @reverse value");
                        }
                        let ex = self.expand(active, Some("@reverse"), &value, base_url, false, depth + 1)?;
                        if let Some(rev) = get(&ex, "@reverse") {
                            for k in keys(rev) {
                                add_value(result, &k, get(rev, &k).cloned().unwrap_or(Json::Null), false);
                            }
                        }
                        let others: Vec<String> = keys(&ex).into_iter().filter(|k| k != "@reverse").collect();
                        if !others.is_empty() {
                            if get(result, "@reverse").is_none() {
                                result.set("@reverse", Json::obj());
                            }
                            for k in others {
                                for item in as_array(get(&ex, &k).cloned().unwrap_or(Json::Null)) {
                                    if is_value_object(&item) || is_list_object(&item) {
                                        return err("invalid reverse property value");
                                    }
                                    if let Json::Obj(f) = result {
                                        if let Some((_, rm)) = f.iter_mut().find(|(n, _)| n == "@reverse") {
                                            add_value(rm, &k, item, false);
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    "@nest" => {
                        nests.push(key.clone());
                        continue;
                    }
                    _ => continue,
                };
                if !(matches!(expanded, Json::Null) && eprop == "@value" && input_type.as_deref() != Some("@json")) {
                    result.set(&eprop, expanded);
                }
                continue;
            }
            let def = active.terms.get(&key).cloned().unwrap_or_default();
            let container = def.container.clone();
            let has = |c: &str| container.iter().any(|x| x == c);
            let mut expanded = if def.type_mapping.as_deref() == Some("@json") {
                Json::obj().with("@value", value.clone()).with("@type", "@json")
            } else if has("@language") && is_map(&value) {
                let mut out = Vec::new();
                let dir = match &def.direction {
                    Some(d) => d.clone(),
                    None => active.direction.clone(),
                };
                let mut langs = keys(&value);
                langs.sort();
                for lang in langs {
                    for item in as_array(get(&value, &lang).cloned().unwrap_or(Json::Null)) {
                        match item {
                            Json::Null => continue,
                            Json::Str(s) => {
                                let mut v = Json::obj().with("@value", s);
                                let none = lang == "@none" || self.expand_iri_simple(active, &lang, false, true)?.as_deref() == Some("@none");
                                if !none {
                                    v.set("@language", lang.to_ascii_lowercase());
                                }
                                if let Some(d) = &dir {
                                    v.set("@direction", d.as_str());
                                }
                                out.push(v);
                            }
                            _ => return err("invalid language map value"),
                        }
                    }
                }
                Json::Arr(out)
            } else if (has("@index") || has("@type") || has("@id")) && is_map(&value) {
                let index_key = def.index.clone().unwrap_or_else(|| "@index".into());
                let mut out = Vec::new();
                let mut indexes = keys(&value);
                indexes.sort();
                for index in indexes {
                    let mut map_ctx = if has("@id") || has("@type") { active.previous.as_deref().cloned().unwrap_or_else(|| active.clone()) } else { active.clone() };
                    if has("@type") {
                        if let Some(c) = map_ctx.terms.get(&index).and_then(|d| d.context.clone()) {
                            let bu = map_ctx.terms.get(&index).and_then(|d| d.base_url.clone()).unwrap_or_else(|| base_url.to_string());
                            map_ctx = self.process_context(&map_ctx, &c, &bu, &mut vec![], false, true, true)?;
                        }
                    }
                    let expanded_index = self.expand_iri_simple(active, &index, false, true)?;
                    let none = expanded_index.as_deref() == Some("@none");
                    let iv = Json::Arr(as_array(get(&value, &index).cloned().unwrap_or(Json::Null)));
                    let items = as_array(self.expand(&map_ctx, Some(&key), &iv, base_url, true, depth + 1)?);
                    for mut item in items {
                        if has("@graph") && !is_graph_object(&item) {
                            item = Json::obj().with("@graph", Json::Arr(as_array(item)));
                        }
                        if has("@index") && index_key != "@index" && !none {
                            let re = self.value_expand(active, Some(&index_key), &Json::Str(index.clone()))?;
                            let eik = self.expand_iri_simple(active, &index_key, false, true)?.unwrap_or_default();
                            let mut vals = vec![re];
                            if let Some(existing) = get(&item, &eik).cloned() {
                                vals.extend(as_array(existing));
                            }
                            if is_value_object(&item) {
                                return err("invalid value object");
                            }
                            item.set(&eik, Json::Arr(vals));
                        } else if has("@index") && get(&item, "@index").is_none() && !none {
                            item.set("@index", index.as_str());
                        } else if has("@id") && get(&item, "@id").is_none() && !none {
                            let id = self.expand_iri_simple(active, &index, true, false)?.map(Json::Str).unwrap_or(Json::Null);
                            item.set("@id", id);
                        } else if has("@type") && !none {
                            let mut types = vec![Json::Str(expanded_index.clone().unwrap_or_default())];
                            if let Some(t) = get(&item, "@type").cloned() {
                                types.extend(as_array(t));
                            }
                            item.set("@type", Json::Arr(types));
                        }
                        out.push(item);
                    }
                }
                Json::Arr(out)
            } else {
                self.expand(active, Some(&key), &value, base_url, false, depth + 1)?
            };
            if matches!(expanded, Json::Null) {
                continue;
            }
            if has("@list") && !is_list_object(&expanded) {
                expanded = Json::obj().with("@list", Json::Arr(as_array(expanded)));
            }
            if has("@graph") && !has("@id") && !has("@index") {
                expanded = Json::Arr(as_array(expanded).into_iter().map(|ev| Json::obj().with("@graph", Json::Arr(as_array(ev)))).collect());
            }
            if def.reverse {
                if get(result, "@reverse").is_none() {
                    result.set("@reverse", Json::obj());
                }
                for item in as_array(expanded) {
                    if is_value_object(&item) || is_list_object(&item) {
                        return err("invalid reverse property value");
                    }
                    if let Json::Obj(f) = result {
                        if let Some((_, rm)) = f.iter_mut().find(|(n, _)| n == "@reverse") {
                            add_value(rm, &eprop, item, false);
                        }
                    }
                }
            } else {
                add_value(result, &eprop, expanded, false);
            }
        }
        Ok(())
    }
}

fn valid_container(list: &[String], mode10: bool) -> bool {
    let has = |c: &str| list.iter().any(|x| x == c);
    let all_known = list.iter().all(|c| matches!(c.as_str(), "@graph" | "@id" | "@index" | "@language" | "@list" | "@set" | "@type"));
    if !all_known {
        return false;
    }
    if mode10 {
        return list.len() == 1 && matches!(list[0].as_str(), "@list" | "@set" | "@index" | "@language");
    }
    if list.len() == 1 {
        return true;
    }
    if has("@list") {
        return false;
    }
    if has("@graph") {
        return list.iter().all(|c| matches!(c.as_str(), "@graph" | "@id" | "@index" | "@set")) && !(has("@id") && has("@index"));
    }
    has("@set") && list.len() == 2
}

// ------------------------------------------------------------------ node map

struct NodeMapper {
    counter: usize,
    labels: HashMap<String, String>,
    /// graph -> subject -> node object
    map: BTreeMap<String, BTreeMap<String, Json>>,
}

impl NodeMapper {
    fn bnode(&mut self, old: Option<&str>) -> String {
        if let Some(o) = old {
            if let Some(n) = self.labels.get(o) {
                return n.clone();
            }
        }
        let n = format!("_:b{}", self.counter);
        self.counter += 1;
        if let Some(o) = old {
            self.labels.insert(o.to_string(), n.clone());
        }
        n
    }

    fn node<'a>(&'a mut self, graph: &str, id: &str) -> &'a mut Json {
        self.map.entry(graph.to_string()).or_default().entry(id.to_string()).or_insert_with(|| Json::obj().with("@id", id))
    }

    /// 7.2.2 Node Map Generation. `list` collects items of an enclosing @list.
    fn generate(&mut self, element: &Json, graph: &str, subject: Option<&Json>, property: Option<&str>, list: &mut Option<Vec<Json>>) -> R<()> {
        if let Json::Arr(items) = element {
            for item in items {
                self.generate(item, graph, subject, property, list)?;
            }
            return Ok(());
        }
        let mut element = element.clone();
        if let Some(Json::Arr(types)) = get(&element, "@type").cloned() {
            let mapped: Vec<Json> = types.into_iter().map(|t| match t {
                Json::Str(s) if is_blank(&s) => Json::Str(self.bnode(Some(&s))),
                other => other,
            }).collect();
            element.set("@type", Json::Arr(mapped));
        }
        if is_value_object(&element) {
            match list {
                Some(l) => l.push(element),
                None => {
                    let (s, p) = (subject.and_then(Json::as_str).unwrap_or(""), property.unwrap_or(""));
                    let node = self.node(graph, s);
                    let existing = get(node, p).cloned().map(as_array).unwrap_or_default();
                    if !existing.contains(&element) {
                        add_value(node, p, element, true);
                    }
                }
            }
            return Ok(());
        }
        if let Some(items) = get(&element, "@list").cloned() {
            let mut result = Some(Vec::new());
            self.generate(&items, graph, subject, property, &mut result)?;
            let lo = Json::obj().with("@list", Json::Arr(result.unwrap()));
            match list {
                Some(l) => l.push(lo),
                None => {
                    let (s, p) = (subject.and_then(Json::as_str).unwrap_or("").to_string(), property.unwrap_or("").to_string());
                    add_value(self.node(graph, &s), &p, lo, true);
                }
            }
            return Ok(());
        }
        // node object
        let id = match get(&element, "@id") {
            Some(Json::Str(s)) if is_blank(s) => self.bnode(Some(s)),
            Some(Json::Str(s)) => s.clone(),
            Some(Json::Null) => return Ok(()), // an id that expanded to null: dropped
            _ => self.bnode(None),
        };
        self.node(graph, &id);
        if let Some(Json::Obj(subj)) = subject {
            // reverse property: the referencing node gets `property` pointing at this node
            let _ = subj;
            let s = get(subject.unwrap(), "@id").and_then(Json::as_str).unwrap_or("").to_string();
            let reference = Json::obj().with("@id", s);
            let p = property.unwrap_or("");
            let node = self.node(graph, &id);
            let existing = get(node, p).cloned().map(as_array).unwrap_or_default();
            if !existing.contains(&reference) {
                add_value(node, p, reference, true);
            }
        } else if let Some(p) = property {
            let reference = Json::obj().with("@id", id.as_str());
            match list {
                Some(l) => l.push(reference),
                None => {
                    let s = subject.and_then(Json::as_str).unwrap_or("").to_string();
                    let node = self.node(graph, &s);
                    let existing = get(node, p).cloned().map(as_array).unwrap_or_default();
                    if !existing.contains(&reference) {
                        add_value(node, p, reference, true);
                    }
                }
            }
        }
        if let Some(Json::Arr(types)) = get(&element, "@type") {
            for t in types.clone() {
                let node = self.node(graph, &id);
                let existing = get(node, "@type").cloned().map(as_array).unwrap_or_default();
                if !existing.contains(&t) {
                    add_value(node, "@type", t, true);
                }
            }
        }
        if let Some(Json::Str(idx)) = get(&element, "@index") {
            let node = self.node(graph, &id);
            match get(node, "@index") {
                Some(Json::Str(e)) if e != idx => return err("conflicting indexes"),
                _ => node.set("@index", idx.as_str()),
            }
        }
        if let Some(rev) = get(&element, "@reverse").cloned() {
            let referenced = Json::obj().with("@id", id.as_str());
            for k in keys(&rev) {
                for v in as_array(get(&rev, &k).cloned().unwrap_or(Json::Null)) {
                    self.generate(&v, graph, Some(&referenced), Some(&k), &mut None)?;
                }
            }
        }
        if let Some(g) = get(&element, "@graph").cloned() {
            self.map.entry(id.clone()).or_default();
            self.generate(&g, &id, None, None, &mut None)?;
        }
        if let Some(inc) = get(&element, "@included").cloned() {
            self.generate(&inc, graph, None, None, &mut None)?;
        }
        let mut props = keys(&element);
        props.sort();
        let subj = Json::Str(id.clone());
        for p in props {
            if matches!(p.as_str(), "@id" | "@type" | "@index" | "@reverse" | "@graph" | "@included" | "@context") {
                continue;
            }
            let value = get(&element, &p).cloned().unwrap_or(Json::Null);
            let p = if is_blank(&p) { self.bnode(Some(&p)) } else { p };
            {
                let node = self.node(graph, &id);
                if get(node, &p).is_none() {
                    node.set(&p, Json::Arr(vec![]));
                }
            }
            self.generate(&value, graph, Some(&subj), Some(&p), &mut None)?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ to RDF

fn canonical_double(f: f64) -> String {
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0E0".into() } else { "0.0E0".into() };
    }
    let s = format!("{f:E}");
    let (m, e) = s.split_once('E').unwrap();
    let m = if m.contains('.') { m.to_string() } else { format!("{m}.0") };
    format!("{m}E{e}")
}

/// ECMAScript Number::toString (shortest round-trip digits; exponent form outside 1e-6..1e21).
fn es_number(n: f64) -> String {
    if n == 0.0 {
        return "0".into();
    }
    let a = n.abs();
    if (1e-6..1e21).contains(&a) {
        let s = format!("{n}");
        return s.strip_suffix(".0").map(String::from).unwrap_or(s);
    }
    let s = format!("{n:e}");
    let (m, e) = s.split_once('e').unwrap();
    if e.starts_with('-') { format!("{m}e{e}") } else { format!("{m}e+{e}") }
}

/// JSON Canonicalization Scheme (RFC 8785), for rdf:JSON literals.
fn jcs(v: &Json, out: &mut String) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Num(n) => out.push_str(&es_number(*n)),
        Json::Str(s) => {
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\u{8}' => out.push_str("\\b"),
                    '\u{c}' => out.push_str("\\f"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        Json::Arr(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                jcs(x, out);
            }
            out.push(']');
        }
        Json::Obj(f) => {
            let mut fields: Vec<&(String, Json)> = f.iter().collect();
            // keys sorted by UTF-16 code units
            fields.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            out.push('{');
            for (i, (k, x)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                jcs(&Json::Str(k.clone()), out);
                out.push(':');
                jcs(x, out);
            }
            out.push('}');
        }
    }
}

struct Rdf<'a> {
    opts: &'a Options,
    out: Vec<Quad>,
    bnodes: usize,
    /// The embedded nodes minted by the pre-pass (`\u{1}` ids): each is the triple term
    /// it describes. Shared with `Star`.
    embeds: HashMap<String, Json>,
}

impl Rdf<'_> {
    fn node_term(&mut self, id: &str) -> Option<Term> {
        if let Some(name) = id.strip_prefix('\u{1}') {
            let node = self.embeds.get(name)?.clone();
            return self.embed_term(&node);
        }
        if is_blank(id) {
            Some(Term::Blank(id[2..].to_string()))
        } else if well_formed_iri(id) {
            Some(Term::Iri(id.to_string()))
        } else {
            None
        }
    }

    /// The triple term an embedded node describes: its id (or a fresh blank node), and
    /// its single property and value — or, with only `@type`, that type triple.
    fn embed_term(&mut self, node: &Json) -> Option<Term> {
        let s = match get(node, "@id").and_then(Json::as_str) {
            Some(s) => self.node_term(s)?,
            None => self.fresh(),
        };
        if let Some(t) = get(node, "@type") {
            let t = match t {
                Json::Str(t) => t.clone(),
                Json::Arr(a) if a.len() == 1 => a[0].as_str()?.to_string(),
                _ => return None,
            };
            return Some(Term::triple(s, Term::iri(RDF_TYPE), self.node_term(&t)?));
        }
        let prop = keys(node).into_iter().find(|k| k != "@id")?;
        let values = get(node, &prop)?.as_array()?;
        if values.len() != 1 {
            return None;
        }
        let p = if is_blank(&prop) {
            if !self.opts.generalized_rdf {
                return None;
            }
            Term::Blank(prop[2..].to_string())
        } else if well_formed_iri(&prop) {
            Term::Iri(prop.clone())
        } else {
            return None;
        };
        let o = self.object(&values[0], &None)?;
        Some(Term::triple(s, p, o))
    }

    fn fresh(&mut self) -> Term {
        self.bnodes += 1;
        Term::Blank(format!("l{}", self.bnodes))
    }

    fn emit(&mut self, s: Term, p: Term, o: Term, g: &Option<Term>) {
        self.out.push(Quad { s, p, o, g: g.clone() });
    }

    /// 8.3 Object to RDF.
    fn object(&mut self, item: &Json, g: &Option<Term>) -> Option<Term> {
        if let Some(Json::Str(id)) = get(item, "@id") {
            if !is_value_object(item) {
                return self.node_term(id);
            }
        }
        if let Some(l) = get(item, "@list") {
            return Some(self.list(l.as_array().unwrap_or(&[]), g));
        }
        let value = get(item, "@value")?;
        let mut datatype = get(item, "@type").and_then(Json::as_str).map(String::from);
        if datatype.as_deref().is_some_and(|d| d != "@json" && !well_formed_iri(d)) {
            return None;
        }
        if let Some(Json::Str(l)) = get(item, "@language") {
            if !super::valid_lang(l) {
                return None;
            }
        }
        let lex = if datatype.as_deref() == Some("@json") {
            datatype = Some(format!("{RDF_NS}JSON"));
            let mut s = String::new();
            jcs(value, &mut s);
            s
        } else {
            match value {
                Json::Bool(b) => {
                    datatype.get_or_insert(format!("{XSD}boolean"));
                    b.to_string()
                }
                Json::Num(n) if n.fract() != 0.0 || n.abs() >= 1e21 || datatype.as_deref() == Some(&format!("{XSD}double")) => {
                    datatype.get_or_insert(format!("{XSD}double"));
                    canonical_double(*n)
                }
                Json::Num(n) => {
                    datatype.get_or_insert(format!("{XSD}integer"));
                    format!("{}", *n as i64)
                }
                Json::Str(s) => s.clone(),
                _ => return None,
            }
        };
        let lang = get(item, "@language").and_then(Json::as_str);
        let dir = get(item, "@direction").and_then(Json::as_str);
        if let Some(d) = dir {
            match self.opts.rdf_direction.as_deref() {
                Some("i18n-datatype") => {
                    let dt = format!("https://www.w3.org/ns/i18n#{}_{d}", lang.unwrap_or("").to_ascii_lowercase());
                    return Some(Term::typed(lex, &dt));
                }
                Some("compound-literal") => {
                    let node = self.fresh();
                    self.emit(node.clone(), Term::iri(format!("{RDF_NS}value")), Term::string(lex), g);
                    if let Some(l) = lang {
                        self.emit(node.clone(), Term::iri(format!("{RDF_NS}language")), Term::string(l.to_ascii_lowercase()), g);
                    }
                    self.emit(node.clone(), Term::iri(format!("{RDF_NS}direction")), Term::string(d), g);
                    return Some(node);
                }
                _ => {}
            }
        }
        Some(match (lang, datatype) {
            (Some(l), _) => Term::lang(lex, l),
            (None, Some(dt)) => Term::typed(lex, &dt),
            (None, None) => Term::string(lex),
        })
    }

    /// 8.4 List to RDF.
    fn list(&mut self, items: &[Json], g: &Option<Term>) -> Term {
        if items.is_empty() {
            return Term::iri(RDF_NIL);
        }
        let nodes: Vec<Term> = (0..items.len()).map(|_| self.fresh()).collect();
        for (i, item) in items.iter().enumerate() {
            if let Some(o) = self.object(item, g) {
                self.emit(nodes[i].clone(), Term::iri(RDF_FIRST), o, g);
            }
            let rest = nodes.get(i + 1).cloned().unwrap_or_else(|| Term::iri(RDF_NIL));
            self.emit(nodes[i].clone(), Term::iri(RDF_REST), rest, g);
        }
        nodes[0].clone()
    }
}

/// Expand a JSON-LD document (5.1 expand() of the API).
fn expand_document(doc: &Json, opts: &Options) -> R<(Json, bool)> {
    let mut p = Processor { opts, contexts: HashMap::new(), star: opts.processing_mode == "json-ld-1.2" };
    let mut active = Context { terms: HashMap::new(), base: Some(opts.base.clone()).filter(|b| !b.is_empty()), original_base: Some(opts.base.clone()).filter(|b| !b.is_empty()), vocab: None, language: None, direction: None, previous: None };
    if let Some(ec) = &opts.expand_context {
        let ctx = get(ec, "@context").cloned().unwrap_or_else(|| ec.clone());
        active = p.process_context(&active, &ctx, &opts.base, &mut vec![], false, true, true)?;
    }
    let mut expanded = p.expand(&active, None, doc, &opts.base, false, 0)?;
    if is_map(&expanded) && keys(&expanded) == ["@graph"] {
        expanded = remove(&mut expanded, "@graph").unwrap();
    }
    Ok((match expanded {
        Json::Null => Json::Arr(vec![]),
        Json::Arr(a) => Json::Arr(a),
        other => Json::Arr(vec![other]),
    }, p.star))
}

/// JSON-LD document text to quads (8.2 Deserialize JSON-LD to RDF).
pub fn to_rdf(text: &str, opts: &Options) -> R<Vec<Quad>> {
    let doc = Json::parse(text).map_err(|e| format!("JSON-LD error: loading document failed ({e})"))?;
    to_rdf_doc(&doc, opts)
}

/// A parsed JSON-LD document (or a YAML-LD document parsed to JSON) to quads.
pub fn to_rdf_doc(doc: &Json, opts: &Options) -> R<Vec<Quad>> {
    if !matches!(opts.processing_mode.as_str(), "json-ld-1.0" | "json-ld-1.1" | "json-ld-1.2") {
        return err("processing mode not supported");
    }
    let (expanded, star) = expand_document(doc, opts)?;
    let mut expanded = expanded;
    let mut star_state = Star::default();
    if star {
        star_pass(&mut expanded, None, None, &mut star_state);
    }
    let mut nm = NodeMapper { counter: 0, labels: HashMap::new(), map: BTreeMap::new() };
    nm.map.insert("@default".into(), BTreeMap::new());
    nm.generate(&expanded, "@default", None, None, &mut None)?;
    let mut rdf = Rdf { opts, out: Vec::new(), bnodes: 0, embeds: star_state.embeds.clone() };
    let map = std::mem::take(&mut nm.map);
    for (graph, nodes) in &map {
        let g = if graph == "@default" {
            None
        } else if is_blank(graph) || well_formed_iri(graph) {
            rdf.node_term(graph)
        } else {
            continue;
        };
        for (subject, node) in nodes {
            let Some(s) = rdf.node_term(subject) else { continue };
            let mut props = keys(node);
            props.sort();
            for p in props {
                let values = as_array(get(node, &p).cloned().unwrap_or(Json::Null));
                if p == "@type" {
                    for t in values {
                        if let Some(o) = t.as_str().and_then(|t| rdf.node_term(t)) {
                            rdf.emit(s.clone(), Term::iri(RDF_TYPE), o, &g);
                        }
                    }
                    continue;
                }
                if is_keyword(&p) {
                    continue;
                }
                let pred = if is_blank(&p) {
                    if !opts.generalized_rdf {
                        continue;
                    }
                    Term::Blank(p[2..].to_string())
                } else if well_formed_iri(&p) {
                    Term::Iri(p.clone())
                } else {
                    continue;
                };
                for item in values {
                    if let Some(o) = rdf.object(&item, &g) {
                        rdf.emit(s.clone(), pred.clone(), o, &g);
                    }
                }
            }
        }
    }
    // @annotation: the annotated triple is asserted above (its value stayed in the node
    // map); as with Turtle's {| … |}, a fresh reifier describes it with rdf:reifies and
    // carries the annotation's own properties
    for (graph, subj, prop, item, ann) in &star_state.annotations {
        let g = graph.as_deref().and_then(|gid| rdf.node_term(gid));
        let s = match rdf.node_term(subj) {
            Some(t) => t,
            None => continue,
        };
        let p = if is_blank(prop) {
            if !opts.generalized_rdf {
                continue;
            }
            Term::Blank(prop[2..].to_string())
        } else if well_formed_iri(prop) {
            Term::Iri(prop.clone())
        } else {
            continue;
        };
        let o = match rdf.object(item, &g) {
            Some(t) => t,
            None => continue,
        };
        let reifier = rdf.fresh();
        rdf.emit(reifier.clone(), Term::iri(RDF_REIFIES), Term::triple(s, p, o), &g);
        for k in keys(ann) {
            if k == "@id" {
                continue;
            }
            let ap = if is_blank(&k) {
                if !opts.generalized_rdf {
                    continue;
                }
                Term::Blank(k[2..].to_string())
            } else if well_formed_iri(&k) {
                Term::Iri(k.clone())
            } else {
                continue;
            };
            for v in as_array(get(ann, &k).cloned().unwrap_or(Json::Null)) {
                if let Some(ao) = rdf.object(&v, &g) {
                    rdf.emit(reifier.clone(), ap.clone(), ao, &g);
                }
            }
        }
    }
    Ok(rdf.out)
}

/// The JSON-LD-star (RDF 1.2 triple terms) state of one document.
#[derive(Default)]
struct Star {
    /// synthetic id -> the embedded node it stands for
    embeds: HashMap<String, Json>,
    /// (graph, subject id, property, the annotated value, the annotation node)
    annotations: Vec<(Option<String>, String, String, Json, Json)>,
    n: usize,
}

/// Register an embedded node and return its synthetic id (`\u{1}`-prefixed, so it can
/// never collide with an IRI or a blank node label).
fn star_mint(st: &mut Star, node: Json) -> String {
    st.n += 1;
    // kept before the pass below, which may mint references of its own
    let n = st.n;
    let name = format!("e{n}");
    let mut node = node;
    // embedded nodes nested inside this one become references of their own
    star_pass(&mut node, None, None, st);
    st.embeds.insert(name, node);
    format!("\u{1}e{n}")
}

/// A node object that describes exactly one relationship (with an optional `@id`): in
/// value position it stands for that triple term instead of being asserted itself.
fn is_embedded(v: &Json) -> bool {
    if is_value_object(v) || is_list_object(v) {
        return false;
    }
    let ks = keys(v);
    let mut props: Vec<&String> = Vec::new();
    for k in &ks {
        match k.as_str() {
            "@id" => {}
            "@type" => {
                let single = match get(v, "@type") {
                    Some(Json::Str(_)) => true,
                    Some(Json::Arr(a)) => a.len() == 1,
                    _ => false,
                };
                return ks.len() == 2 && single;
            }
            _ => props.push(k),
        }
    }
    if props.len() != 1 || ks.len() != 2 {
        return false;
    }
    let p = props[0];
    (well_formed_iri(p) || is_blank(p)) && matches!(get(v, p), Some(Json::Arr(a)) if a.len() == 1)
}

/// Replace embedded nodes with references and collect annotations, so the node map only
/// ever sees plain nodes (an embedded node is not asserted) and the RDF step can resolve
/// the references to triple terms. Runs only in star mode.
fn star_pass(v: &mut Json, parent: Option<(&str, &str)>, graph: Option<&str>, st: &mut Star) {
    match v {
        Json::Arr(items) => {
            for item in items.iter_mut() {
                star_pass(item, parent, graph, st);
            }
        }
        Json::Obj(_) => {
            if is_value_object(v) {
                if let Some(ann) = remove(v, "@annotation") {
                    if let Some((s, p)) = parent {
                        st.annotations.push((graph.map(String::from), s.to_string(), p.to_string(), v.clone(), ann));
                    }
                }
                return;
            }
            if let Some(list) = get(v, "@list").cloned() {
                let mut list = list;
                star_pass(&mut list, parent, graph, st);
                v.set("@list", list);
                return;
            }
            // a node object in value position
            let ann = remove(v, "@annotation");
            if let Some(ann) = ann {
                if let Some((s, p)) = parent {
                    st.annotations.push((graph.map(String::from), s.to_string(), p.to_string(), v.clone(), ann));
                }
            }
            // a node object as @id: the node's own subject is that triple term (also in
            // value position: its properties then describe the term)
            if let Some(Json::Obj(_)) = get(v, "@id").cloned() {
                let inner = remove(v, "@id").unwrap();
                let id = star_mint(st, inner);
                v.set("@id", Json::Str(id));
            } else if parent.is_some() && is_embedded(v) {
                if st.annotations.last().is_some_and(|a| a.3 == *v) {
                    // the annotation asserts the triple; a quoted embedded node is not
                    // asserted, so the two cannot be combined
                    return; // left as it is; the RDF step drops what it cannot express
                }
                let taken = std::mem::replace(v, Json::Null);
                let id = star_mint(st, taken);
                *v = Json::obj().with("@id", Json::Str(id));
                return;
            }
            let id = get(v, "@id").and_then(Json::as_str).unwrap_or_default().to_string();
            if let Some(rev) = get(v, "@reverse").cloned() {
                let mut rev = rev;
                if let Json::Obj(fields) = &mut rev {
                    for (_, vals) in fields.iter_mut() {
                        if let Json::Arr(items) = vals {
                            for item in items.iter_mut() {
                                star_pass(item, None, graph, st);
                            }
                        }
                    }
                }
                v.set("@reverse", rev);
            }
            if let Some(g) = get(v, "@graph").cloned() {
                let mut g = g;
                let gname = get(v, "@id").and_then(Json::as_str).map(String::from);
                star_pass(&mut g, None, gname.as_deref(), st);
                v.set("@graph", g);
            }
            if let Some(inc) = get(v, "@included").cloned() {
                let mut inc = inc;
                star_pass(&mut inc, None, graph, st);
                v.set("@included", inc);
            }
            if let Json::Obj(fields) = v {
                for (k, vals) in fields.iter_mut() {
                    if k.starts_with('@') {
                        continue;
                    }
                    if let Json::Arr(items) = vals {
                        for item in items.iter_mut() {
                            star_pass(item, Some((id.as_str(), k.as_str())), graph, st);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_contexts_and_containers() {
        let doc = r#"{
          "@context": {"@vocab": "http://ex.org/", "name": {"@language": "en"}, "tags": {"@container": "@list"},
                       "knows": {"@type": "@id"}, "label": {"@container": "@language"}},
          "@id": "http://ex.org/a", "@type": "Person", "name": "Ann", "tags": ["x", "y"], "knows": "http://ex.org/b",
          "label": {"de": "Anna", "fr": "Anne"}, "age": 30, "height": 1.5, "ok": true
        }"#;
        let q = to_rdf(doc, &Options::new("http://base/")).unwrap();
        let has = |p: &str, o: &Term| q.iter().any(|q| q.p == Term::iri(p) && &q.o == o);
        assert!(has("http://www.w3.org/1999/02/22-rdf-syntax-ns#type", &Term::iri("http://ex.org/Person")));
        assert!(has("http://ex.org/name", &Term::lang("Ann", "en")));
        assert!(has("http://ex.org/knows", &Term::iri("http://ex.org/b")));
        assert!(has("http://ex.org/label", &Term::lang("Anna", "de")));
        assert!(has("http://ex.org/age", &Term::integer(30)));
        assert!(has("http://ex.org/height", &Term::typed("1.5E0", "http://www.w3.org/2001/XMLSchema#double")));
        assert_eq!(q.iter().filter(|q| q.p == Term::iri(RDF_FIRST)).count(), 2);
    }

    #[test]
    fn safe_mode_refuses_what_it_would_drop() {
        let safe = Options { safe_mode: true, ..Options::new("http://base/") };
        let ctx = r#""@context": {"ex": "http://ex.org/", "name": "http://ex.org/name"}"#;
        // a key the context does not define: dropped normally, an error in safe mode
        let typo = format!(r#"{{ {ctx}, "@id": "http://ex.org/a", "name": "Ann", "nmae": "lost" }}"#);
        assert_eq!(to_rdf(&typo, &Options::new("http://base/")).unwrap().len(), 1);
        assert!(to_rdf(&typo, &safe).unwrap_err().contains("\"nmae\" is not defined"));
        // a type that is not an IRI: a relative IRI normally, an error in safe mode
        let bare = format!(r#"{{ {ctx}, "@id": "http://ex.org/a", "@type": "Person" }}"#);
        assert!(to_rdf(&bare, &Options::new("http://base/")).is_ok());
        assert!(to_rdf(&bare, &safe).unwrap_err().contains("\"Person\""));
        // what the context defines, compact IRIs and full IRIs all pass
        let fine = format!(r#"{{ {ctx}, "@id": "http://ex.org/a", "@type": "ex:Person", "name": "Ann", "ex:age": 3, "http://ex.org/x": 1 }}"#);
        assert_eq!(to_rdf(&fine, &safe).unwrap().len(), 4);
    }

    #[test]
    fn star_triple_term_as_subject_and_object() {
        // JSON-LD-star (RDF 1.2): a node object as @id, or in value position, denotes the
        // single triple it describes
        let opts = Options { processing_mode: "json-ld-1.2".into(), ..Options::new("") };
        let subject = to_rdf(
            r#"{"@context":{"ex":"http://example.org/"},"@id":{"@id":"ex:bob","ex:age":42},"ex:certainty":0.8}"#,
            &opts,
        )
        .unwrap();
        assert_eq!(subject.len(), 1);
        assert!(subject[0].s.is_triple());
        assert_eq!(subject[0].p.value(), "http://example.org/certainty");

        let object = to_rdf(
            r#"{"@context":{"ex":"http://example.org/"},"@id":"ex:alice","ex:claims":{"@id":"ex:bob","ex:knows":{"@id":"ex:alice"}}}"#,
            &opts,
        )
        .unwrap();
        assert_eq!(object.len(), 1);
        assert_eq!(object[0].s.value(), "http://example.org/alice");
        assert_eq!(object[0].p.value(), "http://example.org/claims");
        assert!(object[0].o.is_triple());
    }

    #[test]
    fn star_annotation_asserts_and_annotates() {
        let opts = Options { processing_mode: "json-ld-1.2".into(), ..Options::new("") };
        let quads = to_rdf(
            r#"{"@context":{"ex":"http://example.org/"},"@id":"ex:bob","ex:knows":{"@id":"ex:alice","@annotation":{"ex:certainty":0.8}}}"#,
            &opts,
        )
        .unwrap();
        assert_eq!(quads.len(), 3);
        let plain = quads.iter().find(|q| !q.s.is_triple() && !q.o.is_triple()).expect("the annotated triple is asserted");
        assert_eq!(plain.o.value(), "http://example.org/alice");
        // as with Turtle's {| … |}: a reifier carries rdf:reifies and the annotation
        let reifies = quads.iter().find(|q| q.p.value() == "http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies").expect("rdf:reifies");
        assert!(reifies.o.is_triple());
        assert!(reifies.s.is_blank());
        let ann = quads.iter().find(|q| q.p.value() == "http://example.org/certainty").expect("the annotation");
        assert_eq!(ann.s, reifies.s);
    }

    #[test]
    fn star_needs_1_2() {
        // in 1.1 a node object as @id stays an error
        assert!(to_rdf(
            r#"{"@context":{"ex":"http://example.org/"},"@id":{"@id":"ex:bob","ex:age":42},"ex:certainty":0.8}"#,
            &Options::new("")
        )
        .is_err());
        // @version 1.2 in the document turns the extensions on with default options
        let quads = to_rdf(
            r#"{"@context":{"@version":1.2,"ex":"http://example.org/"},"@id":{"@id":"ex:bob","ex:age":42},"ex:certainty":0.8}"#,
            &Options::new(""),
        )
        .unwrap();
        assert_eq!(quads.len(), 1);
        assert!(quads[0].s.is_triple());
        // only known processing modes are accepted
        assert!(to_rdf("{}", &Options { processing_mode: "json-ld-9".into(), ..Options::new("") }).is_err());
    }

    #[test]
    fn star_round_trips_through_the_writer() {
        let opts = Options { processing_mode: "json-ld-1.2".into(), ..Options::new("") };
        let quads = to_rdf(
            r#"{"@context":{"ex":"http://example.org/"},"@id":"ex:alice","ex:claims":{"@id":"ex:bob","ex:knows":{"@id":"ex:alice"}}}"#,
            &opts,
        )
        .unwrap();
        let out = crate::rdf::write::serialize(&quads, crate::rdf::Format::JsonLd, &[]).unwrap();
        assert!(out.contains(r#""@id":{"@id":"http://example.org/bob""#), "{out}");
        assert!(out.contains(r#""http://example.org/knows":["#), "{out}");
    }

    #[test]
    fn yaml_ld_loads_as_json_ld() {
        // YAML-LD: the same document in YAML
        let doc = r#"
"@context":
  ex: http://example.org/
"@id": ex:book/1
ex:name: Safahat
"#;
        let json = crate::rdf::yaml::parse(doc).unwrap();
        let quads = to_rdf_doc(&json, &Options::new("")).unwrap();
        assert_eq!(quads.len(), 1);
        assert_eq!(quads[0].s.value(), "http://example.org/book/1");
        assert_eq!(quads[0].p.value(), "http://example.org/name");
        assert_eq!(quads[0].o.value(), "Safahat");
        // and back out as YAML-LD
        let out = crate::rdf::write::serialize(&quads, crate::rdf::Format::YamlLd, &[]).unwrap();
        assert!(out.contains("http://example.org/name"), "{out}");
    }
}
