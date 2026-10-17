//! RDF 1.1 data model plus format detection.

pub mod canon;
pub mod jsonld;
pub mod ntriples;
pub mod turtle;
pub mod write;
pub mod xml;
pub mod yaml;

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// The XML Schema namespace, for literal datatypes.
pub const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
/// The RDF namespace.
pub const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
/// The RDFS namespace.
pub const RDFS: &str = "http://www.w3.org/2000/01/rdf-schema#";
/// The OWL namespace.
pub const OWL: &str = "http://www.w3.org/2002/07/owl#";
/// `xsd:string`, the datatype of a plain literal.
pub const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
/// `xsd:boolean`.
pub const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
/// `xsd:integer`.
pub const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
/// `xsd:decimal`.
pub const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
/// `xsd:double`.
pub const XSD_DOUBLE: &str = "http://www.w3.org/2001/XMLSchema#double";
/// `xsd:dateTime`.
pub const XSD_DATETIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
/// `rdf:langString`, the datatype of a language-tagged literal.
pub const RDF_LANGSTRING: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString";
/// RDF 1.2 literals with a base direction; the direction is kept in the language tag
/// (`en--ltr`), so the store needs no extra field.
pub const RDF_DIRLANGSTRING: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#dirLangString";
/// A BCP 47 language tag as RDF allows it: a primary subtag of 1-8 letters, then subtags
/// of 1-8 letters or digits.
pub fn valid_lang(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let first = parts.next().unwrap_or("");
    (1..=8).contains(&first.len()) && first.bytes().all(|b| b.is_ascii_alphabetic()) && parts.all(|p| (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// `rdf:reifies` (RDF 1.2): a reifier's statement about a triple term.
pub const RDF_REIFIES: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies";
/// `rdf:type`.
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
/// `rdf:first`, the head of an RDF list.
pub const RDF_FIRST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#first";
/// `rdf:rest`, the tail of an RDF list.
pub const RDF_REST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#rest";
/// `rdf:nil`, the empty RDF list.
pub const RDF_NIL: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#nil";

#[derive(Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
/// A term of an RDF statement: the subject, predicate or object position.
pub enum Term {
/// An IRI.
    Iri(String),
/// A blank node, by its label.
    Blank(String),
/// A literal (see [`Literal`]).
    Literal(Literal),
    /// RDF-star quoted triple.
    Triple(Box<[Term; 3]>),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
/// A literal: a lexical form with a language tag or a datatype.
pub struct Literal {
/// The lexical form, as written.
    pub value: String,
    /// Full datatype IRI (xsd:string for plain, rdf:langString with a language).
    pub datatype: String,
/// The language tag (lowercase), for a language-tagged literal; an RDF 1.2 base
/// direction may follow after `--` (`en--ltr`).
    pub lang: Option<String>,
}

impl Term {
/// An IRI term.
    pub fn iri(s: impl Into<String>) -> Term {
        Term::Iri(s.into())
    }
/// A plain string literal.
    pub fn string(s: impl Into<String>) -> Term {
        Term::Literal(Literal { value: s.into(), datatype: XSD_STRING.into(), lang: None })
    }
/// A literal with the given datatype.
    pub fn typed(s: impl Into<String>, dt: &str) -> Term {
        Term::Literal(Literal { value: s.into(), datatype: dt.into(), lang: None })
    }
/// A language-tagged literal.
    pub fn lang(s: impl Into<String>, lang: &str) -> Term {
        Term::Literal(Literal {
            value: s.into(),
            datatype: RDF_LANGSTRING.into(),
            lang: Some(lang.to_ascii_lowercase()),
        })
    }
    /// RDF 1.2 literal with a language and a base direction (`ltr` / `rtl`).
    pub fn dir_lang(s: impl Into<String>, lang: &str, dir: &str) -> Term {
        Term::Literal(Literal { value: s.into(), datatype: RDF_DIRLANGSTRING.into(), lang: Some(format!("{}--{dir}", lang.to_ascii_lowercase())) })
    }
/// The `xsd:boolean` literal for `b`.
    pub fn boolean(b: bool) -> Term {
        Term::typed(if b { "true" } else { "false" }, XSD_BOOLEAN)
    }
/// The `xsd:integer` literal for `i`.
    pub fn integer(i: i64) -> Term {
        Term::typed(i.to_string(), XSD_INTEGER)
    }
/// A fresh blank node label, never issued before in this process.
    pub fn fresh_blank() -> Term {
        Term::Blank(fresh_blank_id())
    }
/// Whether this is an IRI.
    pub fn is_iri(&self) -> bool {
        matches!(self, Term::Iri(_))
    }
/// Whether this is a blank node.
    pub fn is_blank(&self) -> bool {
        matches!(self, Term::Blank(_))
    }
/// Whether this is a literal.
    pub fn is_literal(&self) -> bool {
        matches!(self, Term::Literal(_))
    }
/// Whether this is a quoted triple (RDF-star).
    pub fn is_triple(&self) -> bool {
        matches!(self, Term::Triple(_))
    }
/// A quoted triple term of the three terms.
    pub fn triple(s: Term, p: Term, o: Term) -> Term {
        Term::Triple(Box::new([s, p, o]))
    }
/// The literal, when this is one.
    pub fn as_literal(&self) -> Option<&Literal> {
        match self {
            Term::Literal(l) => Some(l),
            _ => None,
        }
    }
    /// The lexical form / IRI / blank label.
    pub fn value(&self) -> &str {
        match self {
            Term::Iri(s) | Term::Blank(s) => s,
            Term::Literal(l) => &l.value,
            Term::Triple(_) => "",
        }
    }
}

static BLANK_COUNTER: AtomicU64 = AtomicU64::new(1);
static BLANK_EPOCH: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Blank node ids are unique across restarts (a per-process prefix), so ids
/// minted now never collide with ones already persisted in a store.
pub fn fresh_blank_id() -> String {
    let epoch = BLANK_EPOCH.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let mut n = nanos;
        let mut s = String::new();
        while n > 0 {
            s.push(char::from_digit((n % 36) as u32, 36).unwrap());
            n /= 36;
        }
        s
    });
    format!("b{epoch}x{}", BLANK_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Write `s` as the lexical form of a literal: quoted, with `\`, `"`, newline and
/// carriage return escaped.
pub fn escape_literal(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
}

/// Write `s` as an IRI between angle brackets, with the characters Turtle forbids
/// escaped.
pub fn escape_iri(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '<' | '>' | '"' | '{' | '}' | '|' | '^' | '`' | '\\' | ' ' => {
                out.push_str(&format!("\\u{:04X}", c as u32))
            }
            c if (c as u32) <= 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
}

/// N-Triples syntax.
impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        match self {
            Term::Iri(i) => {
                s.push('<');
                escape_iri(&mut s, i);
                s.push('>');
            }
            Term::Blank(b) => {
                s.push_str("_:");
                s.push_str(b);
            }
            Term::Triple(t) => {
                s.push_str(&format!("<<( {} {} {} )>>", t[0], t[1], t[2]));
            }
            Term::Literal(l) => {
                s.push('"');
                escape_literal(&mut s, &l.value);
                s.push('"');
                if let Some(lang) = &l.lang {
                    s.push('@');
                    s.push_str(lang);
                } else if l.datatype != XSD_STRING {
                    s.push_str("^^<");
                    escape_iri(&mut s, &l.datatype);
                    s.push('>');
                }
            }
        }
        f.write_str(&s)
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
/// One statement: a triple, and the graph it belongs to.
pub struct Quad {
/// The subject.
    pub s: Term,
/// The predicate (always an IRI in RDF; blank nodes only in generalized RDF).
    pub p: Term,
/// The object.
    pub o: Term,
    /// None = default graph.
    pub g: Option<Term>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
/// A concrete RDF serialization.
pub enum Format {
/// N-Triples: one triple per line.
    NTriples,
/// N-Quads: one quad per line, graphs included.
    NQuads,
/// Turtle.
    Turtle,
/// TriG: Turtle with named graphs.
    TriG,
/// RDF/XML.
    RdfXml,
/// JSON-LD 1.1.
    JsonLd,
    /// YAML-LD 1.0: a JSON-LD document in YAML syntax (application/ld+yaml).
    YamlLd,
    /// Notation3 (read only): formulas become blank-node named graphs.
    N3,
}

impl Format {
/// The format of a media type, ignoring parameters; `None` when the type names none.
    pub fn from_media_type(mt: &str) -> Option<Format> {
        let mt = mt.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        Some(match mt.as_str() {
            "application/n-triples" | "text/plain" | "application/x-ntriples" => Format::NTriples,
            "application/n-quads" | "text/x-nquads" | "text/nquads" => Format::NQuads,
            "text/n3" | "text/rdf+n3" => Format::N3,
            "text/turtle" | "application/x-turtle" | "application/turtle" | "application/x-turtlestar" | "text/x-turtlestar" => Format::Turtle,
            "application/trig" | "application/x-trig" | "application/x-trigstar" => Format::TriG,
            "application/rdf+xml" | "application/xml" | "text/xml" => Format::RdfXml,
            "application/ld+json" | "application/json" => Format::JsonLd,
            "application/ld+yaml" | "application/x-ld+yaml" => Format::YamlLd,
            _ => return None,
        })
    }

/// The format of a file name, by extension; `None` when the extension names none.
    pub fn from_extension(name: &str) -> Option<Format> {
        let lower = name.to_ascii_lowercase();
        let ext = lower.trim_end_matches(".gz").rsplit('.').next()?.to_string();
        Some(match ext.as_str() {
            "nt" => Format::NTriples,
            "nq" => Format::NQuads,
            "ttl" | "ttls" => Format::Turtle,
            "n3" => Format::N3,
            "trig" | "trigs" => Format::TriG,
            "rdf" | "owl" | "xml" | "rdfs" => Format::RdfXml,
            "jsonld" | "json" => Format::JsonLd,
            "yamlld" | "ld+yaml" => Format::YamlLd,
            _ => return None,
        })
    }

/// The format's canonical media type.
    pub fn media_type(self) -> &'static str {
        match self {
            Format::NTriples => "application/n-triples",
            Format::NQuads => "application/n-quads",
            Format::Turtle => "text/turtle",
            Format::TriG => "application/trig",
            Format::RdfXml => "application/rdf+xml",
            Format::JsonLd => "application/ld+json",
            Format::YamlLd => "application/ld+yaml",
            Format::N3 => "text/n3",
        }
    }

/// The format's canonical file extension.
    pub fn extension(self) -> &'static str {
        match self {
            Format::NTriples => "nt",
            Format::NQuads => "nq",
            Format::Turtle => "ttl",
            Format::TriG => "trig",
            Format::RdfXml => "rdf",
            Format::JsonLd => "jsonld",
            Format::YamlLd => "yamlld",
            Format::N3 => "n3",
        }
    }

/// Whether the format can hold named graphs.
    pub fn supports_graphs(self) -> bool {
        matches!(self, Format::NQuads | Format::TriG | Format::JsonLd | Format::YamlLd | Format::N3)
    }
}

/// Accept header for fetching RDF from the web.
pub const RDF_ACCEPT: &str = "text/turtle, application/trig;q=0.95, application/n-triples;q=0.9, application/n-quads;q=0.9, application/rdf+xml;q=0.8, application/ld+json;q=0.7, */*;q=0.1";

/// Download and parse an RDF document; the format comes from the content
/// type, then the URL's extension, defaulting to Turtle.
pub fn fetch(url: &str, default_graph: Option<&Term>) -> Result<Vec<Quad>, String> {
    let (ctype, body) = crate::http::fetch(url, RDF_ACCEPT, std::time::Duration::from_secs(60))?;
    let format = Format::from_media_type(&ctype)
        .filter(|f| *f != Format::NTriples || !ctype.starts_with("text/plain") || url.ends_with(".nt"))
        .or_else(|| Format::from_extension(url.split(['?', '#']).next().unwrap_or(url)))
        .unwrap_or(Format::Turtle);
    let text = String::from_utf8_lossy(&body);
    parse(&text, format, url, default_graph).map_err(|e| format!("{url} ({}): {e}", format.media_type()))
}

/// Parse any supported serialization. Quads without a graph go to `default_graph`.
pub fn parse(data: &str, format: Format, base: &str, default_graph: Option<&Term>) -> Result<Vec<Quad>, String> {
    let mut quads = match format {
        Format::NTriples => ntriples::parse(data, false, false)?,
        Format::NQuads => ntriples::parse(data, true, false)?,
        Format::Turtle => turtle::parse(data, base, turtle::Mode::Turtle)?,
        Format::TriG => turtle::parse(data, base, turtle::Mode::TriG)?,
        Format::RdfXml => xml::parse_rdfxml(data, base)?,
        Format::JsonLd => jsonld::to_rdf(data, &jsonld::Options::new(base))?,
        Format::YamlLd => jsonld::to_rdf_doc(&yaml::parse(data)?, &jsonld::Options::new(base))?,
        Format::N3 => turtle::parse_n3(data, base, false)?,
    };
    if let Some(g) = default_graph {
        for q in &mut quads {
            if q.g.is_none() {
                q.g = Some(g.clone());
            }
        }
    }
    Ok(quads)
}

/// Like `parse`, but statements that do not parse are skipped and their errors added to
/// `skipped` (N-Triples, N-Quads: the line; Turtle: the statement; TriG: the graph block).
/// The other formats have no statement boundaries to resume at and parse strictly.
pub fn parse_lenient(data: &str, format: Format, base: &str, default_graph: Option<&Term>, skipped: &mut Vec<String>) -> Result<Vec<Quad>, String> {
    let mut quads = match format {
        Format::NTriples => ntriples::parse_lenient(data, false, skipped),
        Format::NQuads => ntriples::parse_lenient(data, true, skipped),
        Format::Turtle => turtle::parse_lenient(data, base, turtle::Mode::Turtle, skipped),
        Format::TriG => turtle::parse_lenient(data, base, turtle::Mode::TriG, skipped),
        _ => return parse(data, format, base, default_graph),
    };
    if let Some(g) = default_graph {
        for q in &mut quads {
            if q.g.is_none() {
                q.g = Some(g.clone());
            }
        }
    }
    Ok(quads)
}

/// RFC 3986 reference resolution.
pub fn resolve_iri(base: &str, rel: &str) -> String {
    if rel.is_empty() {
        return base.split('#').next().unwrap_or(base).to_string();
    }
    if has_scheme(rel) || base.is_empty() {
        return rel.to_string();
    }
    let (b_scheme, b_rest) = base.split_once(':').unwrap_or(("", base));
    if let Some(r) = rel.strip_prefix("//") {
        // network-path reference: keep its authority, normalize its path (RFC 3986 5.2.2)
        let end = r.find(['/', '?', '#']).unwrap_or(r.len());
        let (auth, rest) = r.split_at(end);
        let path_end = rest.find(['?', '#']).unwrap_or(rest.len());
        let (path, tail) = rest.split_at(path_end);
        return format!("{b_scheme}://{auth}{}{tail}", remove_dot_segments(path));
    }
    // Split base into authority + path (+ query)
    let (authority, b_path) = if let Some(r) = b_rest.strip_prefix("//") {
        match r.find('/') {
            Some(i) => (format!("//{}", &r[..i]), &r[i..]),
            None => (format!("//{r}"), ""),
        }
    } else {
        (String::new(), b_rest)
    };
    let b_path_noq = b_path.split(['?', '#']).next().unwrap_or("");
    if rel.starts_with('#') {
        let without_frag = base.split('#').next().unwrap_or(base);
        return format!("{without_frag}{rel}");
    }
    if rel.starts_with('?') {
        return format!("{b_scheme}:{authority}{b_path_noq}{rel}");
    }
    let merged = if rel.starts_with('/') {
        rel.to_string()
    } else if !authority.is_empty() && b_path_noq.is_empty() {
        format!("/{rel}")
    } else {
        match b_path_noq.rfind('/') {
            Some(i) => format!("{}{}", &b_path_noq[..=i], rel),
            None => rel.to_string(),
        }
    };
    format!("{b_scheme}:{authority}{}", remove_dot_segments(&merged))
}

fn has_scheme(s: &str) -> bool {
    match s.find(':') {
        Some(i) if i > 0 => {
            let scheme = &s[..i];
            scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        }
        _ => false,
    }
}

fn remove_dot_segments(path: &str) -> String {
    let (path, suffix) = match path.find(['?', '#']) {
        Some(i) => (&path[..i], &path[i..]),
        None => (path, ""),
    };
    let mut out: Vec<&str> = Vec::new();
    let segs: Vec<&str> = path.split('/').collect();
    for (i, seg) in segs.iter().enumerate() {
        let last = i == segs.len() - 1;
        match *seg {
            "." => {
                if last {
                    out.push("");
                }
            }
            ".." => {
                if out.len() > 1 {
                    out.pop();
                }
                if last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    let mut r = out.join("/");
    if path.starts_with('/') && !r.starts_with('/') {
        r.insert(0, '/');
    }
    r + suffix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve() {
        let b = "http://a/b/c/d;p?q";
        assert_eq!(resolve_iri(b, "g"), "http://a/b/c/g");
        assert_eq!(resolve_iri(b, "./g"), "http://a/b/c/g");
        assert_eq!(resolve_iri(b, "../g"), "http://a/b/g");
        assert_eq!(resolve_iri(b, "/g"), "http://a/g");
        assert_eq!(resolve_iri(b, "#s"), "http://a/b/c/d;p?q#s");
        assert_eq!(resolve_iri(b, "g:h"), "g:h");
        assert_eq!(resolve_iri("http://ex.org", "x"), "http://ex.org/x");
    }
}

#[cfg(test)]
mod lenient_tests {
    use super::*;

    #[test]
    fn lenient_parsing_skips_bad_statements() {
        let nt = "<http://a> <http://p> \"ok\" .\n<http://a> <http://p> <bad iri> .\n<http://b> <http://p> \"x\"^^<http://dt .\n<http://c> <http://p> 3 .\n_:b <http://p> \"last\" .\n";
        assert!(parse(nt, Format::NTriples, "", None).is_err());
        let mut skipped = Vec::new();
        let q = parse_lenient(nt, Format::NTriples, "", None, &mut skipped).unwrap();
        assert_eq!(q.len(), 2, "{q:?}");
        assert_eq!(skipped.len(), 3, "{skipped:?}");
        assert!(skipped[0].starts_with("line 2"), "{skipped:?}");

        let ttl = "@prefix ex: <http://ex/> .\nex:a ex:p 1, 2 .\nex:b ex:p \"unterminated .\nex:c ex:p ex:d ;\n  ex:q nope:x .\nex:e ex:p ( 1 2 ) .\n";
        let mut skipped = Vec::new();
        let q = parse_lenient(ttl, Format::Turtle, "", None, &mut skipped).unwrap();
        let subjects: Vec<&str> = q.iter().map(|q| q.s.value()).collect();
        assert!(subjects.contains(&"http://ex/a") && subjects.contains(&"http://ex/e"), "{subjects:?}");
        assert!(!subjects.contains(&"http://ex/c"), "a failed statement leaves nothing behind");
        assert_eq!(skipped.len(), 2, "{skipped:?}");
    }
}
