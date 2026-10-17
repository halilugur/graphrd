//! SHACL Core and SHACL-SPARQL validation. Shapes are read from the shapes
//! shapes graph; data is validated either fully or only for the focus nodes a
//! transaction touched.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use crate::json::Json;
use crate::rdf::{Term, RDF, RDFS, RDF_FIRST, RDF_NIL, RDF_REST, RDF_TYPE, XSD};
use crate::sparql::func;
use crate::sparql::regex::Regex;
use crate::store::Store;

/// The compatibility graph name clients put shapes in; GraphRD answers it too.
/// Where shapes live: the statements of this graph validate every write. The
/// compatibility name some clients use is answered as the same graph.
pub const SHAPES_GRAPH: &str = "urn:graphrd:shapes";
const SHAPES_GRAPH_COMPAT: &str = "http://rdf4j.org/schema/rdf4j#SHACLShapeGraph";
const SH: &str = "http://www.w3.org/ns/shacl#";

/// Nested shape references (sh:node, sh:and, ...) deeper than this count as conforming:
/// SHACL leaves recursive shapes undefined, this keeps them finite.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone)]
enum Target {
    Class(Term),
    Node(Term),
    SubjectsOf(Term),
    ObjectsOf(Term),
    /// sh:target [ a sh:SPARQLTarget ; sh:select "SELECT ?this ..." ]
    Sparql(SparqlQuery),
}

/// A SHACL property path.
#[derive(Debug, Clone, PartialEq)]
pub enum PathExpr {
/// A predicate, directly.
    Pred(Term),
/// `^path`: walked backwards.
    Inverse(Box<PathExpr>),
/// `p1/p2`: one after the other.
    Seq(Vec<PathExpr>),
/// `p1|p2`: either.
    Alt(Vec<PathExpr>),
/// `p*`.
    ZeroOrMore(Box<PathExpr>),
/// `p+`.
    OneOrMore(Box<PathExpr>),
/// `p?`.
    ZeroOrOne(Box<PathExpr>),
}

impl PathExpr {
    /// SPARQL property path syntax (for $PATH and the JSON report).
    pub fn sparql(&self) -> String {
        let join = |items: &[PathExpr], sep: &str| items.iter().map(|p| p.sparql()).collect::<Vec<_>>().join(sep);
        match self {
            PathExpr::Pred(p) => p.to_string(),
            PathExpr::Inverse(p) => format!("^({})", p.sparql()),
            PathExpr::Seq(items) => format!("({})", join(items, "/")),
            PathExpr::Alt(items) => format!("({})", join(items, "|")),
            PathExpr::ZeroOrMore(p) => format!("({})*", p.sparql()),
            PathExpr::OneOrMore(p) => format!("({})+", p.sparql()),
            PathExpr::ZeroOrOne(p) => format!("({})?", p.sparql()),
        }
    }

    /// The path as a SHACL path node in Turtle (for sh:resultPath).
    fn turtle(&self) -> String {
        let list = |items: &[PathExpr]| format!("( {} )", items.iter().map(|p| p.turtle()).collect::<Vec<_>>().join(" "));
        match self {
            PathExpr::Pred(p) => p.to_string(),
            PathExpr::Inverse(p) => format!("[ sh:inversePath {} ]", p.turtle()),
            PathExpr::Seq(items) => list(items),
            PathExpr::Alt(items) => format!("[ sh:alternativePath {} ]", list(items)),
            PathExpr::ZeroOrMore(p) => format!("[ sh:zeroOrMorePath {} ]", p.turtle()),
            PathExpr::OneOrMore(p) => format!("[ sh:oneOrMorePath {} ]", p.turtle()),
            PathExpr::ZeroOrOne(p) => format!("[ sh:zeroOrOnePath {} ]", p.turtle()),
        }
    }
}

/// A SPARQL query run per focus or value node: an sh:sparql constraint or a
/// validator of a SPARQL-based constraint component.
#[derive(Debug, Clone)]
struct SparqlQuery {
    /// The constraint node (sh:sourceConstraint), for sh:sparql.
    node: Option<Term>,
    /// PREFIX declarations followed by the query.
    text: String,
    ask: bool,
    messages: Vec<Term>,
}

#[derive(Debug, Clone)]
enum Constraint {
    MinCount(usize),
    MaxCount(usize),
    Datatype(String),
    Class(Term),
    NodeKind(String),
    Pattern(Regex, String),
    MinLength(usize),
    MaxLength(usize),
    LanguageIn(Vec<String>),
    UniqueLang,
    In(Vec<Term>),
    HasValue(Term),
    Range(&'static str, Term), // MinInclusive, MaxInclusive, MinExclusive, MaxExclusive
    Equals(Term),
    Disjoint(Term),
    LessThan(Term),
    LessThanOrEquals(Term),
    Not(usize),
    And(Vec<usize>),
    Or(Vec<usize>),
    Xone(Vec<usize>),
    Node(usize),
    Property(usize),
    Qualified { shape: usize, min: Option<usize>, max: Option<usize>, siblings: Vec<usize> },
    Closed(HashSet<Term>),
    Sparql(SparqlQuery),
    /// A SPARQL-based constraint component: its IRI, parameter bindings and validator.
    Component(Term, Vec<(String, Term)>, SparqlQuery),
}

#[derive(Debug, Clone)]
struct Shape {
    id: Term,
    /// Some for property shapes.
    path: Option<PathExpr>,
    targets: Vec<Target>,
    constraints: Vec<Constraint>,
    messages: Vec<Term>,
    severity: Term,
    deactivated: bool,
}

/// One constraint violation.
pub struct Violation {
/// The value the shape was checked on.
    pub focus: Term,
/// The path from the focus node to the value, when there is one.
    pub path: Option<PathExpr>,
/// The offending value, when the constraint names one.
    pub value: Option<Term>,
/// The shape that was violated.
    pub shape: Term,
/// The constraint component's IRI (`sh:MinCountConstraintComponent` ...).
    pub component: String,
/// The message, or one made from the parts.
    pub message: String,
/// `sh:Info`, `sh:Warning` or `sh:Violation`.
    pub severity: Term,
/// The constraint itself, when a node carries it.
    pub source_constraint: Option<Term>,
}

/// Every shape of a repository, ready to validate against.
pub struct Shapes {
    shapes: Vec<Shape>,
}

fn sh(local: &str) -> Term {
    Term::iri(format!("{SH}{local}"))
}

/// The shapes graph, as subject → (predicate, object) pairs.
struct Graph {
    by_s: HashMap<Term, Vec<(Term, Term)>>,
}

impl Graph {
    fn get(&self, s: &Term, p: &str) -> Vec<Term> {
        self.by_s.get(s).map(|v| v.iter().filter(|(pp, _)| pp.value() == p).map(|(_, o)| o.clone()).collect()).unwrap_or_default()
    }
    fn sh(&self, s: &Term, local: &str) -> Vec<Term> {
        self.get(s, &format!("{SH}{local}"))
    }
    fn one(&self, s: &Term, local: &str) -> Option<Term> {
        self.sh(s, local).into_iter().next()
    }
    fn has_type(&self, s: &Term, class: &str) -> bool {
        self.get(s, RDF_TYPE).iter().any(|t| t.value() == class)
    }
    /// rdf:type, then rdfs:subClassOf*, within the shapes graph.
    fn instance_of(&self, s: &Term, class: &str) -> bool {
        let mut seen = HashSet::new();
        let mut queue = self.get(s, RDF_TYPE);
        while let Some(c) = queue.pop() {
            if c.value() == class {
                return true;
            }
            if seen.insert(c.clone()) {
                queue.extend(self.get(&c, &format!("{RDFS}subClassOf")));
            }
        }
        false
    }

    fn list(&self, head: &Term) -> Vec<Term> {
        let mut out = Vec::new();
        let mut cur = head.clone();
        for _ in 0..10_000 {
            if cur.value() == RDF_NIL {
                break;
            }
            let Some(first) = self.get(&cur, RDF_FIRST).into_iter().next() else { break };
            out.push(first);
            match self.get(&cur, RDF_REST).into_iter().next() {
                Some(r) => cur = r,
                None => break,
            }
        }
        out
    }
    fn is_list(&self, t: &Term) -> bool {
        t.value() == RDF_NIL || !self.get(t, RDF_FIRST).is_empty()
    }
    fn count(&self, s: &Term, local: &str) -> Option<usize> {
        self.one(s, local).and_then(|t| t.value().parse::<usize>().ok())
    }
    fn truthy(&self, s: &Term, local: &str) -> bool {
        // only "true": "1"^^xsd:boolean is a different value and leaves the constraint off
        self.one(s, local).is_some_and(|t| t.value() == "true")
    }

    fn path(&self, t: &Term) -> Option<PathExpr> {
        self.path_at(t, 0)
    }

    fn path_at(&self, t: &Term, depth: usize) -> Option<PathExpr> {
        if depth > 32 {
            return None;
        }
        if t.is_iri() {
            return Some(PathExpr::Pred(t.clone()));
        }
        if !t.is_blank() {
            return None;
        }
        let sub = |p: &Term| self.path_at(p, depth + 1).map(Box::new);
        if self.is_list(t) {
            let items = self.list(t).iter().map(|i| self.path_at(i, depth + 1)).collect::<Option<Vec<_>>>()?;
            return (items.len() >= 2).then_some(PathExpr::Seq(items));
        }
        if let Some(p) = self.one(t, "inversePath") {
            return Some(PathExpr::Inverse(sub(&p)?));
        }
        if let Some(l) = self.one(t, "alternativePath") {
            let items = self.list(&l).iter().map(|i| self.path_at(i, depth + 1)).collect::<Option<Vec<_>>>()?;
            return (items.len() >= 2).then_some(PathExpr::Alt(items));
        }
        if let Some(p) = self.one(t, "zeroOrMorePath") {
            return Some(PathExpr::ZeroOrMore(sub(&p)?));
        }
        if let Some(p) = self.one(t, "oneOrMorePath") {
            return Some(PathExpr::OneOrMore(sub(&p)?));
        }
        if let Some(p) = self.one(t, "zeroOrOnePath") {
            return Some(PathExpr::ZeroOrOne(sub(&p)?));
        }
        None
    }

    /// PREFIX lines for the sh:prefixes of a SPARQL constraint or validator.
    fn prefixes(&self, node: &Term) -> String {
        let mut out = String::new();
        let mut seen = HashSet::new();
        let mut queue = self.sh(node, "prefixes");
        while let Some(ont) = queue.pop() {
            if !seen.insert(ont.clone()) {
                continue;
            }
            for d in self.sh(&ont, "declare") {
                if let (Some(p), Some(ns)) = (self.one(&d, "prefix"), self.one(&d, "namespace")) {
                    out.push_str(&format!("PREFIX {}: <{}>\n", p.value(), ns.value()));
                }
            }
            queue.extend(self.get(&ont, "http://www.w3.org/2002/07/owl#imports"));
        }
        out
    }

    fn query(&self, node: &Term, kind: &str) -> Option<SparqlQuery> {
        let text = self.one(node, kind)?;
        Some(SparqlQuery { node: None, text: format!("{}{}", self.prefixes(node), text.value()), ask: kind == "ask", messages: self.sh(node, "message") })
    }
}

/// A SPARQL-based constraint component (sh:ConstraintComponent in the shapes graph).
struct Component {
    id: Term,
    /// Parameter path and whether it is optional.
    params: Vec<(Term, bool)>,
    validator: Option<SparqlQuery>,
    node_validator: Option<SparqlQuery>,
    property_validator: Option<SparqlQuery>,
    messages: Vec<Term>,
}

/// Compiles shapes into `Shapes`, giving each shape node one index.
struct Builder<'g> {
    g: &'g Graph,
    index: HashMap<Term, usize>,
    shapes: Vec<Shape>,
    components: Vec<Component>,
}

impl Builder<'_> {
    fn shape(&mut self, id: &Term) -> usize {
        if let Some(&i) = self.index.get(id) {
            return i;
        }
        let g = self.g;
        let i = self.shapes.len();
        self.index.insert(id.clone(), i);
        let path = g.one(id, "path").and_then(|p| g.path(&p));
        self.shapes.push(Shape {
            id: id.clone(),
            path: path.clone(),
            targets: Vec::new(),
            constraints: Vec::new(),
            messages: g.sh(id, "message"),
            severity: g.one(id, "severity").unwrap_or_else(|| sh("Violation")),
            deactivated: g.truthy(id, "deactivated"),
        });
        let mut targets: Vec<Target> = g.sh(id, "targetClass").into_iter().map(Target::Class).collect();
        targets.extend(g.sh(id, "targetNode").into_iter().map(Target::Node));
        targets.extend(g.sh(id, "targetSubjectsOf").into_iter().map(Target::SubjectsOf));
        targets.extend(g.sh(id, "targetObjectsOf").into_iter().map(Target::ObjectsOf));
        targets.extend(g.sh(id, "target").iter().filter_map(|t| g.query(t, "select")).map(Target::Sparql));
        // implicit class target: a shape that is also an rdfs:Class
        if g.has_type(id, &format!("{RDFS}Class")) && (g.has_type(id, &format!("{SH}NodeShape")) || g.has_type(id, &format!("{SH}PropertyShape"))) {
            targets.push(Target::Class(id.clone()));
        }
        let constraints = self.constraints(id, path.is_some());
        self.shapes[i].targets = targets;
        self.shapes[i].constraints = constraints;
        i
    }

    fn constraints(&mut self, s: &Term, property: bool) -> Vec<Constraint> {
        let g = self.g;
        let mut c = Vec::new();
        if let Some(n) = g.count(s, "minCount") {
            c.push(Constraint::MinCount(n));
        }
        if let Some(n) = g.count(s, "maxCount") {
            c.push(Constraint::MaxCount(n));
        }
        for d in g.sh(s, "datatype") {
            c.push(Constraint::Datatype(d.value().to_string()));
        }
        for k in g.sh(s, "class") {
            c.push(Constraint::Class(k));
        }
        for k in g.sh(s, "nodeKind") {
            c.push(Constraint::NodeKind(k.value().trim_start_matches(SH).to_string()));
        }
        for p in g.sh(s, "pattern") {
            let flags = g.one(s, "flags").map(|f| f.value().to_string()).unwrap_or_default();
            if let Ok(r) = Regex::new(p.value(), &flags) {
                c.push(Constraint::Pattern(r, p.value().to_string()));
            }
        }
        if let Some(n) = g.count(s, "minLength") {
            c.push(Constraint::MinLength(n));
        }
        if let Some(n) = g.count(s, "maxLength") {
            c.push(Constraint::MaxLength(n));
        }
        for l in g.sh(s, "languageIn") {
            c.push(Constraint::LanguageIn(g.list(&l).iter().map(|t| t.value().to_ascii_lowercase()).collect()));
        }
        if property && g.truthy(s, "uniqueLang") {
            c.push(Constraint::UniqueLang);
        }
        for l in g.sh(s, "in") {
            c.push(Constraint::In(g.list(&l)));
        }
        for v in g.sh(s, "hasValue") {
            c.push(Constraint::HasValue(v));
        }
        for (k, name) in [("minInclusive", "MinInclusive"), ("maxInclusive", "MaxInclusive"), ("minExclusive", "MinExclusive"), ("maxExclusive", "MaxExclusive")] {
            for v in g.sh(s, k) {
                c.push(Constraint::Range(name, v));
            }
        }
        for p in g.sh(s, "equals") {
            c.push(Constraint::Equals(p));
        }
        for p in g.sh(s, "disjoint") {
            c.push(Constraint::Disjoint(p));
        }
        if property {
            for p in g.sh(s, "lessThan") {
                c.push(Constraint::LessThan(p));
            }
            for p in g.sh(s, "lessThanOrEquals") {
                c.push(Constraint::LessThanOrEquals(p));
            }
        }
        for n in g.sh(s, "not") {
            let i = self.shape(&n);
            c.push(Constraint::Not(i));
        }
        for (k, make) in [("and", Constraint::And as fn(Vec<usize>) -> Constraint), ("or", Constraint::Or), ("xone", Constraint::Xone)] {
            for l in g.sh(s, k) {
                let members = g.list(&l).iter().map(|m| self.shape(m)).collect();
                c.push(make(members));
            }
        }
        for n in g.sh(s, "node") {
            let i = self.shape(&n);
            c.push(Constraint::Node(i));
        }
        for p in g.sh(s, "property") {
            let i = self.shape(&p);
            c.push(Constraint::Property(i));
        }
        if property {
            for q in g.sh(s, "qualifiedValueShape") {
                let shape = self.shape(&q);
                let mut siblings = Vec::new();
                if g.truthy(s, "qualifiedValueShapesDisjoint") {
                    // the qualified value shapes of the other property shapes of the parent shapes
                    let parents: Vec<Term> = g.by_s.iter().filter(|(_, po)| po.iter().any(|(p, o)| p.value() == format!("{SH}property") && o == s)).map(|(k, _)| k.clone()).collect();
                    for parent in parents {
                        for sib in g.sh(&parent, "property") {
                            if sib != *s {
                                for sq in g.sh(&sib, "qualifiedValueShape") {
                                    let i = self.shape(&sq);
                                    siblings.push(i);
                                }
                            }
                        }
                    }
                }
                c.push(Constraint::Qualified { shape, min: g.count(s, "qualifiedMinCount"), max: g.count(s, "qualifiedMaxCount"), siblings });
            }
        }
        if g.truthy(s, "closed") {
            let mut allowed: HashSet<Term> = g.sh(s, "property").iter().filter_map(|p| g.one(p, "path")).filter(|p| p.is_iri()).collect();
            for l in g.sh(s, "ignoredProperties") {
                allowed.extend(g.list(&l));
            }
            c.push(Constraint::Closed(allowed));
        }
        for q in g.sh(s, "sparql") {
            if g.truthy(&q, "deactivated") {
                continue;
            }
            if let Some(mut query) = g.query(&q, "select") {
                query.node = Some(q.clone());
                c.push(Constraint::Sparql(query));
            }
        }
        for comp in &self.components {
            let mut bindings = Vec::new();
            let mut active = true;
            for (param, optional) in &comp.params {
                match g.get(s, param.value()).into_iter().next() {
                    Some(v) => bindings.push((local_name(param.value()).to_string(), v)),
                    None => active &= *optional,
                }
            }
            if !active || bindings.is_empty() {
                continue;
            }
            let validator = if property { comp.property_validator.as_ref() } else { comp.node_validator.as_ref() }.or(comp.validator.as_ref());
            if let Some(v) = validator {
                let mut v = v.clone();
                if v.messages.is_empty() {
                    v.messages = comp.messages.clone();
                }
                c.push(Constraint::Component(comp.id.clone(), bindings, v));
            }
        }
        c
    }
}

fn local_name(iri: &str) -> &str {
    iri.rsplit(['#', '/', ':']).next().unwrap_or(iri)
}

impl Shapes {
    /// Read shapes from the shapes graph (None when it is empty).
    pub fn load(store: &Store) -> Option<Shapes> {
        let g = store
            .dict
            .lookup(&Term::iri(SHAPES_GRAPH))
            .or_else(|| store.dict.lookup(&Term::iri(SHAPES_GRAPH_COMPAT)))?;
        let mut by_s: HashMap<Term, Vec<(Term, Term)>> = HashMap::new();
        for q in store.explicit_matching(None, None, None, Some(g)) {
            let q = store.decode(q);
            by_s.entry(q.s).or_default().push((q.p, q.o));
        }
        if by_s.is_empty() {
            return None;
        }
        let graph = Graph { by_s };
        let g = &graph;
        let components = g
            .by_s
            .keys()
            .filter(|k| g.instance_of(k, &format!("{SH}ConstraintComponent")))
            .map(|k| Component {
                id: k.clone(),
                params: g.sh(k, "parameter").iter().filter_map(|p| Some((g.one(p, "path")?, g.truthy(p, "optional")))).collect(),
                validator: g.one(k, "validator").and_then(|v| g.query(&v, "ask").or_else(|| g.query(&v, "select"))),
                node_validator: g.one(k, "nodeValidator").and_then(|v| g.query(&v, "select")),
                property_validator: g.one(k, "propertyValidator").and_then(|v| g.query(&v, "select")),
                messages: g.sh(k, "message"),
            })
            .collect();
        let mut b = Builder { g, index: HashMap::new(), shapes: Vec::new(), components };
        // every shape with a target, plus every node or property shape, compiled up front
        let mut roots: Vec<Term> = g
            .by_s
            .iter()
            .filter(|(s, props)| {
                props.iter().any(|(p, _)| p.value().starts_with(&format!("{SH}target")))
                    || g.has_type(s, &format!("{SH}NodeShape"))
                    || g.has_type(s, &format!("{SH}PropertyShape"))
            })
            .map(|(s, _)| s.clone())
            .collect();
        roots.sort();
        for r in roots {
            b.shape(&r);
        }
        Some(Shapes { shapes: b.shapes })
    }

    /// Validate every focus node, or only those in `only`.
    pub fn validate(&self, store: &Store, only: Option<&HashSet<Term>>) -> Vec<Violation> {
        self.check(store, only).unwrap_or_else(|e| {
            vec![Violation {
                focus: Term::iri(SHAPES_GRAPH),
                path: None,
                value: None,
                shape: Term::iri(SHAPES_GRAPH),
                component: format!("{SH}SPARQLConstraintComponent"),
                message: format!("validation failure: {e}"),
                severity: sh("Violation"),
                source_constraint: None,
            }]
        })
    }

    /// Validate; an error is a validation failure (an unusable SPARQL query, `?failure`).
    pub fn check(&self, store: &Store, only: Option<&HashSet<Term>>) -> Result<Vec<Violation>, String> {
        let v = Validator { store, shapes: &self.shapes, depth: Cell::new(0), sg: store_shapes_graph(store) };
        let mut out = Vec::new();
        for (i, shape) in self.shapes.iter().enumerate() {
            if shape.targets.is_empty() || shape.deactivated {
                continue;
            }
            for focus in focus_nodes(&v, shape, only)? {
                v.validate(i, &focus, &mut out)?;
            }
        }
        Ok(out)
    }
}

struct Validator<'a> {
    store: &'a Store,
    shapes: &'a [Shape],
    depth: Cell<usize>,
    sg: u32,
}

impl Validator<'_> {
    fn objects(&self, s: &Term, p: &Term) -> Vec<Term> {
        let (Some(s), Some(p)) = (self.store.dict.lookup(s), self.store.dict.lookup(p)) else { return Vec::new() };
        let mut seen = HashSet::new();
        self.store.quads_matching(Some(s), Some(p), None, None).filter(|q| q[3] != self.sg).map(|q| q[2]).filter(|o| seen.insert(*o)).map(|o| self.store.dict.term(o)).collect()
    }

    fn subjects(&self, p: &Term, o: &Term) -> Vec<Term> {
        let (Some(p), Some(o)) = (self.store.dict.lookup(p), self.store.dict.lookup(o)) else { return Vec::new() };
        let mut seen = HashSet::new();
        self.store.quads_matching(None, Some(p), Some(o), None).filter(|q| q[3] != self.sg).map(|q| q[0]).filter(|s| seen.insert(*s)).map(|s| self.store.dict.term(s)).collect()
    }

    /// Every (predicate, object) of a node.
    fn outgoing(&self, s: &Term) -> Vec<(Term, Term)> {
        let Some(s) = self.store.dict.lookup(s) else { return Vec::new() };
        let mut seen = HashSet::new();
        self.store.quads_matching(Some(s), None, None, None).filter(|q| q[3] != self.sg).map(|q| (q[1], q[2])).filter(|po| seen.insert(*po)).map(|(p, o)| (self.store.dict.term(p), self.store.dict.term(o))).collect()
    }

    /// Is `node` an instance of `class` (rdf:type, then rdfs:subClassOf*)?
    fn instance_of(&self, node: &Term, class: &Term) -> bool {
        let sub = Term::iri(format!("{RDFS}subClassOf"));
        let mut seen = HashSet::new();
        let mut queue: Vec<Term> = self.objects(node, &Term::iri(RDF_TYPE));
        while let Some(c) = queue.pop() {
            if c == *class {
                return true;
            }
            if seen.insert(c.clone()) {
                queue.extend(self.objects(&c, &sub));
            }
        }
        false
    }

    /// The values a path reaches from `node` (`inverse`: the nodes that reach `node`).
    fn path(&self, p: &PathExpr, node: &Term, inverse: bool) -> Vec<Term> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut push = |t: Term, out: &mut Vec<Term>| {
            if seen.insert(t.clone()) {
                out.push(t);
            }
        };
        match p {
            PathExpr::Pred(pr) => {
                for t in if inverse { self.subjects(pr, node) } else { self.objects(node, pr) } {
                    push(t, &mut out);
                }
            }
            PathExpr::Inverse(inner) => {
                for t in self.path(inner, node, !inverse) {
                    push(t, &mut out);
                }
            }
            PathExpr::Seq(items) => {
                let mut cur = vec![node.clone()];
                let order: Box<dyn Iterator<Item = &PathExpr>> = if inverse { Box::new(items.iter().rev()) } else { Box::new(items.iter()) };
                for step in order {
                    let mut next = Vec::new();
                    let mut s2 = HashSet::new();
                    for n in &cur {
                        for t in self.path(step, n, inverse) {
                            if s2.insert(t.clone()) {
                                next.push(t);
                            }
                        }
                    }
                    cur = next;
                }
                for t in cur {
                    push(t, &mut out);
                }
            }
            PathExpr::Alt(items) => {
                for step in items {
                    for t in self.path(step, node, inverse) {
                        push(t, &mut out);
                    }
                }
            }
            PathExpr::ZeroOrOne(inner) => {
                push(node.clone(), &mut out);
                for t in self.path(inner, node, inverse) {
                    push(t, &mut out);
                }
            }
            PathExpr::ZeroOrMore(inner) | PathExpr::OneOrMore(inner) => {
                if matches!(p, PathExpr::ZeroOrMore(_)) {
                    push(node.clone(), &mut out);
                }
                let mut queue = self.path(inner, node, inverse);
                let mut visited = HashSet::new();
                while let Some(n) = queue.pop() {
                    if !visited.insert(n.clone()) {
                        continue;
                    }
                    queue.extend(self.path(inner, &n, inverse));
                    push(n, &mut out);
                }
            }
        }
        out
    }

    fn conforms(&self, shape: usize, node: &Term) -> Result<bool, String> {
        if self.depth.get() >= MAX_DEPTH {
            return Ok(true);
        }
        self.depth.set(self.depth.get() + 1);
        let mut out = Vec::new();
        let r = self.validate(shape, node, &mut out);
        self.depth.set(self.depth.get() - 1);
        r.map(|_| out.is_empty())
    }

    /// Validate `focus` against shape `si`.
    fn validate(&self, si: usize, focus: &Term, out: &mut Vec<Violation>) -> Result<(), String> {
        let shape = &self.shapes[si];
        if shape.deactivated {
            return Ok(());
        }
        let values = match &shape.path {
            Some(p) => self.path(p, focus, false),
            None => vec![focus.clone()],
        };
        for c in &shape.constraints {
            self.check(c, shape, focus, &values, out)?;
        }
        Ok(())
    }

    fn check(&self, c: &Constraint, shape: &Shape, focus: &Term, values: &[Term], out: &mut Vec<Violation>) -> Result<(), String> {
        let path = shape.path.as_ref();
        let path_name = path.map_or("the node".to_string(), |p| p.sparql());
        let message = |default: String| shape.messages.first().map(|m| m.value().to_string()).unwrap_or(default);
        let mut fail = |component: &str, value: Option<&Term>, default: String| {
            out.push(Violation {
                focus: focus.clone(),
                path: path.cloned(),
                value: value.cloned(),
                shape: shape.id.clone(),
                component: format!("{SH}{component}ConstraintComponent"),
                message: message(default),
                severity: shape.severity.clone(),
                source_constraint: None,
            });
        };
        match c {
            Constraint::MinCount(n) => {
                if values.len() < *n {
                    fail("MinCount", None, format!("{path_name} needs at least {n} value(s), found {}", values.len()));
                }
            }
            Constraint::MaxCount(n) => {
                if values.len() > *n {
                    fail("MaxCount", None, format!("{path_name} allows at most {n} value(s), found {}", values.len()));
                }
            }
            Constraint::HasValue(v) => {
                if !values.contains(v) {
                    fail("HasValue", None, format!("{path_name} must include {v}"));
                }
            }
            Constraint::UniqueLang => {
                let mut counts: Vec<(String, usize)> = Vec::new();
                for v in values {
                    if let Some(lang) = v.as_literal().and_then(|l| l.lang.as_ref()) {
                        let lang = lang.to_ascii_lowercase();
                        match counts.iter_mut().find(|(l, _)| *l == lang) {
                            Some(e) => e.1 += 1,
                            None => counts.push((lang, 1)),
                        }
                    }
                }
                for (lang, n) in counts {
                    if n > 1 {
                        fail("UniqueLang", None, format!("{path_name} has {n} values with language \"{lang}\""));
                    }
                }
            }
            Constraint::Equals(p) | Constraint::Disjoint(p) => {
                let other = self.objects(focus, p);
                if let Constraint::Equals(_) = c {
                    for v in values.iter().filter(|v| !other.contains(v)).chain(other.iter().filter(|o| !values.contains(o))) {
                        fail("Equals", Some(v), format!("{v} is not a value of both {path_name} and {p}"));
                    }
                } else {
                    for v in values.iter().filter(|v| other.contains(v)) {
                        fail("Disjoint", Some(v), format!("{v} is a value of both {path_name} and {p}"));
                    }
                }
            }
            Constraint::LessThan(p) | Constraint::LessThanOrEquals(p) => {
                let strict = matches!(c, Constraint::LessThan(_));
                let other = self.objects(focus, p);
                // one result per pair that is not in order (or not comparable)
                for v in values {
                    for o in &other {
                        if !func::compare(v, o).is_some_and(|ord| if strict { ord.is_lt() } else { ord.is_le() }) {
                            let name = if strict { "LessThan" } else { "LessThanOrEquals" };
                            fail(name, Some(v), format!("{v} is not {} {o} ({p})", if strict { "less than" } else { "at most" }));
                        }
                    }
                }
            }
            Constraint::Qualified { shape: q, min, max, siblings } => {
                let mut n = 0;
                for v in values {
                    if self.conforms(*q, v)? {
                        let mut in_sibling = false;
                        for s in siblings {
                            if self.conforms(*s, v)? {
                                in_sibling = true;
                                break;
                            }
                        }
                        n += usize::from(!in_sibling);
                    }
                }
                if min.is_some_and(|m| n < m) {
                    fail("QualifiedMinCount", None, format!("{path_name} needs at least {} value(s) conforming to {}, found {n}", min.unwrap_or(0), self.shapes[*q].id));
                }
                if max.is_some_and(|m| n > m) {
                    fail("QualifiedMaxCount", None, format!("{path_name} allows at most {} value(s) conforming to {}, found {n}", max.unwrap_or(0), self.shapes[*q].id));
                }
            }
            Constraint::Closed(allowed) => {
                for v in values {
                    for (p, o) in self.outgoing(v) {
                        if !allowed.contains(&p) {
                            out.push(Violation {
                                focus: focus.clone(),
                                path: Some(PathExpr::Pred(p.clone())),
                                value: Some(o),
                                shape: shape.id.clone(),
                                component: format!("{SH}ClosedConstraintComponent"),
                                message: message(format!("{p} is not allowed by the closed shape")),
                                severity: shape.severity.clone(),
                                source_constraint: None,
                            });
                        }
                    }
                }
            }
            Constraint::Property(p) => {
                for v in values {
                    self.validate(*p, v, out)?;
                }
            }
            Constraint::Sparql(q) => self.sparql(q, shape, focus, &[], "SPARQL", out)?,
            Constraint::Component(id, params, q) => {
                if q.ask {
                    for v in values {
                        let mut bind: Vec<(&str, Term)> = vec![("this", focus.clone()), ("value", v.clone())];
                        bind.extend(params.iter().map(|(n, t)| (n.as_str(), t.clone())));
                        bind.extend(self.common(shape));
                        let text = self.with_path(&q.text, shape);
                        if !self.ask(&text, &bind)? {
                            let msg = substitute(q.messages.first().map(|m| m.value()).unwrap_or("constraint not met"), &bind);
                            out.push(Violation {
                                focus: focus.clone(),
                                path: path.cloned(),
                                value: Some(v.clone()),
                                shape: shape.id.clone(),
                                component: id.value().to_string(),
                                message: shape.messages.first().map(|m| m.value().to_string()).unwrap_or(msg),
                                severity: shape.severity.clone(),
                                source_constraint: None,
                            });
                        }
                    }
                } else {
                    let bind: Vec<(&str, Term)> = params.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
                    let before = out.len();
                    self.sparql(q, shape, focus, &bind, "", out)?;
                    for v in &mut out[before..] {
                        v.component = id.value().to_string();
                    }
                }
            }
            _ => {
                for v in values {
                    let ok = match c {
                        Constraint::Datatype(dt) => v.as_literal().is_some_and(|l| l.datatype == *dt && well_formed(&l.value, dt)),
                        Constraint::Class(k) => !v.is_literal() && self.instance_of(v, k),
                        Constraint::NodeKind(k) => match k.as_str() {
                            "IRI" => v.is_iri(),
                            "BlankNode" => v.is_blank(),
                            "Literal" => v.is_literal(),
                            "BlankNodeOrIRI" => v.is_iri() || v.is_blank(),
                            "BlankNodeOrLiteral" => v.is_blank() || v.is_literal(),
                            "IRIOrLiteral" => v.is_iri() || v.is_literal(),
                            _ => true,
                        },
                        Constraint::Pattern(r, _) => !v.is_blank() && r.is_match(v.value()),
                        Constraint::MinLength(n) => !v.is_blank() && v.value().chars().count() >= *n,
                        Constraint::MaxLength(n) => !v.is_blank() && v.value().chars().count() <= *n,
                        Constraint::LanguageIn(langs) => v.as_literal().and_then(|l| l.lang.as_ref()).is_some_and(|tag| {
                            let tag = tag.to_ascii_lowercase();
                            langs.iter().any(|r| r == "*" || tag == *r || tag.starts_with(&format!("{r}-")))
                        }),
                        Constraint::In(list) => list.contains(v),
                        Constraint::Range(kind, bound) => match func::compare(v, bound) {
                            Some(o) => match *kind {
                                "MinInclusive" => o.is_ge(),
                                "MaxInclusive" => o.is_le(),
                                "MinExclusive" => o.is_gt(),
                                _ => o.is_lt(),
                            },
                            None => false,
                        },
                        Constraint::Not(s) => !self.conforms(*s, v)?,
                        Constraint::Node(s) => self.conforms(*s, v)?,
                        Constraint::And(list) => {
                            let mut all = true;
                            for s in list {
                                if !self.conforms(*s, v)? {
                                    all = false;
                                    break;
                                }
                            }
                            all
                        }
                        Constraint::Or(list) | Constraint::Xone(list) => {
                            let mut n = 0;
                            for s in list {
                                n += usize::from(self.conforms(*s, v)?);
                            }
                            if let Constraint::Or(_) = c { n > 0 } else { n == 1 }
                        }
                        _ => true,
                    };
                    if !ok {
                        let (name, msg) = match c {
                            Constraint::Datatype(dt) => ("Datatype", format!("{v} is not a valid <{dt}>")),
                            Constraint::Class(k) => ("Class", format!("{v} is not an instance of {k}")),
                            Constraint::NodeKind(k) => ("NodeKind", format!("{v} is not a {k}")),
                            Constraint::Pattern(_, p) => ("Pattern", format!("{v} does not match /{p}/")),
                            Constraint::MinLength(n) => ("MinLength", format!("{v} is shorter than {n}")),
                            Constraint::MaxLength(n) => ("MaxLength", format!("{v} is longer than {n}")),
                            Constraint::LanguageIn(l) => ("LanguageIn", format!("{v} is not in one of the languages {}", l.join(", "))),
                            Constraint::In(_) => ("In", format!("{v} is not one of the allowed values")),
                            Constraint::Range(kind, b) => (*kind, format!("{v} is outside the allowed range ({kind} {b})")),
                            Constraint::Not(s) => ("Not", format!("{v} conforms to {}", self.shapes[*s].id)),
                            Constraint::Node(s) => ("Node", format!("{v} does not conform to {}", self.shapes[*s].id)),
                            Constraint::And(_) => ("And", format!("{v} does not conform to all of the shapes")),
                            Constraint::Or(_) => ("Or", format!("{v} does not conform to any of the shapes")),
                            Constraint::Xone(_) => ("Xone", format!("{v} does not conform to exactly one of the shapes")),
                            _ => ("Unknown", String::new()),
                        };
                        fail(name, Some(v), msg);
                    }
                }
            }
        }
        Ok(())
    }

    /// $currentShape and $shapesGraph.
    fn common(&self, shape: &Shape) -> [(&'static str, Term); 2] {
        [("currentShape", shape.id.clone()), ("shapesGraph", Term::iri(SHAPES_GRAPH))]
    }

    fn with_path(&self, text: &str, shape: &Shape) -> String {
        match &shape.path {
            Some(p) => text.replace("$PATH", &p.sparql()),
            None => text.to_string(),
        }
    }

    /// The ?this values of a SPARQL-based target.
    fn select_this(&self, text: &str) -> Result<Vec<Term>, String> {
        let prefixes = HashMap::new();
        let opts = crate::sparql::QueryOptions { base: "", prefixes: &prefixes, dataset: None, timeout_secs: None, infer: true, at_query: false };
        match crate::sparql::query_prebound(self.store, text, &opts, &[])? {
            crate::sparql::QueryResult::Solutions { vars, rows } => {
                let Some(i) = vars.iter().position(|v| v == "this") else { return Err("a SPARQL target must select ?this".into()) };
                Ok(rows.into_iter().filter_map(|mut r| r[i].take()).collect())
            }
            _ => Err("a SPARQL target must be a SELECT query".into()),
        }
    }

    fn ask(&self, text: &str, bind: &[(&str, Term)]) -> Result<bool, String> {
        let prefixes = HashMap::new();
        let opts = crate::sparql::QueryOptions { base: "", prefixes: &prefixes, dataset: None, timeout_secs: None, infer: true, at_query: false };
        match crate::sparql::query_prebound(self.store, text, &opts, bind)? {
            crate::sparql::QueryResult::Boolean(b) => Ok(b),
            _ => Err("a SHACL ASK validator must be an ASK query".into()),
        }
    }

    /// A SELECT-based constraint: each solution is a result. `kind` is the component's
    /// local name (SPARQL), or empty for a constraint component (the caller sets it).
    fn sparql(&self, q: &SparqlQuery, shape: &Shape, focus: &Term, params: &[(&str, Term)], kind: &str, out: &mut Vec<Violation>) -> Result<(), String> {
        let mut bind: Vec<(&str, Term)> = vec![("this", focus.clone())];
        bind.extend(params.iter().cloned());
        bind.extend(self.common(shape));
        let text = self.with_path(&q.text, shape);
        let prefixes = HashMap::new();
        let opts = crate::sparql::QueryOptions { base: "", prefixes: &prefixes, dataset: None, timeout_secs: None, infer: true, at_query: false };
        let crate::sparql::QueryResult::Solutions { vars, rows } = crate::sparql::query_prebound(self.store, &text, &opts, &bind)? else {
            return Err("a SHACL SPARQL constraint must be a SELECT query".into());
        };
        let col = |name: &str| vars.iter().position(|v| v == name);
        for row in rows {
            let get = |name: &str| col(name).and_then(|i| row[i].clone());
            if get("failure").is_some_and(|f| f.value() == "true") {
                return Err(format!("the SPARQL constraint of {} reported a failure", shape.id));
            }
            let value = get("value").or_else(|| shape.path.is_none().then(|| focus.clone()));
            let path = match get("path") {
                Some(p) if p.is_iri() => Some(PathExpr::Pred(p)),
                _ => shape.path.clone(),
            };
            let mut row_bind: Vec<(&str, Term)> = bind.clone();
            for (i, v) in vars.iter().enumerate() {
                if let Some(t) = &row[i] {
                    row_bind.push((v.as_str(), t.clone()));
                }
            }
            let template = shape.messages.first().or(q.messages.first()).map(|m| m.value().to_string());
            out.push(Violation {
                focus: get("this").unwrap_or_else(|| focus.clone()),
                path,
                value,
                shape: shape.id.clone(),
                component: format!("{SH}{kind}ConstraintComponent"),
                message: substitute(template.as_deref().unwrap_or("SPARQL constraint violated"), &row_bind),
                severity: shape.severity.clone(),
                source_constraint: q.node.clone(),
            });
        }
        Ok(())
    }
}

/// Replace {?var} / {$var} in a message with the variables' values.
fn substitute(template: &str, bind: &[(&str, Term)]) -> String {
    let mut s = template.to_string();
    for (name, t) in bind {
        let v = match t {
            Term::Iri(_) | Term::Blank(_) => t.to_string(),
            _ => t.value().to_string(),
        };
        s = s.replace(&format!("{{?{name}}}"), &v).replace(&format!("{{${name}}}"), &v);
    }
    s
}

/// Is `v` a valid lexical form of the XSD datatype `dt` (other datatypes always are)?
fn well_formed(v: &str, dt: &str) -> bool {
    let Some(local) = dt.strip_prefix(XSD) else { return true };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let int = |s: &str| digits(s.strip_prefix(['+', '-']).unwrap_or(s));
    let ranged = |lo: i128, hi: i128| int(v) && v.parse::<i128>().is_ok_and(|n| n >= lo && n <= hi);
    match local {
        "integer" => int(v),
        "long" => ranged(i64::MIN as i128, i64::MAX as i128),
        "int" => ranged(i32::MIN as i128, i32::MAX as i128),
        "short" => ranged(i16::MIN as i128, i16::MAX as i128),
        "byte" => ranged(i8::MIN as i128, i8::MAX as i128),
        "nonNegativeInteger" => int(v) && !v.starts_with('-') || v == "-0",
        "positiveInteger" => int(v) && v.parse::<i128>().is_ok_and(|n| n > 0),
        "nonPositiveInteger" => int(v) && v.parse::<i128>().is_ok_and(|n| n <= 0),
        "negativeInteger" => int(v) && v.parse::<i128>().is_ok_and(|n| n < 0),
        "unsignedLong" => ranged(0, u64::MAX as i128),
        "unsignedInt" => ranged(0, u32::MAX as i128),
        "unsignedShort" => ranged(0, u16::MAX as i128),
        "unsignedByte" => ranged(0, u8::MAX as i128),
        "decimal" => {
            let s = v.strip_prefix(['+', '-']).unwrap_or(v);
            let (a, b) = s.split_once('.').unwrap_or((s, ""));
            (!a.is_empty() || !b.is_empty()) && a.bytes().all(|c| c.is_ascii_digit()) && b.bytes().all(|c| c.is_ascii_digit())
        }
        "double" | "float" => matches!(v, "INF" | "-INF" | "+INF" | "NaN") || (v.parse::<f64>().is_ok() && !v.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E')),
        "boolean" => matches!(v, "true" | "false" | "1" | "0"),
        "dateTime" => func::parse_datetime(v).is_some() && v.contains('T'),
        "date" => {
            let s = v.strip_prefix('-').unwrap_or(v);
            let body = s.get(..10).unwrap_or("");
            let tz = s.get(10..).unwrap_or("");
            let b = body.as_bytes();
            body.len() == 10 && b[4] == b'-' && b[7] == b'-' && digits(&body[..4]) && digits(&body[5..7]) && digits(&body[8..10])
                && (1..=12).contains(&body[5..7].parse::<u32>().unwrap_or(0)) && (1..=31).contains(&body[8..10].parse::<u32>().unwrap_or(0))
                && (tz.is_empty() || tz == "Z" || (tz.len() == 6 && (tz.starts_with('+') || tz.starts_with('-'))))
        }
        _ => true,
    }
}

fn store_shapes_graph(store: &Store) -> u32 {
    store.dict
        .lookup(&Term::iri(SHAPES_GRAPH))
        .or_else(|| store.dict.lookup(&Term::iri(SHAPES_GRAPH_COMPAT)))
        .unwrap_or(u32::MAX - 7)
}

fn focus_nodes(v: &Validator, shape: &Shape, only: Option<&HashSet<Term>>) -> Result<Vec<Term>, String> {
    let store = v.store;
    let mut out: Vec<Term> = Vec::new();
    let mut seen = HashSet::new();
    let ty = Term::iri(RDF_TYPE);
    for t in &shape.targets {
        let candidates: Vec<Term> = match (t, only) {
            (Target::Node(n), _) => vec![n.clone()],
            (Target::Class(c), Some(set)) => set.iter().filter(|n| v.instance_of(n, c)).cloned().collect(),
            (Target::Class(c), None) => {
                // instances of the class and of its subclasses
                let sub = Term::iri(format!("{RDFS}subClassOf"));
                let mut classes = vec![c.clone()];
                let mut i = 0;
                while i < classes.len() {
                    for s in v.subjects(&sub, &classes[i].clone()) {
                        if !classes.contains(&s) {
                            classes.push(s);
                        }
                    }
                    i += 1;
                }
                classes.iter().flat_map(|c| v.subjects(&ty, c)).collect()
            }
            (Target::SubjectsOf(p), _) => {
                let Some(pid) = store.dict.lookup(p) else { continue };
                store.quads_matching(None, Some(pid), None, None).filter(|q| q[3] != v.sg).map(|q| store.dict.term(q[0])).collect()
            }
            (Target::ObjectsOf(p), _) => {
                let Some(pid) = store.dict.lookup(p) else { continue };
                store.quads_matching(None, Some(pid), None, None).filter(|q| q[3] != v.sg).map(|q| store.dict.term(q[2])).collect()
            }
            (Target::Sparql(q), _) => v.select_this(&q.text)?,
        };
        for n in candidates {
            if only.is_none_or(|set| set.contains(&n)) && seen.insert(n.clone()) {
                out.push(n);
            }
        }
    }
    Ok(out)
}

/// Validation report as JSON (for the REST API and the UI).
pub fn report_json(violations: &[Violation]) -> Json {
    Json::obj().with("conforms", violations.is_empty()).with(
        "results",
        Json::Arr(
            violations
                .iter()
                .map(|v| {
                    Json::obj()
                        .with("focusNode", v.focus.to_string())
                        .with("resultPath", v.path.as_ref().map(|p| p.sparql()))
                        .with("value", v.value.as_ref().map(|p| p.to_string()))
                        .with("sourceShape", v.shape.to_string())
                        .with("sourceConstraintComponent", v.component.as_str())
                        .with("resultSeverity", v.severity.value().trim_start_matches(SH))
                        .with("resultMessage", v.message.as_str())
                })
                .collect(),
        ),
    )
}

/// Validation report as Turtle (sh:ValidationReport).
pub fn report_turtle(violations: &[Violation]) -> String {
    let mut s = format!("@prefix sh: <{SH}> .\n@prefix rdf: <{RDF}> .\n\n[] a sh:ValidationReport ;\n  sh:conforms {}", violations.is_empty());
    for v in violations {
        s.push_str(&format!(" ;\n  sh:result [\n    a sh:ValidationResult ;\n    sh:resultSeverity {} ;\n", v.severity));
        s.push_str(&format!("    sh:focusNode {} ;\n", v.focus));
        if let Some(p) = &v.path {
            s.push_str(&format!("    sh:resultPath {} ;\n", p.turtle()));
        }
        if let Some(val) = &v.value {
            s.push_str(&format!("    sh:value {val} ;\n"));
        }
        if let Some(c) = &v.source_constraint {
            s.push_str(&format!("    sh:sourceConstraint {c} ;\n"));
        }
        s.push_str(&format!("    sh:sourceShape {} ;\n    sh:sourceConstraintComponent <{}> ;\n    sh:resultMessage {}\n  ]", v.shape, v.component, Term::string(v.message.clone())));
    }
    s.push_str(" .\n");
    s
}

/// Does a result block a write (sh:Violation; sh:Warning and sh:Info are reported only)?
pub fn blocking(v: &Violation) -> bool {
    v.severity == sh("Violation")
}

/// Add the subjects that point at any of `nodes`: their value nodes changed
/// (e.g. sh:class on a link whose target lost its type).
pub fn with_referrers(store: &Store, nodes: HashSet<Term>) -> HashSet<Term> {
    let mut out = nodes.clone();
    for n in &nodes {
        if let Some(id) = store.dict.lookup(n) {
            for q in store.quads_matching(None, None, Some(id), None).take(10_000) {
                out.insert(store.dict.term(q[0]));
            }
        }
    }
    out
}

/// Focus candidates a change affects: subjects and IRI/blank objects.
pub fn affected(changes: &[crate::rdf::Quad]) -> (HashSet<Term>, bool) {
    let mut set = HashSet::new();
    let mut shapes_changed = false;
    for q in changes {
        if q.g.as_ref().is_some_and(|g| g.value() == SHAPES_GRAPH || g.value() == SHAPES_GRAPH_COMPAT) {
            shapes_changed = true;
            continue;
        }
        set.insert(q.s.clone());
        if !q.o.is_literal() {
            set.insert(q.o.clone());
        }
    }
    (set, shapes_changed)
}
