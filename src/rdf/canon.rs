//! RDF Dataset Canonicalization (RDFC-1.0, W3C Recommendation): canonical blank node labels
//! (`c14n0`, `c14n1`, ...) and canonical N-Quads, so that isomorphic datasets serialize to
//! identical bytes (for hashing, signing and diffing).

use std::collections::{BTreeMap, HashMap};

use super::{Quad, Term, XSD_STRING};
use crate::sparql::hash::hex_digest;

/// Work limit for the N-degree hashing: datasets built to make canonicalization exponential
/// ("poison graphs") are rejected instead of running forever.
const MAX_NDEGREE_CALLS: usize = 25_000;

/// Canonical N-Triples literal escaping: `"` `\` and BS HT LF FF CR as ECHAR, other control
/// characters as upper-case UCHAR, everything else as is.
fn escape(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32))
            }
            c => out.push(c),
        }
    }
}

/// A term in canonical N-Quads; blank nodes go through `label`.
fn term(out: &mut String, t: &Term, label: &dyn Fn(&str) -> String) {
    match t {
        Term::Iri(i) => {
            out.push('<');
            out.push_str(i);
            out.push('>');
        }
        Term::Blank(b) => {
            out.push_str("_:");
            out.push_str(&label(b));
        }
        Term::Literal(l) => {
            out.push('"');
            escape(out, &l.value);
            out.push('"');
            if let Some(lang) = &l.lang {
                out.push('@');
                out.push_str(lang);
            } else if l.datatype != XSD_STRING {
                out.push_str("^^<");
                out.push_str(&l.datatype);
                out.push('>');
            }
        }
        Term::Triple(t) => {
            out.push_str("<<( ");
            for x in t.iter() {
                term(out, x, label);
                out.push(' ');
            }
            out.push_str(")>>");
        }
    }
}

fn nquad(q: &Quad, label: &dyn Fn(&str) -> String) -> String {
    let mut s = String::new();
    term(&mut s, &q.s, label);
    s.push(' ');
    term(&mut s, &q.p, label);
    s.push(' ');
    term(&mut s, &q.o, label);
    if let Some(g) = &q.g {
        s.push(' ');
        term(&mut s, g, label);
    }
    s.push_str(" .\n");
    s
}

/// Blank node identifiers issued in order (`prefix` + counter).
#[derive(Clone)]
struct Issuer {
    prefix: &'static str,
    issued: Vec<String>,
    map: HashMap<String, String>,
}

impl Issuer {
    fn new(prefix: &'static str) -> Issuer {
        Issuer {
            prefix,
            issued: Vec::new(),
            map: HashMap::new(),
        }
    }

    fn issue(&mut self, b: &str) -> String {
        if let Some(id) = self.map.get(b) {
            return id.clone();
        }
        let id = format!("{}{}", self.prefix, self.issued.len());
        self.issued.push(b.to_string());
        self.map.insert(b.to_string(), id.clone());
        id
    }
}

struct State<'a> {
    quads: &'a [Quad],
    /// Hash algorithm (SPARQL name: SHA256, the default, or SHA384...).
    alg: &'a str,
    /// Blank node -> indexes of the quads that mention it.
    bnode_quads: HashMap<String, Vec<usize>>,
    canonical: Issuer,
    calls: usize,
}

fn blanks(q: &Quad) -> Vec<(&str, char)> {
    let mut out = Vec::new();
    for (t, pos) in [(&q.s, 's'), (&q.o, 'o')] {
        if let Term::Blank(b) = t {
            out.push((b.as_str(), pos));
        }
    }
    if let Some(Term::Blank(b)) = &q.g {
        out.push((b.as_str(), 'g'));
    }
    out
}

impl State<'_> {
    fn hash(&self, s: &str) -> String {
        hex_digest(self.alg, s.as_bytes()).unwrap_or_default()
    }

    /// 4.6 Hash First Degree Quads.
    fn first_degree(&self, n: &str) -> String {
        let mut lines: Vec<String> = self.bnode_quads[n]
            .iter()
            .map(|&i| {
                nquad(&self.quads[i], &|b| {
                    if b == n { "a".into() } else { "z".into() }
                })
            })
            .collect();
        lines.sort();
        self.hash(&lines.concat())
    }

    /// 4.7 Hash Related Blank Node.
    fn related(&self, related: &str, q: &Quad, issuer: &Issuer, pos: char) -> String {
        let mut input = pos.to_string();
        if pos != 'g' {
            input.push('<');
            input.push_str(q.p.value());
            input.push('>');
        }
        // issued identifiers are written as `_:id`; otherwise the first-degree hash, bare
        match (self.canonical.map.get(related), issuer.map.get(related)) {
            (Some(id), _) | (None, Some(id)) => {
                input.push_str("_:");
                input.push_str(id);
            }
            (None, None) => input.push_str(&self.first_degree(related)),
        }
        self.hash(&input)
    }

    /// 4.8 Hash N-Degree Quads.
    fn n_degree(&mut self, n: &str, issuer: &Issuer) -> Result<(String, Issuer), String> {
        self.calls += 1;
        if self.calls > MAX_NDEGREE_CALLS {
            return Err(
                "RDFC-1.0: dataset too complex to canonicalize (possible poison graph)".into(),
            );
        }
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for &i in &self.bnode_quads[n].clone() {
            let q = &self.quads[i];
            for (b, pos) in blanks(q) {
                if b != n {
                    let h = self.related(b, q, issuer, pos);
                    groups.entry(h).or_default().push(b.to_string());
                }
            }
        }
        let mut data = String::new();
        let mut issuer = issuer.clone();
        for (h, list) in groups {
            data.push_str(&h);
            let mut chosen_path = String::new();
            let mut chosen_issuer: Option<Issuer> = None;
            let mut perm: Vec<usize> = (0..list.len()).collect();
            loop {
                let mut copy = issuer.clone();
                let mut path = String::new();
                let mut recursion = Vec::new();
                let mut skip = false;
                for &k in &perm {
                    let related = &list[k];
                    match self.canonical.map.get(related) {
                        Some(id) => {
                            path.push_str("_:");
                            path.push_str(id);
                        }
                        None => {
                            if !copy.map.contains_key(related) {
                                recursion.push(related.clone());
                            }
                            path.push_str("_:");
                            path.push_str(&copy.issue(related));
                        }
                    }
                    if !chosen_path.is_empty()
                        && path.len() >= chosen_path.len()
                        && path > chosen_path
                    {
                        skip = true;
                        break;
                    }
                }
                if !skip {
                    for related in &recursion {
                        let (rh, ri) = self.n_degree(related, &copy)?;
                        path.push_str("_:");
                        path.push_str(&copy.issue(related));
                        path.push('<');
                        path.push_str(&rh);
                        path.push('>');
                        copy = ri;
                        if !chosen_path.is_empty()
                            && path.len() >= chosen_path.len()
                            && path > chosen_path
                        {
                            skip = true;
                            break;
                        }
                    }
                }
                if !skip && (chosen_path.is_empty() || path < chosen_path) {
                    chosen_path = path;
                    chosen_issuer = Some(copy);
                }
                if !next_permutation(&mut perm) {
                    break;
                }
            }
            data.push_str(&chosen_path);
            if let Some(i) = chosen_issuer {
                issuer = i;
            }
        }
        Ok((self.hash(&data), issuer))
    }
}

/// Lexicographic next permutation; false after the last one.
fn next_permutation(p: &mut [usize]) -> bool {
    if p.len() < 2 {
        return false;
    }
    let mut i = p.len() - 1;
    while i > 0 && p[i - 1] >= p[i] {
        i -= 1;
    }
    if i == 0 {
        return false;
    }
    let mut j = p.len() - 1;
    while p[j] <= p[i - 1] {
        j -= 1;
    }
    p.swap(i - 1, j);
    p[i..].reverse();
    true
}

/// The canonical label of every blank node: input label -> `c14nN` (RDFC-1.0 section 4.4).
pub fn issued_identifiers(quads: &[Quad]) -> Result<Vec<(String, String)>, String> {
    issued_identifiers_with(quads, "SHA256")
}

/// `issued_identifiers` with another hash algorithm (`SHA256`, `SHA384`, `SHA512`).
pub fn issued_identifiers_with(quads: &[Quad], alg: &str) -> Result<Vec<(String, String)>, String> {
    if hex_digest(alg, b"").is_none() {
        return Err(format!("RDFC-1.0: unknown hash algorithm {alg}"));
    }
    let mut st = State {
        quads,
        alg,
        bnode_quads: HashMap::new(),
        canonical: Issuer::new("c14n"),
        calls: 0,
    };
    for (i, q) in quads.iter().enumerate() {
        for (b, _) in blanks(q) {
            let list = st.bnode_quads.entry(b.to_string()).or_default();
            if list.last() != Some(&i) {
                list.push(i);
            }
        }
    }
    let mut by_hash: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut nodes: Vec<&String> = st.bnode_quads.keys().collect();
    nodes.sort();
    for n in nodes {
        by_hash
            .entry(st.first_degree(n))
            .or_default()
            .push(n.clone());
    }
    // unique first-degree hashes are issued in hash order
    let mut shared = Vec::new();
    for (h, list) in by_hash {
        if list.len() == 1 {
            st.canonical.issue(&list[0]);
        } else {
            shared.push((h, list));
        }
    }
    for (_, list) in shared {
        let mut paths = Vec::new();
        for n in &list {
            if st.canonical.map.contains_key(n) {
                continue;
            }
            let mut temp = Issuer::new("b");
            temp.issue(n);
            paths.push(st.n_degree(n, &temp)?);
        }
        paths.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, issuer) in paths {
            for b in &issuer.issued {
                st.canonical.issue(b);
            }
        }
    }
    let c = st.canonical;
    Ok(c.issued
        .iter()
        .map(|b| (b.clone(), c.map[b].clone()))
        .collect())
}

/// The dataset as canonical N-Quads (RDFC-1.0): canonical blank node labels, canonical term
/// forms, one statement per line, lines sorted, duplicates removed.
pub fn canonical_nquads(quads: &[Quad]) -> Result<String, String> {
    canonical_nquads_with(quads, "SHA256")
}

/// RDFC-1.0 (or URGNA2012) canonical N-Quads of a dataset.
pub fn canonical_nquads_with(quads: &[Quad], alg: &str) -> Result<String, String> {
    let ids: HashMap<String, String> = issued_identifiers_with(quads, alg)?.into_iter().collect();
    let mut lines: Vec<String> = quads
        .iter()
        .map(|q| nquad(q, &|b| ids.get(b).cloned().unwrap_or_else(|| b.to_string())))
        .collect();
    lines.sort();
    lines.dedup();
    Ok(lines.concat())
}

#[cfg(test)]
mod tests {
    #[test]
    fn isomorphic_datasets_canonicalize_identically() {
        let a = "_:x <http://p> _:y .\n_:y <http://p> _:x .\n_:x <http://q> \"v\" .\n";
        let b = "_:k <http://p> _:j .\n_:j <http://p> _:k .\n_:j <http://q> \"v\" .\n";
        let ca = super::canonical_nquads(&super::super::ntriples::parse(a, true, true).unwrap())
            .unwrap();
        let cb = super::canonical_nquads(&super::super::ntriples::parse(b, true, true).unwrap())
            .unwrap();
        assert_eq!(ca, cb);
        assert!(ca.contains("_:c14n0") && ca.contains("_:c14n1"), "{ca}");
    }
}
