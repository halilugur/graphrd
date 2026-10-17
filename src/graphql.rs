//! GraphQL over RDF with a schema derived from the data itself: every
//! class (object of rdf:type) is a query field and a type; every predicate
//! used by its instances is a field, named by its local name.
//!
//! ```graphql
//! { Person(where: {name: "Ann"}, limit: 10, orderBy: "age") { id name knows { id name } } }
//! ```
//! Root field arguments: `id`, `where` (equality on fields), `limit`,
//! `offset`, `orderBy`, `desc`. A field with a selection set follows links
//! to other resources; without one it yields the value(s). `id` is the IRI.
//!
//! Mutations come from the same schema: `create<Type>(id, input)`,
//! `update<Type>(id, input)` (each given field's values are replaced; `null`
//! removes them) and `delete<Type>(id)`; each returns the resource with its
//! selection. Input values follow what the predicate already holds (links stay
//! IRIs, typed values keep their datatype).
//! ```graphql
//! mutation { createPerson(id: "http://ex.org/dan", input: {name: "Dan", knows: ["http://ex.org/ann"]}) { id name } }
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::json::Json;
use crate::rdf::{Quad, Term, RDF_DIRLANGSTRING, RDF_LANGSTRING, RDF_TYPE, XSD_DECIMAL, XSD_INTEGER, XSD_STRING};
use crate::sparql::func;
use crate::store::{Id, Store};

// ------------------------------------------------------------------ parser

#[derive(Debug, Clone)]
/// A GraphQL value, as an argument carries it.
pub enum Value {
/// `null`.
    Null,
/// `true` or `false`.
    Bool(bool),
/// An integer.
    Int(i64),
/// A float.
    Float(f64),
/// A string.
    Str(String),
/// An enum value (its name).
    Enum(String),
    #[allow(dead_code)] // parsed for completeness; lists are not used as arguments yet
/// A list of values.
    List(Vec<Value>),
/// An input object's fields.
    Object(Vec<(String, Value)>),
/// A `$variable` from the request's variables.
    Var(String),
}

#[derive(Debug, Clone)]
/// One selected field: its (possibly aliased) name, arguments and subselection.
pub struct Field {
    alias: Option<String>,
    name: String,
    args: Vec<(String, Value)>,
    selection: Vec<Field>,
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while let Some(&c) = self.s.get(self.i) {
            if c.is_ascii_whitespace() || c == b',' {
                self.i += 1;
            } else if c == b'#' {
                while self.s.get(self.i).is_some_and(|&c| c != b'\n') {
                    self.i += 1;
                }
            } else {
                break;
            }
        }
    }
    fn err<T>(&self, m: &str) -> Result<T, String> {
        Err(format!("GraphQL syntax error: {m} at offset {}", self.i))
    }
    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn name(&mut self) -> Result<String, String> {
        self.ws();
        let start = self.i;
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
            self.i += 1;
        }
        if start == self.i {
            return self.err("expected a name");
        }
        Ok(String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
    }
    fn value(&mut self) -> Result<Value, String> {
        self.ws();
        match self.s.get(self.i) {
            Some(b'$') => {
                self.i += 1;
                Ok(Value::Var(self.name()?))
            }
            Some(b'"') => {
                self.i += 1;
                let mut out = String::new();
                loop {
                    match self.s.get(self.i) {
                        None => return self.err("unterminated string"),
                        Some(b'"') => {
                            self.i += 1;
                            break;
                        }
                        Some(b'\\') => {
                            let c = *self.s.get(self.i + 1).unwrap_or(&b'\\');
                            out.push(match c {
                                b'n' => '\n',
                                b't' => '\t',
                                c => c as char,
                            });
                            self.i += 2;
                        }
                        Some(_) => {
                            let start = self.i;
                            while self.s.get(self.i).is_some_and(|c| *c != b'"' && *c != b'\\') {
                                self.i += 1;
                            }
                            out.push_str(&String::from_utf8_lossy(&self.s[start..self.i]));
                        }
                    }
                }
                Ok(Value::Str(out))
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                while !self.eat(b']') {
                    items.push(self.value()?);
                }
                Ok(Value::List(items))
            }
            Some(b'{') => {
                self.i += 1;
                let mut fields = Vec::new();
                while !self.eat(b'}') {
                    let k = self.name()?;
                    if !self.eat(b':') {
                        return self.err("expected ':'");
                    }
                    fields.push((k, self.value()?));
                }
                Ok(Value::Object(fields))
            }
            Some(c) if c.is_ascii_digit() || *c == b'-' => {
                let start = self.i;
                self.i += 1;
                while self.s.get(self.i).is_some_and(|c| c.is_ascii_digit() || b".eE+-".contains(c)) {
                    self.i += 1;
                }
                let t = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
                if let Ok(n) = t.parse::<i64>() {
                    Ok(Value::Int(n))
                } else {
                    t.parse::<f64>().map(Value::Float).or_else(|_| self.err("bad number"))
                }
            }
            _ => {
                let n = self.name()?;
                Ok(match n.as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    "null" => Value::Null,
                    _ => Value::Enum(n),
                })
            }
        }
    }
    fn selection(&mut self) -> Result<Vec<Field>, String> {
        if !self.eat(b'{') {
            return self.err("expected '{'");
        }
        let mut fields = Vec::new();
        while !self.eat(b'}') {
            if self.s.get(self.i) == Some(&b'.') {
                return self.err("fragments are not supported");
            }
            let first = self.name()?;
            let (alias, name) = if self.eat(b':') { (Some(first), self.name()?) } else { (None, first) };
            let mut args = Vec::new();
            if self.eat(b'(') {
                while !self.eat(b')') {
                    let k = self.name()?;
                    if !self.eat(b':') {
                        return self.err("expected ':'");
                    }
                    args.push((k, self.value()?));
                }
            }
            self.ws();
            let selection = if self.s.get(self.i) == Some(&b'{') { self.selection()? } else { Vec::new() };
            fields.push(Field { alias, name, args, selection });
        }
        Ok(fields)
    }
}

/// Whether a request is a mutation (it needs write access).
pub fn is_mutation(src: &str) -> bool {
    let mut p = P { s: src.as_bytes(), i: 0 };
    p.ws();
    p.s[p.i..].starts_with(b"mutation")
}

/// Parse a request: (whether it is a mutation, its root fields).
pub fn parse(src: &str) -> Result<(bool, Vec<Field>), String> {
    let mut p = P { s: src.as_bytes(), i: 0 };
    p.ws();
    // optional `query Name($v: T = default)` / `mutation Name(...)`
    let mut mutation = false;
    if p.s[p.i..].starts_with(b"query") || p.s[p.i..].starts_with(b"mutation") {
        mutation = p.name()? == "mutation";
        p.ws();
        if p.s.get(p.i).is_some_and(|c| c.is_ascii_alphabetic()) {
            p.name()?;
        }
        if p.eat(b'(') {
            let mut depth = 1;
            while depth > 0 && p.i < p.s.len() {
                match p.s[p.i] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                p.i += 1;
            }
        }
    }
    let sel = p.selection()?;
    p.ws();
    if p.i < p.s.len() {
        return p.err("unexpected trailing input");
    }
    Ok((mutation, sel))
}

// ------------------------------------------------------------------ schema

fn local_name(iri: &str) -> String {
    let l = iri.rsplit(['#', '/', ':']).next().unwrap_or(iri);
    let clean: String = l.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if clean.chars().next().is_some_and(|c| c.is_ascii_digit()) { format!("_{clean}") } else { clean }
}

/// The query schema derived from the data: classes, their fields, links.
pub struct Schema {
    /// type name -> class id
    classes: BTreeMap<String, Id>,
    /// field name -> predicate ids with that local name
    fields: HashMap<String, Vec<Id>>,
    /// type name -> field names its instances use (for SDL)
    type_fields: BTreeMap<String, BTreeMap<String, bool>>, // field -> links to resources
}

impl Schema {
    /// Derive the schema by sampling up to `sample` instances per class.
    pub fn build(store: &Store, sample: usize) -> Schema {
        let mut classes = BTreeMap::new();
        let mut fields: HashMap<String, Vec<Id>> = HashMap::new();
        let mut type_fields: BTreeMap<String, BTreeMap<String, bool>> = BTreeMap::new();
        let Some(ty) = store.dict.lookup(&Term::iri(RDF_TYPE)) else {
            return Schema { classes, fields, type_fields };
        };
        for c in store.distinct_objects(ty).into_iter().take(5000) {
            let Term::Iri(iri) = store.dict.term(c) else { continue };
            let name = local_name(&iri);
            classes.entry(name.clone()).or_insert(c);
            let tf = type_fields.entry(name).or_default();
            for inst in store.quads_matching(None, Some(ty), Some(c), None).take(sample) {
                for q in store.quads_matching(Some(inst[0]), None, None, None) {
                    if q[1] == ty {
                        continue;
                    }
                    let Term::Iri(p) = store.dict.term(q[1]) else { continue };
                    let f = local_name(&p);
                    let ids = fields.entry(f.clone()).or_default();
                    if !ids.contains(&q[1]) {
                        ids.push(q[1]);
                    }
                    let links = !store.dict.term(q[2]).is_literal();
                    let e = tf.entry(f).or_insert(false);
                    *e |= links;
                }
            }
        }
        Schema { classes, fields, type_fields }
    }

    /// Class IRI for a type name.
    pub fn class(&self, name: &str) -> Option<Id> {
        self.classes.get(name).copied().or_else(|| self.classes.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| *v))
    }

    /// Predicates behind a field name.
    pub fn predicates(&self, field: &str) -> Vec<Id> {
        match self.fields.get(field) {
            Some(p) => p.clone(),
            None => self.fields.iter().find(|(k, _)| k.eq_ignore_ascii_case(field)).map(|(_, v)| v.clone()).unwrap_or_default(),
        }
    }

    /// Type name -> its field names (for SQL table listings).
    pub fn tables(&self) -> Vec<(String, Vec<String>)> {
        self.type_fields.iter().map(|(t, f)| (t.clone(), f.keys().cloned().collect())).collect()
    }

    /// GraphQL SDL describing the derived schema.
    pub fn sdl(&self) -> String {
        let mut s = String::from("type Query {\n");
        for name in self.classes.keys() {
            s.push_str(&format!("  {name}(id: ID, where: {name}Filter, limit: Int, offset: Int, orderBy: String, desc: Boolean): [{name}]\n"));
        }
        s.push_str("}\n\ntype Mutation {\n");
        for name in self.classes.keys() {
            s.push_str(&format!("  create{name}(id: ID, input: {name}Input, graph: ID): {name}\n"));
            s.push_str(&format!("  update{name}(id: ID!, input: {name}Input, graph: ID): {name}\n"));
            s.push_str(&format!("  delete{name}(id: ID!, graph: ID): {name}\n"));
        }
        s.push_str("}\n");
        for (name, fields) in &self.type_fields {
            s.push_str(&format!("\ntype {name} {{\n  id: ID!\n  types: [ID]\n"));
            for (f, links) in fields {
                s.push_str(&format!("  {f}: {}\n", if *links { "[Resource]" } else { "[String]" }));
            }
            s.push_str("}\n");
            s.push_str(&format!("input {name}Filter {{\n"));
            for f in fields.keys() {
                s.push_str(&format!("  {f}: String\n"));
            }
            s.push_str("}\n");
            s.push_str(&format!("input {name}Input {{\n"));
            for (f, links) in fields {
                s.push_str(&format!("  {f}: {}\n", if *links { "[ID]" } else { "[String]" }));
            }
            s.push_str("}\n");
        }
        s.push_str("\ntype Resource {\n  id: ID!\n  # any field of the linked resource\n}\n");
        s
    }
}

// ------------------------------------------------------------------ execution

fn from_json(j: &Json) -> Value {
    match j {
        Json::Str(s) => Value::Str(s.clone()),
        Json::Num(n) if n.fract() == 0.0 && n.abs() < 9e15 => Value::Int(*n as i64),
        Json::Num(n) => Value::Float(*n),
        Json::Bool(b) => Value::Bool(*b),
        Json::Arr(a) => Value::List(a.iter().map(from_json).collect()),
        Json::Obj(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), from_json(v))).collect()),
        Json::Null => Value::Null,
    }
}

/// A value with its variables (also those nested in lists and objects) filled in.
fn resolve_var(v: &Value, vars: &Json) -> Value {
    match v {
        Value::Var(n) => vars.get(n).map(from_json).unwrap_or(Value::Null),
        Value::List(items) => Value::List(items.iter().map(|i| resolve_var(i, vars)).collect()),
        Value::Object(kv) => Value::Object(kv.iter().map(|(k, v)| (k.clone(), resolve_var(v, vars))).collect()),
        other => other.clone(),
    }
}

/// A scalar value as text.
pub fn text(v: &Value) -> Option<String> {
    as_text(v)
}

fn as_text(v: &Value) -> Option<String> {
    Some(match v {
        Value::Str(s) | Value::Enum(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return None,
    })
}

fn term_json(t: &Term) -> Json {
    match t {
        Term::Literal(l) => {
            if let Some(n) = func::num(t) {
                Json::Num(n.f())
            } else if l.datatype.ends_with("#boolean") {
                Json::Bool(l.value == "true")
            } else {
                Json::Str(l.value.clone())
            }
        }
        Term::Blank(b) => Json::Str(format!("_:{b}")),
        other => Json::Str(other.value().to_string()),
    }
}

/// One GraphQL request running against a store and its schema.
pub struct Exec<'a> {
    store: &'a Store,
    schema: &'a Schema,
    vars: Json,
    budget: std::cell::Cell<usize>,
}

impl<'a> Exec<'a> {
/// An execution with the request's variables.
    pub fn new(store: &'a Store, schema: &'a Schema, vars: Json) -> Self {
        Exec { store, schema, vars, budget: std::cell::Cell::new(100_000) }
    }

    fn values(&self, node: Id, field: &str) -> Vec<Id> {
        let Some(preds) = self.schema.fields.get(field) else { return Vec::new() };
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for &p in preds {
            for q in self.store.quads_matching(Some(node), Some(p), None, None) {
                if seen.insert(q[2]) {
                    out.push(q[2]);
                }
            }
        }
        out
    }

/// Run the top-level fields; the JSON is the `data` of the response.
    pub fn run(&self, query: &[Field]) -> Result<Json, String> {
        let mut data = Vec::new();
        for f in query {
            let key = f.alias.clone().unwrap_or_else(|| f.name.clone());
            if f.name == "__schema" || f.name == "__typename" {
                data.push((key, Json::Str("Query".into())));
                continue;
            }
            let Some(&class) = self.schema.classes.get(&f.name) else {
                return Err(format!("Unknown type '{}'. Available: {}", f.name, self.schema.classes.keys().cloned().collect::<Vec<_>>().join(", ")));
            };
            data.push((key, self.root(f, class)?));
        }
        Ok(Json::Obj(data))
    }

    fn arg(&self, f: &Field, name: &str) -> Option<Value> {
        f.args.iter().find(|(k, _)| k == name).map(|(_, v)| resolve_var(v, &self.vars))
    }

    fn root(&self, f: &Field, class: Id) -> Result<Json, String> {
        let ty = self.store.dict.lookup(&Term::iri(RDF_TYPE)).ok_or("no rdf:type in data")?;
        let mut nodes: Vec<Id> = match self.arg(f, "id").and_then(|v| as_text(&v)) {
            Some(iri) => self.store.dict.lookup(&Term::iri(iri)).into_iter().filter(|&n| self.store.quads_matching(Some(n), Some(ty), Some(class), None).next().is_some()).collect(),
            None => {
                let mut seen = HashSet::new();
                self.store.quads_matching(None, Some(ty), Some(class), None).map(|q| q[0]).filter(|n| seen.insert(*n)).collect()
            }
        };
        if let Some(Value::Object(conds)) = self.arg(f, "where") {
            for (field, want) in conds {
                let want = as_text(&resolve_var(&want, &self.vars)).unwrap_or_default();
                nodes.retain(|&n| self.values(n, &field).iter().any(|&v| self.store.dict.term(v).value() == want));
            }
        }
        if let Some(order) = self.arg(f, "orderBy").and_then(|v| as_text(&v)) {
            let mut keyed: Vec<(Option<Term>, Id)> = nodes.iter().map(|&n| (self.values(n, &order).first().map(|&v| self.store.dict.term(v)), n)).collect();
            keyed.sort_by(|a, b| func::order_cmp(a.0.as_ref(), b.0.as_ref()));
            if matches!(self.arg(f, "desc"), Some(Value::Bool(true))) {
                keyed.reverse();
            }
            nodes = keyed.into_iter().map(|(_, n)| n).collect();
        }
        let offset = match self.arg(f, "offset") {
            Some(Value::Int(n)) => n.max(0) as usize,
            _ => 0,
        };
        let limit = match self.arg(f, "limit") {
            Some(Value::Int(n)) => n.max(0) as usize,
            _ => 100,
        };
        let items: Vec<Json> = nodes.into_iter().skip(offset).take(limit).map(|n| self.object(n, &f.selection, 0)).collect::<Result<_, _>>()?;
        Ok(Json::Arr(items))
    }

    fn object(&self, node: Id, selection: &[Field], depth: usize) -> Result<Json, String> {
        if depth > 8 {
            return Err("selection nested too deeply (max 8)".into());
        }
        let left = self.budget.get();
        if left == 0 {
            return Err("result too large; narrow the query with where/limit".into());
        }
        self.budget.set(left - 1);
        let mut out = Vec::new();
        for f in selection {
            let key = f.alias.clone().unwrap_or_else(|| f.name.clone());
            let v = match f.name.as_str() {
                "id" => term_json(&self.store.dict.term(node)),
                "__typename" => Json::Str("Resource".into()),
                "types" => {
                    let ty = self.store.dict.lookup(&Term::iri(RDF_TYPE));
                    Json::Arr(self.store.quads_matching(Some(node), ty, None, None).map(|q| term_json(&self.store.dict.term(q[2]))).collect())
                }
                name => {
                    let vals = self.values(node, name);
                    if f.selection.is_empty() {
                        let mut js: Vec<Json> = vals.iter().map(|&v| term_json(&self.store.dict.term(v))).collect();
                        match js.len() {
                            0 => Json::Null,
                            1 => js.pop().unwrap(),
                            _ => Json::Arr(js),
                        }
                    } else {
                        let limit = match self.arg(f, "limit") {
                            Some(Value::Int(n)) => n.max(0) as usize,
                            _ => 100,
                        };
                        Json::Arr(
                            vals.into_iter()
                                .filter(|&v| !self.store.dict.term(v).is_literal())
                                .take(limit)
                                .map(|v| self.object(v, &f.selection, depth + 1))
                                .collect::<Result<_, _>>()?,
                        )
                    }
                }
            };
            out.push((key, v));
        }
        Ok(Json::Obj(out))
    }
}

// ------------------------------------------------------------------ mutations

/// What a type's new fields and generated ids are made from: its class IRI up
/// to the last `#` or `/`.
fn namespace(iri: &str) -> &str {
    iri.rfind(['#', '/']).map_or(iri, |i| &iri[..=i])
}

fn node_term(id: &str) -> Term {
    match id.strip_prefix("_:") {
        Some(b) => Term::Blank(b.to_string()),
        None => Term::iri(id),
    }
}

/// A fresh IRI for a created resource: `<namespace><type>/<16 hex digits>`.
fn new_id(class_iri: &str, type_name: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    format!("{}{}/{:016x}", namespace(class_iri), type_name.to_ascii_lowercase(), h.finish())
}

/// The predicate behind an input field: the one the schema knows (the one in the
/// type's namespace when several share the name), else a new one in that namespace.
fn predicate(schema: &Schema, store: &Store, class_iri: &str, field: &str) -> Result<String, String> {
    let iris: Vec<String> = schema.predicates(field).iter().map(|&p| store.dict.term(p).value().to_string()).collect();
    let ns = namespace(class_iri);
    match iris.len() {
        0 => Ok(format!("{ns}{field}")),
        1 => Ok(iris[0].clone()),
        _ => iris.iter().find(|i| i.starts_with(ns)).cloned().ok_or_else(|| format!("Field '{field}' is ambiguous: {}", iris.join(", "))),
    }
}

/// The RDF terms an input value stands for. A list gives several, `null` none;
/// `{id}` is a link and `{value, lang | datatype}` a literal spelled out. A plain
/// value follows what the predicate already holds: links stay links and typed
/// values keep their datatype.
fn input_terms(store: &Store, pred: &str, v: &Value, out: &mut Vec<Term>) -> Result<(), String> {
    match v {
        Value::Null => {}
        Value::List(items) => {
            for i in items {
                input_terms(store, pred, i, out)?;
            }
        }
        Value::Object(kv) => {
            let get = |k: &str| kv.iter().find(|(n, _)| n == k).and_then(|(_, v)| as_text(v));
            out.push(match (get("id"), get("value"), get("lang"), get("datatype")) {
                (Some(id), ..) => node_term(&id),
                (None, Some(val), Some(lang), _) => Term::lang(val, &lang),
                (None, Some(val), None, Some(dt)) => Term::typed(val, &dt),
                (None, Some(val), None, None) => Term::string(val),
                _ => return Err("An object value needs an id (a link) or a value (a literal)".into()),
            });
        }
        Value::Var(_) => {}
        scalar => {
            let text = as_text(scalar).unwrap_or_default();
            let sample = store.dict.lookup(&Term::iri(pred)).and_then(|p| store.explicit_matching(None, Some(p), None, None).next()).map(|q| store.dict.term(q[2]));
            out.push(match (scalar, sample) {
                (Value::Bool(b), _) => Term::boolean(*b),
                (Value::Str(_) | Value::Enum(_), Some(Term::Iri(_) | Term::Blank(_))) => node_term(&text),
                (Value::Str(_) | Value::Int(_) | Value::Float(_), Some(sample @ Term::Literal(_))) if !matches!(&sample, Term::Literal(l) if [XSD_STRING, RDF_LANGSTRING, RDF_DIRLANGSTRING].contains(&l.datatype.as_str())) => {
                    let Term::Literal(l) = &sample else { unreachable!() };
                    let t = Term::typed(text.clone(), &l.datatype);
                    if func::num(&sample).is_some() && func::num(&t).is_none() {
                        return Err(format!("'{text}' is not a valid <{}>", l.datatype));
                    }
                    t
                }
                (Value::Int(_), _) => Term::typed(text, XSD_INTEGER),
                (Value::Float(_), _) => Term::typed(text, XSD_DECIMAL),
                _ => Term::string(text),
            });
        }
    }
    Ok(())
}

/// One write to a resource of a type: what a mutation field (and a SQL INSERT,
/// UPDATE or DELETE row) does.
pub struct Write<'a> {
    /// "create", "update" or "delete"
    pub op: &'a str,
/// The type the mutation works on.
    pub type_name: &'a str,
    /// the class IRI, for a type the data does not have yet (create only)
    pub class: Option<String>,
    /// generated for a create without one
    pub id: Option<String>,
/// The fields to create or replace, with their values.
    pub input: &'a [(String, Value)],
/// The graph to write into, when the request names one.
    pub graph: Option<String>,
}

/// Apply one write; returns the resource's id. `schema` learns the fields it
/// introduces.
pub fn write(store: &mut Store, schema: &mut Schema, w: &Write) -> Result<String, String> {
    let (op, type_name) = (w.op, w.type_name);
    // a type the data does not have yet can be created when its class IRI is given
    let class_iri = match (schema.class(type_name), &w.class) {
        (Some(c), _) => store.dict.term(c).value().to_string(),
        (None, Some(iri)) if op == "create" => iri.clone(),
        _ => return Err(format!("Unknown type '{type_name}'. Available: {}", schema.classes.keys().cloned().collect::<Vec<_>>().join(", "))),
    };
    let graph = w.graph.as_deref().map(Term::iri);
    let id = match &w.id {
        Some(id) => id.clone(),
        None if op == "create" => new_id(&class_iri, type_name),
        None => return Err(format!("{op} {type_name} needs an id")),
    };
    let (node, ty, class) = (node_term(&id), Term::iri(RDF_TYPE), Term::iri(class_iri.as_str()));
    let gid = graph.as_ref().map(|g| store.dict.lookup(g));
    if gid == Some(None) && op != "create" {
        return Err(format!("No {type_name} with id {id} in graph {}", w.graph.as_deref().unwrap_or_default()));
    }
    let lookup = |t: &Term| store.dict.lookup(t);
    let type_quad = match (lookup(&node), lookup(&ty), lookup(&class)) {
        (Some(n), Some(t), Some(c)) => store.explicit_matching(Some(n), Some(t), Some(c), gid.flatten()).next().map(|q| store.decode(q)),
        _ => None,
    };
    match (op, &type_quad) {
        ("create", Some(_)) => return Err(format!("{type_name} {id} already exists")),
        ("update" | "delete", None) => return Err(format!("No {type_name} with id {id}")),
        ("create" | "update" | "delete", _) => {}
        _ => return Err(format!("Unknown write '{op}'")),
    }
    let mut remove = Vec::new();
    let mut insert = Vec::new();
    if op == "delete" {
        let n = lookup(&node).expect("exists");
        let g = gid.flatten();
        remove.extend(store.explicit_matching(Some(n), None, None, g).chain(store.explicit_matching(None, None, Some(n), g)).map(|q| store.decode(q)));
        store.apply(&remove, &[]).map_err(|e| e.to_string())?;
        return Ok(id);
    }
    // new values go to the given graph, else where the resource's type statement is
    let g = graph.clone().or_else(|| type_quad.as_ref().and_then(|q| q.g.clone()));
    if op == "create" {
        insert.push(Quad { s: node.clone(), p: ty.clone(), o: class.clone(), g: g.clone() });
    }
    let mut new_fields = Vec::new();
    for (field, value) in w.input {
        if field == "id" || field == "types" {
            return Err(format!("'{field}' cannot be set in the input"));
        }
        let pred = predicate(schema, store, &class_iri, field)?;
        if op == "update" {
            if let (Some(n), Some(p)) = (lookup(&node), lookup(&Term::iri(pred.as_str()))) {
                remove.extend(store.explicit_matching(Some(n), Some(p), None, gid.flatten()).map(|q| store.decode(q)));
            }
        }
        let mut terms = Vec::new();
        input_terms(store, &pred, value, &mut terms)?;
        for o in terms {
            insert.push(Quad { s: node.clone(), p: Term::iri(pred.as_str()), o, g: g.clone() });
        }
        new_fields.push((field.clone(), pred));
    }
    store.apply(&remove, &insert).map_err(|e| e.to_string())?;
    // fields first used here are known from now on
    for (field, pred) in new_fields {
        if let Some(p) = store.dict.lookup(&Term::iri(pred)) {
            let ids = schema.fields.entry(field).or_default();
            if !ids.contains(&p) {
                ids.push(p);
            }
        }
    }
    Ok(id)
}

/// Run the root fields of a mutation one after another on the store. The caller
/// makes the request all or nothing (an error leaves the changes made so far to undo).
pub fn mutate(store: &mut Store, fields: &[Field], vars: &Json) -> Result<Json, String> {
    let mut schema = Schema::build(store, 200);
    let mut data = Vec::new();
    for f in fields {
        let key = f.alias.clone().unwrap_or_else(|| f.name.clone());
        if f.name == "__typename" {
            data.push((key, Json::Str("Mutation".into())));
            continue;
        }
        let Some((op, type_name)) = ["create", "update", "delete"].iter().find_map(|op| f.name.strip_prefix(op).filter(|t| !t.is_empty()).map(|t| (*op, t))) else {
            return Err(format!("Unknown mutation '{}'; use create<Type>, update<Type> or delete<Type>", f.name));
        };
        let arg = |name: &str| f.args.iter().find(|(k, _)| k == name).map(|(_, v)| resolve_var(v, vars));
        let input = match arg("input") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Object(input)) => input,
            Some(_) => return Err("input must be an object".into()),
        };
        let w = Write {
            op,
            type_name,
            class: arg("type").and_then(|v| as_text(&v)),
            id: arg("id").and_then(|v| as_text(&v)),
            input: &input,
            graph: arg("graph").and_then(|v| as_text(&v)),
        };
        // a deleted resource is shown as it was
        let before = match (op, &w.id) {
            ("delete", Some(id)) if !f.selection.is_empty() => store.dict.lookup(&node_term(id)).map(|n| Exec::new(store, &schema, vars.clone()).object(n, &f.selection, 0)).transpose()?,
            _ => None,
        };
        let id = write(store, &mut schema, &w)?;
        let out = match before {
            Some(out) => out,
            None if f.selection.is_empty() || op == "delete" => Json::Str(id),
            None => {
                let n = store.dict.lookup(&node_term(&id)).ok_or("the resource was not stored")?;
                Exec::new(store, &schema, vars.clone()).object(n, &f.selection, 0)?
            }
        };
        data.push((key, out));
    }
    Ok(Json::Obj(data))
}

fn errors(message: impl Into<String>) -> Json {
    Json::obj().with("errors", Json::Arr(vec![Json::obj().with("message", message.into())]))
}

/// Run a GraphQL query (not a mutation); returns the standard `{data}` / `{errors}` body.
pub fn execute(store: &Store, query: &str, variables: Json) -> Json {
    let fields = match parse(query) {
        Ok((false, f)) => f,
        Ok((true, _)) => return errors("A mutation needs a POST request"),
        Err(e) => return errors(e),
    };
    let schema = Schema::build(store, 200);
    match Exec::new(store, &schema, variables).run(&fields) {
        Ok(data) => Json::obj().with("data", data),
        Err(e) => errors(e),
    }
}

/// Run a mutation request on the store; `Err` holds the `{errors}` body, after
/// which the caller undoes what was changed.
pub fn execute_mutation(store: &mut Store, query: &str, variables: &Json) -> Result<Json, Json> {
    let fields = match parse(query) {
        Ok((true, f)) => f,
        Ok((false, _)) => return Err(errors("Not a mutation")),
        Err(e) => return Err(errors(e)),
    };
    mutate(store, &fields, variables).map(|data| Json::obj().with("data", data)).map_err(errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdf::Quad;

    fn st() -> Store {
        let mut st = Store::memory();
        let ttl = r#"@prefix ex: <http://ex.org/> .
            ex:ann a ex:Person ; ex:name "Ann" ; ex:age 31 ; ex:knows ex:bob .
            ex:bob a ex:Person ; ex:name "Bob" ; ex:age 25 ; ex:knows ex:ann, ex:cy .
            ex:cy a ex:Person ; ex:name "Cy" ; ex:age 40 .
            ex:acme a ex:Company ; ex:name "Acme" ."#;
        let quads: Vec<Quad> = crate::rdf::turtle::parse(ttl, "", crate::rdf::turtle::Mode::Turtle).unwrap();
        st.insert(&quads).unwrap();
        st
    }

    #[test]
    fn queries_with_filters_order_and_nesting() {
        let st = st();
        let r = execute(&st, r#"{ people: Person(orderBy: "age", desc: true) { name age } }"#, Json::obj()).to_string();
        assert_eq!(r, r#"{"data":{"people":[{"name":"Cy","age":40},{"name":"Ann","age":31},{"name":"Bob","age":25}]}}"#);
        let r = execute(&st, r#"query Q($n: String) { Person(where: {name: $n}) { id knows(limit: 5) { name } } }"#, Json::obj().with("n", "Bob")).to_string();
        assert!(r.contains(r#""id":"http://ex.org/bob""#) && r.contains(r#"{"name":"Ann"}"#) && r.contains(r#"{"name":"Cy"}"#), "{r}");
        let r = execute(&st, "{ Company { name } }", Json::obj()).to_string();
        assert_eq!(r, r#"{"data":{"Company":[{"name":"Acme"}]}}"#);
        assert!(execute(&st, "{ Nope { id } }", Json::obj()).to_string().contains("Unknown type"));
        let sdl = Schema::build(&st, 10).sdl();
        assert!(sdl.contains("Person(") && sdl.contains("knows: [Resource]") && sdl.contains("age: [String]"), "{sdl}");
    }

    #[test]
    fn mutations_create_update_and_delete() {
        let mut st = st();
        let vars = Json::obj();
        let run = |st: &mut Store, q: &str| match execute_mutation(st, q, &vars) {
            Ok(j) | Err(j) => j.to_string(),
        };
        // values follow the predicate: knows links, age stays an integer
        let r = run(&mut st, r#"mutation { dan: createPerson(id: "http://ex.org/dan", input: {name: "Dan", age: "22", knows: ["http://ex.org/ann"]}) { id name age knows { name } } }"#);
        assert_eq!(r, r#"{"data":{"dan":{"id":"http://ex.org/dan","name":"Dan","age":22,"knows":[{"name":"Ann"}]}}}"#);
        assert!(run(&mut st, r#"mutation { createPerson(id: "http://ex.org/dan") { id } }"#).contains("already exists"));
        assert!(run(&mut st, r#"mutation { updatePerson(id: "http://ex.org/dan", input: {age: "old"}) { id } }"#).contains("not a valid"));
        // update replaces the given fields only; null removes; a new field lands in the type's namespace
        let r = run(&mut st, r#"mutation { updatePerson(id: "http://ex.org/dan", input: {age: 23, knows: null, email: "dan@ex.org"}) { name age knows { id } email } }"#);
        assert_eq!(r, r#"{"data":{"updatePerson":{"name":"Dan","age":23,"knows":[],"email":"dan@ex.org"}}}"#);
        let email = st.dict.lookup(&Term::iri("http://ex.org/email")).unwrap();
        assert_eq!(st.quads_matching(None, Some(email), None, None).count(), 1);
        // a generated id, variables nested in the input, and a new type with its class IRI
        let vars = Json::obj().with("n", "Eve");
        let r = match execute_mutation(&mut st, r#"mutation M($n: String) { createPerson(input: {name: $n}) { id name } createRobot(type: "http://ex.org/Robot", input: {name: "R2"}) { types } }"#, &vars) {
            Ok(j) | Err(j) => j.to_string(),
        };
        assert!(r.contains(r#""id":"http://ex.org/person/"#) && r.contains(r#""name":"Eve""#) && r.contains(r#""types":["http://ex.org/Robot"]"#), "{r}");
        // delete removes the resource's statements and the links to it
        let r = run(&mut st, r#"mutation { deletePerson(id: "http://ex.org/ann") { name } }"#);
        assert_eq!(r, r#"{"data":{"deletePerson":{"name":"Ann"}}}"#);
        let ann = st.dict.lookup(&Term::iri("http://ex.org/ann")).unwrap();
        assert_eq!(st.quads_matching(Some(ann), None, None, None).count() + st.quads_matching(None, None, Some(ann), None).count(), 0);
        assert!(run(&mut st, r#"mutation { deletePerson(id: "http://ex.org/ann") }"#).contains("No Person"));
        assert!(run(&mut st, r#"mutation { frobPerson(id: "x") }"#).contains("Unknown mutation"));
        assert!(execute(&st, r#"mutation { deletePerson(id: "http://ex.org/bob") }"#, Json::obj()).to_string().contains("needs a POST"));
        assert!(Schema::build(&st, 10).sdl().contains("updatePerson(id: ID!, input: PersonInput"));
    }
}
