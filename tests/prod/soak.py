"""Soak / isolation test. For SOAK_SECONDS (default 120) many clients run at once:
  * transfer writers move money between accounts with DELETE/INSERT WHERE (read-modify-write);
    the total must never change (no lost updates, no torn writes)
  * pair writers insert two triples per transaction; readers must never see one without the other
  * readers run point lookups, joins, aggregates, paths and full-text search, and check invariants
    inside a single query (each query sees one consistent state)
Reports throughput, latency percentiles, errors and memory growth."""
import os, random, sys, threading, time
sys.path.insert(0, os.path.dirname(__file__))
from common import Server, Report

EX = "http://ex.org/"
P = f"PREFIX ex: <{EX}> "
SECONDS = int(os.environ.get("SOAK_SECONDS", 120))
ACCOUNTS, START = 50, 1000


class Stats:
    def __init__(self):
        self.lock = threading.Lock()
        self.lat = {}
        self.errors = []

    def add(self, kind, dt):
        with self.lock:
            self.lat.setdefault(kind, []).append(dt)

    def err(self, what):
        with self.lock:
            if len(self.errors) < 20:
                self.errors.append(what)
            self.lat.setdefault("errors", []).append(0)


def timed(stats, kind, fn):
    t = time.perf_counter()
    try:
        r = fn()
    except Exception as e:
        stats.err(f"{kind}: {e}")
        return None
    stats.add(kind, time.perf_counter() - t)
    return r


def transfer(srv, stats, stop, seed):
    rng = random.Random(seed)
    while not stop.is_set():
        a, b = rng.sample(range(ACCOUNTS), 2)
        amt = rng.randint(1, 20)
        q = P + f"""DELETE {{ ex:acc{a} ex:balance ?x . ex:acc{b} ex:balance ?y }}
                   INSERT {{ ex:acc{a} ex:balance ?x2 . ex:acc{b} ex:balance ?y2 }}
                   WHERE {{ ex:acc{a} ex:balance ?x . ex:acc{b} ex:balance ?y FILTER(?x >= {amt}) BIND(?x - {amt} AS ?x2) BIND(?y + {amt} AS ?y2) }}"""
        r = timed(stats, "transfer", lambda: srv.update("soak", q))
        if r and r[0] not in (200, 204):
            stats.err(f"transfer HTTP {r[0]} {r[1][:200]}")


def pairs(srv, stats, stop, wid):
    i = 0
    while not stop.is_set():
        i += 1
        q = P + f'INSERT DATA {{ ex:w{wid}n{i} ex:left {i} ; ex:right {i} ; ex:knows ex:w{wid}n{max(1, i - 1)} ; ex:text "soak{wid}x{i} words here" }}'
        r = timed(stats, "insert", lambda: srv.update("soak", q))
        if r and r[0] not in (200, 204):
            stats.err(f"insert HTTP {r[0]}")
        if i % 10 == 0:
            q = P + f"DELETE WHERE {{ ex:w{wid}n{i - 5} ?p ?o }}"
            timed(stats, "delete", lambda: srv.update("soak", q))


def reader(srv, stats, stop, rep, seed):
    rng = random.Random(seed)
    one = lambda res, v: int(res["results"]["bindings"][0][v]["value"])
    while not stop.is_set():
        k = rng.random()
        if k < 0.25:
            res = timed(stats, "sum-check", lambda: srv.query("soak", P + "SELECT (SUM(?b) AS ?t) (COUNT(?b) AS ?n) WHERE { ?a ex:balance ?b }"))
            if res:
                t, n = one(res, "t"), one(res, "n")
                if t != ACCOUNTS * START or n != ACCOUNTS:
                    rep.check(False, f"bank invariant broken: total={t} accounts={n}")
        elif k < 0.5:
            res = timed(stats, "pair-check", lambda: srv.query("soak", P + "SELECT (COUNT(*) AS ?n) WHERE { { ?s ex:left ?x FILTER NOT EXISTS { ?s ex:right ?x } } UNION { ?s ex:right ?x FILTER NOT EXISTS { ?s ex:left ?x } } }"))
            if res and one(res, "n") != 0:
                rep.check(False, f"torn pair visible to a reader: {one(res, 'n')}")
        elif k < 0.7:
            w, n = rng.randrange(4), rng.randint(1, 50)
            timed(stats, "point", lambda: srv.query("soak", P + f"SELECT * WHERE {{ ex:w{w}n{n} ?p ?o }}"))
        elif k < 0.8:
            timed(stats, "path", lambda: srv.query("soak", P + f"SELECT (COUNT(?x) AS ?n) WHERE {{ ex:w{rng.randrange(4)}n{rng.randint(20, 60)} ex:knows+ ?x }}"))
        elif k < 0.9:
            timed(stats, "aggregate", lambda: srv.query("soak", P + "SELECT ?p (COUNT(*) AS ?n) WHERE { ?s ?p ?o } GROUP BY ?p"))
        else:
            timed(stats, "fts", lambda: srv.query("soak", P + f"PREFIX g: <http://graphrd.org/fn#> SELECT ?s WHERE {{ ?l g:fts 'soak{rng.randrange(4)}x{rng.randint(1, 100)}' . ?s ex:text ?l }}"))


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(len(xs) * p))] * 1000


def main():
    rep = Report(f"soak and isolation, {SECONDS}s")
    srv = Server(args=os.environ.get("SERVER_ARGS", "").split()).start()
    srv.create_repo("soak", "rdfsplus-optimized", {"enableFtsIndex": {"value": "true"}})
    srv.update("soak", P + "INSERT DATA { " + " ".join(f"ex:acc{i} ex:balance {START} ." for i in range(ACCOUNTS)) + " }")
    stats, stop = Stats(), threading.Event()
    threads = [threading.Thread(target=transfer, args=(srv, stats, stop, i)) for i in range(4)]
    threads += [threading.Thread(target=pairs, args=(srv, stats, stop, i)) for i in range(4)]
    threads += [threading.Thread(target=reader, args=(srv, stats, stop, rep, i)) for i in range(8)]
    for t in threads:
        t.start()
    rss = []
    t0 = time.time()
    while time.time() - t0 < SECONDS:
        time.sleep(min(10, SECONDS / 6))
        rss.append(srv.rss_mb())
        rep.check(srv.alive(), "server alive")
        print(f"  t={time.time() - t0:5.0f}s  rss={rss[-1]:.0f} MB  statements={srv.size('soak')}", flush=True)
    stop.set()
    for t in threads:
        t.join(60)
    total = sum(len(v) for k, v in stats.lat.items() if k != "errors")
    print(f"  {total} operations, {total / SECONDS:.0f}/s")
    for k, v in sorted(stats.lat.items()):
        if k != "errors":
            print(f"    {k:11s} {len(v):7d}  p50 {pct(v, .5):7.2f} ms  p99 {pct(v, .99):8.2f} ms  max {max(v) * 1000:8.1f} ms")
    rep.check(not stats.errors, f"errors: {len(stats.lat.get('errors', []))} e.g. {stats.errors[:5]}")
    final = srv.query("soak", P + "SELECT (SUM(?b) AS ?t) WHERE { ?a ex:balance ?b }")["results"]["bindings"][0]["t"]["value"]
    rep.check(int(final) == ACCOUNTS * START, f"final total {final}")
    # memory must level off: the last third may not keep climbing far above the first third
    if len(rss) >= 3:
        a, b = sum(rss[: len(rss) // 3]) / (len(rss) // 3), max(rss[-(len(rss) // 3):])
        print(f"  memory: early avg {a:.0f} MB, late max {b:.0f} MB")
        rep.check(b < max(a * 2.5, a + 300), f"memory keeps growing: {a:.0f} -> {b:.0f} MB")
    # restart and check everything is still consistent
    n = srv.size("soak")
    srv.stop()
    srv.start()
    rep.check(srv.size("soak") == n, f"size after clean restart {srv.size('soak')} != {n}")
    final = srv.query("soak", P + "SELECT (SUM(?b) AS ?t) WHERE { ?a ex:balance ?b }")["results"]["bindings"][0]["t"]["value"]
    rep.check(int(final) == ACCOUNTS * START, f"total after restart {final}")
    srv.stop()
    rep.done()


if __name__ == "__main__":
    main()
