//! Log-structured merge index over fixed-size `[u32; 4]` keys.
//!
//! Writes go to an in-memory memtable (key -> present/tombstone). A flush
//! writes the memtable as an immutable sorted run file; runs are merged in a
//! size-tiered fashion so their number stays logarithmic. Reads merge the
//! memtable and all runs, newest first. Run files carry a sparse block index
//! in a footer, and blocks are read on demand through a shared LRU cache, so
//! memory use is bounded by the memtable and the cache, not the data size.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// An index key: four dictionary ids in one of the index's orders.
pub type Key = [u32; 4];

const MAGIC: &[u8; 8] = b"GRDRUN01";
const ENTRY: usize = 17; // 16-byte key + 1 flag byte
const BLOCK_ENTRIES: usize = 256;
const HEADER: u64 = 16; // magic + entry count

// ------------------------------------------------------------------ file io

#[cfg(unix)]
/// Read exactly `buf` bytes at `off`.
pub fn read_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, off)
}

#[cfg(windows)]
pub fn read_at(f: &File, buf: &mut [u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = f.seek_read(&mut buf[done..], off)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
        }
        done += n;
        off += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
/// Write all of `buf` at `off`.
pub fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, off)
}

#[cfg(windows)]
pub fn write_at(f: &File, buf: &[u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = f.seek_write(&buf[done..], off)?;
        done += n;
        off += n as u64;
    }
    Ok(())
}

fn encode(k: &Key, present: bool, out: &mut Vec<u8>) {
    for v in k {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.push(present as u8);
}

fn decode(b: &[u8]) -> (Key, bool) {
    let k = [
        u32::from_le_bytes(b[0..4].try_into().unwrap()),
        u32::from_le_bytes(b[4..8].try_into().unwrap()),
        u32::from_le_bytes(b[8..12].try_into().unwrap()),
        u32::from_le_bytes(b[12..16].try_into().unwrap()),
    ];
    (k, b[16] != 0)
}

// ------------------------------------------------------------------ block cache

type Block = Arc<Vec<(Key, bool)>>;

/// Cache of decoded run blocks, shared by all indexes of a store: a block read again since it
/// was last considered gets a second chance before eviction (CLOCK, close to LRU, while a hit
/// only sets a flag under the shared lock). Sharded so concurrent readers rarely contend.
pub struct BlockCache {
    shards: Vec<RwLock<CacheInner>>,
}

const CACHE_SHARDS: usize = 16;

struct CacheInner {
    /// Each block with whether it was read since eviction last passed it.
    map: HashMap<(u64, u32), (Block, std::sync::atomic::AtomicBool)>,
    order: VecDeque<(u64, u32)>,
    capacity: usize,
}

impl BlockCache {
    /// A block cache over `capacity_blocks` decoded blocks, sharded for concurrency.
    pub fn new(capacity_blocks: usize) -> Arc<BlockCache> {
        let per = (capacity_blocks / CACHE_SHARDS).max(4);
        Arc::new(BlockCache {
            shards: (0..CACHE_SHARDS)
                .map(|_| {
                    RwLock::new(CacheInner {
                        map: HashMap::new(),
                        order: VecDeque::new(),
                        capacity: per,
                    })
                })
                .collect(),
        })
    }

    fn shard(&self, key: (u64, u32)) -> &RwLock<CacheInner> {
        let h = key.0.wrapping_mul(0x9E3779B97F4A7C15)
            ^ (key.1 as u64).wrapping_mul(0xC2B2AE3D27D4EB4F);
        &self.shards[(h >> 59) as usize % CACHE_SHARDS]
    }

    fn get_or_load(&self, run: &RunFile, block: u32) -> io::Result<Block> {
        let key = (run.uid, block);
        let shard = self.shard(key);
        if let Some((b, used)) = shard
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .get(&key)
        {
            used.store(true, std::sync::atomic::Ordering::Relaxed);
            crate::metrics::CACHE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(b.clone());
        }
        crate::metrics::CACHE_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let loaded = Arc::new(run.load_block(block)?);
        let mut c = shard.write().unwrap_or_else(|e| e.into_inner());
        if c.map
            .insert(key, (loaded.clone(), Default::default()))
            .is_none()
        {
            c.order.push_back(key);
        }
        while c.map.len() > c.capacity {
            let Some(old) = c.order.pop_front() else {
                break;
            };
            let used = c
                .map
                .get(&old)
                .is_some_and(|(_, u)| u.swap(false, std::sync::atomic::Ordering::Relaxed));
            if used {
                c.order.push_back(old); // read since: a second chance
            } else {
                c.map.remove(&old);
            }
        }
        Ok(loaded)
    }

    fn forget(&self, uid: u64) {
        for shard in &self.shards {
            let mut c = shard.write().unwrap_or_else(|e| e.into_inner());
            c.map.retain(|(u, _), _| *u != uid);
            c.order.retain(|(u, _)| *u != uid);
        }
    }
}

// ------------------------------------------------------------------ run files

static RUN_UID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// One sorted, immutable run file on disk.
pub struct RunFile {
    uid: u64,
    /// Where the run lives (its name is what the manifest records).
    pub path: PathBuf,
    file: File,
    /// How many keys the run holds.
    pub count: u64,
    /// First key of every block.
    sparse: Vec<Key>,
}

impl RunFile {
    /// Write a sorted run from an ascending iterator of (key, present).
    fn write(path: &Path, entries: impl Iterator<Item = (Key, bool)>) -> io::Result<RunFile> {
        let tmp = path.with_extension("tmp");
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        w.write_all(MAGIC)?;
        w.write_all(&0u64.to_le_bytes())?; // count, patched below
        let mut count = 0u64;
        let mut sparse = Vec::new();
        let mut buf = Vec::with_capacity(ENTRY);
        for (k, present) in entries {
            if count % BLOCK_ENTRIES as u64 == 0 {
                sparse.push(k);
            }
            buf.clear();
            encode(&k, present, &mut buf);
            w.write_all(&buf)?;
            count += 1;
        }
        // footer: sparse index, then its entry count and magic
        for k in &sparse {
            for v in k {
                w.write_all(&v.to_le_bytes())?;
            }
        }
        w.write_all(&(sparse.len() as u64).to_le_bytes())?;
        w.write_all(MAGIC)?;
        let file = w.into_inner().map_err(|e| e.into_error())?;
        write_at(&file, &count.to_le_bytes(), 8)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        RunFile::open(path)
    }

    fn open(path: &Path) -> io::Result<RunFile> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        let bad = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("corrupt run file {}", path.display()),
            )
        };
        if len < HEADER + 16 {
            return Err(bad());
        }
        let mut head = [0u8; 16];
        read_at(&file, &mut head, 0)?;
        if &head[..8] != MAGIC {
            return Err(bad());
        }
        let count = u64::from_le_bytes(head[8..16].try_into().unwrap());
        let mut tail = [0u8; 16];
        read_at(&file, &mut tail, len - 16)?;
        if &tail[8..] != MAGIC {
            return Err(bad());
        }
        let nsparse = u64::from_le_bytes(tail[..8].try_into().unwrap()) as usize;
        let sparse_off = HEADER + count * ENTRY as u64;
        if sparse_off + nsparse as u64 * 16 + 16 != len {
            return Err(bad());
        }
        let mut raw = vec![0u8; nsparse * 16];
        read_at(&file, &mut raw, sparse_off)?;
        let sparse = raw
            .chunks_exact(16)
            .map(|c| {
                [
                    u32::from_le_bytes(c[0..4].try_into().unwrap()),
                    u32::from_le_bytes(c[4..8].try_into().unwrap()),
                    u32::from_le_bytes(c[8..12].try_into().unwrap()),
                    u32::from_le_bytes(c[12..16].try_into().unwrap()),
                ]
            })
            .collect();
        Ok(RunFile {
            uid: RUN_UID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            path: path.to_path_buf(),
            file,
            count,
            sparse,
        })
    }

    fn blocks(&self) -> u32 {
        self.sparse.len() as u32
    }

    fn load_block(&self, block: u32) -> io::Result<Vec<(Key, bool)>> {
        let first = block as u64 * BLOCK_ENTRIES as u64;
        let n = (self.count - first).min(BLOCK_ENTRIES as u64) as usize;
        let mut raw = vec![0u8; n * ENTRY];
        read_at(&self.file, &mut raw, HEADER + first * ENTRY as u64)?;
        Ok(raw.chunks_exact(ENTRY).map(decode).collect())
    }

    /// Block that may contain the first key >= `lo`.
    fn first_block(&self, lo: &Key) -> u32 {
        let idx = self.sparse.partition_point(|k| k <= lo);
        idx.saturating_sub(1) as u32
    }
}

/// Ascending iterator over one run between two bounds.
struct RunIter<'a> {
    run: &'a RunFile,
    cache: &'a BlockCache,
    block: u32,
    data: Block,
    pos: usize,
    lo: Key,
    hi: Key,
    done: bool,
}

impl<'a> RunIter<'a> {
    fn new(run: &'a RunFile, cache: &'a BlockCache, lo: Key, hi: Key) -> Self {
        let block = run.first_block(&lo);
        RunIter {
            run,
            cache,
            block,
            data: Arc::new(Vec::new()),
            pos: 0,
            lo,
            hi,
            done: run.count == 0,
        }
    }
}

impl Iterator for RunIter<'_> {
    type Item = (Key, bool);
    fn next(&mut self) -> Option<(Key, bool)> {
        while !self.done {
            if self.pos >= self.data.len() {
                if !self.data.is_empty() {
                    self.block += 1;
                }
                if self.block >= self.run.blocks() {
                    self.done = true;
                    return None;
                }
                match self.cache.get_or_load(self.run, self.block) {
                    Ok(b) => self.data = b,
                    Err(e) => {
                        eprintln!("graphrd: read error in {}: {e}", self.run.path.display());
                        self.done = true;
                        return None;
                    }
                }
                // position at the first key >= lo inside the first block
                self.pos = self.data.partition_point(|(k, _)| *k < self.lo);
                if self.data.is_empty() {
                    self.done = true;
                    return None;
                }
                continue;
            }
            let (k, p) = self.data[self.pos];
            self.pos += 1;
            if k > self.hi {
                self.done = true;
                return None;
            }
            return Some((k, p));
        }
        None
    }
}

// ------------------------------------------------------------------ index

/// The in-memory part of an index: a mutable head that only the writer touches, and frozen
/// sorted levels (newest first) that snapshots share. `freeze` turns the head into a level;
/// levels of similar size are merged so their number stays logarithmic.
#[derive(Default)]
struct Mem {
    head: BTreeMap<Key, bool>,
    levels: Vec<Arc<Vec<(Key, bool)>>>,
}

/// A log-structured merge index: a memtable frozen into levels, then run files.
pub struct Index {
    dir: Option<PathBuf>,
    name: String,
    mem: Mem,
    /// Oldest first.
    runs: Vec<Arc<RunFile>>,
    cache: Arc<BlockCache>,
    next_run: u64,
}

fn level_range(v: &[(Key, bool)], lo: Key, hi: Key) -> &[(Key, bool)] {
    let a = v.partition_point(|(k, _)| *k < lo);
    let b = v.partition_point(|(k, _)| *k <= hi);
    &v[a..b.max(a)]
}

impl Index {
    /// An index that never reaches the disk (an in-memory repository).
    pub fn memory(name: &str, cache: Arc<BlockCache>) -> Index {
        Index {
            dir: None,
            name: name.into(),
            mem: Mem::default(),
            runs: Vec::new(),
            cache,
            next_run: 1,
        }
    }

    /// Open with the run files listed in the manifest (oldest first).
    pub fn open(
        dir: &Path,
        name: &str,
        runs: &[String],
        cache: Arc<BlockCache>,
    ) -> io::Result<Index> {
        let mut opened = Vec::new();
        let mut next_run = 1;
        for r in runs {
            opened.push(Arc::new(RunFile::open(&dir.join(r))?));
            if let Some(n) = r
                .rsplit('-')
                .next()
                .and_then(|s| s.trim_end_matches(".run").parse::<u64>().ok())
            {
                next_run = next_run.max(n + 1);
            }
        }
        Ok(Index {
            dir: Some(dir.to_path_buf()),
            name: name.into(),
            mem: Mem::default(),
            runs: opened,
            cache,
            next_run,
        })
    }

    /// A copy sharing this index's data that takes writes of its own in memory, leaving `self`
    /// as it is (it never writes files). For a past state derived from a snapshot.
    pub fn fork(&self) -> Index {
        let mut levels = self.mem.levels.clone();
        if !self.mem.head.is_empty() {
            levels.insert(
                0,
                Arc::new(self.mem.head.iter().map(|(k, v)| (*k, *v)).collect()),
            );
        }
        Index {
            dir: None,
            name: self.name.clone(),
            mem: Mem {
                head: BTreeMap::new(),
                levels,
            },
            runs: self.runs.clone(),
            cache: self.cache.clone(),
            next_run: self.next_run,
        }
    }

    /// A read-only view of the index as it is now, sharing all data: the head is frozen first.
    /// Later writes to `self` do not show in the snapshot.
    pub fn snapshot(&mut self) -> Index {
        self.freeze();
        Index {
            dir: self.dir.clone(),
            name: self.name.clone(),
            mem: Mem {
                head: BTreeMap::new(),
                levels: self.mem.levels.clone(),
            },
            runs: self.runs.clone(),
            cache: self.cache.clone(),
            next_run: self.next_run,
        }
    }

    /// Turn the head into the newest frozen level, merging levels of similar size.
    pub fn freeze(&mut self) {
        if self.mem.head.is_empty() {
            return;
        }
        let head: Vec<(Key, bool)> = std::mem::take(&mut self.mem.head).into_iter().collect();
        self.mem.levels.insert(0, Arc::new(head));
        while self.mem.levels.len() >= 2
            && self.mem.levels[0].len().saturating_mul(4) >= self.mem.levels[1].len()
        {
            let newer = self.mem.levels.remove(0);
            let older = self.mem.levels.remove(0);
            // tombstones shadow nothing below the last level when there are no runs
            let bottom = self.runs.is_empty() && self.mem.levels.is_empty();
            let a: Box<dyn Iterator<Item = (Key, bool)>> = Box::new(newer.iter().copied());
            let b: Box<dyn Iterator<Item = (Key, bool)>> = Box::new(older.iter().copied());
            let merged: Vec<(Key, bool)> = MergeIter::new(vec![a, b])
                .filter(|(_, p)| !bottom || *p)
                .collect();
            self.mem.levels.insert(0, Arc::new(merged));
        }
    }

    /// About how many entries lie in [lo, hi]: exact for the memtable levels, to the block
    /// (256 entries) for runs, read from the sparse index only, so it costs no disk reads.
    /// Deletion markers count too. For choosing a join order.
    pub fn estimate(&self, lo: Key, hi: Key) -> usize {
        let mut n = self.mem.head.range(lo..=hi).take(100_000).count();
        for l in &self.mem.levels {
            n += level_range(l, lo, hi).len();
        }
        for r in &self.runs {
            let first = r.sparse.partition_point(|k| *k < lo).saturating_sub(1);
            let last = r.sparse.partition_point(|k| *k <= hi);
            if last > first {
                n += ((last - first) * BLOCK_ENTRIES).min(r.count as usize);
            }
        }
        n
    }

    /// Paths of the live run files.
    pub fn run_paths(&self) -> Vec<PathBuf> {
        self.runs.iter().map(|r| r.path.clone()).collect()
    }

    /// The run files' names, oldest first.
    pub fn run_names(&self) -> Vec<String> {
        self.runs
            .iter()
            .map(|r| r.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// How many keys the memtables hold.
    pub fn mem_len(&self) -> usize {
        self.mem.head.len() + self.mem.levels.iter().map(|l| l.len()).sum::<usize>()
    }

    /// Add a key.
    pub fn put(&mut self, k: Key) {
        self.mem.head.insert(k, true);
    }

    /// Remove a key, or mark it deleted when a run may hold it.
    pub fn delete(&mut self, k: Key) {
        if self.runs.is_empty() && self.mem.levels.is_empty() {
            self.mem.head.remove(&k);
        } else {
            self.mem.head.insert(k, false);
        }
    }

    /// Whether the key is present (deletions included in the search).
    pub fn contains(&self, k: &Key) -> bool {
        self.range(*k, *k).next().is_some()
    }

    /// All in-memory versions in [lo, hi], newest source first.
    fn mem_sources(&self, lo: Key, hi: Key) -> Vec<Box<dyn Iterator<Item = (Key, bool)> + '_>> {
        let mut sources: Vec<Box<dyn Iterator<Item = (Key, bool)> + '_>> =
            Vec::with_capacity(1 + self.mem.levels.len() + self.runs.len());
        if !self.mem.head.is_empty() {
            sources.push(Box::new(
                self.mem
                    .head
                    .range((Bound::Included(lo), Bound::Included(hi)))
                    .map(|(k, p)| (*k, *p)),
            ));
        }
        for l in &self.mem.levels {
            let part = level_range(l, lo, hi);
            if !part.is_empty() {
                sources.push(Box::new(part.iter().copied()));
            }
        }
        sources
    }

    /// Present keys in [lo, hi], ascending, newest version wins.
    pub fn range(&self, lo: Key, hi: Key) -> Box<dyn Iterator<Item = Key> + '_> {
        let mut sources = self.mem_sources(lo, hi);
        for run in self.runs.iter().rev() {
            sources.push(Box::new(RunIter::new(run, &self.cache, lo, hi)));
        }
        match sources.len() {
            0 => Box::new(std::iter::empty()),
            1 => Box::new(sources.pop().unwrap().filter(|(_, p)| *p).map(|(k, _)| k)),
            _ => Box::new(MergeIter::new(sources).filter(|(_, p)| *p).map(|(k, _)| k)),
        }
    }

    /// Write the memtable as a new run and merge similar-sized runs.
    pub fn flush(&mut self) -> io::Result<()> {
        let Some(dir) = self.dir.clone() else {
            return Ok(());
        };
        if self.mem_len() == 0 {
            return Ok(());
        }
        let bottom = self.runs.is_empty();
        let path = dir.join(format!("{}-{:08}.run", self.name, self.next_run));
        self.next_run += 1;
        // With no older runs, tombstones have nothing left to shadow.
        let run = {
            let sources = self.mem_sources([0; 4], [u32::MAX; 4]);
            RunFile::write(
                &path,
                MergeIter::new(sources).filter(|(_, p)| !bottom || *p),
            )?
        };
        self.mem = Mem::default();
        self.runs.push(Arc::new(run));
        self.merge_similar(&dir)
    }

    /// Merge the newest runs while they are of similar size.
    fn merge_similar(&mut self, dir: &Path) -> io::Result<()> {
        while self.runs.len() >= 2 {
            let n = self.runs.len();
            if self.runs[n - 2].count > self.runs[n - 1].count.saturating_mul(4).max(1 << 16) {
                break;
            }
            self.merge_last_two(dir)?;
        }
        Ok(())
    }

    /// Bulk loading: add ascending, distinct, present keys as a run of their own, written
    /// straight from the slice (the memtable is flushed first, so the run is the newest
    /// source). The run is live once the caller writes the manifest.
    pub fn add_sorted(&mut self, keys: &[Key]) -> io::Result<()> {
        let Some(dir) = self.dir.clone() else {
            for k in keys {
                self.mem.head.insert(*k, true);
            }
            return Ok(());
        };
        self.flush()?;
        if keys.is_empty() {
            return Ok(());
        }
        let path = dir.join(format!("{}-{:08}.run", self.name, self.next_run));
        self.next_run += 1;
        let run = RunFile::write(&path, keys.iter().map(|k| (*k, true)))?;
        self.runs.push(Arc::new(run));
        self.merge_similar(&dir)
    }

    fn merge_last_two(&mut self, dir: &Path) -> io::Result<()> {
        let newer = self.runs.pop().unwrap();
        let older = self.runs.pop().unwrap();
        let bottom = self.runs.is_empty();
        let path = dir.join(format!("{}-{:08}.run", self.name, self.next_run));
        self.next_run += 1;
        let merged = {
            let lo = [0; 4];
            let hi = [u32::MAX; 4];
            let a: Box<dyn Iterator<Item = (Key, bool)>> =
                Box::new(RunIter::new(&newer, &self.cache, lo, hi));
            let b: Box<dyn Iterator<Item = (Key, bool)>> =
                Box::new(RunIter::new(&older, &self.cache, lo, hi));
            RunFile::write(
                &path,
                MergeIter::new(vec![a, b]).filter(|(_, p)| !bottom || *p),
            )?
        };
        self.runs.push(Arc::new(merged));
        // Snapshots may still read the old runs (their files stay open); the files are removed
        // only after the manifest stops naming them: callers sweep orphans.
        for old in [newer, older] {
            self.cache.forget(old.uid);
        }
        Ok(())
    }

    /// Merge every run into one (after a flush): reads touch a single file and tombstones
    /// are gone. The old run files are swept after the next manifest.
    pub fn compact(&mut self) -> io::Result<()> {
        let Some(dir) = self.dir.clone() else {
            return Ok(());
        };
        self.flush()?;
        while self.runs.len() >= 2 {
            self.merge_last_two(&dir)?;
        }
        Ok(())
    }

    /// Drop every entry (the run files are swept after the next manifest).
    pub fn clear(&mut self) {
        self.mem = Mem::default();
        for r in self.runs.drain(..) {
            self.cache.forget(r.uid);
        }
    }

    /// Delete run files in `dir` for this index that are not live.
    pub fn sweep(&self) -> io::Result<()> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let live = self.run_names();
        for e in fs::read_dir(dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            let ours = name.starts_with(&format!("{}-", self.name))
                && (name.ends_with(".run") || name.ends_with(".tmp"));
            if ours && !live.contains(&name) {
                let _ = fs::remove_file(e.path());
            }
        }
        Ok(())
    }
}

/// K-way merge of ascending sources; sources[0] is the newest. Equal keys
/// yield once, taking the newest source's flag.
struct MergeIter<'a> {
    sources: Vec<std::iter::Peekable<Box<dyn Iterator<Item = (Key, bool)> + 'a>>>,
}

impl<'a> MergeIter<'a> {
    fn new(sources: Vec<Box<dyn Iterator<Item = (Key, bool)> + 'a>>) -> Self {
        MergeIter {
            sources: sources.into_iter().map(|s| s.peekable()).collect(),
        }
    }
}

impl Iterator for MergeIter<'_> {
    type Item = (Key, bool);
    fn next(&mut self) -> Option<(Key, bool)> {
        let mut best: Option<(usize, Key)> = None;
        for (i, s) in self.sources.iter_mut().enumerate() {
            if let Some(&(k, _)) = s.peek() {
                if best.is_none_or(|(_, b)| k < b) {
                    best = Some((i, k));
                }
            }
        }
        let (idx, key) = best?;
        let (_, flag) = self.sources[idx].next().unwrap();
        // drop older versions of the same key
        for s in self.sources.iter_mut().skip(idx + 1) {
            while s.peek().is_some_and(|(k, _)| *k == key) {
                s.next();
            }
        }
        Some((key, flag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_merge_and_tombstones() {
        let dir = std::env::temp_dir().join(format!("graphrd-lsm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cache = BlockCache::new(64);
        let mut idx = Index::open(&dir, "t", &[], cache.clone()).unwrap();
        for i in 0..1000u32 {
            idx.put([i % 7, i, 0, 0]);
        }
        idx.flush().unwrap();
        for i in (0..1000u32).step_by(2) {
            idx.delete([i % 7, i, 0, 0]);
        }
        idx.put([9, 9, 9, 9]);
        assert_eq!(idx.range([0; 4], [u32::MAX; 4]).count(), 501);
        idx.flush().unwrap();
        assert_eq!(
            idx.range([3, 0, 0, 0], [3, u32::MAX, u32::MAX, u32::MAX])
                .count(),
            1000 / 7 / 2 + 1
        );
        assert!(idx.contains(&[9, 9, 9, 9]));
        assert!(!idx.contains(&[0, 0, 0, 0]));
        // reopen from the run list
        let names = idx.run_names();
        idx.sweep().unwrap();
        let idx2 = Index::open(&dir, "t", &names, cache).unwrap();
        assert_eq!(idx2.range([0; 4], [u32::MAX; 4]).count(), 501);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_keeps_blocks_in_use() {
        let dir = std::env::temp_dir().join(format!("graphrd-lsm-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cache = BlockCache::new(64); // 4 blocks per shard
        let mut idx = Index::open(&dir, "c", &[], cache.clone()).unwrap();
        for i in 0..(400 * BLOCK_ENTRIES as u32) {
            idx.put([i, 0, 0, 0]);
        }
        idx.flush().unwrap();
        let run = idx.runs[0].clone();
        let cached = |b: u32| {
            cache
                .shard((run.uid, b))
                .read()
                .unwrap()
                .map
                .contains_key(&(run.uid, b))
        };
        cache.get_or_load(&run, 0).unwrap();
        for b in 1..run.blocks() {
            cache.get_or_load(&run, b).unwrap(); // a scan passing through
            assert!(
                cached(0),
                "a block read all along stays cached (evicted at {b})"
            );
            cache.get_or_load(&run, 0).unwrap(); // the hot block, read all along
        }
        assert!(!cached(1), "blocks read once make room");
        drop(idx);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshots_are_isolated() {
        let cache = BlockCache::new(64);
        let mut idx = Index::memory("s", cache);
        for i in 0..100u32 {
            idx.put([i, 0, 0, 0]);
            if i % 10 == 9 {
                idx.freeze();
            }
        }
        let snap = idx.snapshot();
        for i in 0..50u32 {
            idx.delete([i, 0, 0, 0]);
        }
        idx.put([500, 0, 0, 0]);
        idx.freeze();
        assert_eq!(snap.range([0; 4], [u32::MAX; 4]).count(), 100);
        assert_eq!(idx.range([0; 4], [u32::MAX; 4]).count(), 51);
        assert!(
            idx.mem.levels.len() <= 4,
            "levels stay few: {}",
            idx.mem.levels.len()
        );
    }
}
