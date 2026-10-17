//! Forward-chaining inference (materialization) for named rulesets:
//! RDFS, RDFS-Plus and OWL-Horst (pD*), each with an "-optimized" variant
//! that skips the noisy axiomatic/resource-typing rules.
//!
//! Rules are triple-pattern Horn clauses evaluated semi-naively: each round
//! joins the previous round's new triples (the delta) against everything.

use std::collections::HashSet;

use crate::rdf::{OWL, RDF, RDFS};
use crate::store::{Dict, Id};

/// A triple of dictionary ids.
pub type Triple = [Id; 3];

#[derive(Clone, Copy, Debug)]
enum T {
    V(u8),
    C(Id),
}

/// Maximum distinct variables in one rule.
const VARS: usize = 16;

#[derive(Clone, Debug)]
struct Rule {
    body: Vec<[T; 3]>,
    head: Vec<[T; 3]>,
    /// `[Constraint a != b]` pairs that must differ.
    distinct: Vec<(T, T)>,
}

/// The fact base the reasoner reads from and writes conclusions into.
pub trait Facts {
    /// Distinct triples matching a pattern (None = wildcard), across graphs.
    fn matching(&self, s: Option<Id>, p: Option<Id>, o: Option<Id>) -> Vec<Triple>;
/// Whether the store holds this triple.
    fn contains(&self, t: &Triple) -> bool;
    /// Record an inferred triple; false if it was already known.
    fn add_inferred(&mut self, t: Triple) -> bool;
/// Whether the id is a literal.
    fn is_literal(&self, id: Id) -> bool;
    /// Whether any triple matches the pattern (None = wildcard).
    fn has_any(&self, s: Option<Id>, p: Option<Id>, o: Option<Id>) -> bool {
        !self.matching(s, p, o).is_empty()
    }
}

/// The rulesets a repository may choose, by their well-known names.
pub const RULESETS: [&str; 13] = [
    "empty",
    "rdfs",
    "rdfs-optimized",
    "rdfsplus",
    "rdfsplus-optimized",
    "owl-horst",
    "owl-horst-optimized",
    "owl-max",
    "owl-max-optimized",
    "owl2-ql",
    "owl2-ql-optimized",
    "owl2-rl",
    "owl2-rl-optimized",
];

/// One ruleset: its rules and axioms, ready to run.
pub struct Reasoner {
    rules: Vec<Rule>,
    axioms: Vec<Triple>,
    /// OWL 2 RL: the vocabulary of axioms written as RDF lists, which `compile` turns into
    /// rules for the lists the data holds.
    lists: Option<ListVocab>,
}

/// Ids of the OWL list vocabulary (OWL 2 RL only).
#[derive(Clone)]
struct ListVocab {
    first: Id,
    rest: Id,
    nil: Id,
    ty: Id,
    same_as: Option<Id>,
    sub_class: Id,
    intersection: Id,
    union: Id,
    chain: Id,
    has_key: Id,
    one_of: Id,
}

impl Reasoner {
    /// Build a reasoner, or None for the empty ruleset.
    pub fn new(ruleset: &str, same_as: bool, dict: &Dict) -> Option<Reasoner> {
        if ruleset.ends_with(".pie") {
            return match std::fs::read_to_string(ruleset).map_err(|e| e.to_string()).and_then(|t| Reasoner::from_pie(&t, dict)) {
                Ok(r) => Some(r),
                Err(e) => {
                    eprintln!("graphrd: cannot use ruleset {ruleset}: {e}; inference is off");
                    None
                }
            };
        }
        let base = ruleset.trim_end_matches("-optimized");
        let optimized = ruleset.ends_with("-optimized");
        let level = match base {
            "rdf" => 0,
            "rdfs" => 1,
            "rdfsplus" | "owl2-ql" => 2,
            "owl-horst" => 3,
            "owl-max" | "owl2-rl" => 4,
            _ => return None,
        };
        let c = |local: &str| -> T {
            let iri = if let Some(l) = local.strip_prefix("rdf:") {
                format!("{RDF}{l}")
            } else if let Some(l) = local.strip_prefix("rdfs:") {
                format!("{RDFS}{l}")
            } else {
                format!("{OWL}{}", local.trim_start_matches("owl:"))
            };
            T::C(dict.intern(&crate::rdf::Term::Iri(iri)))
        };
        let (a, b, x, y, z, p, q, r) = (T::V(0), T::V(1), T::V(2), T::V(3), T::V(4), T::V(5), T::V(6), T::V(7));
        let ty = c("rdf:type");
        let sub_class = c("rdfs:subClassOf");
        let sub_prop = c("rdfs:subPropertyOf");
        let domain = c("rdfs:domain");
        let range = c("rdfs:range");
        let rule = |body: Vec<[T; 3]>, head: Vec<[T; 3]>| Rule { body, head, distinct: Vec::new() };
        if level == 0 {
            // RDF entailment: every predicate is a property
            let property = c("rdf:Property");
            let id = |t: T| if let T::C(i) = t { i } else { unreachable!() };
            let axioms = ["rdf:type", "rdf:subject", "rdf:predicate", "rdf:object", "rdf:first", "rdf:rest", "rdf:value"].iter().map(|p| [id(c(p)), id(ty), id(property)]).collect();
            return Some(Reasoner { rules: vec![rule(vec![[x, p, y]], vec![[p, ty, property]])], axioms, lists: None });
        }

        let mut rules = vec![
            rule(vec![[p, domain, a], [x, p, y]], vec![[x, ty, a]]),
            rule(vec![[p, range, a], [x, p, y]], vec![[y, ty, a]]),
            rule(vec![[p, sub_prop, q], [q, sub_prop, r]], vec![[p, sub_prop, r]]),
            rule(vec![[p, sub_prop, q], [x, p, y]], vec![[x, q, y]]),
            rule(vec![[a, sub_class, b], [x, ty, a]], vec![[x, ty, b]]),
            rule(vec![[a, sub_class, b], [b, sub_class, z]], vec![[a, sub_class, z]]),
        ];
        let mut axioms_t: Vec<[T; 3]> = Vec::new();
        // RDFS axioms and resource typing (OWL 2 RL has neither: its own scm-cls rules below)
        if !optimized && level < 4 {
            let (property, class, resource, literal_c, datatype, member, cmp) = (
                c("rdf:Property"),
                c("rdfs:Class"),
                c("rdfs:Resource"),
                c("rdfs:Literal"),
                c("rdfs:Datatype"),
                c("rdfs:member"),
                c("rdfs:ContainerMembershipProperty"),
            );
            rules.extend([
                rule(vec![[x, p, y]], vec![[p, ty, property], [x, ty, resource]]),
                rule(vec![[x, p, y]], vec![[y, ty, resource]]),
                rule(vec![[p, ty, property]], vec![[p, sub_prop, p]]),
                rule(vec![[a, ty, class]], vec![[a, sub_class, resource], [a, sub_class, a]]),
                rule(vec![[p, ty, cmp]], vec![[p, sub_prop, member]]),
                rule(vec![[a, ty, datatype]], vec![[a, sub_class, literal_c]]),
            ]);
            axioms_t.extend([
                [ty, ty, property],
                [sub_class, domain, class],
                [sub_class, range, class],
                [sub_prop, domain, property],
                [sub_prop, range, property],
                [domain, domain, property],
                [domain, range, class],
                [range, domain, property],
                [range, range, class],
                [ty, range, class],
                [resource, ty, class],
                [class, ty, class],
                [property, ty, class],
            ]);
        }
        if level >= 2 {
            let (inverse, symmetric, transitive, eq_class, eq_prop, functional, inv_functional) = (
                c("owl:inverseOf"),
                c("owl:SymmetricProperty"),
                c("owl:TransitiveProperty"),
                c("owl:equivalentClass"),
                c("owl:equivalentProperty"),
                c("owl:FunctionalProperty"),
                c("owl:InverseFunctionalProperty"),
            );
            rules.extend([
                rule(vec![[p, inverse, q], [x, p, y]], vec![[y, q, x]]),
                rule(vec![[p, inverse, q], [x, q, y]], vec![[y, p, x]]),
                rule(vec![[p, ty, symmetric], [x, p, y]], vec![[y, p, x]]),
                rule(vec![[p, ty, transitive], [x, p, y], [y, p, z]], vec![[x, p, z]]),
                rule(vec![[a, eq_class, b]], vec![[a, sub_class, b], [b, sub_class, a]]),
                rule(vec![[p, eq_prop, q]], vec![[p, sub_prop, q], [q, sub_prop, p]]),
                rule(vec![[a, sub_class, b], [b, sub_class, a]], vec![[a, eq_class, b]]),
                rule(vec![[p, sub_prop, q], [q, sub_prop, p]], vec![[p, eq_prop, q]]),
            ]);
            if same_as {
                let same = c("owl:sameAs");
                rules.extend([
                    rule(vec![[x, same, y]], vec![[y, same, x]]),
                    rule(vec![[x, same, y], [y, same, z]], vec![[x, same, z]]),
                    rule(vec![[x, same, y], [x, p, z]], vec![[y, p, z]]),
                    rule(vec![[x, same, y], [z, p, x]], vec![[z, p, y]]),
                    rule(vec![[p, ty, functional], [x, p, y], [x, p, z]], vec![[y, same, z]]),
                    rule(vec![[p, ty, inv_functional], [x, p, z], [y, p, z]], vec![[x, same, y]]),
                ]);
            }
        }
        if level >= 3 {
            let (has_value, on_prop, some_values, all_values, thing, class, owl_class) = (
                c("owl:hasValue"),
                c("owl:onProperty"),
                c("owl:someValuesFrom"),
                c("owl:allValuesFrom"),
                c("owl:Thing"),
                c("rdfs:Class"),
                c("owl:Class"),
            );
            rules.extend([
                rule(vec![[r, has_value, y], [r, on_prop, p], [x, ty, r]], vec![[x, p, y]]),
                rule(vec![[r, has_value, y], [r, on_prop, p], [x, p, y]], vec![[x, ty, r]]),
                rule(vec![[r, some_values, a], [r, on_prop, p], [x, p, y], [y, ty, a]], vec![[x, ty, r]]),
                rule(vec![[r, all_values, a], [r, on_prop, p], [x, ty, r], [x, p, y]], vec![[y, ty, a]]),
                rule(vec![[a, ty, owl_class]], vec![[a, ty, class], [a, sub_class, thing]]),
            ]);
        }
        let mut lists = None;
        if level >= 4 {
            let (thing, nothing, owl_class, obj_prop, dt_prop, eq_class, eq_prop) =
                (c("owl:Thing"), c("owl:Nothing"), c("owl:Class"), c("owl:ObjectProperty"), c("owl:DatatypeProperty"), c("owl:equivalentClass"), c("owl:equivalentProperty"));
            let (has_value, on_prop, some_values, all_values) = (c("owl:hasValue"), c("owl:onProperty"), c("owl:someValuesFrom"), c("owl:allValuesFrom"));
            let (c1, c2, y1, y2, p1, p2) = (T::V(8), T::V(9), T::V(10), T::V(11), T::V(12), T::V(13));
            rules.extend([
                // scm-dom1/2, scm-rng1/2: domains and ranges follow the hierarchies
                rule(vec![[p, domain, c1], [c1, sub_class, c2]], vec![[p, domain, c2]]),
                rule(vec![[p2, domain, a], [p1, sub_prop, p2]], vec![[p1, domain, a]]),
                rule(vec![[p, range, c1], [c1, sub_class, c2]], vec![[p, range, c2]]),
                rule(vec![[p2, range, a], [p1, sub_prop, p2]], vec![[p1, range, a]]),
                // scm-hv, scm-svf1/2, scm-avf1/2: subsumption between restrictions
                rule(vec![[c1, has_value, y], [c1, on_prop, p1], [c2, has_value, y], [c2, on_prop, p2], [p1, sub_prop, p2]], vec![[c1, sub_class, c2]]),
                rule(vec![[c1, some_values, y1], [c1, on_prop, p], [c2, some_values, y2], [c2, on_prop, p], [y1, sub_class, y2]], vec![[c1, sub_class, c2]]),
                rule(vec![[c1, some_values, y], [c1, on_prop, p1], [c2, some_values, y], [c2, on_prop, p2], [p1, sub_prop, p2]], vec![[c1, sub_class, c2]]),
                rule(vec![[c1, all_values, y1], [c1, on_prop, p], [c2, all_values, y2], [c2, on_prop, p], [y1, sub_class, y2]], vec![[c1, sub_class, c2]]),
                rule(vec![[c1, all_values, y], [c1, on_prop, p1], [c2, all_values, y], [c2, on_prop, p2], [p1, sub_prop, p2]], vec![[c2, sub_class, c1]]),
                // the domain of a property is the range of its inverse, and the other way round
                rule(vec![[p, c("owl:inverseOf"), q], [p, domain, a]], vec![[q, range, a]]),
                rule(vec![[p, c("owl:inverseOf"), q], [p, range, a]], vec![[q, domain, a]]),
                rule(vec![[p, c("owl:inverseOf"), q], [q, domain, a]], vec![[p, range, a]]),
                rule(vec![[p, c("owl:inverseOf"), q], [q, range, a]], vec![[p, domain, a]]),
                // cls-svf2: someValuesFrom owl:Thing
                rule(vec![[r, some_values, thing], [r, on_prop, p], [x, p, y]], vec![[x, ty, r]]),
                // scm-cls, scm-op, scm-dp: every class sits between owl:Nothing and owl:Thing
                rule(vec![[a, ty, owl_class]], vec![[a, sub_class, a], [a, eq_class, a], [a, sub_class, thing], [nothing, sub_class, a]]),
                rule(vec![[p, ty, obj_prop]], vec![[p, sub_prop, p], [p, eq_prop, p]]),
                rule(vec![[p, ty, dt_prop]], vec![[p, sub_prop, p], [p, eq_prop, p]]),
            ]);
            if same_as {
                let same = c("owl:sameAs");
                let max_card = c("owl:maxCardinality");
                let one = T::C(dict.intern(&crate::rdf::Term::typed("1", "http://www.w3.org/2001/XMLSchema#nonNegativeInteger")));
                // cls-maxc2: at most one value
                rules.push(rule(vec![[r, max_card, one], [r, on_prop, p], [x, ty, r], [x, p, y1], [x, p, y2]], vec![[y1, same, y2]]));
                if !optimized {
                    // eq-ref: everything is the same as itself
                    rules.push(rule(vec![[x, p, y]], vec![[x, same, x], [p, same, p], [y, same, y]]));
                }
            }
            let id = |t: T| if let T::C(i) = t { i } else { unreachable!() };
            lists = Some(ListVocab {
                first: id(c("rdf:first")),
                rest: id(c("rdf:rest")),
                nil: id(c("rdf:nil")),
                ty: id(ty),
                same_as: same_as.then(|| id(c("owl:sameAs"))),
                sub_class: id(sub_class),
                intersection: id(c("owl:intersectionOf")),
                union: id(c("owl:unionOf")),
                chain: id(c("owl:propertyChainAxiom")),
                has_key: id(c("owl:hasKey")),
                one_of: id(c("owl:oneOf")),
            });
        }
        let axioms = axioms_t
            .into_iter()
            .map(|t| t.map(|x| match x {
                T::C(id) => id,
                T::V(_) => unreachable!(),
            }))
            .collect();
        Some(Reasoner { rules, axioms, lists })
    }

    /// Predicates whose triples (in the data) change the rules `compile` makes.
    pub fn schema_predicates(&self) -> Vec<Id> {
        match &self.lists {
            Some(l) => vec![l.first, l.rest, l.intersection, l.union, l.chain, l.has_key, l.one_of],
            None => Vec::new(),
        }
    }

    /// OWL 2 RL axioms written as RDF lists, turned into rules and axioms for the lists in
    /// `facts`: `owl:intersectionOf` (cls-int1/2, scm-int), `owl:unionOf` (cls-uni, scm-uni),
    /// `owl:propertyChainAxiom` (prp-spo2), `owl:hasKey` (prp-key, with `owl:sameAs`) and
    /// `owl:oneOf` (cls-oo). None for rulesets without them. `base` must be the reasoner as
    /// `new` made it.
    pub fn compile(base: &Reasoner, facts: &dyn Facts) -> Option<Reasoner> {
        let l = base.lists.clone()?;
        let list = |head: Id| -> Option<Vec<Id>> {
            let (mut out, mut node, mut seen) = (Vec::new(), head, HashSet::new());
            while node != l.nil {
                if !seen.insert(node) || out.len() > 64 {
                    return None; // a cycle or an unreasonably long list
                }
                out.push(facts.matching(Some(node), Some(l.first), None).first()?[2]);
                node = facts.matching(Some(node), Some(l.rest), None).first()?[2];
            }
            Some(out)
        };
        let (mut rules, mut axioms) = (base.rules.clone(), base.axioms.clone());
        let (ty, x, y) = (T::C(l.ty), T::V(0), T::V(1));
        let rule = |body: Vec<[T; 3]>, head: Vec<[T; 3]>| Rule { body, head, distinct: Vec::new() };
        for [c, _, head] in facts.matching(None, Some(l.intersection), None) {
            let Some(parts) = list(head) else { continue };
            rules.push(rule(parts.iter().map(|&ci| [x, ty, T::C(ci)]).collect(), vec![[x, ty, T::C(c)]]));
            rules.push(rule(vec![[x, ty, T::C(c)]], parts.iter().map(|&ci| [x, ty, T::C(ci)]).collect()));
            axioms.extend(parts.iter().map(|&ci| [c, l.sub_class, ci]));
        }
        for [c, _, head] in facts.matching(None, Some(l.union), None) {
            let Some(parts) = list(head) else { continue };
            for &ci in &parts {
                rules.push(rule(vec![[x, ty, T::C(ci)]], vec![[x, ty, T::C(c)]]));
                axioms.push([ci, l.sub_class, c]);
            }
        }
        for [p, _, head] in facts.matching(None, Some(l.chain), None) {
            let Some(steps) = list(head) else { continue };
            if steps.is_empty() || steps.len() + 1 > VARS {
                continue;
            }
            let body = steps.iter().enumerate().map(|(i, &pi)| [T::V(i as u8), T::C(pi), T::V(i as u8 + 1)]).collect();
            rules.push(rule(body, vec![[T::V(0), T::C(p), T::V(steps.len() as u8)]]));
        }
        if let Some(same) = l.same_as {
            for [c, _, head] in facts.matching(None, Some(l.has_key), None) {
                let Some(keys) = list(head) else { continue };
                if keys.is_empty() || keys.len() + 2 > VARS {
                    continue;
                }
                let mut body = vec![[x, ty, T::C(c)], [y, ty, T::C(c)]];
                for (i, &k) in keys.iter().enumerate() {
                    let z = T::V(i as u8 + 2);
                    body.push([x, T::C(k), z]);
                    body.push([y, T::C(k), z]);
                }
                rules.push(rule(body, vec![[x, T::C(same), y]]));
            }
        }
        for [c, _, head] in facts.matching(None, Some(l.one_of), None) {
            let Some(members) = list(head) else { continue };
            axioms.extend(members.iter().map(|&m| [m, l.ty, c]));
        }
        Some(Reasoner { rules, axioms, lists: Some(l) })
    }

    /// Identifies the rules and axioms, to tell whether a new compilation changed them.
    pub fn signature(&self) -> String {
        format!("{:?}{:?}", self.rules, self.axioms)
    }

    /// Derive everything that follows from `delta` (triples already present
    /// in `facts`), adding conclusions to `facts` round by round.
    pub fn run(&self, facts: &mut dyn Facts, delta: Vec<Triple>) -> usize {
        let mut total = 0;
        let mut delta = delta;
        for ax in &self.axioms {
            if facts.add_inferred(*ax) {
                total += 1;
                delta.push(*ax);
            }
        }
        while !delta.is_empty() {
            let mut fresh: Vec<Triple> = Vec::new();
            let mut fresh_set: HashSet<Triple> = HashSet::new();
            // the new triples by predicate, so an atom with a fixed predicate sees only its own
            let mut by_pred: std::collections::HashMap<Id, Vec<Triple>> = std::collections::HashMap::new();
            for t in &delta {
                by_pred.entry(t[1]).or_default().push(*t);
            }
            // a rule with another atom that no triple matches, even ignoring its variables,
            // cannot fire (the common case of instance data without schema triples)
            let mut known: std::collections::HashMap<[Option<Id>; 3], bool> = std::collections::HashMap::new();
            for rule in &self.rules {
                for (i, atom) in rule.body.iter().enumerate() {
                    let rest: Vec<&[T; 3]> = rule.body.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, a)| a).collect();
                    let viable = rest.iter().all(|a| {
                        let c = |t: &T| if let T::C(id) = t { Some(*id) } else { None };
                        let key = [c(&a[0]), c(&a[1]), c(&a[2])];
                        key == [None; 3] || *known.entry(key).or_insert_with(|| facts.has_any(key[0], key[1], key[2]))
                    });
                    if !viable {
                        continue;
                    }
                    let candidates: &[Triple] = match atom[1] {
                        T::C(p) => by_pred.get(&p).map_or(&[], |v| v.as_slice()),
                        T::V(_) => &delta,
                    };
                    for t in candidates {
                        let mut b = [None; VARS];
                        if !unify(atom, t, &mut b) {
                            continue;
                        }
                        join(&*facts, &rest, &mut b, &mut |b| {
                            if rule.distinct.iter().any(|(x, y)| resolve(*x, b) == resolve(*y, b)) {
                                return;
                            }
                            for h in &rule.head {
                                let t = [resolve(h[0], b), resolve(h[1], b), resolve(h[2], b)];
                                if fresh_set.contains(&t) || facts.is_literal(t[0]) || facts.is_literal(t[1]) {
                                    continue;
                                }
                                if !facts.contains(&t) {
                                    fresh_set.insert(t);
                                    fresh.push(t);
                                }
                            }
                        });
                    }
                }
            }
            fresh.retain(|t| facts.add_inferred(*t));
            total += fresh.len();
            delta = fresh;
        }
        total
    }
}

impl Reasoner {
    /// Build a reasoner from a `.pie` rule file: `Prefices`,
    /// `Axioms` and `Rules` blocks, `Id:` rules with premises, a dashed
    /// line, consequences, and `[Constraint a != b]` annotations.
    pub fn from_pie(text: &str, dict: &Dict) -> Result<Reasoner, String> {
        let mut prefixes: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut section = "";
        let mut axioms = Vec::new();
        let mut rules = Vec::new();
        // current rule under construction: (vars, body, head, distinct, in_head)
        let mut cur: Option<(Vec<String>, Vec<[T; 3]>, Vec<[T; 3]>, Vec<(T, T)>, bool)> = None;
        let finish = |cur: &mut Option<(Vec<String>, Vec<[T; 3]>, Vec<[T; 3]>, Vec<(T, T)>, bool)>, rules: &mut Vec<Rule>| {
            if let Some((_, body, head, distinct, _)) = cur.take() {
                if !body.is_empty() && !head.is_empty() {
                    rules.push(Rule { body, head, distinct });
                }
            }
        };
        for raw in text.lines() {
            // comments are whole lines; "//" also occurs inside IRIs
            let line = raw.trim();
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("prefices") || lower.starts_with("prefixes") {
                section = "prefixes";
                continue;
            }
            if lower.starts_with("axioms") {
                section = "axioms";
                continue;
            }
            if lower.starts_with("rules") {
                section = "rules";
                continue;
            }
            if line == "{" {
                continue;
            }
            if line == "}" {
                finish(&mut cur, &mut rules);
                section = "";
                continue;
            }
            match section {
                "prefixes" => {
                    if let Some((p, uri)) = line.split_once(':') {
                        let uri = uri.trim().trim_start_matches('<').trim_end_matches('>');
                        prefixes.insert(p.trim().to_string(), uri.to_string());
                    }
                }
                "axioms" => {
                    let toks = pie_tokens(line);
                    if toks.len() >= 3 {
                        let mut t = [0; 3];
                        for k in 0..3 {
                            t[k] = dict.intern(&crate::rdf::Term::Iri(pie_iri(&toks[k], &prefixes)?));
                        }
                        axioms.push(t);
                    }
                }
                "rules" => {
                    if let Some(id) = line.strip_prefix("Id:") {
                        finish(&mut cur, &mut rules);
                        let _ = id;
                        cur = Some((Vec::new(), Vec::new(), Vec::new(), Vec::new(), false));
                        continue;
                    }
                    let Some(c) = cur.as_mut() else { continue };
                    if line.chars().all(|ch| ch == '-') {
                        c.4 = true;
                        continue;
                    }
                    let (triple_part, constraint) = match line.find('[') {
                        Some(i) => (&line[..i], Some(&line[i..])),
                        None => (line, None),
                    };
                    let toks = pie_tokens(triple_part);
                    let var = |name: &str, vars: &mut Vec<String>| -> Result<T, String> {
                        if let Some(i) = vars.iter().position(|v| v == name) {
                            return Ok(T::V(i as u8));
                        }
                        if vars.len() >= VARS {
                            return Err(format!("a rule uses more than {VARS} variables"));
                        }
                        vars.push(name.to_string());
                        Ok(T::V((vars.len() - 1) as u8))
                    };
                    let term = |tok: &str, vars: &mut Vec<String>| -> Result<T, String> {
                        if tok.starts_with('<') {
                            Ok(T::C(dict.intern(&crate::rdf::Term::Iri(pie_iri(tok, &prefixes)?))))
                        } else {
                            var(tok, vars)
                        }
                    };
                    if toks.len() >= 3 {
                        let atom = [term(&toks[0], &mut c.0)?, term(&toks[1], &mut c.0)?, term(&toks[2], &mut c.0)?];
                        if c.4 { c.2.push(atom) } else { c.1.push(atom) }
                    }
                    if let Some(con) = constraint {
                        let inner = con.trim_start_matches('[').trim_end_matches(']').trim();
                        if let Some(rest) = inner.strip_prefix("Constraint") {
                            for pair in rest.split(',') {
                                if let Some((a, b)) = pair.split_once("!=") {
                                    let (a, b) = (term(a.trim(), &mut c.0)?, term(b.trim(), &mut c.0)?);
                                    c.3.push((a, b));
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        finish(&mut cur, &mut rules);
        if rules.is_empty() && axioms.is_empty() {
            return Err("no rules or axioms found".into());
        }
        Ok(Reasoner { rules, axioms, lists: None })
    }

    /// DRed step 1: everything that could have been derived (directly or
    /// transitively) through the `deleted` triples, evaluated while they are
    /// still in `facts`.
    pub fn overdelete(&self, facts: &dyn Facts, deleted: &[Triple]) -> HashSet<Triple> {
        let mut out: HashSet<Triple> = HashSet::new();
        let mut delta: Vec<Triple> = deleted.to_vec();
        while !delta.is_empty() {
            let mut next = Vec::new();
            for rule in &self.rules {
                for (i, atom) in rule.body.iter().enumerate() {
                    for t in &delta {
                        let mut b = [None; VARS];
                        if !unify(atom, t, &mut b) {
                            continue;
                        }
                        let rest: Vec<&[T; 3]> = rule.body.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, a)| a).collect();
                        join(facts, &rest, &mut b, &mut |b| {
                            if rule.distinct.iter().any(|(x, y)| resolve(*x, b) == resolve(*y, b)) {
                                return;
                            }
                            for h in &rule.head {
                                let t = [resolve(h[0], b), resolve(h[1], b), resolve(h[2], b)];
                                if !self.axioms.contains(&t) && out.insert(t) {
                                    next.push(t);
                                }
                            }
                        });
                    }
                }
            }
            delta = next;
        }
        out
    }

    /// DRed step 2: can `t` be derived in one rule application from `facts`?
    pub fn derivable(&self, facts: &dyn Facts, t: &Triple) -> bool {
        if self.axioms.contains(t) {
            return true;
        }
        for rule in &self.rules {
            for h in &rule.head {
                let mut b = [None; VARS];
                if !unify(h, t, &mut b) {
                    continue;
                }
                let body: Vec<&[T; 3]> = rule.body.iter().collect();
                let mut found = false;
                join(facts, &body, &mut b, &mut |b| {
                    if !rule.distinct.iter().any(|(x, y)| resolve(*x, b) == resolve(*y, b)) {
                        found = true;
                    }
                });
                if found {
                    return true;
                }
            }
        }
        false
    }
}

fn join(facts: &dyn Facts, rest: &[&[T; 3]], b: &mut [Option<Id>; VARS], emit: &mut dyn FnMut(&[Option<Id>; VARS])) {
    if rest.is_empty() {
        emit(b);
        return;
    }
    // Most-bound atom first.
    let (idx, _) = rest
        .iter()
        .enumerate()
        .max_by_key(|(_, a)| a.iter().filter(|t| matches!(t, T::C(_)) || matches!(t, T::V(v) if b[*v as usize].is_some())).count())
        .unwrap();
    let atom = rest[idx];
    let others: Vec<&[T; 3]> = rest.iter().enumerate().filter(|(j, _)| *j != idx).map(|(_, a)| *a).collect();
    let get = |t: T, b: &[Option<Id>; VARS]| match t {
        T::C(id) => Some(id),
        T::V(v) => b[v as usize],
    };
    for t in facts.matching(get(atom[0], b), get(atom[1], b), get(atom[2], b)) {
        let saved = *b;
        if unify(atom, &t, b) {
            join(facts, &others, b, emit);
        }
        *b = saved;
    }
}

fn unify(atom: &[T; 3], t: &Triple, b: &mut [Option<Id>; VARS]) -> bool {
    for k in 0..3 {
        match atom[k] {
            T::C(id) => {
                if id != t[k] {
                    return false;
                }
            }
            T::V(v) => match b[v as usize] {
                Some(x) if x != t[k] => return false,
                _ => b[v as usize] = Some(t[k]),
            },
        }
    }
    true
}

fn resolve(t: T, b: &[Option<Id>; VARS]) -> Id {
    match t {
        T::C(id) => id,
        T::V(v) => b[v as usize].expect("head variable bound by body"),
    }
}

fn pie_tokens(line: &str) -> Vec<String> {
    line.split_whitespace().map(String::from).collect()
}

/// `<prefix:local>` or `<http://...>` to a full IRI.
fn pie_iri(tok: &str, prefixes: &std::collections::HashMap<String, String>) -> Result<String, String> {
    let inner = tok.trim_start_matches('<').trim_end_matches('>');
    if inner.contains("://") {
        return Ok(inner.to_string());
    }
    let (p, local) = inner.split_once(':').ok_or_else(|| format!("bad term {tok}"))?;
    let ns = prefixes.get(p).ok_or_else(|| format!("unknown prefix {p}"))?;
    Ok(format!("{ns}{local}"))
}
