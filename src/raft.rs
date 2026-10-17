//! Raft consensus for a GraphRD cluster: leader election, log replication
//! and majority commit, with the term, vote and log persisted to disk.
//! Log entries are opaque JSON operations applied, in order, by the server's
//! state machine on every node.
//!
//! Applied entries are compacted away once there are many of them: the server's data holds
//! their effect, and the log keeps only what follows (`base`, the last compacted index). A
//! follower that needs compacted entries gets a snapshot of the leader's data instead
//! (`/cluster/raft/snapshot`). Learners are members that receive everything but do not vote
//! and do not count for a majority: read replicas.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::http;
use crate::json::Json;

#[derive(Clone, Copy, PartialEq, Debug)]
/// A member's role in the protocol.
pub enum Role {
    /// Answers votes and appends, and may become a candidate.
    Follower,
    /// Asks for votes to become the leader.
    Candidate,
    /// Serves writes and replicates them.
    Leader,
    /// A member that only follows (`--cluster-learners`).
    Learner,
}

#[derive(Clone, Debug)]
/// One entry of the Raft log.
pub struct Entry {
    /// The term the entry was written in.
    pub term: u64,
    /// The operation, as the JSON the cluster protocol carries.
    pub op: Json,
}

/// Another member of the cluster.
pub struct Peer {
    /// The member's id (`--node-id`).
    pub id: String,
    /// Where to reach it, `host:port`.
    pub host: String, // host:port
    /// Learners receive the log but do not vote.
    pub voter: bool,
}

struct State {
    role: Role,
    term: u64,
    voted_for: Option<String>,
    /// Entries after `base`: log[0] is index base + 1.
    log: Vec<Entry>,
    /// The last index compacted into the data (and its term).
    base: u64,
    base_term: u64,
    commit: u64,
    applied: u64,
    leader: Option<String>,
    last_heard: Instant,
    election_timeout: Duration,
    next_index: Vec<u64>,
    match_index: Vec<u64>,
    /// When each peer last answered (leader only).
    last_contact: Vec<Option<Instant>>,
}

/// Making and installing snapshots of the server's data (a backup archive).
pub struct Snapshots {
    /// Takes a snapshot of every repository (a backup archive).
    pub make: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    /// Installs a snapshot into every repository.
    pub install: Arc<dyn Fn(&[u8]) -> Result<(), String> + Send + Sync>,
}

/// The Raft state machine of one cluster member.
pub struct Raft {
    /// This member's id.
    pub id: String,
    /// The other members.
    pub peers: Vec<Peer>,
    /// Whether this node votes (false for a learner).
    pub voter: bool,
    dir: PathBuf,
    state: Mutex<State>,
    changed: Condvar,
    log_file: Mutex<File>,
    secret: String,
    /// Held while an entry is applied or a snapshot is made or installed, so a snapshot is
    /// exactly the data up to an index.
    apply_lock: Mutex<()>,
    snapshots: std::sync::OnceLock<Snapshots>,
}

/// Applies one committed operation to the local stores.
pub type Apply = Arc<dyn Fn(&Json) + Send + Sync>;

/// Applied entries kept before the log is compacted (`GRAPHRD_RAFT_COMPACT` changes it).
fn compact_after() -> u64 {
    std::env::var("GRAPHRD_RAFT_COMPACT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000)
}

fn rand_ms(lo: u64, hi: u64) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(7, |d| d.subsec_nanos() as u64);
    let x = nanos
        .wrapping_mul(6364136223846793005)
        .wrapping_add(std::process::id() as u64 * 1442695040888963407);
    Duration::from_millis(lo + (x >> 33) % (hi - lo))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn num(j: &Json, k: &str) -> u64 {
    j.get(k)
        .and_then(|v| {
            if let Json::Num(n) = v {
                Some(*n as u64)
            } else {
                None
            }
        })
        .unwrap_or(0)
}

fn entry_json(e: &Entry) -> Json {
    Json::obj()
        .with("t", e.term as usize)
        .with("op", e.op.clone())
}

/// An entry as stored in `raft.log`: with its index, so the file and the compaction base in
/// `raft.meta` (written separately) can never be read out of step.
fn stored_json(index: u64, e: &Entry) -> Json {
    Json::obj()
        .with("i", index as usize)
        .with("t", e.term as usize)
        .with("op", e.op.clone())
}

impl State {
    fn last_index(&self) -> u64 {
        self.base + self.log.len() as u64
    }

    /// Term of the entry at `i` (the base's term for the base; 0 before it).
    fn term_at(&self, i: u64) -> u64 {
        if i == self.base {
            self.base_term
        } else if i > self.base && i <= self.last_index() {
            self.log[(i - self.base - 1) as usize].term
        } else {
            0
        }
    }

    fn entry(&self, i: u64) -> &Entry {
        &self.log[(i - self.base - 1) as usize]
    }
}

impl Raft {
    /// Load persisted state from `dir` (term, vote, compaction base, log).
    pub fn open(
        dir: &Path,
        id: &str,
        peers: Vec<Peer>,
        voter: bool,
        secret: &str,
    ) -> std::io::Result<Arc<Raft>> {
        fs::create_dir_all(dir)?;
        let meta = fs::read_to_string(dir.join("raft.meta"))
            .ok()
            .and_then(|t| Json::parse(&t).ok())
            .unwrap_or(Json::obj());
        let (term, base, base_term) = (
            num(&meta, "term"),
            num(&meta, "base"),
            num(&meta, "baseTerm"),
        );
        let voted_for = meta
            .get("votedFor")
            .and_then(|v| v.as_str())
            .map(String::from);
        let mut log = Vec::new();
        if let Ok(f) = File::open(dir.join("raft.log")) {
            let mut expected = base + 1;
            for line in BufReader::new(f).lines() {
                let Ok(line) = line else { break };
                // a torn last line from a crash is dropped
                let Ok(j) = Json::parse(&line) else { break };
                // entries at or below the base were compacted (the log may predate the base)
                let i = if j.get("i").is_some() {
                    num(&j, "i")
                } else {
                    expected
                };
                if i < expected {
                    continue;
                }
                if i > expected {
                    break; // a gap: what follows cannot be trusted
                }
                log.push(Entry {
                    term: num(&j, "t"),
                    op: j.get("op").cloned().unwrap_or(Json::Null),
                });
                expected += 1;
            }
        }
        // rewrite the log so a dropped torn tail does not linger
        let mut f = File::create(dir.join("raft.log"))?;
        for (k, e) in log.iter().enumerate() {
            writeln!(f, "{}", stored_json(base + 1 + k as u64, e))?;
        }
        f.sync_all()?;
        let log_file = OpenOptions::new().append(true).open(dir.join("raft.log"))?;
        let n = peers.len();
        Ok(Arc::new(Raft {
            id: id.to_string(),
            peers,
            voter,
            dir: dir.to_path_buf(),
            state: Mutex::new(State {
                role: if voter { Role::Follower } else { Role::Learner },
                term,
                voted_for,
                log,
                base,
                base_term,
                // compacted entries are in the data already
                commit: base,
                applied: base,
                leader: None,
                last_heard: Instant::now(),
                election_timeout: rand_ms(400, 800),
                next_index: vec![1; n],
                match_index: vec![0; n],
                last_contact: vec![None; n],
            }),
            changed: Condvar::new(),
            log_file: Mutex::new(log_file),
            secret: secret.to_string(),
            apply_lock: Mutex::new(()),
            snapshots: std::sync::OnceLock::new(),
        }))
    }

    fn persist_meta(&self, st: &State) {
        let meta = Json::obj()
            .with("term", st.term as usize)
            .with("votedFor", st.voted_for.clone())
            .with("base", st.base as usize)
            .with("baseTerm", st.base_term as usize);
        let tmp = self.dir.join("raft.meta.tmp");
        if let Ok(mut f) = File::create(&tmp) {
            if f.write_all(meta.to_string().as_bytes()).is_ok() && f.sync_all().is_ok() {
                let _ = fs::rename(&tmp, self.dir.join("raft.meta"));
            }
        }
    }

    /// Append entries whose first index is `first`.
    fn append_log(&self, first: u64, entries: &[Entry]) {
        let mut f = lock(&self.log_file);
        for (k, e) in entries.iter().enumerate() {
            let _ = writeln!(f, "{}", stored_json(first + k as u64, e));
        }
        let _ = f.sync_data();
    }

    /// Write the log file again from `st.log` (after a truncation, compaction or snapshot).
    fn rewrite_log(&self, st: &State) {
        let mut f = lock(&self.log_file);
        let tmp = self.dir.join("raft.log.tmp");
        if let Ok(mut nf) = File::create(&tmp) {
            for (k, e) in st.log.iter().enumerate() {
                let _ = writeln!(nf, "{}", stored_json(st.base + 1 + k as u64, e));
            }
            let _ = nf.sync_all();
            let _ = fs::rename(&tmp, self.dir.join("raft.log"));
        }
        if let Ok(af) = OpenOptions::new()
            .append(true)
            .open(self.dir.join("raft.log"))
        {
            *f = af;
        }
    }

    /// Truncate the log after index `len` (conflict with the leader).
    fn truncate_log(&self, st: &mut State, len: u64) {
        st.log.truncate((len - st.base) as usize);
        self.rewrite_log(st);
    }

    fn last(st: &State) -> (u64, u64) {
        (st.last_index(), st.term_at(st.last_index()))
    }

    fn become_follower(&self, st: &mut State, term: u64) {
        if term > st.term {
            st.term = term;
            st.voted_for = None;
            self.persist_meta(st);
        }
        if self.voter {
            st.role = Role::Follower;
        }
    }

    /// Provide the snapshot functions (they need every open repository).
    pub fn set_snapshots(&self, s: Snapshots) {
        let _ = self.snapshots.set(s);
    }

    /// This member's state for the Cluster page: role, term, indexes, peers.
    pub fn status(&self) -> Json {
        let st = lock(&self.state);
        let peers = self.peers.iter().enumerate().map(|(i, p)| {
            let mut j = Json::obj()
                .with("id", p.id.as_str())
                .with("address", p.host.as_str())
                .with("voter", p.voter);
            if st.role == Role::Leader {
                j = j.with("matchIndex", st.match_index[i] as usize).with(
                    "lastContactMs",
                    st.last_contact[i].map(|t| t.elapsed().as_millis() as usize),
                );
            }
            j
        });
        Json::obj()
            .with("nodeId", self.id.as_str())
            .with("role", format!("{:?}", st.role).to_lowercase())
            .with("voter", self.voter)
            .with("term", st.term as usize)
            .with("leader", st.leader.clone())
            .with("logLength", st.log.len())
            .with("snapshotIndex", st.base as usize)
            .with("lastIndex", st.last_index() as usize)
            .with("commitIndex", st.commit as usize)
            .with("appliedIndex", st.applied as usize)
            .with("peers", Json::Arr(peers.collect()))
    }

    /// Whether this member currently leads the cluster.
    pub fn is_leader(&self) -> bool {
        lock(&self.state).role == Role::Leader
    }

    /// Address (host:port) of the current leader, if known.
    pub fn leader_host(&self) -> Option<String> {
        let st = lock(&self.state);
        self.leader_host_locked(&st)
    }

    // ------------------------------------------------------------ RPC handlers

    /// One candidate's vote request; the answer follows the protocol's rules.
    pub fn handle_vote(&self, req: &Json) -> Json {
        let (term, cand, li, lt) = (
            num(req, "term"),
            req.str_at(&["candidate"]).to_string(),
            num(req, "lastIndex"),
            num(req, "lastTerm"),
        );
        let mut st = lock(&self.state);
        if term > st.term {
            self.become_follower(&mut st, term);
        }
        let (my_i, my_t) = Self::last(&st);
        let up_to_date = lt > my_t || (lt == my_t && li >= my_i);
        let granted = self.voter
            && term == st.term
            && up_to_date
            && st.voted_for.as_ref().is_none_or(|v| *v == cand);
        if granted {
            st.voted_for = Some(cand);
            st.last_heard = Instant::now();
            self.persist_meta(&st);
        }
        Json::obj()
            .with("term", st.term as usize)
            .with("granted", granted)
    }

    /// The leader's append: entries, a heartbeat, or a compaction snapshot, by what
    /// the request carries.
    pub fn handle_append(&self, req: &Json) -> Json {
        let (term, prev_i, prev_t, leader_commit) = (
            num(req, "term"),
            num(req, "prevIndex"),
            num(req, "prevTerm"),
            num(req, "commit"),
        );
        let mut st = lock(&self.state);
        if term < st.term {
            return Json::obj()
                .with("term", st.term as usize)
                .with("success", false)
                .with("match", 0usize);
        }
        self.become_follower(&mut st, term);
        st.leader = Some(req.str_at(&["leader"]).to_string());
        st.last_heard = Instant::now();
        if prev_i > st.last_index() || (prev_i > st.base && st.term_at(prev_i) != prev_t) {
            // tell the leader where our log ends so it can back up quickly
            let hint = st.last_index().min(prev_i.saturating_sub(1)).max(st.base);
            return Json::obj()
                .with("term", st.term as usize)
                .with("success", false)
                .with("match", hint as usize);
        }
        let entries: Vec<Entry> = req
            .get("entries")
            .and_then(|e| e.as_array())
            .map(|a| {
                a.iter()
                    .map(|e| Entry {
                        term: num(e, "t"),
                        op: e.get("op").cloned().unwrap_or(Json::Null),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut idx = prev_i;
        let mut new = Vec::new();
        for e in entries {
            idx += 1;
            if idx <= st.base {
                continue; // already compacted here
            }
            if idx <= st.last_index() {
                if st.term_at(idx) != e.term {
                    self.truncate_log(&mut st, idx - 1);
                    new.push(e.clone());
                    st.log.push(e);
                }
            } else {
                new.push(e.clone());
                st.log.push(e);
            }
        }
        if !new.is_empty() {
            self.append_log(st.last_index() + 1 - new.len() as u64, &new);
        }
        if leader_commit > st.commit {
            st.commit = leader_commit.min(idx).max(st.commit);
            self.changed.notify_all();
        }
        Json::obj()
            .with("term", st.term as usize)
            .with("success", true)
            .with("match", idx as usize)
    }

    /// A snapshot from the leader: the data up to `index` replaces this node's data and log.
    pub fn handle_snapshot(
        &self,
        term: u64,
        leader: &str,
        index: u64,
        index_term: u64,
        data: &[u8],
    ) -> Json {
        {
            let mut st = lock(&self.state);
            if term < st.term {
                return Json::obj()
                    .with("term", st.term as usize)
                    .with("success", false)
                    .with("match", 0usize);
            }
            self.become_follower(&mut st, term);
            st.leader = Some(leader.to_string());
            st.last_heard = Instant::now();
            if index <= st.applied {
                let m = st.last_index().max(index);
                return Json::obj()
                    .with("term", st.term as usize)
                    .with("success", true)
                    .with("match", m as usize);
            }
        }
        let Some(snapshots) = self.snapshots.get() else {
            return Json::obj()
                .with("term", 0usize)
                .with("success", false)
                .with("match", 0usize);
        };
        let _applying = lock(&self.apply_lock);
        if let Err(e) = (snapshots.install)(data) {
            eprintln!("raft: installing a snapshot failed: {e}");
            let st = lock(&self.state);
            return Json::obj()
                .with("term", st.term as usize)
                .with("success", false)
                .with("match", st.applied as usize);
        }
        let mut st = lock(&self.state);
        // keep the entries after the snapshot when they agree with it
        if st.term_at(index) == index_term && index <= st.last_index() {
            let keep = (index - st.base) as usize;
            st.log.drain(..keep);
        } else {
            st.log.clear();
        }
        st.base = index;
        st.base_term = index_term;
        st.commit = st.commit.max(index);
        st.applied = index;
        st.last_heard = Instant::now();
        self.persist_meta(&st);
        self.rewrite_log(&st);
        eprintln!("raft: {} installed a snapshot up to index {index}", self.id);
        self.changed.notify_all();
        Json::obj()
            .with("term", st.term as usize)
            .with("success", true)
            .with("match", st.last_index() as usize)
    }

    // ------------------------------------------------------------ client API

    /// Replicate an operation; returns once it is committed and applied
    /// locally. Err(Some(leader)) when this node is not the leader.
    pub fn propose(&self, op: Json, timeout: Duration) -> Result<u64, Option<String>> {
        let index = {
            let mut st = lock(&self.state);
            if st.role != Role::Leader {
                return Err(self.leader_host_locked(&st));
            }
            let e = Entry { term: st.term, op };
            st.log.push(e.clone());
            let i = st.last_index();
            self.append_log(i, &[e]);
            self.advance_commit(&mut st);
            self.changed.notify_all();
            i
        };
        let deadline = Instant::now() + timeout;
        let mut st = lock(&self.state);
        while st.applied < index {
            if Instant::now() > deadline || st.role != Role::Leader {
                return Err(self.leader_host_locked(&st));
            }
            st = self
                .changed
                .wait_timeout(st, Duration::from_millis(20))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Ok(index)
    }

    fn leader_host_locked(&self, st: &State) -> Option<String> {
        let l = st.leader.clone()?;
        self.peers
            .iter()
            .find(|p| p.id == l)
            .map(|p| p.host.clone())
    }

    /// Votes needed: a majority of the voting members (learners do not count).
    fn majority(&self) -> usize {
        let voters = self.peers.iter().filter(|p| p.voter).count() + usize::from(self.voter);
        voters / 2 + 1
    }

    fn advance_commit(&self, st: &mut State) {
        let n = st.last_index();
        for idx in (st.commit.max(st.base) + 1..=n).rev() {
            if st.term_at(idx) != st.term {
                continue;
            }
            let votes = usize::from(self.voter)
                + self
                    .peers
                    .iter()
                    .zip(&st.match_index)
                    .filter(|(p, m)| p.voter && **m >= idx)
                    .count();
            if votes >= self.majority() {
                st.commit = idx;
                break;
            }
        }
    }

    // ------------------------------------------------------------ background loops

    fn rpc(&self, host: &str, path: &str, body: &Json) -> Option<Json> {
        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("X-GraphRD-Cluster".to_string(), self.secret.clone()),
        ];
        let (status, _, resp) = http::call(
            host,
            "POST",
            path,
            &headers,
            body.to_string().as_bytes(),
            Duration::from_millis(500),
        )
        .ok()?;
        if status != 200 {
            return None;
        }
        Json::parse(&String::from_utf8_lossy(&resp)).ok()
    }

    /// Start election, replication and apply threads.
    pub fn start(self: &Arc<Self>, apply: Apply) {
        let me = self.clone();
        std::thread::spawn(move || me.ticker());
        for i in 0..self.peers.len() {
            let me = self.clone();
            std::thread::spawn(move || me.replicator(i));
        }
        let me = self.clone();
        std::thread::spawn(move || me.applier(apply));
    }

    fn ticker(self: Arc<Self>) {
        if !self.voter {
            return; // learners never stand for election
        }
        loop {
            std::thread::sleep(Duration::from_millis(30));
            let (term, li, lt) = {
                let mut st = lock(&self.state);
                if st.role == Role::Leader || st.last_heard.elapsed() < st.election_timeout {
                    continue;
                }
                st.role = Role::Candidate;
                st.term += 1;
                st.voted_for = Some(self.id.clone());
                st.leader = None;
                st.last_heard = Instant::now();
                st.election_timeout = rand_ms(400, 800);
                self.persist_meta(&st);
                let (li, lt) = Self::last(&st);
                (st.term, li, lt)
            };
            let req = Json::obj()
                .with("term", term as usize)
                .with("candidate", self.id.as_str())
                .with("lastIndex", li as usize)
                .with("lastTerm", lt as usize);
            let votes = Arc::new(Mutex::new(1usize));
            let handles: Vec<_> = self
                .peers
                .iter()
                .filter(|p| p.voter)
                .map(|p| {
                    let (me, host, req, votes) =
                        (self.clone(), p.host.clone(), req.clone(), votes.clone());
                    std::thread::spawn(move || {
                        if let Some(r) = me.rpc(&host, "/cluster/raft/vote", &req) {
                            let rt = num(&r, "term");
                            if r.get("granted").and_then(|g| g.as_bool_opt()) == Some(true) {
                                *lock(&votes) += 1;
                            } else if rt > term {
                                let mut st = lock(&me.state);
                                me.become_follower(&mut st, rt);
                            }
                        }
                    })
                })
                .collect();
            for h in handles {
                let _ = h.join();
            }
            let mut st = lock(&self.state);
            if st.role == Role::Candidate && st.term == term && *lock(&votes) >= self.majority() {
                st.role = Role::Leader;
                st.leader = Some(self.id.clone());
                let next = st.last_index() + 1;
                st.next_index = vec![next; self.peers.len()];
                st.match_index = vec![0; self.peers.len()];
                st.last_contact = vec![None; self.peers.len()];
                // a no-op entry in the new term lets earlier entries commit
                let e = Entry {
                    term: st.term,
                    op: Json::obj().with("kind", "noop"),
                };
                st.log.push(e.clone());
                self.append_log(st.last_index(), &[e]);
                self.advance_commit(&mut st);
                eprintln!("raft: {} is leader for term {}", self.id, st.term);
                self.changed.notify_all();
            }
        }
    }

    /// Send a snapshot of the data to peer `i` (its next entry is compacted away here).
    fn send_snapshot(&self, i: usize, term: u64) -> Option<u64> {
        let snapshots = self.snapshots.get()?;
        let (index, index_term, data) = {
            let _applying = lock(&self.apply_lock);
            let (index, index_term) = {
                let st = lock(&self.state);
                (st.applied, st.term_at(st.applied))
            };
            (index, index_term, (snapshots.make)())
        };
        let path = format!(
            "/cluster/raft/snapshot?term={term}&leader={}&index={index}&indexTerm={index_term}",
            http::percent_encode(&self.id)
        );
        let headers = vec![
            ("Content-Type".to_string(), "application/x-tar".to_string()),
            ("X-GraphRD-Cluster".to_string(), self.secret.clone()),
        ];
        let (status, _, resp) = http::call(
            &self.peers[i].host,
            "POST",
            &path,
            &headers,
            &data,
            Duration::from_secs(600),
        )
        .ok()?;
        if status != 200 {
            return None;
        }
        let r = Json::parse(&String::from_utf8_lossy(&resp)).ok()?;
        let mut st = lock(&self.state);
        if num(&r, "term") > st.term {
            self.become_follower(&mut st, num(&r, "term"));
            return None;
        }
        (r.get("success").and_then(|s| s.as_bool_opt()) == Some(true)).then(|| num(&r, "match"))
    }

    fn replicator(self: Arc<Self>, i: usize) {
        loop {
            let req = {
                let st = lock(&self.state);
                if st.role != Role::Leader {
                    drop(st);
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                let next = st.next_index[i].max(1);
                if next <= st.base {
                    // the entries it needs are compacted: send the data instead
                    let term = st.term;
                    drop(st);
                    match self.send_snapshot(i, term) {
                        Some(m) => {
                            let mut st = lock(&self.state);
                            st.match_index[i] = m;
                            st.next_index[i] = m + 1;
                            st.last_contact[i] = Some(Instant::now());
                            self.advance_commit(&mut st);
                            self.changed.notify_all();
                        }
                        None => std::thread::sleep(Duration::from_millis(200)),
                    }
                    continue;
                }
                let prev_i = next - 1;
                let prev_t = st.term_at(prev_i);
                let entries: Vec<Json> = st.log[((prev_i - st.base) as usize)..]
                    .iter()
                    .take(512)
                    .map(entry_json)
                    .collect();
                Json::obj()
                    .with("term", st.term as usize)
                    .with("leader", self.id.as_str())
                    .with("prevIndex", prev_i as usize)
                    .with("prevTerm", prev_t as usize)
                    .with("entries", Json::Arr(entries))
                    .with("commit", st.commit as usize)
            };
            let sent_term = num(&req, "term");
            let n_sent = req
                .get("entries")
                .and_then(|e| e.as_array())
                .map_or(0, |a| a.len());
            match self.rpc(&self.peers[i].host, "/cluster/raft/append", &req) {
                Some(r) => {
                    let mut st = lock(&self.state);
                    if num(&r, "term") > st.term {
                        self.become_follower(&mut st, num(&r, "term"));
                        continue;
                    }
                    if st.role != Role::Leader || st.term != sent_term {
                        continue;
                    }
                    st.last_contact[i] = Some(Instant::now());
                    if r.get("success").and_then(|s| s.as_bool_opt()) == Some(true) {
                        st.match_index[i] = num(&r, "match");
                        st.next_index[i] = st.match_index[i] + 1;
                        self.advance_commit(&mut st);
                        self.changed.notify_all();
                        if n_sent > 0 && st.next_index[i] <= st.last_index() {
                            continue; // more to send right away
                        }
                    } else {
                        st.next_index[i] = (num(&r, "match") + 1)
                            .min(st.next_index[i].saturating_sub(1))
                            .max(1);
                        continue;
                    }
                }
                None => std::thread::sleep(Duration::from_millis(100)),
            }
            // heartbeat interval, or until new entries arrive
            let st = lock(&self.state);
            let _ = self.changed.wait_timeout(st, Duration::from_millis(80));
        }
    }

    fn applier(self: Arc<Self>, apply: Apply) {
        loop {
            let applying = lock(&self.apply_lock);
            let next = {
                let st = lock(&self.state);
                if st.applied >= st.commit {
                    drop(applying);
                    let _ = self.changed.wait_timeout(st, Duration::from_millis(100));
                    continue;
                }
                st.entry(st.applied + 1).op.clone()
            };
            apply(&next);
            let mut st = lock(&self.state);
            st.applied += 1;
            // fold applied entries into the data once there are many
            if st.applied - st.base >= compact_after() {
                let (applied, keep_from) = (st.applied, (st.applied - st.base) as usize);
                st.base_term = st.term_at(applied);
                st.log.drain(..keep_from);
                st.base = applied;
                self.persist_meta(&st);
                self.rewrite_log(&st);
            }
            drop(applying);
            self.changed.notify_all();
        }
    }

    /// Whether a request carries the cluster secret.
    pub fn secret_ok(&self, s: Option<&str>) -> bool {
        s == Some(self.secret.as_str())
    }
}

trait BoolOpt {
    fn as_bool_opt(&self) -> Option<bool>;
}

impl BoolOpt for Json {
    fn as_bool_opt(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
}
