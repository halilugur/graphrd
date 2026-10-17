//! Search connectors: a repository's entities of given classes kept in an OpenSearch or
//! Elasticsearch index, so full-text search, facets and relevance tuning of a search engine work
//! on graph data.
//!
//! A connector maps fields to predicates. Each entity (a subject with one of the connector's
//! classes, stated or inferred) becomes a document whose id is the entity's IRI, with its `types`
//! and the values of each field (literals as their text, resources as their IRI). The store
//! records the subjects that changed; a thread per connector sends their documents (or deletes
//! them) with the `_bulk` API, so writes never wait for the search engine. In SPARQL,
//! `?e <http://graphrd.org/connector/NAME> "query"` binds `?e` to the entities the engine finds
//! for a `query_string` query, best first.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::json::Json;
use crate::rdf::{Term, RDF};
use crate::store::{Id, Store};

/// The predicate namespace of connector search: `?e <…/connector/NAME> "query"`.
pub const NAMESPACE: &str = "http://graphrd.org/connector/";

#[derive(Clone, Debug)]
/// One search connector's definition (an OpenSearch or Elasticsearch index).
pub struct Connector {
/// The connector's name, as queries address it.
    pub name: String,
    /// Base URL of the engine, e.g. `http://localhost:9200`.
    pub url: String,
/// The index documents go to.
    pub index: String,
    /// Classes whose instances are indexed.
    pub types: Vec<String>,
    /// (field name, predicate IRI).
    pub fields: Vec<(String, String)>,
}

/// What a connector's thread reports.
#[derive(Clone, Debug, Default)]
pub struct Status {
/// Documents sent to the index.
    pub indexed: u64,
/// Documents removed from it.
    pub deleted: u64,
/// Subjects still waiting for a round (a first sync, or a failed one).
    pub pending: usize,
/// The last failed call's error, if any.
    pub last_error: Option<String>,
/// When the queue last drained, Unix seconds.
    pub last_sync: Option<i64>,
}

fn now_millis() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

impl Connector {
/// A connector from its `PUT` body: name, URL, index, types and field mapping.
    pub fn from_json(name: &str, j: &Json) -> Result<Connector, String> {
        let s = |k: &str| j.get(k).and_then(|v| v.as_str()).map(String::from);
        let kind = s("type").unwrap_or_else(|| "opensearch".into());
        if !matches!(kind.as_str(), "opensearch" | "elasticsearch") {
            return Err(format!("unknown connector type {kind:?} (opensearch or elasticsearch)"));
        }
        let url = s("url").ok_or("a connector needs a url (e.g. http://localhost:9200)")?.trim_end_matches('/').to_string();
        let index = s("index").unwrap_or_else(|| name.to_ascii_lowercase());
        if index.is_empty() || !index.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c)) {
            return Err(format!("index {index:?}: lower-case letters, digits, - _ . only"));
        }
        let types: Vec<String> = j.get("types").and_then(|t| t.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        if types.is_empty() {
            return Err("a connector needs \"types\": the classes whose instances it indexes".into());
        }
        let mut fields = Vec::new();
        for f in j.get("fields").and_then(|f| f.as_array()).unwrap_or(&[]) {
            let (Some(n), Some(p)) = (f.get("name").and_then(|v| v.as_str()), f.get("property").and_then(|v| v.as_str())) else {
                return Err("each field needs a name and a property".into());
            };
            if n == "id" || n == "types" {
                return Err(format!("field name {n:?} is reserved"));
            }
            fields.push((n.to_string(), p.to_string()));
        }
        if fields.is_empty() {
            return Err("a connector needs \"fields\": [{\"name\": ..., \"property\": ...}]".into());
        }
        Ok(Connector { name: name.into(), url, index, types, fields })
    }

/// The definition as it goes back to the client.
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("type", "opensearch")
            .with("url", self.url.as_str())
            .with("index", self.index.as_str())
            .with("types", Json::Arr(self.types.iter().map(|t| Json::from(t.as_str())).collect()))
            .with("fields", Json::Arr(self.fields.iter().map(|(n, p)| Json::obj().with("name", n.as_str()).with("property", p.as_str())).collect()))
    }

    fn call(&self, method: &str, path: &str, body: Option<(&str, &[u8])>) -> Result<Json, String> {
        let url = format!("{}/{}", self.url, path.trim_start_matches('/'));
        let (_, resp) = crate::http::fetch_with(&url, method, "application/json", body, Duration::from_secs(60))?;
        Json::parse(&String::from_utf8_lossy(&resp)).map_err(|e| format!("{url}: {e}"))
    }

    /// Create the index (an index that exists already is kept).
    pub fn ensure_index(&self) -> Result<(), String> {
        if self.call("HEAD", &self.index, None).is_ok() {
            return Ok(());
        }
        let mut props = Json::obj().with("types", Json::obj().with("type", "keyword"));
        for (n, _) in &self.fields {
            props.set(n, Json::obj().with("type", "text").with("fields", Json::obj().with("raw", Json::obj().with("type", "keyword").with("ignore_above", 256usize))));
        }
        let body = Json::obj().with("mappings", Json::obj().with("properties", props)).to_string();
        match self.call("PUT", &self.index, Some(("application/json", body.as_bytes()))) {
            Ok(_) => Ok(()),
            Err(e) if e.contains("resource_already_exists") => Ok(()),
            Err(e) => Err(e),
        }
    }

/// Delete the index (the connector's data only).
    pub fn drop_index(&self) -> Result<(), String> {
        self.call("DELETE", &self.index, None).map(|_| ())
    }

    /// The document of an entity, or None when it is not (or no longer) one of the classes.
    pub fn document(&self, store: &Store, subject: Id) -> Option<Json> {
        let ty = store.dict.lookup(&Term::Iri(format!("{RDF}type")))?;
        let types: Vec<String> = store.quads_matching(Some(subject), Some(ty), None, None).map(|q| store.dict.term(q[2]).value().to_string()).collect();
        if !types.iter().any(|t| self.types.contains(t)) {
            return None;
        }
        let mut doc = Json::obj().with("id", store.dict.term(subject).value()).with("types", Json::Arr(types.iter().map(|t| Json::from(t.as_str())).collect()));
        for (name, prop) in &self.fields {
            let Some(p) = store.dict.lookup(&Term::Iri(prop.clone())) else { continue };
            let mut vals: Vec<String> = store.quads_matching(Some(subject), Some(p), None, None).map(|q| store.dict.term(q[2]).value().to_string()).collect();
            vals.sort();
            vals.dedup();
            match vals.len() {
                0 => {}
                1 => doc.set(name, vals.pop().unwrap()),
                _ => doc.set(name, Json::Arr(vals.into_iter().map(Json::from).collect())),
            }
        }
        Some(doc)
    }

    /// Send the documents of these subjects (deleting those that are no longer entities).
    /// Returns (indexed, deleted).
    pub fn sync(&self, store: &Store, subjects: &[Id]) -> Result<(u64, u64), String> {
        let (mut indexed, mut deleted) = (0, 0);
        for part in subjects.chunks(1000) {
            let mut bulk = String::new();
            for &s in part {
                let Term::Iri(iri) = store.dict.term(s) else { continue };
                let meta = |op: &str| Json::obj().with(op, Json::obj().with("_index", self.index.as_str()).with("_id", iri.as_str())).to_string();
                match self.document(store, s) {
                    Some(doc) => {
                        bulk.push_str(&meta("index"));
                        bulk.push('\n');
                        bulk.push_str(&doc.to_string());
                        bulk.push('\n');
                        indexed += 1;
                    }
                    None => {
                        bulk.push_str(&meta("delete"));
                        bulk.push('\n');
                        deleted += 1;
                    }
                }
            }
            if bulk.is_empty() {
                continue;
            }
            let r = self.call("POST", "_bulk?refresh=wait_for", Some(("application/x-ndjson", bulk.as_bytes())))?;
            if r.get("errors") == Some(&Json::Bool(true)) {
                // deleting a document that is not there is not an error
                let real = r.get("items").and_then(|i| i.as_array()).unwrap_or(&[]).iter().find_map(|it| {
                    let (op, v) = match it {
                        Json::Obj(f) => f.first()?,
                        _ => return None,
                    };
                    let status = v.get("status").and_then(|s| s.as_f64()).unwrap_or(0.0) as u32;
                    (status >= 300 && !(op == "delete" && status == 404)).then(|| v.get("error").map(|e| e.to_string()).unwrap_or_default())
                });
                if let Some(e) = real {
                    return Err(format!("{}: bulk indexing failed: {e}", self.index));
                }
            }
        }
        Ok((indexed, deleted))
    }

    /// Every entity of the connector's classes, for the first sync.
    pub fn entities(&self, store: &Store) -> Vec<Id> {
        let Some(ty) = store.dict.lookup(&Term::Iri(format!("{RDF}type"))) else { return Vec::new() };
        let mut out = HashSet::new();
        for t in &self.types {
            if let Some(c) = store.dict.lookup(&Term::Iri(t.clone())) {
                out.extend(store.quads_matching(None, Some(ty), Some(c), None).map(|q| q[0]));
            }
        }
        out.into_iter().collect()
    }

    /// Entity IRIs for a `query_string` query, best first.
    pub fn search(&self, query: &str, size: usize) -> Result<Vec<(String, f64)>, String> {
        let body = Json::obj()
            .with("size", size)
            .with("_source", false)
            .with("query", Json::obj().with("query_string", Json::obj().with("query", query)))
            .to_string();
        let r = self.call("POST", &format!("{}/_search", self.index), Some(("application/json", body.as_bytes())))?;
        let hits = r.get("hits").and_then(|h| h.get("hits")).and_then(|h| h.as_array()).unwrap_or(&[]);
        Ok(hits.iter().filter_map(|h| Some((h.get("_id")?.as_str()?.to_string(), h.get("_score").and_then(|s| s.as_f64()).unwrap_or(0.0)))).collect())
    }
}

/// A connector at work: its definition, the subjects waiting to be synced, and its status.
pub struct Running {
/// The connector's definition.
    pub def: Connector,
/// Counters and the last error, for the status page.
    pub status: Mutex<Status>,
    /// Subjects still to sync (a first full sync, or a round that failed).
    pub pending: Mutex<HashSet<Id>>,
}

impl Running {
/// A running connector: definition plus state.
    pub fn new(def: Connector) -> Arc<Running> {
        Arc::new(Running { def, status: Mutex::default(), pending: Mutex::default() })
    }

/// The status as the REST API shows it.
    pub fn status_json(&self) -> Json {
        let s = self.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Json::obj()
            .with("name", self.def.name.as_str())
            .with("definition", self.def.to_json())
            .with("indexed", s.indexed as usize)
            .with("deleted", s.deleted as usize)
            .with("pending", s.pending)
            .with("lastSync", s.last_sync)
            .with("lastError", s.last_error)
    }

    /// One round: sync these subjects, recording the outcome. Subjects that failed are returned
    /// to be tried again.
    pub fn round(&self, store: &Store, changed: &[Id]) {
        let subjects: Vec<Id> = {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            p.extend(changed);
            p.drain().collect()
        };
        if subjects.is_empty() {
            return;
        }
        let r = self.def.ensure_index().and_then(|()| self.def.sync(store, &subjects));
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        match r {
            Ok((i, d)) => {
                s.indexed += i;
                s.deleted += d;
                s.last_sync = Some(now_millis());
                s.last_error = None;
                s.pending = 0;
            }
            Err(e) => {
                s.last_error = Some(e);
                s.pending = subjects.len();
                drop(s);
                // tried again next round
                self.pending.lock().unwrap_or_else(|e| e.into_inner()).extend(subjects);
            }
        }
    }
}
