//! RDF serializers, plus a pragmatic JSON-LD reader.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use super::*;
use crate::json::Json;

/// Quads as a serialization (the prefixes abbreviate Turtle and RDF/XML).
pub fn serialize(quads: &[Quad], format: Format, prefixes: &[(String, String)]) -> Result<String, String> {
    Ok(match format {
        Format::NTriples => {
            let mut out = String::new();
            for q in quads {
                let _ = writeln!(out, "{} {} {} .", q.s, q.p, q.o);
            }
            out
        }
        Format::NQuads => {
            let mut out = String::new();
            for q in quads {
                match &q.g {
                    Some(g) => {
                        let _ = writeln!(out, "{} {} {} {} .", q.s, q.p, q.o, g);
                    }
                    None => {
                        let _ = writeln!(out, "{} {} {} .", q.s, q.p, q.o);
                    }
                }
            }
            out
        }
        Format::Turtle => turtle(quads, prefixes, false),
        Format::TriG => turtle(quads, prefixes, true),
        Format::JsonLd => jsonld(quads),
        Format::YamlLd => yaml::write(&jsonld_json(quads)),
        // Turtle is valid N3; named graphs have no N3 equivalent
        Format::N3 if quads.iter().all(|q| q.g.is_none()) => turtle(quads, prefixes, false),
        Format::N3 => return Err("N3 output cannot hold named graphs; use TriG or N-Quads".into()),
        Format::RdfXml => super::xml::write_rdfxml(quads, prefixes)?,
    })
}

fn is_pn_local(s: &str) -> bool {
    !s.is_empty()
        && !s.ends_with('.')
        && !s.starts_with('.')
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
}

struct Abbrev<'a> {
    prefixes: &'a [(String, String)],
    used: std::cell::RefCell<std::collections::BTreeSet<usize>>,
}

impl Abbrev<'_> {
    fn iri(&self, iri: &str) -> String {
        let mut best: Option<(usize, &str, &str)> = None;
        for (i, (p, ns)) in self.prefixes.iter().enumerate() {
            if let Some(local) = iri.strip_prefix(ns.as_str()) {
                if is_pn_local(local) || local.is_empty() {
                    if best.is_none_or(|(_, _, n)| ns.len() > n.len()) {
                        best = Some((i, p, ns));
                    }
                }
            }
        }
        match best {
            Some((i, p, ns)) => {
                self.used.borrow_mut().insert(i);
                format!("{p}:{}", &iri[ns.len()..])
            }
            None => Term::Iri(iri.to_string()).to_string(),
        }
    }

    fn term(&self, t: &Term) -> String {
        match t {
            Term::Triple(q) => {
                let p = match &q[1] {
                    Term::Iri(i) => self.iri(i),
                    other => other.to_string(),
                };
                format!("<<( {} {} {} )>>", self.term(&q[0]), p, self.term(&q[2]))
            }
            Term::Iri(i) => {
                if i == RDF_TYPE {
                    "a".into()
                } else {
                    self.iri(i)
                }
            }
            Term::Blank(_) => t.to_string(),
            Term::Literal(l) => {
                if l.lang.is_none() && l.datatype != XSD_STRING {
                    let bare = match l.datatype.as_str() {
                        XSD_INTEGER => l.value.parse::<i64>().is_ok() && !l.value.starts_with('+'),
                        XSD_BOOLEAN => l.value == "true" || l.value == "false",
                        _ => false,
                    };
                    if bare {
                        return l.value.clone();
                    }
                    let mut s = String::from("\"");
                    escape_literal(&mut s, &l.value);
                    s.push_str("\"^^");
                    s.push_str(&self.iri(&l.datatype));
                    return s;
                }
                t.to_string()
            }
        }
    }
}

fn turtle(quads: &[Quad], prefixes: &[(String, String)], graphs: bool) -> String {
    let ab = Abbrev { prefixes, used: Default::default() };
    let mut out = String::new();
    // graph -> subject -> [(p, o)], keeping first-seen order
    let mut order: Vec<Option<&Term>> = Vec::new();
    let mut by_graph: HashMap<Option<&Term>, (Vec<&Term>, HashMap<&Term, Vec<(&Term, &Term)>>)> = HashMap::new();
    for q in quads {
        let g = if graphs { q.g.as_ref() } else { None };
        let entry = by_graph.entry(g).or_insert_with(|| {
            order.push(g);
            (Vec::new(), HashMap::new())
        });
        let po = entry.1.entry(&q.s).or_insert_with(|| {
            entry.0.push(&q.s);
            Vec::new()
        });
        po.push((&q.p, &q.o));
    }
    for g in order {
        let (subjects, map) = &by_graph[&g];
        let indent = if g.is_some() { "    " } else { "" };
        if let Some(g) = g {
            let _ = writeln!(out, "{} {{", ab.term(g));
        }
        for s in subjects {
            let pos = &map[s];
            let _ = write!(out, "{indent}{}", ab.term(s));
            let mut last_p: Option<&Term> = None;
            for (k, (p, o)) in pos.iter().enumerate() {
                if last_p == Some(*p) {
                    let _ = write!(out, ", {}", ab.term(o));
                } else {
                    if k > 0 {
                        out.push_str(" ;");
                        let _ = write!(out, "\n{indent}    ");
                    } else {
                        out.push(' ');
                    }
                    let _ = write!(out, "{} {}", ab.term(p), ab.term(o));
                }
                last_p = Some(*p);
            }
            out.push_str(" .\n");
        }
        if g.is_some() {
            out.push_str("}\n");
        }
        out.push('\n');
    }
    // Declare only the prefixes the body actually uses.
    let mut head = String::new();
    for &i in ab.used.borrow().iter() {
        let (p, ns) = &prefixes[i];
        let _ = writeln!(head, "@prefix {p}: <{ns}> .");
    }
    if !head.is_empty() {
        head.push('\n');
    }
    head + &out
}

fn jsonld_object(t: &Term) -> Json {
    match t {
        // JSON-LD-star embedded node
        Term::Triple(q) => Json::obj().with(
            "@id",
            Json::obj().with("@id", id_of(&q[0])).with(q[1].value(), Json::Arr(vec![jsonld_object(&q[2])])),
        ),
        Term::Iri(i) => Json::obj().with("@id", i.as_str()),
        Term::Blank(b) => Json::obj().with("@id", format!("_:{b}")),
        Term::Literal(l) => {
            let mut o = Json::obj().with("@value", l.value.as_str());
            if let Some(lang) = &l.lang {
                // JSON-LD 1.1 keeps the base direction in @direction
                match lang.split_once("--") {
                    Some((lang, dir)) => {
                        o.set("@language", lang);
                        o.set("@direction", dir);
                    }
                    None => o.set("@language", lang.as_str()),
                }
            } else if l.datatype != XSD_STRING {
                o.set("@type", l.datatype.as_str());
            }
            o
        }
    }
}

fn id_of(t: &Term) -> String {
    match t {
        Term::Blank(b) => format!("_:{b}"),
        // a triple term: a unique key; the node map keeps the embedded node for `@id`
        Term::Triple(_) => t.to_string(),
        other => other.value().to_string(),
    }
}

/// Expanded, flattened JSON-LD.
fn jsonld(quads: &[Quad]) -> String {
    format!("{}", jsonld_json(quads))
}

/// The `@id` value of a triple-term subject: the embedded node it describes.
fn star_id(t: &Term) -> Json {
    match jsonld_object(t) {
        // jsonld_object wraps the embedded node in {"@id": …}; @id wants the node itself
        o => o.get("@id").cloned().unwrap_or(Json::Str(t.to_string())),
    }
}

/// The JSON-LD document of `quads`, as a `Json` value (also the YAML-LD writer's input).
fn jsonld_json(quads: &[Quad]) -> Json {
    let mut graphs: BTreeMap<Option<String>, BTreeMap<String, Vec<(String, Json)>>> = BTreeMap::new();
    let mut star_ids: BTreeMap<String, Json> = BTreeMap::new();
    for q in quads {
        let g = q.g.as_ref().map(id_of);
        if q.s.is_triple() {
            let id = id_of(&q.s);
            star_ids.entry(id.clone()).or_insert_with(|| star_id(&q.s));
        }
        let node = graphs.entry(g).or_default().entry(id_of(&q.s)).or_default();
        if q.p.value() == RDF_TYPE && !q.o.is_literal() {
            node.push(("@type".into(), Json::Str(id_of(&q.o))));
        } else {
            node.push((q.p.value().to_string(), jsonld_object(&q.o)));
        }
    }
    let render = |nodes: &BTreeMap<String, Vec<(String, Json)>>| -> Vec<Json> {
        nodes
            .iter()
            .map(|(id, props)| {
                let id_value = star_ids.get(id).cloned().unwrap_or_else(|| Json::Str(id.clone()));
                let mut obj = Json::obj().with("@id", id_value);
                let mut grouped: Vec<(String, Vec<Json>)> = Vec::new();
                for (k, v) in props {
                    match grouped.iter_mut().find(|(gk, _)| gk == k) {
                        Some((_, vs)) => vs.push(v.clone()),
                        None => grouped.push((k.clone(), vec![v.clone()])),
                    }
                }
                for (k, vs) in grouped {
                    obj.set(&k, Json::Arr(vs));
                }
                obj
            })
            .collect()
    };
    let mut top = Vec::new();
    for (g, nodes) in &graphs {
        match g {
            None => top.extend(render(nodes)),
            Some(g) => top.push(Json::obj().with("@id", g.as_str()).with("@graph", Json::Arr(render(nodes)))),
        }
    }
    Json::Arr(top)
}

/// A float in the canonical xsd:double form (`1.0E3`, `NaN`, `INF`).
pub fn format_double(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { "INF".into() } else { "-INF".into() };
    }
    let s = format!("{n:E}");
    // Rust prints "1E3"; canonical xsd:double is "1.0E3"
    match s.split_once('E') {
        Some((m, e)) if !m.contains('.') => format!("{m}.0E{e}"),
        _ => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonld_compact() {
        let doc = r#"{"@context":{"ex":"http://ex.org/","name":"ex:name","knows":{"@id":"ex:knows","@type":"@id"}},
            "@id":"ex:a","@type":"ex:Person","name":"Alice","knows":"ex:b"}"#;
        let q = super::super::jsonld::to_rdf(doc, &super::super::jsonld::Options::new("")).unwrap();
        assert_eq!(q.len(), 3);
        assert!(q.iter().any(|q| q.o == Term::iri("http://ex.org/b")));
        assert!(q.iter().any(|q| q.o == Term::string("Alice")));
    }

    #[test]
    fn turtle_roundtrip() {
        let ttl = "@prefix ex: <http://ex.org/> . ex:a ex:p 1, \"x\"@en ; a ex:C .";
        let q = turtle::parse(ttl, "", turtle::Mode::Turtle).unwrap();
        let out = serialize(&q, Format::Turtle, &[("ex".into(), "http://ex.org/".into())]).unwrap();
        let q2 = turtle::parse(&out, "", turtle::Mode::Turtle).unwrap();
        assert_eq!(q.len(), q2.len());
    }
}
