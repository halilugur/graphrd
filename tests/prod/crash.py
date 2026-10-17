"""Crash test: concurrent writers (small transactions, deletes, bulk loads that force checkpoints)
while the server is killed with SIGKILL at random moments, many times over the same data
directory. After every restart:
  * every acknowledged insert is present and every acknowledged delete is gone (durability)
  * every transaction is all-or-nothing, including in-flight ones (atomicity)
  * inferred statements match the explicit ones (reasoner state survives)
  * counts reported by /size match a full scan, and the full-text index finds new literals."""
import os, random, sys, threading, time
sys.path.insert(0, os.path.dirname(__file__))
from common import Server, Report

EX = "http://ex.org/"
CYCLES = int(os.environ.get("CYCLES", 12))
rng = random.Random(7)


class Model:
    """Expected state per transaction key: True (present), False (absent), None (unknown: in flight at the crash)."""
    def __init__(self):
        self.lock = threading.Lock()
        self.state = {}
        self.bulk = {}

    def set(self, k, v):
        with self.lock:
            self.state[k] = v


def writer(srv, model, wid, stop, cycle):
    i = 0
    while not stop.is_set():
        i += 1
        k = f"w{wid}c{cycle}n{i}"
        r = rng.random()
        if r < 0.75:
            q = f"PREFIX ex: <{EX}> INSERT DATA {{ ex:{k} ex:a {i} ; ex:b \"word{k} text\" . GRAPH ex:g{wid} {{ ex:{k} ex:c ex:{k} }} }}"
            model.set(k, None)
            try:
                st, _ = srv.update("crash", q, timeout=30)
                if st in (200, 204):
                    model.set(k, True)
            except Exception:
                return
        elif r < 0.95:
            with model.lock:
                present = [x for x, v in model.state.items() if v is True and x.startswith(f"w{wid}")]
            if not present:
                continue
            victim = rng.choice(present)
            q = f"PREFIX ex: <{EX}> DELETE {{ ex:{victim} ?p ?o . GRAPH ?g {{ ex:{victim} ?p2 ?o2 }} }} WHERE {{ {{ ex:{victim} ?p ?o }} UNION {{ GRAPH ?g {{ ex:{victim} ?p2 ?o2 }} }} }}"
            model.set(victim, None)
            try:
                st, _ = srv.update("crash", q, timeout=30)
                if st in (200, 204):
                    model.set(victim, False)
            except Exception:
                return
        else:
            # bulk load into its own graph: 60k quads in one transaction (crosses the memtable limit)
            g = f"bulk{wid}c{cycle}n{i}"
            data = "".join(f"<{EX}{g}s{j}> <{EX}v> \"{j}\" <{EX}{g}> .\n" for j in range(60000))
            with model.lock:
                model.bulk[g] = None
            try:
                st, _ = srv.req("POST", "/repositories/crash/statements", data.encode(), "application/n-quads", timeout=60)
                if st in (200, 204):
                    with model.lock:
                        model.bulk[g] = True
            except Exception:
                return


def verify(srv, model, rep, cycle):
    rows = srv.query("crash", f"PREFIX ex: <{EX}> SELECT ?s (COUNT(?o) AS ?n) WHERE {{ ?s ex:a|ex:b ?o }} GROUP BY ?s")["results"]["bindings"]
    found = {r["s"]["value"][len(EX):]: int(r["n"]["value"]) for r in rows}
    graphs = srv.query("crash", f"PREFIX ex: <{EX}> SELECT ?s WHERE {{ GRAPH ?g {{ ?s ex:c ?s }} }}")["results"]["bindings"]
    in_graph = {r["s"]["value"][len(EX):] for r in graphs}
    lost = dup = torn = 0
    for k, want in list(model.state.items()):
        n = found.get(k, 0)
        has = n > 0
        if n not in (0, 2) or has != (k in in_graph):
            torn += 1
            rep.check(False, f"cycle {cycle}: transaction {k} partially applied (a/b={n}, graph={k in in_graph})")
            continue
        if want is None:
            model.state[k] = has
        elif want != has:
            lost += 1
            rep.check(False, f"cycle {cycle}: acknowledged {'insert' if want else 'delete'} of {k} lost")
    for g, want in list(model.bulk.items()):
        n = int(srv.query("crash", f"SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{EX}{g}> {{ ?s ?p ?o }} }}")["results"]["bindings"][0]["n"]["value"])
        if n not in (0, 60000):
            rep.check(False, f"cycle {cycle}: bulk load {g} partially applied ({n})")
        elif want is None:
            model.bulk[g] = n == 60000
        elif want != (n == 60000):
            rep.check(False, f"cycle {cycle}: acknowledged bulk load {g} lost")
    # inference: ex:a has rdfs:domain ex:Thing, so every subject with ex:a is a Thing
    q = lambda s: int(srv.query("crash", f"PREFIX ex: <{EX}> " + s)["results"]["bindings"][0]["n"]["value"])
    things = q("SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE { ?s a ex:Thing }")
    with_a = q("SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE { ?s ex:a ?x }")
    rep.check(things == with_a, f"cycle {cycle}: inferred types {things} != subjects with ex:a {with_a}")
    explicit = q("SELECT (COUNT(*) AS ?n) FROM <http://graphrd.org/graph#explicit> WHERE { ?s ?p ?o }")
    rep.check(explicit == srv.size("crash"), f"cycle {cycle}: /size {srv.size('crash')} != scanned explicit {explicit}")
    present = [k for k, v in model.state.items() if v]
    if present:
        k = rng.choice(present)
        hits = q(f"PREFIX g: <http://graphrd.org/fn#> SELECT (COUNT(*) AS ?n) WHERE {{ ?l g:fts 'word{k}' }}")
        rep.check(hits == 1, f"cycle {cycle}: full-text search for word{k} found {hits}")
    acked = sum(1 for v in model.state.values() if v)
    print(f"  cycle {cycle}: {len(model.state)} txns ({acked} present), {sum(1 for v in model.bulk.values() if v)} bulk loads, "
          f"{srv.size('crash')} statements; lost={lost} torn={torn}", flush=True)


def main():
    rep = Report(f"crash recovery, {CYCLES} SIGKILL cycles")
    srv = Server().start()
    srv.create_repo("crash", "rdfs", {"enableFtsIndex": {"value": "true"}})
    srv.update("crash", f"PREFIX ex: <{EX}> PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> INSERT DATA {{ ex:a rdfs:domain ex:Thing }}")
    model = Model()
    try:
        for cycle in range(CYCLES):
            stop = threading.Event()
            ts = [threading.Thread(target=writer, args=(srv, model, w, stop, cycle), daemon=True) for w in range(6)]
            for t in ts:
                t.start()
            time.sleep(rng.uniform(0.5, 4.0))
            srv.kill9()
            stop.set()
            for t in ts:
                t.join(70)
            t0 = time.time()
            srv.start()
            print(f"  restart after SIGKILL took {time.time() - t0:.2f}s", flush=True)
            verify(srv, model, rep, cycle)
    finally:
        srv.stop()
    rep.done()


if __name__ == "__main__":
    main()
