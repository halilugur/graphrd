//! Query-time inference (`infer=at-query`): backward chaining over the schema instead of
//! materialized statements. The schema of the repository's explicit statements is read once
//! per query; a triple pattern with a fixed predicate is then answered as the entailments of
//! RDFS and OWL RL's property axioms require:
//!
//! - `?x rdf:type C`: stated types of C and of its subclasses (`rdfs:subClassOf`,
//!   `owl:equivalentClass`), plus subjects and objects of properties whose domain or range is
//!   one of them;
//! - `?x p ?y`: `p` and its subproperties (`rdfs:subPropertyOf`, `owl:equivalentProperty`),
//!   read backwards for `owl:inverseOf` and symmetric properties, and transitively when `p` is
//!   an `owl:TransitiveProperty`.
//!
//! Nothing is stored: answers follow the schema as it is when the query starts.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::rdf::{OWL, RDF, RDFS};
use crate::sparql::ast::Path;
use crate::store::{Id, Store};

/// The schema as `infer=at-query` walks it backwards.
pub struct AtQuery {
    /// `rdf:type`.
    pub ty: Id,
    /// C -> its direct subclasses (equivalences both ways); and the other direction.
    sub_class: HashMap<Id, Vec<Id>>,
    super_class: HashMap<Id, Vec<Id>>,
    sub_prop: HashMap<Id, Vec<Id>>,
    /// (property, class) pairs of rdfs:domain / rdfs:range.
    domains: Vec<(Id, Id)>,
    ranges: Vec<(Id, Id)>,
    inverse: HashMap<Id, Vec<Id>>,
    symmetric: HashSet<Id>,
    transitive: HashSet<Id>,
    closure_down: RefCell<HashMap<Id, Vec<Id>>>,
    closure_up: RefCell<HashMap<Id, Vec<Id>>>,
    props_down: RefCell<HashMap<Id, Vec<Id>>>,
}

fn reach(edges: &HashMap<Id, Vec<Id>>, from: Id) -> Vec<Id> {
    let mut seen = HashSet::from([from]);
    let mut queue = VecDeque::from([from]);
    while let Some(n) = queue.pop_front() {
        for &m in edges.get(&n).map_or(&[][..], |v| v.as_slice()) {
            if seen.insert(m) {
                queue.push_back(m);
            }
        }
    }
    let mut out: Vec<Id> = seen.into_iter().collect();
    out.sort_unstable();
    out
}

impl AtQuery {
    /// Read the schema from the store's explicit statements. None when the store has no
    /// `rdf:type` term at all (then nothing can be entailed).
    pub fn build(store: &Store) -> Option<AtQuery> {
        let id = |iri: String| store.dict.lookup(&crate::rdf::Term::Iri(iri));
        let ty = id(format!("{RDF}type"))?;
        let mut q = AtQuery {
            ty,
            sub_class: HashMap::new(),
            super_class: HashMap::new(),
            sub_prop: HashMap::new(),
            domains: Vec::new(),
            ranges: Vec::new(),
            inverse: HashMap::new(),
            symmetric: HashSet::new(),
            transitive: HashSet::new(),
            closure_down: RefCell::default(),
            closure_up: RefCell::default(),
            props_down: RefCell::default(),
        };
        let pairs = |p: Option<Id>| -> Vec<(Id, Id)> {
            match p {
                Some(p) => store
                    .explicit_matching(None, Some(p), None, None)
                    .map(|t| (t[0], t[2]))
                    .collect(),
                None => Vec::new(),
            }
        };
        let edge = |down: &mut HashMap<Id, Vec<Id>>, sup: Id, sub: Id| {
            down.entry(sup).or_default().push(sub)
        };
        let (mut down, mut up) = (HashMap::new(), HashMap::new());
        for (a, b) in pairs(id(format!("{RDFS}subClassOf"))) {
            edge(&mut down, b, a);
            edge(&mut up, a, b);
        }
        for (a, b) in pairs(id(format!("{OWL}equivalentClass"))) {
            for (x, y) in [(a, b), (b, a)] {
                edge(&mut down, x, y);
                edge(&mut up, y, x);
            }
        }
        q.sub_class = down;
        q.super_class = up;
        let mut props = HashMap::new();
        for (a, b) in pairs(id(format!("{RDFS}subPropertyOf"))) {
            edge(&mut props, b, a);
        }
        for (a, b) in pairs(id(format!("{OWL}equivalentProperty"))) {
            edge(&mut props, a, b);
            edge(&mut props, b, a);
        }
        q.sub_prop = props;
        q.domains = pairs(id(format!("{RDFS}domain")));
        q.ranges = pairs(id(format!("{RDFS}range")));
        for (a, b) in pairs(id(format!("{OWL}inverseOf"))) {
            q.inverse.entry(a).or_default().push(b);
            q.inverse.entry(b).or_default().push(a);
        }
        let typed = |class: String| -> HashSet<Id> {
            match id(class) {
                Some(c) => store
                    .explicit_matching(None, Some(ty), Some(c), None)
                    .map(|t| t[0])
                    .collect(),
                None => HashSet::new(),
            }
        };
        q.symmetric = typed(format!("{OWL}SymmetricProperty"));
        q.transitive = typed(format!("{OWL}TransitiveProperty"));
        Some(q)
    }

    /// C and every class below it.
    pub fn subclasses(&self, c: Id) -> Vec<Id> {
        self.closure_down
            .borrow_mut()
            .entry(c)
            .or_insert_with(|| reach(&self.sub_class, c))
            .clone()
    }

    /// C and every class above it.
    pub fn superclasses(&self, c: Id) -> Vec<Id> {
        self.closure_up
            .borrow_mut()
            .entry(c)
            .or_insert_with(|| reach(&self.super_class, c))
            .clone()
    }

    /// p and every property below it.
    pub fn subproperties(&self, p: Id) -> Vec<Id> {
        self.props_down
            .borrow_mut()
            .entry(p)
            .or_insert_with(|| reach(&self.sub_prop, p))
            .clone()
    }

    /// Properties whose subjects (`domain`) or objects (`range`) are instances of C.
    pub fn typing_properties(&self, c: Id, range: bool) -> Vec<Id> {
        let below: HashSet<Id> = self.subclasses(c).into_iter().collect();
        let mut out = Vec::new();
        for &(p, d) in if range { &self.ranges } else { &self.domains } {
            if below.contains(&d) {
                out.extend(self.subproperties(p));
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The classes a property's subjects (or objects) belong to by its domain (range),
    /// counting the domains of the properties it is a subproperty of.
    pub fn typed_by(&self, p: Id, range: bool) -> Vec<Id> {
        let mut out = Vec::new();
        for &(q, d) in if range { &self.ranges } else { &self.domains } {
            if self.subproperties(q).contains(&p) {
                out.extend(self.superclasses(d));
            }
        }
        out
    }

    /// Whether the schema says anything about types at all.
    pub fn has_typing(&self) -> bool {
        !self.domains.is_empty() || !self.ranges.is_empty()
    }

    /// The property path that answers `?x p ?y`, or None when only `p` itself does.
    pub fn path_for(&self, p: Id, iri: &dyn Fn(Id) -> String) -> Option<Path> {
        let mut alts = Vec::new();
        for q in self.subproperties(p) {
            alts.push(Path::Iri(iri(q)));
            if self.symmetric.contains(&q) {
                alts.push(Path::Inverse(Box::new(Path::Iri(iri(q)))));
            }
            for &inv in self.inverse.get(&q).map_or(&[][..], |v| v.as_slice()) {
                for r in self.subproperties(inv) {
                    alts.push(Path::Inverse(Box::new(Path::Iri(iri(r)))));
                }
            }
        }
        let transitive = self.transitive.contains(&p);
        if alts.len() == 1 && !transitive {
            return None;
        }
        let step = if alts.len() == 1 {
            alts.pop().unwrap()
        } else {
            Path::Alt(alts)
        };
        Some(if transitive {
            Path::OneOrMore(Box::new(step))
        } else {
            step
        })
    }
}
