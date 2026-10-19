//! Term dictionary: RDF term <-> u32 id.
//!
//! On disk it is three files: `terms.dat` (length-prefixed encoded terms,
//! append-only), `terms.idx` (u64 offset per id) and `terms.hash` (an
//! open-addressing table of ids keyed by the term's hash). Terms created
//! since the last checkpoint live in memory and are appended at checkpoint.
//! Hot terms are kept in bounded caches, so memory does not grow with data.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::RwLock;

use crate::lsm::{read_at, write_at};
use crate::rdf::{Literal, RDF_LANGSTRING, Term, XSD_STRING};

/// A term's id in the dictionary (0 is the default graph's reserved id).
pub type Id = u32;

const CACHE_TERMS: usize = 200_000;

/// A term's disk bytes: a kind tag, then its parts.
pub fn encode_term(t: &Term, out: &mut Vec<u8>) {
    match t {
        Term::Iri(i) => {
            out.push(0);
            out.extend_from_slice(i.as_bytes());
        }
        Term::Blank(b) => {
            out.push(1);
            out.extend_from_slice(b.as_bytes());
        }
        Term::Triple(t) => {
            out.push(5);
            for part in t.iter() {
                let mut inner = Vec::new();
                encode_term(part, &mut inner);
                out.extend_from_slice(&(inner.len() as u32).to_le_bytes());
                out.extend_from_slice(&inner);
            }
        }
        Term::Literal(l) => {
            if let Some(lang) = &l.lang {
                out.push(3);
                out.extend_from_slice(&(lang.len() as u16).to_le_bytes());
                out.extend_from_slice(lang.as_bytes());
            } else if l.datatype == XSD_STRING {
                out.push(2);
            } else {
                out.push(4);
                out.extend_from_slice(&(l.datatype.len() as u32).to_le_bytes());
                out.extend_from_slice(l.datatype.as_bytes());
            }
            out.extend_from_slice(l.value.as_bytes());
        }
    }
}

/// A term from its disk bytes; `None` when they are not one.
pub fn decode_term(b: &[u8]) -> Option<Term> {
    let s = |x: &[u8]| String::from_utf8_lossy(x).into_owned();
    Some(match *b.first()? {
        0 => Term::Iri(s(&b[1..])),
        1 => Term::Blank(s(&b[1..])),
        2 => Term::Literal(Literal {
            value: s(&b[1..]),
            datatype: XSD_STRING.into(),
            lang: None,
        }),
        3 => {
            let n = u16::from_le_bytes(b.get(1..3)?.try_into().ok()?) as usize;
            Term::Literal(Literal {
                value: s(b.get(3 + n..)?),
                datatype: RDF_LANGSTRING.into(),
                lang: Some(s(b.get(3..3 + n)?)),
            })
        }
        4 => {
            let n = u32::from_le_bytes(b.get(1..5)?.try_into().ok()?) as usize;
            Term::Literal(Literal {
                value: s(b.get(5 + n..)?),
                datatype: s(b.get(5..5 + n)?),
                lang: None,
            })
        }
        5 => {
            let mut parts = Vec::with_capacity(3);
            let mut i = 1;
            for _ in 0..3 {
                let n = u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?) as usize;
                parts.push(decode_term(b.get(i + 4..i + 4 + n)?)?);
                i += 4 + n;
            }
            let o = parts.pop()?;
            let p = parts.pop()?;
            let s = parts.pop()?;
            Term::triple(s, p, o)
        }
        _ => return None,
    })
}

fn hash_bytes(b: &[u8]) -> u64 {
    // FNV-1a, then a final avalanche so low bits are well mixed for masking.
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51afd7ed558ccd);
    h ^ (h >> 33)
}

struct Lru<K, V> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
    cap: usize,
}

impl<K: std::hash::Hash + Eq + Clone, V: Clone> Lru<K, V> {
    fn new(cap: usize) -> Self {
        Lru {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }
    fn get(&self, k: &K) -> Option<V> {
        self.map.get(k).cloned()
    }
    fn put(&mut self, k: K, v: V) {
        if self.map.insert(k.clone(), v).is_none() {
            self.order.push_back(k);
            while self.map.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
}

struct DiskDict {
    dir: std::path::PathBuf,
    data: File,
    idx: File,
    hash: File,
    hash_cap: u64,
    data_len: u64,
}

/// The term dictionary. Append-only, so a reader working on an older snapshot of the store
/// can share it with the writer: ids it finds in its indexes are always present. The state is
/// behind a short-held lock; decoded terms are cached in sharded LRUs.
pub struct Dict {
    inner: RwLock<DictInner>,
    by_id: Sharded<Id, Term>,
    by_term: Sharded<Term, Id>,
}

struct DictInner {
    disk: Option<DiskDict>,
    /// Ids below `base` are on disk.
    base: Id,
    pending: Vec<Term>,
    pending_map: HashMap<Term, Id>,
}

const SHARDS: usize = 16;

/// FIFO-evicting cache split across independently locked shards. Lookups take a shared lock
/// (they do not reorder anything), so hot terms such as common predicates do not serialize
/// concurrent queries.
struct Sharded<K, V> {
    shards: Vec<RwLock<Lru<K, V>>>,
}

impl<K: std::hash::Hash + Eq + Clone, V: Clone> Sharded<K, V> {
    fn new(cap: usize) -> Self {
        Sharded {
            shards: (0..SHARDS)
                .map(|_| RwLock::new(Lru::new((cap / SHARDS).max(1))))
                .collect(),
        }
    }
    fn shard(&self, k: &K) -> &RwLock<Lru<K, V>> {
        use std::hash::{BuildHasher, Hasher};
        let mut hasher = FIXED.build_hasher();
        k.hash(&mut hasher);
        &self.shards[(hasher.finish() as usize) % SHARDS]
    }
    fn get(&self, k: &K) -> Option<V> {
        self.shard(k)
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(k)
    }
    fn contains(&self, k: &K) -> bool {
        self.shard(k)
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .contains_key(k)
    }
    fn put(&self, k: K, v: V) {
        self.shard(&k)
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .put(k, v)
    }
}

/// FNV-1a: only picks a cache shard, so it needs to be fast, not collision-resistant.
#[derive(Default)]
struct Fnv(u64);

impl std::hash::Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut h = if self.0 == 0 {
            0xcbf29ce484222325
        } else {
            self.0
        };
        for &b in bytes {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
        self.0 = h;
    }
}

static FIXED: std::hash::BuildHasherDefault<Fnv> = std::hash::BuildHasherDefault::new();

impl Dict {
    /// A dictionary that never writes to disk.
    pub fn memory() -> Dict {
        Dict {
            // id 0 is reserved (default graph)
            inner: RwLock::new(DictInner {
                disk: None,
                base: 1,
                pending: Vec::new(),
                pending_map: HashMap::new(),
            }),
            by_id: Sharded::new(SHARDS),
            by_term: Sharded::new(SHARDS),
        }
    }

    /// Open the dictionary in `dir`, trusting only the first `count` ids
    /// (the count recorded by the last checkpoint).
    pub fn open(dir: &Path, count: Id) -> io::Result<Dict> {
        let open = |name: &str| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(dir.join(name))
        };
        let data = open("terms.dat")?;
        let idx = open("terms.idx")?;
        let hash = open("terms.hash")?;
        let count = count.max(1);
        // Drop anything a crashed checkpoint appended past `count`.
        idx.set_len(count as u64 * 8)?;
        let data_len = if count > 1 {
            let mut b = [0u8; 8];
            read_at(&idx, &mut b, (count as u64 - 1) * 8)?;
            let off = u64::from_le_bytes(b);
            let mut l = [0u8; 4];
            read_at(&data, &mut l, off)?;
            off + 4 + u32::from_le_bytes(l) as u64
        } else {
            0
        };
        data.set_len(data_len)?;
        let mut hash_cap = hash.metadata()?.len() / 4;
        if hash_cap == 0 {
            hash_cap = 1024;
            hash.set_len(hash_cap * 4)?;
        }
        Ok(Dict {
            inner: RwLock::new(DictInner {
                disk: Some(DiskDict {
                    dir: dir.to_path_buf(),
                    data,
                    idx,
                    hash,
                    hash_cap,
                    data_len,
                }),
                base: count,
                pending: Vec::new(),
                pending_map: HashMap::new(),
            }),
            by_id: Sharded::new(CACHE_TERMS),
            by_term: Sharded::new(CACHE_TERMS),
        })
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, DictInner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, DictInner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Number of ids handed out (including the reserved id 0).
    pub fn count(&self) -> Id {
        let d = self.read();
        d.base + d.pending.len() as Id
    }

    fn read_disk(d: &DictInner, id: Id) -> Option<Term> {
        let disk = d.disk.as_ref()?;
        // this term's offset and the next one's give its extent: two reads instead of three
        let mut b = [0u8; 16];
        let next_known = id + 1 < d.base;
        read_at(
            &disk.idx,
            &mut b[..if next_known { 16 } else { 8 }],
            id as u64 * 8,
        )
        .ok()?;
        let off = u64::from_le_bytes(b[..8].try_into().unwrap());
        let end = if next_known {
            u64::from_le_bytes(b[8..].try_into().unwrap())
        } else {
            disk.data_len
        };
        let mut buf = vec![0u8; end.checked_sub(off)? as usize];
        read_at(&disk.data, &mut buf, off).ok()?;
        let len = u32::from_le_bytes(buf.get(..4)?.try_into().ok()?) as usize;
        decode_term(buf.get(4..4 + len)?)
    }

    fn disk_lookup(&self, inner: &DictInner, t: &Term) -> Option<Id> {
        let d = inner.disk.as_ref()?;
        let mut enc = Vec::new();
        encode_term(t, &mut enc);
        let mask = d.hash_cap - 1;
        let mut slot = hash_bytes(&enc) & mask;
        for _ in 0..d.hash_cap {
            let mut b = [0u8; 4];
            read_at(&d.hash, &mut b, slot * 4).ok()?;
            let id = u32::from_le_bytes(b);
            if id == 0 {
                return None;
            }
            if id < inner.base && self.term_in(inner, id) == *t {
                return Some(id);
            }
            slot = (slot + 1) & mask;
        }
        None
    }

    fn lookup_in(&self, inner: &DictInner, t: &Term) -> Option<Id> {
        if let Some(&id) = inner.pending_map.get(t) {
            return Some(id);
        }
        inner.disk.as_ref()?;
        if let Some(id) = self.by_term.get(t) {
            return Some(id);
        }
        let id = self.disk_lookup(inner, t)?;
        self.by_term.put(t.clone(), id);
        Some(id)
    }

    /// The id of a term the dictionary has; `None` when it does not.
    pub fn lookup(&self, t: &Term) -> Option<Id> {
        self.lookup_in(&self.read(), t)
    }

    /// The id of a term, adding it when the dictionary lacks it.
    pub fn intern(&self, t: &Term) -> Id {
        if let Some(id) = self.lookup(t) {
            return id;
        }
        let mut d = self.write();
        // another writer may have added it in between
        if let Some(id) = self.lookup_in(&d, t) {
            return id;
        }
        let id = d.base + d.pending.len() as Id;
        d.pending.push(t.clone());
        d.pending_map.insert(t.clone(), id);
        id
    }

    /// Intern many terms under one lock (bulk loading). The on-disk hash table is read into
    /// memory once instead of probed with a read per term, and is skipped while the
    /// dictionary has nothing on disk.
    pub fn intern_all<'t>(&self, terms: impl Iterator<Item = &'t Term>) -> io::Result<Vec<Id>> {
        let mut guard = self.write();
        let inner = &mut *guard;
        let table: Option<(Vec<u32>, u64)> = match &inner.disk {
            Some(d) if inner.base > 1 => {
                let mut bytes = vec![0u8; (d.hash_cap * 4) as usize];
                read_at(&d.hash, &mut bytes, 0)?;
                Some((
                    bytes
                        .chunks_exact(4)
                        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                        .collect(),
                    d.hash_cap - 1,
                ))
            }
            _ => None,
        };
        let mut out = Vec::new();
        let mut enc = Vec::new();
        for t in terms {
            if let Some(&id) = inner.pending_map.get(t) {
                out.push(id);
                continue;
            }
            let mut found = None;
            if let Some((table, mask)) = &table {
                enc.clear();
                encode_term(t, &mut enc);
                let mut slot = hash_bytes(&enc) & mask;
                loop {
                    let id = table[slot as usize];
                    if id == 0 {
                        break;
                    }
                    if id < inner.base && self.term_in(inner, id) == *t {
                        found = Some(id);
                        break;
                    }
                    slot = (slot + 1) & mask;
                }
            }
            let id = found.unwrap_or_else(|| {
                let id = inner.base + inner.pending.len() as Id;
                inner.pending.push(t.clone());
                inner.pending_map.insert(t.clone(), id);
                id
            });
            out.push(id);
        }
        Ok(out)
    }

    fn term_in(&self, d: &DictInner, id: Id) -> Term {
        if id >= d.base {
            return d
                .pending
                .get((id - d.base) as usize)
                .cloned()
                .unwrap_or_else(|| Term::Iri(String::new()));
        }
        if id == 0 {
            return Term::Iri(String::new());
        }
        if let Some(t) = self.by_id.get(&id) {
            return t;
        }
        let t = Dict::read_disk(d, id).unwrap_or_else(|| Term::Iri(String::new()));
        self.by_id.put(id, t.clone());
        t
    }

    /// Read the terms of `ids` that are on disk and not cached into `out`, for a loop that reads
    /// them one by one; they bypass the cache, so a large scan does not evict the hot terms.
    /// They are read in a few large pieces: the ids sorted, near ones grouped, each group's
    /// offsets and bytes taken with one read each (rather than two reads a term).
    pub fn read_many(&self, ids: &mut Vec<Id>, out: &mut HashMap<Id, Term>) {
        const GAP: Id = 64; // ids further apart start a new group
        const SPAN: Id = 16_384; // ids per group at most
        const BYTES: u64 = 4 << 20; // a group whose bytes are larger reads its terms singly
        let d = self.read();
        let Some(disk) = d.disk.as_ref() else { return };
        ids.retain(|&id| id != 0 && id < d.base && !self.by_id.contains(&id));
        if ids.is_empty() {
            return;
        }
        ids.sort_unstable();
        ids.dedup();
        let mut i = 0;
        while i < ids.len() {
            let first = ids[i];
            let mut j = i + 1;
            while j < ids.len() && ids[j] - ids[j - 1] <= GAP && ids[j] - first < SPAN {
                j += 1;
            }
            let last = ids[j - 1];
            let group = &ids[i..j];
            i = j;
            // offsets of first..=last, plus the next one's for the end of `last`
            let n = (last - first + 1) as usize + (last + 1 < d.base) as usize;
            let mut idx = vec![0u8; n * 8];
            if read_at(&disk.idx, &mut idx, first as u64 * 8).is_err() {
                continue;
            }
            let off = |k: usize| u64::from_le_bytes(idx[k * 8..k * 8 + 8].try_into().unwrap());
            let start = off(0);
            let end = if last + 1 < d.base {
                off(n - 1)
            } else {
                disk.data_len
            };
            if end < start || end - start > BYTES || group.len() < 2 {
                for &id in group {
                    if let Some(t) = Dict::read_disk(&d, id) {
                        out.insert(id, t);
                    }
                }
                continue;
            }
            let mut data = vec![0u8; (end - start) as usize];
            if read_at(&disk.data, &mut data, start).is_err() {
                continue;
            }
            for &id in group {
                let at = (off((id - first) as usize) - start) as usize;
                let Some(len) = data
                    .get(at..at + 4)
                    .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
                else {
                    continue;
                };
                if let Some(t) = data.get(at + 4..at + 4 + len).and_then(decode_term) {
                    out.insert(id, t);
                }
            }
        }
    }

    /// The term an id stands for (the cache serves the hot ones).
    pub fn term(&self, id: Id) -> Term {
        if id != 0 {
            // disk ids are immutable: the cache needs no dictionary lock
            if let Some(t) = self.by_id.get(&id) {
                return t;
            }
        }
        self.term_in(&self.read(), id)
    }

    /// Persist pending terms. Returns the new durable count.
    pub fn checkpoint(&self) -> io::Result<Id> {
        let mut guard = self.write();
        let inner = &mut *guard;
        let Some(d) = &mut inner.disk else {
            return Ok(inner.base + inner.pending.len() as Id);
        };
        if inner.pending.is_empty() {
            return Ok(inner.base);
        }
        let base = inner.base;
        let total = base as u64 + inner.pending.len() as u64;
        if total * 2 > d.hash_cap {
            // Grow the hash table and reinsert every id.
            let mut cap = d.hash_cap;
            while total * 2 > cap {
                cap *= 2;
            }
            let mut table = vec![0u32; cap as usize];
            let ins = |table: &mut Vec<u32>, enc: &[u8], id: Id| {
                let mut slot = hash_bytes(enc) & (cap - 1);
                while table[slot as usize] != 0 {
                    slot = (slot + 1) & (cap - 1);
                }
                table[slot as usize] = id;
            };
            // existing terms, read sequentially in large pieces
            let mut off = 0u64;
            let mut id = 1;
            let mut buf = Vec::new();
            while id < base {
                let want = (d.data_len - off).min(64 << 20) as usize;
                buf.resize(want, 0);
                read_at(&d.data, &mut buf, off)?;
                let mut pos = 0;
                while id < base && pos + 4 <= buf.len() {
                    let n = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
                    if pos + 4 + n > buf.len() {
                        break; // continues in the next piece
                    }
                    ins(&mut table, &buf[pos + 4..pos + 4 + n], id);
                    pos += 4 + n;
                    id += 1;
                }
                if pos == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "terms.dat is shorter than its index",
                    ));
                }
                off += pos as u64;
            }
            let mut enc = Vec::new();
            for (i, t) in inner.pending.iter().enumerate() {
                enc.clear();
                encode_term(t, &mut enc);
                ins(&mut table, &enc, base + i as Id);
            }
            // The grown table is written beside the old one and renamed over it, so a crash
            // leaves either the complete old table or the complete new one.
            let bytes: Vec<u8> = table.iter().flat_map(|v| v.to_le_bytes()).collect();
            let tmp = d.dir.join("terms.hash.tmp");
            {
                let mut f = File::create(&tmp)?;
                f.write_all(&bytes)?;
                f.sync_all()?;
            }
            std::fs::rename(&tmp, d.dir.join("terms.hash"))?;
            d.hash = OpenOptions::new()
                .read(true)
                .write(true)
                .open(d.dir.join("terms.hash"))?;
            d.hash_cap = cap;
        } else if inner.pending.len() as u64 * 64 > d.hash_cap {
            // many new terms: update the table in memory and write it back whole
            let mask = d.hash_cap - 1;
            let limit = base + inner.pending.len() as Id;
            let mut bytes = vec![0u8; (d.hash_cap * 4) as usize];
            read_at(&d.hash, &mut bytes, 0)?;
            let mut enc = Vec::new();
            for (i, t) in inner.pending.iter().enumerate() {
                enc.clear();
                encode_term(t, &mut enc);
                let mut slot = hash_bytes(&enc) & mask;
                loop {
                    let at = slot as usize * 4;
                    let cur = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
                    if cur == 0 || cur >= limit {
                        bytes[at..at + 4].copy_from_slice(&(base + i as Id).to_le_bytes());
                        break;
                    }
                    slot = (slot + 1) & mask;
                }
            }
            write_at(&d.hash, &bytes, 0)?;
        } else {
            let mask = d.hash_cap - 1;
            let limit = base + inner.pending.len() as Id;
            let mut enc = Vec::new();
            for (i, t) in inner.pending.iter().enumerate() {
                enc.clear();
                encode_term(t, &mut enc);
                let mut slot = hash_bytes(&enc) & mask;
                loop {
                    let mut b = [0u8; 4];
                    read_at(&d.hash, &mut b, slot * 4)?;
                    let cur = u32::from_le_bytes(b);
                    // ids beyond everything being assigned now are crash leftovers
                    if cur == 0 || cur >= limit {
                        write_at(&d.hash, &(base + i as Id).to_le_bytes(), slot * 4)?;
                        break;
                    }
                    slot = (slot + 1) & mask;
                }
            }
        }
        // append term data and offsets
        let mut data_buf = Vec::new();
        let mut idx_buf = Vec::new();
        let mut enc = Vec::new();
        for t in &inner.pending {
            enc.clear();
            encode_term(t, &mut enc);
            idx_buf.extend_from_slice(&(d.data_len + data_buf.len() as u64).to_le_bytes());
            data_buf.extend_from_slice(&(enc.len() as u32).to_le_bytes());
            data_buf.extend_from_slice(&enc);
        }
        write_at(&d.data, &data_buf, d.data_len)?;
        let idx_off = d.idx.metadata()?.len();
        write_at(&d.idx, &idx_buf, idx_off)?;
        d.data.sync_all()?;
        d.idx.sync_all()?;
        d.hash.sync_all()?;
        d.data_len += data_buf.len() as u64;
        // pending terms become disk terms; keep them warm in the caches (only the newest
        // ones after a bulk load: the caches would evict the rest anyway)
        let skip = inner.pending.len().saturating_sub(CACHE_TERMS / 2);
        for (i, t) in inner.pending.drain(..).enumerate().skip(skip) {
            let id = base + i as Id;
            self.by_term.put(t.clone(), id);
            self.by_id.put(id, t);
        }
        inner.pending_map.clear();
        inner.base = total as Id;
        Ok(inner.base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_roundtrip() {
        let nested = Term::triple(
            Term::iri("http://s"),
            Term::iri("http://p"),
            Term::triple(
                Term::iri("http://a"),
                Term::iri("http://b"),
                Term::lang("c", "en"),
            ),
        );
        for t in [
            Term::iri("http://a"),
            Term::Blank("b1".into()),
            Term::string("x"),
            Term::lang("hi", "en"),
            Term::integer(5),
            nested,
        ] {
            let mut e = Vec::new();
            encode_term(&t, &mut e);
            assert_eq!(decode_term(&e).unwrap(), t);
        }
    }

    #[test]
    fn prefetch_reads_groups() {
        let dir = std::env::temp_dir().join(format!("graphrd-dict-pf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = Dict::open(&dir, 0).unwrap();
        let terms: Vec<Term> = (0..40_000)
            .map(|i| {
                if i % 3 == 0 {
                    Term::lang(format!("name {i}"), "en")
                } else {
                    Term::iri(format!("http://ex.org/{i}"))
                }
            })
            .collect();
        let ids: Vec<Id> = terms.iter().map(|t| d.intern(t)).collect();
        let count = d.checkpoint().unwrap();
        drop(d);
        // a cold dictionary: near ids, far ones, the last id, duplicates
        let d = Dict::open(&dir, count).unwrap();
        let mut want: Vec<Id> = ids
            .iter()
            .step_by(7)
            .copied()
            .chain([ids[39_999], ids[5], ids[5], ids[20_000]])
            .collect();
        let _ = d.term(ids[14]); // a cached term is left to the cache
        let mut out = HashMap::new();
        d.read_many(&mut want, &mut out);
        assert_eq!(out.len(), 40_000 / 7 + 1 + 3 - 1);
        for (id, t) in &out {
            assert_eq!(t, &terms[ids.iter().position(|x| x == id).unwrap()]);
        }
        assert!(
            !out.contains_key(&ids[14])
                && out.contains_key(&ids[39_999])
                && !d.by_id.contains(&ids[7])
        );
        for (k, &id) in ids.iter().enumerate() {
            assert_eq!(d.term(id), terms[k]);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn disk_dictionary() {
        let dir = std::env::temp_dir().join(format!("graphrd-dict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = Dict::open(&dir, 0).unwrap();
        let ids: Vec<Id> = (0..3000)
            .map(|i| d.intern(&Term::iri(format!("http://ex.org/{i}"))))
            .collect();
        assert_eq!(ids[0], 1);
        let count = d.checkpoint().unwrap();
        assert_eq!(count, 3001);
        let extra = d.intern(&Term::string("pending"));
        assert_eq!(extra, 3001);
        drop(d);
        // reopen with the durable count: the uncheckpointed term is gone
        let d = Dict::open(&dir, count).unwrap();
        assert_eq!(d.lookup(&Term::iri("http://ex.org/1234")), Some(1235));
        assert_eq!(d.term(42), Term::iri("http://ex.org/41"));
        assert_eq!(d.lookup(&Term::string("pending")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
