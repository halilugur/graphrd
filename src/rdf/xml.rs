//! A small XML parser (elements, attributes, text, CDATA, comments, internal
//! DTD entities) with an RDF/XML reader and writer on top.

use std::collections::HashMap;

use super::*;

#[derive(Debug, Clone)]
/// A node of a parsed XML tree.
pub enum Node {
/// An element with its name, attributes and children.
    Element(Element),
/// Text between the tags.
    Text(String),
}

#[derive(Debug, Clone)]
/// An XML element.
pub struct Element {
/// The qualified tag name, as written.
    pub name: String,
/// The attributes, in order, namespace prefixes unresolved.
    pub attrs: Vec<(String, String)>,
/// The children, in order.
    pub children: Vec<Node>,
}

impl Element {
/// The element children only.
    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Element(e) => Some(e),
            _ => None,
        })
    }
/// All the direct text, concatenated.
    pub fn text(&self) -> String {
        let mut s = String::new();
        for c in &self.children {
            if let Node::Text(t) = c {
                s.push_str(t);
            }
        }
        s
    }
}

/// Entity references may add at most this many bytes to a document ("billion laughs").
const MAX_ENTITY_EXPANSION: usize = 8 << 20;

/// Parse an XML document into a tree; DTD entities are expanded with a cap and a
/// nesting limit (no billion laughs).
pub fn parse_xml(text: &str) -> Result<Element, String> {
    let mut p = XmlParser { s: text.as_bytes(), i: 0, entities: HashMap::new(), expanded: 0 };
    p.prolog()?;
    let root = p.element(0)?;
    Ok(root)
}

struct XmlParser<'a> {
    s: &'a [u8],
    i: usize,
    entities: HashMap<String, String>,
    /// bytes produced by entity references so far, capped against expansion bombs
    expanded: usize,
}

impl XmlParser<'_> {
    fn err<T>(&self, msg: &str) -> Result<T, String> {
        let line = self.s[..self.i.min(self.s.len())].iter().filter(|&&b| b == b'\n').count() + 1;
        Err(format!("XML: {msg} at line {line}"))
    }
    fn starts(&self, lit: &str) -> bool {
        self.s[self.i..].starts_with(lit.as_bytes())
    }
    fn skip_until(&mut self, end: &str) -> Result<(), String> {
        while self.i < self.s.len() {
            if self.starts(end) {
                self.i += end.len();
                return Ok(());
            }
            self.i += 1;
        }
        self.err(&format!("missing {end}"))
    }
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn prolog(&mut self) -> Result<(), String> {
        if self.starts("\u{feff}") {
            self.i += 3;
        }
        loop {
            self.ws();
            if self.starts("<?") {
                self.skip_until("?>")?;
            } else if self.starts("<!--") {
                self.skip_until("-->")?;
            } else if self.starts("<!DOCTYPE") {
                self.doctype()?;
            } else {
                return Ok(());
            }
        }
    }

    fn doctype(&mut self) -> Result<(), String> {
        self.i += 9;
        while self.i < self.s.len() && self.s[self.i] != b'[' && self.s[self.i] != b'>' {
            self.i += 1;
        }
        if self.s.get(self.i) == Some(&b'[') {
            self.i += 1;
            loop {
                self.ws();
                if self.starts("]") {
                    self.i += 1;
                    break;
                } else if self.starts("<!ENTITY") {
                    self.i += 8;
                    self.ws();
                    let name = self.name();
                    self.ws();
                    let q = self.s.get(self.i).copied().unwrap_or(b'"');
                    if q != b'"' && q != b'\'' {
                        self.skip_until(">")?;
                        continue;
                    }
                    self.i += 1;
                    let start = self.i;
                    while self.i < self.s.len() && self.s[self.i] != q {
                        self.i += 1;
                    }
                    let raw = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
                    self.i += 1;
                    let val = self.decode(&raw)?;
                    self.entities.insert(name, val);
                    self.skip_until(">")?;
                } else if self.starts("<!--") {
                    self.skip_until("-->")?;
                } else if self.starts("<") {
                    self.skip_until(">")?;
                } else if self.i >= self.s.len() {
                    return self.err("unterminated DOCTYPE");
                } else {
                    self.i += 1;
                }
            }
        }
        self.skip_until(">")
    }

    fn name(&mut self) -> String {
        let start = self.i;
        while self.i < self.s.len() {
            let c = self.s[self.i];
            if c.is_ascii_whitespace() || b"=/>\"'[".contains(&c) {
                break;
            }
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
    }

    fn decode(&mut self, raw: &str) -> Result<String, String> {
        if !raw.contains('&') {
            return Ok(raw.to_string());
        }
        let mut out = String::with_capacity(raw.len());
        let mut rest = raw;
        while let Some(i) = rest.find('&') {
            out.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            let Some(end) = after.find(';') else {
                out.push('&');
                rest = after;
                continue;
            };
            let ent = &after[..end];
            match ent {
                "lt" => out.push('<'),
                "gt" => out.push('>'),
                "amp" => out.push('&'),
                "quot" => out.push('"'),
                "apos" => out.push('\''),
                e if e.starts_with("#x") || e.starts_with("#X") => {
                    let c = u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32);
                    out.push(c.unwrap_or('\u{FFFD}'));
                }
                e if e.starts_with('#') => {
                    let c = e[1..].parse::<u32>().ok().and_then(char::from_u32);
                    out.push(c.unwrap_or('\u{FFFD}'));
                }
                e => match self.entities.get(e) {
                    Some(v) => {
                        self.expanded += v.len();
                        if self.expanded > MAX_ENTITY_EXPANSION {
                            return Err("XML: entity expansion is too large".into());
                        }
                        out.push_str(v)
                    }
                    None => return Err(format!("XML: undefined entity &{e};")),
                },
            }
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn element(&mut self, depth: usize) -> Result<Element, String> {
        if depth > 512 {
            return self.err("nesting too deep");
        }
        self.ws();
        if !self.starts("<") {
            return self.err("expected element");
        }
        self.i += 1;
        let name = self.name();
        let mut attrs = Vec::new();
        loop {
            self.ws();
            if self.starts("/>") {
                self.i += 2;
                return Ok(Element { name, attrs, children: Vec::new() });
            }
            if self.starts(">") {
                self.i += 1;
                break;
            }
            let an = self.name();
            if an.is_empty() {
                return self.err("bad attribute");
            }
            self.ws();
            if !self.starts("=") {
                return self.err("expected '='");
            }
            self.i += 1;
            self.ws();
            let q = self.s.get(self.i).copied().ok_or("XML: unexpected end")?;
            if q != b'"' && q != b'\'' {
                return self.err("expected quoted attribute value");
            }
            self.i += 1;
            let start = self.i;
            while self.i < self.s.len() && self.s[self.i] != q {
                self.i += 1;
            }
            let raw = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
            self.i += 1;
            attrs.push((an, self.decode(&raw)?));
        }
        let mut children = Vec::new();
        loop {
            if self.i >= self.s.len() {
                return self.err(&format!("unclosed <{name}>"));
            }
            if self.starts("</") {
                self.i += 2;
                let close = self.name();
                if close != name {
                    return self.err(&format!("mismatched </{close}> for <{name}>"));
                }
                self.ws();
                self.i += 1; // '>'
                return Ok(Element { name, attrs, children });
            } else if self.starts("<!--") {
                self.skip_until("-->")?;
            } else if self.starts("<![CDATA[") {
                self.i += 9;
                let start = self.i;
                self.skip_until("]]>")?;
                children.push(Node::Text(String::from_utf8_lossy(&self.s[start..self.i - 3]).into_owned()));
            } else if self.starts("<?") {
                self.skip_until("?>")?;
            } else if self.starts("<") {
                children.push(Node::Element(self.element(depth + 1)?));
            } else {
                let start = self.i;
                while self.i < self.s.len() && self.s[self.i] != b'<' {
                    self.i += 1;
                }
                let raw = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
                children.push(Node::Text(self.decode(&raw)?));
            }
        }
    }
}

// ---------------------------------------------------------------- RDF/XML

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

#[derive(Clone)]
struct Scope {
    ns: HashMap<String, String>,
    base: String,
    lang: Option<String>,
    /// RDF 1.2 base direction from its:dir ("ltr" / "rtl").
    dir: Option<String>,
    /// Inside an element with rdf:version: RDF 1.2 features (its:dir, parseType="Triple") apply.
    versioned: bool,
}

/// The W3C Internationalization Tag Set namespace (its:dir gives literals a base direction).
const ITS_NS: &str = "http://www.w3.org/2005/11/its";

struct RdfXml {
    out: Vec<Quad>,
    bnodes: HashMap<String, String>,
    /// rdf:ID values already used (each may appear once per base IRI).
    ids: std::collections::HashSet<String>,
}

/// RDF/XML 7.2: names reserved by the syntax, and names removed from it.
const CORE_SYNTAX_TERMS: [&str; 7] = ["RDF", "ID", "about", "parseType", "resource", "nodeID", "datatype"];
const OLD_TERMS: [&str; 3] = ["aboutEach", "aboutEachPrefix", "bagID"];

fn rdf_local(iri: &str) -> Option<&str> {
    iri.strip_prefix(RDF)
}

/// XML NCName (the value space of rdf:ID and rdf:nodeID).
fn is_ncname(s: &str) -> bool {
    let mut cs = s.chars();
    matches!(cs.next(), Some(c) if c.is_alphabetic() || c == '_') && cs.all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '\u{B7}') || ('\u{300}'..='\u{36F}').contains(&c) || ('\u{203F}'..='\u{2040}').contains(&c))
}

/// Parse an RDF/XML document into quads (relative to `base`).
pub fn parse_rdfxml(text: &str, base: &str) -> Result<Vec<Quad>, String> {
    let root = parse_xml(text)?;
    let mut ns = HashMap::new();
    ns.insert("xml".to_string(), XML_NS.to_string());
    let scope = Scope { ns, base: base.to_string(), lang: None, dir: None, versioned: false };
    let mut st = RdfXml { out: Vec::new(), bnodes: HashMap::new(), ids: Default::default() };
    let scope = st.enter(&root, &scope);
    if st.qname(&root.name, &scope)? == format!("{RDF}RDF") {
        for child in root.elements() {
            st.node_element(child, &scope)?;
        }
    } else {
        st.node_element(&root, &scope)?;
    }
    Ok(st.out)
}

impl RdfXml {
    fn enter(&self, e: &Element, parent: &Scope) -> Scope {
        let mut s = parent.clone();
        for (k, v) in &e.attrs {
            if k == "xmlns" {
                s.ns.insert(String::new(), v.clone());
            } else if let Some(p) = k.strip_prefix("xmlns:") {
                s.ns.insert(p.to_string(), v.clone());
            } else if k == "xml:base" {
                s.base = resolve_iri(&parent.base, v);
            } else if k == "xml:lang" {
                s.lang = if v.is_empty() { None } else { Some(v.to_ascii_lowercase()) };
            }
        }
        // rdf:version and its:dir are resolved after the namespace declarations of this element
        for (k, _) in &e.attrs {
            if let Some((p, "version")) = k.split_once(':') {
                if s.ns.get(p).is_some_and(|n| n == RDF) {
                    s.versioned = true;
                }
            }
        }
        for (k, v) in &e.attrs {
            if let Some((p, "dir")) = k.split_once(':') {
                if s.versioned && s.ns.get(p).is_some_and(|n| n == ITS_NS) {
                    s.dir = matches!(v.as_str(), "ltr" | "rtl").then(|| v.clone());
                }
            }
        }
        s
    }

    fn qname(&self, name: &str, scope: &Scope) -> Result<String, String> {
        let (p, local) = name.split_once(':').unwrap_or(("", name));
        match scope.ns.get(p) {
            Some(ns) => Ok(format!("{ns}{local}")),
            None if p.is_empty() => Ok(local.to_string()),
            None => Err(format!("RDF/XML: undeclared namespace prefix '{p}'")),
        }
    }

    /// Attributes other than namespace declarations and xml:*, as expanded IRIs.
    fn rdf_attrs(&self, e: &Element, scope: &Scope) -> Result<Vec<(String, String)>, String> {
        let mut out = Vec::new();
        for (k, v) in &e.attrs {
            // names starting with "xml" (any case) are reserved by XML and ignored by RDF/XML
            if k.get(..3).is_some_and(|x| x.eq_ignore_ascii_case("xml")) {
                continue;
            }
            // ITS attributes (its:dir, its:version) and rdf:version are not properties
            if let Some((p, local)) = k.split_once(':') {
                if scope.ns.get(p).is_some_and(|n| n == ITS_NS) || (scope.ns.get(p).is_some_and(|n| n == RDF) && local == "version") {
                    continue;
                }
            }
            let iri = if k.contains(':') {
                self.qname(k, scope)?
            } else {
                // Unqualified rdf attribute names are tolerated (about, resource, ...)
                format!("{RDF}{k}")
            };
            out.push((iri, v.clone()));
        }
        Ok(out)
    }

    fn blank(&mut self, label: &str) -> Term {
        Term::Blank(self.bnodes.entry(label.to_string()).or_insert_with(fresh_blank_id).clone())
    }

    fn emit(&mut self, s: Term, p: Term, o: Term) {
        self.out.push(Quad { s, p, o, g: None });
    }

    fn literal(&self, text: String, scope: &Scope) -> Term {
        match (&scope.lang, &scope.dir) {
            (Some(l), Some(d)) => Term::dir_lang(text, l, d),
            (Some(l), None) => Term::lang(text, l),
            _ => Term::string(text),
        }
    }

    fn node_element(&mut self, e: &Element, parent: &Scope) -> Result<Term, String> {
        let scope = self.enter(e, parent);
        let attrs = self.rdf_attrs(e, &scope)?;
        let name = self.qname(&e.name, &scope)?;
        if let Some(l) = rdf_local(&name) {
            if CORE_SYNTAX_TERMS.contains(&l) || OLD_TERMS.contains(&l) || l == "li" {
                return Err(format!("RDF/XML: rdf:{l} cannot be used as a node element"));
            }
        }
        let mut subject = None;
        let mut identifying = 0;
        for (k, v) in &attrs {
            match rdf_local(k) {
                Some("about") => {
                    identifying += 1;
                    subject = Some(Term::Iri(resolve_iri(&scope.base, v)));
                }
                Some("ID") => {
                    identifying += 1;
                    subject = Some(self.id_iri(v, &scope)?);
                }
                Some("nodeID") => {
                    identifying += 1;
                    if !is_ncname(v) {
                        return Err(format!("RDF/XML: rdf:nodeID \"{v}\" is not an XML name"));
                    }
                    subject = Some(self.blank(v));
                }
                Some(l) if OLD_TERMS.contains(&l) || l == "li" || matches!(l, "parseType" | "resource" | "datatype" | "RDF" | "Description") => {
                    return Err(format!("RDF/XML: rdf:{l} is not allowed on a node element"));
                }
                _ => {}
            }
        }
        if identifying > 1 {
            return Err("RDF/XML: a node element may have only one of rdf:about, rdf:ID and rdf:nodeID".into());
        }
        let subject = subject.unwrap_or_else(Term::fresh_blank);
        if name != format!("{RDF}Description") {
            self.emit(subject.clone(), Term::iri(RDF_TYPE), Term::Iri(name));
        }
        for (k, v) in &attrs {
            match rdf_local(k) {
                Some("about" | "ID" | "nodeID") => {}
                Some("type") => self.emit(subject.clone(), Term::iri(RDF_TYPE), Term::Iri(resolve_iri(&scope.base, v))),
                _ => {
                    let lit = self.literal(v.clone(), &scope);
                    self.emit(subject.clone(), Term::Iri(k.clone()), lit);
                }
            }
        }
        let mut li = 0;
        for child in e.elements() {
            self.property_element(child, &subject, &scope, &mut li)?;
        }
        Ok(subject)
    }

    /// The IRI named by an rdf:ID, which must be an XML name used only once per base.
    fn id_iri(&mut self, id: &str, scope: &Scope) -> Result<Term, String> {
        if !is_ncname(id) {
            return Err(format!("RDF/XML: rdf:ID \"{id}\" is not an XML name"));
        }
        let iri = resolve_iri(&scope.base, &format!("#{id}"));
        if !self.ids.insert(iri.clone()) {
            return Err(format!("RDF/XML: rdf:ID \"{id}\" is used twice"));
        }
        Ok(Term::Iri(iri))
    }

    /// Emit a property statement, and its reification when the property element has rdf:ID.
    fn statement(&mut self, s: Term, p: Term, o: Term, reify: Option<&Term>, ann: Option<&Term>) {
        if let Some(r) = ann {
            self.emit(r.clone(), Term::iri(super::RDF_REIFIES), Term::triple(s.clone(), p.clone(), o.clone()));
        }
        if let Some(r) = reify {
            self.emit(r.clone(), Term::iri(RDF_TYPE), Term::iri(format!("{RDF}Statement")));
            self.emit(r.clone(), Term::iri(format!("{RDF}subject")), s.clone());
            self.emit(r.clone(), Term::iri(format!("{RDF}predicate")), p.clone());
            self.emit(r.clone(), Term::iri(format!("{RDF}object")), o.clone());
        }
        self.emit(s, p, o);
    }

    fn property_element(&mut self, e: &Element, subject: &Term, parent: &Scope, li: &mut usize) -> Result<(), String> {
        let scope = self.enter(e, parent);
        let mut pred = self.qname(&e.name, &scope)?;
        if pred == format!("{RDF}li") {
            *li += 1;
            pred = format!("{RDF}_{li}");
        }
        if let Some(l) = rdf_local(&pred) {
            if CORE_SYNTAX_TERMS.contains(&l) || OLD_TERMS.contains(&l) || l == "Description" {
                return Err(format!("RDF/XML: rdf:{l} cannot be used as a property element"));
            }
        }
        let pred = Term::Iri(pred);
        let attrs = self.rdf_attrs(e, &scope)?;
        let get = |n: &str| attrs.iter().find(|(k, _)| k == &format!("{RDF}{n}")).map(|(_, v)| v.clone());
        for (k, _) in &attrs {
            if let Some(l) = rdf_local(k) {
                if OLD_TERMS.contains(&l) || matches!(l, "li" | "about" | "RDF" | "Description") {
                    return Err(format!("RDF/XML: rdf:{l} is not allowed on a property element"));
                }
            }
        }
        let parse_type = get("parseType");
        let has = |n: &str| get(n).is_some();
        let property_attrs = attrs.iter().any(|(k, _)| !matches!(rdf_local(k), Some("ID" | "datatype" | "parseType" | "resource" | "nodeID" | "annotation" | "annotationNodeID")));
        if has("resource") && has("nodeID") {
            return Err("RDF/XML: rdf:resource and rdf:nodeID cannot be used together".into());
        }
        if parse_type.is_some() && (has("resource") || has("nodeID") || has("datatype") || property_attrs) {
            return Err("RDF/XML: rdf:parseType cannot be combined with rdf:resource, rdf:nodeID, rdf:datatype or property attributes".into());
        }
        if has("datatype") && (has("resource") || has("nodeID") || property_attrs) {
            return Err("RDF/XML: rdf:datatype is only allowed on a literal property element".into());
        }
        if let Some(n) = get("nodeID") {
            if !is_ncname(&n) {
                return Err(format!("RDF/XML: rdf:nodeID \"{n}\" is not an XML name"));
            }
        }
        let reify = match get("ID") {
            Some(id) => Some(self.id_iri(&id, &scope)?),
            None => None,
        };
        let reify = reify.as_ref();
        // RDF 1.2: rdf:annotation / rdf:annotationNodeID name a reifier of the statement
        let ann = match (get("annotation"), get("annotationNodeID")) {
            (Some(_), Some(_)) => return Err("RDF/XML: rdf:annotation and rdf:annotationNodeID cannot be used together".into()),
            (Some(a), None) => Some(Term::Iri(resolve_iri(&scope.base, &a))),
            (None, Some(n)) => {
                if !is_ncname(&n) {
                    return Err(format!("RDF/XML: rdf:annotationNodeID \"{n}\" is not an XML name"));
                }
                Some(self.blank(&n))
            }
            (None, None) => None,
        };
        let children: Vec<&Element> = e.elements().collect();

        match parse_type.as_deref() {
            Some("Literal") => {
                let mut xml = String::new();
                for c in &e.children {
                    write_xml_node(&mut xml, c);
                }
                self.statement(subject.clone(), pred, Term::typed(xml, &format!("{RDF}XMLLiteral")), reify, ann.as_ref());
                return Ok(());
            }
            Some("Resource") => {
                let node = Term::fresh_blank();
                self.statement(subject.clone(), pred, node.clone(), reify, ann.as_ref());
                let mut li2 = 0;
                for c in children {
                    self.property_element(c, &node, &scope, &mut li2)?;
                }
                return Ok(());
            }
            Some("Triple") if !scope.versioned => return Ok(()), // an RDF 1.1 reader ignores it
            Some("Triple") => {
                // the single statement of the content, as a triple term (not asserted)
                let [child] = children.as_slice() else {
                    return Err("RDF/XML: rdf:parseType=\"Triple\" needs exactly one node element".into());
                };
                let mark = self.out.len();
                self.node_element(child, &scope)?;
                let inner: Vec<Quad> = self.out.drain(mark..).collect();
                let [q] = inner.as_slice() else {
                    return Err("RDF/XML: rdf:parseType=\"Triple\" content must produce exactly one triple".into());
                };
                let tt = Term::triple(q.s.clone(), q.p.clone(), q.o.clone());
                self.statement(subject.clone(), pred, tt, reify, ann.as_ref());
                return Ok(());
            }
            Some("Collection") => {
                let mut items = Vec::new();
                for c in children {
                    items.push(self.node_element(c, &scope)?);
                }
                let mut head = Term::iri(RDF_NIL);
                for item in items.into_iter().rev() {
                    let n = Term::fresh_blank();
                    self.emit(n.clone(), Term::iri(RDF_FIRST), item);
                    self.emit(n.clone(), Term::iri(RDF_REST), head);
                    head = n;
                }
                self.statement(subject.clone(), pred, head, reify, ann.as_ref());
                return Ok(());
            }
            _ => {}
        }

        if let Some(child) = children.first() {
            if children.len() > 1 {
                return Err("RDF/XML: a property element may contain only one node element".into());
            }
            let obj = self.node_element(child, &scope)?;
            self.statement(subject.clone(), pred, obj, reify, ann.as_ref());
            return Ok(());
        }

        let extra: Vec<(String, String)> = attrs
            .iter()
            .filter(|(k, _)| !matches!(k.strip_prefix(RDF), Some("resource" | "nodeID" | "datatype" | "parseType" | "ID" | "annotation" | "annotationNodeID")))
            .cloned()
            .collect();
        let object = if let Some(r) = get("resource") {
            Some(Term::Iri(resolve_iri(&scope.base, &r)))
        } else if let Some(n) = get("nodeID") {
            Some(self.blank(&n))
        } else if !extra.is_empty() {
            Some(Term::fresh_blank())
        } else {
            None
        };
        match object {
            Some(obj) => {
                for (k, v) in extra {
                    if k == RDF_TYPE {
                        self.emit(obj.clone(), Term::iri(RDF_TYPE), Term::Iri(resolve_iri(&scope.base, &v)));
                    } else {
                        let lit = self.literal(v, &scope);
                        self.emit(obj.clone(), Term::Iri(k), lit);
                    }
                }
                self.statement(subject.clone(), pred, obj, reify, ann.as_ref());
            }
            None => {
                let text = e.text();
                let lit = match get("datatype") {
                    Some(dt) => Term::typed(text, &resolve_iri(&scope.base, &dt)),
                    None => self.literal(text, &scope),
                };
                self.statement(subject.clone(), pred, lit, reify, ann.as_ref());
            }
        }
        Ok(())
    }
}

fn xml_escape(out: &mut String, s: &str, attr: bool) {
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' if attr => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

fn write_xml_node(out: &mut String, n: &Node) {
    match n {
        Node::Text(t) => xml_escape(out, t, false),
        Node::Element(e) => {
            out.push('<');
            out.push_str(&e.name);
            for (k, v) in &e.attrs {
                out.push(' ');
                out.push_str(k);
                out.push_str("=\"");
                xml_escape(out, v, true);
                out.push('"');
            }
            // exclusive canonical XML (the value of an rdf:XMLLiteral): no empty-element tags
            out.push('>');
            for c in &e.children {
                write_xml_node(out, c);
            }
            out.push_str("</");
            out.push_str(&e.name);
            out.push('>');
        }
    }
}

/// Split an IRI into (namespace, local) where local is a valid XML NCName.
fn split_iri(iri: &str) -> Option<(&str, &str)> {
    let idx = iri.char_indices().rev().take_while(|(_, c)| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == '.').last()?.0;
    let local = &iri[idx..];
    let first = local.chars().next()?;
    if first.is_alphabetic() || first == '_' {
        Some((&iri[..idx], local))
    } else {
        // shrink until the local name starts with a letter
        let pos = local.char_indices().find(|(_, c)| c.is_alphabetic() || *c == '_')?.0;
        Some((&iri[..idx + pos], &local[pos..]))
    }
}

/// Write quads as RDF/XML (a striped document with typed nodes).
pub fn write_rdfxml(quads: &[Quad], prefixes: &[(String, String)]) -> Result<String, String> {
    let mut ns: Vec<(String, String)> = vec![("rdf".into(), RDF.into())];
    for (p, n) in prefixes {
        if !ns.iter().any(|(_, x)| x == n) && !p.is_empty() && p != "rdf" {
            ns.push((p.clone(), n.clone()));
        }
    }
    let mut body = String::new();
    let mut i = 0;
    while i < quads.len() {
        let s = &quads[i].s;
        let mut j = i;
        while j < quads.len() && &quads[j].s == s {
            j += 1;
        }
        body.push_str("  <rdf:Description ");
        match s {
            Term::Blank(b) => {
                body.push_str("rdf:nodeID=\"");
                xml_escape(&mut body, b, true);
            }
            other => {
                body.push_str("rdf:about=\"");
                xml_escape(&mut body, other.value(), true);
            }
        }
        body.push_str("\">\n");
        for q in &quads[i..j] {
            let (pns, local) = split_iri(q.p.value()).ok_or_else(|| format!("RDF/XML cannot express predicate <{}>", q.p.value()))?;
            let prefix = match ns.iter().find(|(_, n)| n == pns) {
                Some((p, _)) => p.clone(),
                None => {
                    let p = format!("ns{}", ns.len());
                    ns.push((p.clone(), pns.to_string()));
                    p
                }
            };
            let tag = format!("{prefix}:{local}");
            body.push_str("    <");
            body.push_str(&tag);
            match &q.o {
                Term::Triple(_) => return Err("RDF/XML cannot express RDF-star quoted triples; use Turtle-star or TriG-star".into()),
                Term::Iri(o) => {
                    body.push_str(" rdf:resource=\"");
                    xml_escape(&mut body, o, true);
                    body.push_str("\"/>\n");
                    continue;
                }
                Term::Blank(b) => {
                    body.push_str(" rdf:nodeID=\"");
                    xml_escape(&mut body, b, true);
                    body.push_str("\"/>\n");
                    continue;
                }
                Term::Literal(l) => {
                    if let Some(lang) = &l.lang {
                        body.push_str(" xml:lang=\"");
                        xml_escape(&mut body, lang, true);
                        body.push('"');
                    } else if l.datatype != XSD_STRING {
                        body.push_str(" rdf:datatype=\"");
                        xml_escape(&mut body, &l.datatype, true);
                        body.push('"');
                    }
                    body.push('>');
                    xml_escape(&mut body, &l.value, false);
                    body.push_str("</");
                    body.push_str(&tag);
                    body.push_str(">\n");
                }
            }
        }
        body.push_str("  </rdf:Description>\n");
        i = j;
    }
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rdf:RDF");
    for (p, n) in &ns {
        out.push_str(&format!("\n    xmlns:{p}=\""));
        xml_escape(&mut out, n, true);
        out.push('"');
    }
    out.push_str(">\n");
    out.push_str(&body);
    out.push_str("</rdf:RDF>\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn entity_expansion_is_capped() {
        let mut doc = String::from("<?xml version=\"1.0\"?><!DOCTYPE r [<!ENTITY a0 \"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\">");
        for i in 1..12 {
            let refs: String = (0..10).map(|_| format!("&a{};", i - 1)).collect();
            doc.push_str(&format!("<!ENTITY a{i} \"{refs}\">"));
        }
        doc.push_str("]><r>&a11;</r>");
        assert!(parse_xml(&doc).unwrap_err().contains("too large"));
        assert!(parse_xml("<!DOCTYPE r [<!ENTITY n \"name\">]><r>&n;&n;</r>").is_ok());
    }

    use super::*;

    #[test]
    fn rdfxml_basics() {
        let doc = r#"<?xml version="1.0"?>
<!DOCTYPE rdf:RDF [ <!ENTITY ex "http://ex.org/"> ]>
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:ex="http://ex.org/" xml:base="http://ex.org/">
  <ex:Person rdf:about="&ex;alice" ex:name="Alice">
    <ex:knows rdf:resource="bob"/>
    <ex:age rdf:datatype="http://www.w3.org/2001/XMLSchema#integer">30</ex:age>
    <ex:note xml:lang="en">hi &amp; bye</ex:note>
    <ex:addr rdf:parseType="Resource"><ex:city>X</ex:city></ex:addr>
  </ex:Person>
</rdf:RDF>"#;
        let q = parse_rdfxml(doc, "").unwrap();
        assert_eq!(q.len(), 7);
        assert!(q.iter().any(|q| q.o == Term::iri("http://ex.org/bob")));
        assert!(q.iter().any(|q| q.o == Term::lang("hi & bye", "en")));
        let out = write_rdfxml(&q, &[]).unwrap();
        assert_eq!(parse_rdfxml(&out, "").unwrap().len(), 7);
    }
}
