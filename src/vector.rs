//! Vector search: an HNSW graph (hierarchical navigable small world) over the vector literals
//! of a repository, for approximate nearest-neighbour queries by cosine similarity.
//!
//! A vector is a literal of datatype `g:vector` (`http://graphrd.org/fn#vector`) whose lexical
//! form lists numbers: `"[0.12, -0.5, 0.33]"^^g:vector` (brackets and commas optional). Every
//! such literal added to the repository is indexed. Vectors are normalized, so the dot product is
//! the cosine similarity.
//!
//! The graph lives in memory and is written to `vectors.bin` at checkpoints; statements replayed
//! from the log after that are indexed again (inserting a vector twice is a no-op). Nodes are
//! never removed: searches skip literals the repository no longer holds.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::rdf::Term;
use crate::store::Id;

/// The datatype of a vector literal: `"1 0.5 …"^^g:vector`.
pub const VECTOR: &str = "http://graphrd.org/fn#vector";

/// The numbers of a vector literal (or a plain string of numbers, for query arguments).
pub fn parse(t: &Term) -> Option<Vec<f32>> {
    let l = t.as_literal()?;
    let text = l.value.trim().trim_start_matches('[').trim_end_matches(']');
    let v: Option<Vec<f32>> = text.split([',', ' ', '\t', '\n']).filter(|s| !s.is_empty()).map(|s| s.parse::<f32>().ok().filter(|x| x.is_finite())).collect();
    v.filter(|v| !v.is_empty())
}

/// Whether the term is a vector literal.
pub fn is_vector_literal(t: &Term) -> bool {
    t.as_literal().is_some_and(|l| l.datatype == VECTOR)
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    // in chunks of 8, so the compiler vectorizes it
    let mut acc = [0f32; 8];
    let (ca, cb) = (a.chunks_exact(8), b.chunks_exact(8));
    let (ra, rb) = (ca.remainder(), cb.remainder());
    for (x, y) in ca.zip(cb) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    acc.iter().sum::<f32>() + ra.iter().zip(rb).map(|(x, y)| x * y).sum::<f32>()
}

/// Cosine similarity of two vectors of the same length.
pub fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() {
        return None;
    }
    let (na, nb) = (dot(a, a).sqrt(), dot(b, b).sqrt());
    (na > 0.0 && nb > 0.0).then(|| dot(a, b) / (na * nb))
}

/// (similarity, node index), ordered by similarity.
#[derive(Clone, Copy, PartialEq)]
struct Cand(f32, u32);
impl Eq for Cand {}
impl PartialOrd for Cand {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Cand {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&o.0).then(self.1.cmp(&o.1))
    }
}

const M: usize = 16;
const M0: usize = 32;
const EF_CONSTRUCTION: usize = 100;

/// A Hierarchical Navigable Small World graph over the vector literals.
pub struct Hnsw {
    dim: usize,
    /// Node index -> literal id, vector, links per level.
    ids: Vec<Id>,
    vecs: Vec<f32>,
    links: Vec<Vec<Vec<u32>>>,
    index_of: HashMap<Id, u32>,
    entry: Option<u32>,
    rng: u64,
/// Set when the graph changed since it was written out.
    pub dirty: bool,
}

impl Default for Hnsw {
    fn default() -> Self {
        Hnsw { dim: 0, ids: Vec::new(), vecs: Vec::new(), links: Vec::new(), index_of: HashMap::new(), entry: None, rng: 0x9E37_79B9_7F4A_7C15, dirty: false }
    }
}

impl Hnsw {
/// The number of vectors.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

/// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

/// The vectors' dimension.
    pub fn dim(&self) -> usize {
        self.dim
    }

    fn vec(&self, n: u32) -> &[f32] {
        &self.vecs[n as usize * self.dim..(n as usize + 1) * self.dim]
    }

    fn random_level(&mut self) -> usize {
        // xorshift; levels are geometric with ratio 1/M
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        let u = (x >> 11) as f64 / (1u64 << 53) as f64;
        (-(u.max(1e-12)).ln() / (M as f64).ln()) as usize
    }

    /// Index a vector literal. Vectors of another length than the first one indexed are
    /// ignored (one repository, one embedding size). False when nothing was added.
    pub fn insert(&mut self, id: Id, v: Vec<f32>) -> bool {
        if self.index_of.contains_key(&id) || v.is_empty() {
            return false;
        }
        if self.dim == 0 {
            self.dim = v.len();
        } else if v.len() != self.dim {
            return false;
        }
        let v = normalize(v);
        let level = self.random_level().min(16);
        let n = self.ids.len() as u32;
        self.ids.push(id);
        self.vecs.extend_from_slice(&v);
        self.links.push(vec![Vec::new(); level + 1]);
        self.index_of.insert(id, n);
        self.dirty = true;
        let Some(mut ep) = self.entry else {
            self.entry = Some(n);
            return true;
        };
        let top = self.links[ep as usize].len() - 1;
        for l in (level + 1..=top).rev() {
            ep = self.greedy(&v, ep, l);
        }
        for l in (0..=level.min(top)).rev() {
            let found = self.search_level(&v, &[ep], EF_CONSTRUCTION, l, &|_| true);
            let max = if l == 0 { M0 } else { M };
            let neighbours = self.select(&found, max);
            self.links[n as usize][l] = neighbours.clone();
            for &m in &neighbours {
                self.links[m as usize][l].push(n);
                if self.links[m as usize][l].len() > max {
                    let base = self.vec(m).to_vec();
                    let mut ls: Vec<Cand> = self.links[m as usize][l].iter().map(|&o| Cand(dot(&base, self.vec(o)), o)).collect();
                    ls.sort_by(|a, b| b.cmp(a));
                    self.links[m as usize][l] = self.select(&ls, max);
                }
            }
            if let Some(c) = found.first() {
                ep = c.1;
            }
        }
        if level > top {
            self.entry = Some(n);
        }
        true
    }

    /// Choose up to `max` neighbours from candidates (most similar first) with the HNSW
    /// heuristic: a candidate is kept when it is closer to the node than to every neighbour
    /// kept so far, so links spread out in every direction (and reach other clusters) instead
    /// of bunching in one; the remaining places go to the closest of the rest.
    fn select(&self, cands: &[Cand], max: usize) -> Vec<u32> {
        let mut kept: Vec<u32> = Vec::with_capacity(max);
        let mut skipped = Vec::new();
        for c in cands {
            if kept.len() >= max {
                break;
            }
            let v = self.vec(c.1);
            if kept.iter().all(|&k| dot(v, self.vec(k)) < c.0) {
                kept.push(c.1);
            } else {
                skipped.push(c.1);
            }
        }
        for s in skipped {
            if kept.len() >= max {
                break;
            }
            kept.push(s);
        }
        kept
    }

    fn greedy(&self, q: &[f32], mut ep: u32, l: usize) -> u32 {
        let mut best = dot(q, self.vec(ep));
        loop {
            let mut moved = false;
            for &m in &self.links[ep as usize][l] {
                let s = dot(q, self.vec(m));
                if s > best {
                    best = s;
                    ep = m;
                    moved = true;
                }
            }
            if !moved {
                return ep;
            }
        }
    }

    /// Best-first search on one level; results most similar first. `accept` decides which
    /// nodes may be results (all are still walked through).
    fn search_level(&self, q: &[f32], eps: &[u32], ef: usize, l: usize, accept: &dyn Fn(u32) -> bool) -> Vec<Cand> {
        let mut visited: HashSet<u32> = eps.iter().copied().collect();
        let mut frontier: BinaryHeap<Cand> = BinaryHeap::new();
        let mut results: BinaryHeap<std::cmp::Reverse<Cand>> = BinaryHeap::new();
        for &e in eps {
            let c = Cand(dot(q, self.vec(e)), e);
            frontier.push(c);
            if accept(e) {
                results.push(std::cmp::Reverse(c));
            }
        }
        while let Some(c) = frontier.pop() {
            if results.len() >= ef && results.peek().is_some_and(|w| c.0 < w.0 .0) {
                break;
            }
            for &m in self.links[c.1 as usize].get(l).map_or(&[][..], |v| v.as_slice()) {
                if !visited.insert(m) {
                    continue;
                }
                let s = dot(q, self.vec(m));
                if results.len() < ef || results.peek().is_some_and(|w| s > w.0 .0) {
                    frontier.push(Cand(s, m));
                    if accept(m) {
                        results.push(std::cmp::Reverse(Cand(s, m)));
                        if results.len() > ef {
                            results.pop();
                        }
                    }
                }
            }
        }
        let mut out: Vec<Cand> = results.into_iter().map(|r| r.0).collect();
        out.sort_by(|a, b| b.cmp(a));
        out
    }

    /// The `k` literals most similar to `q` among those `accept`ed: (literal id, similarity).
    pub fn search(&self, q: &[f32], k: usize, accept: &dyn Fn(Id) -> bool) -> Vec<(Id, f32)> {
        let Some(mut ep) = self.entry else { return Vec::new() };
        if q.len() != self.dim || k == 0 {
            return Vec::new();
        }
        let q = normalize(q.to_vec());
        let top = self.links[ep as usize].len() - 1;
        for l in (1..=top).rev() {
            ep = self.greedy(&q, ep, l);
        }
        let ef = (k * 4).max(64);
        let found = self.search_level(&q, &[ep], ef, 0, &|n| accept(self.ids[n as usize]));
        found.into_iter().take(k).map(|c| (self.ids[c.1 as usize], c.0)).collect()
    }

    /// Exact search over every vector (for checking recall, and small indexes).
    pub fn exact(&self, q: &[f32], k: usize, accept: &dyn Fn(Id) -> bool) -> Vec<(Id, f32)> {
        if q.len() != self.dim {
            return Vec::new();
        }
        let q = normalize(q.to_vec());
        let mut all: Vec<Cand> = (0..self.ids.len() as u32).filter(|&n| accept(self.ids[n as usize])).map(|n| Cand(dot(&q, self.vec(n)), n)).collect();
        all.sort_by(|a, b| b.cmp(a));
        all.into_iter().take(k).map(|c| (self.ids[c.1 as usize], c.0)).collect()
    }

    // ------------------------------------------------------------ persistence

    /// Write the graph (temporary file, fsync, rename).
    pub fn save(&mut self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("tmp");
        let mut w = io::BufWriter::with_capacity(1 << 20, fs::File::create(&tmp)?);
        w.write_all(b"GRDVEC01")?;
        for x in [self.dim as u64, self.ids.len() as u64, self.entry.map_or(u64::MAX, |e| e as u64), self.rng] {
            w.write_all(&x.to_le_bytes())?;
        }
        for (n, &id) in self.ids.iter().enumerate() {
            w.write_all(&id.to_le_bytes())?;
            for x in self.vec(n as u32) {
                w.write_all(&x.to_le_bytes())?;
            }
            w.write_all(&(self.links[n].len() as u32).to_le_bytes())?;
            for ls in &self.links[n] {
                w.write_all(&(ls.len() as u32).to_le_bytes())?;
                for m in ls {
                    w.write_all(&m.to_le_bytes())?;
                }
            }
        }
        let f = w.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        self.dirty = false;
        Ok(())
    }

/// Read the graph back from its file (as the optimized builder wrote it).
    pub fn load(path: &Path) -> io::Result<Hnsw> {
        let b = fs::read(path)?;
        let bad = || io::Error::new(io::ErrorKind::InvalidData, format!("corrupt {}", path.display()));
        let mut at = 0usize;
        let mut take = |n: usize| -> io::Result<&[u8]> {
            let s = b.get(at..at + n).ok_or_else(bad)?;
            at += n;
            Ok(s)
        };
        if take(8)? != b"GRDVEC01" {
            return Err(bad());
        }
        let mut u64s = [0u64; 4];
        for x in &mut u64s {
            *x = u64::from_le_bytes(take(8)?.try_into().unwrap());
        }
        let (dim, count, entry, rng) = (u64s[0] as usize, u64s[1] as usize, u64s[2], u64s[3]);
        let mut h = Hnsw { dim, rng, entry: (entry != u64::MAX).then_some(entry as u32), ..Default::default() };
        let u32_ = |s: &[u8]| u32::from_le_bytes(s.try_into().unwrap());
        for n in 0..count {
            let id = u32_(take(4)?);
            for _ in 0..dim {
                h.vecs.push(f32::from_le_bytes(take(4)?.try_into().unwrap()));
            }
            let levels = u32_(take(4)?) as usize;
            let mut ls = Vec::with_capacity(levels);
            for _ in 0..levels {
                let len = u32_(take(4)?) as usize;
                let mut l = Vec::with_capacity(len);
                for _ in 0..len {
                    l.push(u32_(take(4)?));
                }
                ls.push(l);
            }
            h.ids.push(id);
            h.links.push(ls);
            h.index_of.insert(id, n as u32);
        }
        Ok(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hnsw_finds_the_nearest_vectors() {
        let mut h = Hnsw::default();
        let mut x = 7u64;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 40) as f32 / (1u64 << 24) as f32 - 0.5
        };
        let vecs: Vec<Vec<f32>> = (0..1200).map(|_| (0..16).map(|_| rnd()).collect()).collect();
        for (i, v) in vecs.iter().enumerate() {
            assert!(h.insert(i as Id + 1, v.clone()));
        }
        assert!(!h.insert(1, vecs[0].clone()), "inserting twice is a no-op");
        assert!(!h.insert(99_999, vec![1.0; 3]), "another length is ignored");
        let mut hits = 0;
        for q in vecs.iter().take(30) {
            let approx: HashSet<Id> = h.search(q, 10, &|_| true).into_iter().map(|r| r.0).collect();
            let exact: Vec<Id> = h.exact(q, 10, &|_| true).into_iter().map(|r| r.0).collect();
            hits += exact.iter().filter(|e| approx.contains(e)).count();
        }
        assert!(hits as f64 / 300.0 > 0.95, "recall@10 {hits}/300");
        // a filter leaves out nodes but the search still walks through them
        let odd = h.search(&vecs[0], 5, &|id| id % 2 == 0);
        assert!(odd.iter().all(|r| r.0 % 2 == 0) && odd.len() == 5);
        let path = std::env::temp_dir().join(format!("graphrd-vec-{}", std::process::id()));
        h.save(&path).unwrap();
        let back = Hnsw::load(&path).unwrap();
        assert_eq!(back.search(&vecs[3], 10, &|_| true), h.search(&vecs[3], 10, &|_| true));
        let _ = fs::remove_file(&path);
        assert_eq!(parse(&Term::typed("[1, 2.5,-3]", VECTOR)), Some(vec![1.0, 2.5, -3.0]));
        assert!(parse(&Term::typed("[1, x]", VECTOR)).is_none());
    }
}
