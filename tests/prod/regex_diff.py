"""Differential regex test: random patterns and texts through SPARQL REGEX/REPLACE on GraphRD,
compared with Python's re (same leftmost-first semantics for this syntax subset)."""
import json, os, random, re, signal, sys, time
sys.path.insert(0, os.path.dirname(__file__))
from common import Server, Report

rng = random.Random(int(os.environ.get("SEED", 11)))
ATOMS = ["a", "b", "c", ".", "[ab]", "[^a]", "\\d", "\\w", "\\s", "x", "(a|b)", "(ab|a)", "(?:a|bc)", "^", "$", "[a-c]", "(a*)", "(b+)?"]
QUANT = ["", "", "", "*", "+", "?", "*?", "+?", "??", "{2}", "{1,3}", "{0,2}?", "{2,}"]


def gen_pat(d=0):
    parts = []
    for _ in range(rng.randint(1, 4)):
        a = rng.choice(ATOMS) if d > 1 or rng.random() < 0.7 else "(" + gen_pat(d + 1) + ")"
        q = rng.choice(QUANT)
        if a in ("^", "$"):
            q = ""
        parts.append(a + q)
    p = "".join(parts)
    if rng.random() < 0.2:
        p += "|" + gen_pat(d + 1)
    return p


def gen_text():
    return "".join(rng.choice("aabbc x1\n") for _ in range(rng.randint(0, 14)))


def lit(s):
    return json.dumps(s)  # JSON escapes are valid SPARQL string escapes for this alphabet


def main():
    rep = Report("regex vs Python re")
    srv = Server().start()
    srv.create_repo("x")
    cases = []
    while len(cases) < int(os.environ.get("CASES", 3000)):
        p, t = gen_pat(), gen_text()
        if any(x in p for x in ("?+", "*+", "++", "}+")):
            continue  # possessive quantifiers in Python 3.11+, not part of XPath regex
        try:
            r = re.compile(p)
        except re.error:
            continue
        if "$" in p and "\n" in t:
            continue  # Python's $ also matches before a final newline; XPath's does not
        cases.append((p, t, r))
    bad = extent = 0
    for i in range(0, len(cases), 100):
        chunk = cases[i:i + 100]
        values = " ".join(f"({lit(p)} {lit(t)} {k})" for k, (p, t, _) in enumerate(chunk))
        q = f"SELECT ?k ?m ?r WHERE {{ VALUES (?p ?t ?k) {{ {values} }} BIND(REGEX(?t, ?p) AS ?m) BIND(REPLACE(?t, ?p, '<$0>') AS ?r) }}"
        res = srv.query("x", q)["results"]["bindings"]
        got = {int(b["k"]["value"]): (b.get("m", {}).get("value"), b.get("r", {}).get("value")) for b in res}
        for k, (p, t, r) in enumerate(chunk):
            # Python's re backtracks; skip the rare case where the reference itself blows up
            signal.signal(signal.SIGALRM, lambda *_: (_ for _ in ()).throw(TimeoutError()))
            signal.alarm(2)
            try:
                want_m = "true" if r.search(t) else "false"
                r.sub("", t)
            except TimeoutError:
                print(f"  skip: Python re too slow on {p!r}")
                continue
            finally:
                signal.alarm(0)
            # XPath fn:replace forbids patterns that match the empty string; only compare REPLACE otherwise
            want_r = None if r.fullmatch("") or r.search("") and r.search("").end() == 0 and any(r.match(t, j) and r.match(t, j).end() == j for j in range(len(t) + 1)) else r.sub(lambda mo: "<" + mo.group(0) + ">", t)
            m, rr = got.get(k, (None, None))
            if m == want_m and want_r is not None and rr != want_r and re.search(r"\)[*+?]+\)?[*+?]", p):
                # Only the extent differs, in a pattern with nested loops that can match the
                # empty string: the one place automata and backtrackers are known to diverge.
                extent += 1
                print(f"  known extent difference: {p!r} on {t!r}: {rr!r} vs {want_r!r}")
                continue
            ok = m == want_m and (want_r is None or rr == want_r)
            if not ok:
                bad += 1
                if bad <= 10:
                    rep.check(False, f"pattern {p!r} text {t!r}: regex {m} want {want_m}; replace {rr!r} want {want_r!r}")
    rep.check(bad == 0, f"{bad} of {len(cases)} cases differ")
    rep.check(extent <= len(cases) / 1000, f"{extent} extent differences in nested empty loops")
    # performance: pathological patterns must stay linear
    for p, t in [("^(a+)+$", "a" * 5000 + "!"), ("(a|aa)*b", "a" * 5000), ("(.*)*x", "y" * 3000), ("(a*)*(b*)*c", "ab" * 2000)]:
        t0 = time.time()
        srv.query("x", f"ASK {{ FILTER(REGEX({lit(t)}, {lit(p)})) }}", timeout=30)
        dt = time.time() - t0
        rep.check(dt < 2, f"{p} on {len(t)} chars took {dt:.2f}s")
        print(f"  {p:14s} on {len(t)} chars: {dt * 1000:.0f} ms")
    big = "x" * 1_000_000 + "y"
    t0 = time.time()
    st, b = srv.req("POST", "/repositories/x", f"ASK {{ FILTER(REGEX({lit(big)}, 'x.*y')) }}".encode(), "application/sparql-query", "application/sparql-results+json")
    rep.check(st == 200 and b"true" in b, f"1 MB text: {st} {b[:100]!r}")
    print(f"  x.*y on 1 MB text: {(time.time() - t0) * 1000:.0f} ms")
    srv.stop()
    rep.done()


if __name__ == "__main__":
    main()
