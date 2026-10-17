"""How often a language model's SPARQL answers questions correctly (FEATURES.md F-13: at least 80%).
Loads one block of the comparison data (~782K triples: persons, companies, cities, posts, tags),
starts a server with the given model options, asks each question through
/rest/repositories/{id}/nl-query and compares the results with a reference query: correct when
the row counts match and every reference row's values are among one answer row's values (extra
columns, such as a name next to an IRI, are fine; numbers compare to 3 decimals).

Calls the real model: every run costs what the provider charges for ~20 questions.

    ANTHROPIC_API_KEY=... python3 tests/prod/nlq_eval.py --llm-api anthropic
    python3 tests/prod/nlq_eval.py --llm-url http://127.0.0.1:11434/v1 --llm-model qwen3-coder
"""
import json, os, subprocess, sys, tempfile, time, urllib.parse

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "bench"))
from common import BIN, Server  # noqa: E402
from scale import block  # noqa: E402

EX = "PREFIX ex: <http://example.org/>\nPREFIX xsd: <http://www.w3.org/2001/XMLSchema#>\n"
P7 = "<http://example.org/person/7>"
QUESTIONS = [
    ("How many people are there?", "SELECT (COUNT(?p) AS ?n) { ?p a ex:Person }"),
    ("How many companies are there?", "SELECT (COUNT(?c) AS ?n) { ?c a ex:Company }"),
    ("What is the name of the person http://example.org/person/123?", "SELECT ?n { <http://example.org/person/123> ex:name ?n }"),
    ("How many people are older than 75?", "SELECT (COUNT(?p) AS ?n) { ?p a ex:Person ; ex:age ?a FILTER(?a > 75) }"),
    ("What is the average age of all people?", "SELECT (AVG(?a) AS ?avg) { ?p a ex:Person ; ex:age ?a }"),
    ("Which 5 companies have the most employees? Give each company and its number of employees.",
     "SELECT ?c (COUNT(?p) AS ?n) { ?p ex:worksFor ?c } GROUP BY ?c ORDER BY DESC(?n) ?c LIMIT 5"),
    ("How many people live in the city named City 7?", "SELECT (COUNT(?p) AS ?n) { ?p ex:livesIn ?c . ?c ex:name \"City 7\" }"),
    ("How many posts were created on or after 1 January 2024?", "SELECT (COUNT(?p) AS ?n) { ?p a ex:Post ; ex:createdOn ?d FILTER(?d >= \"2024-01-01\"^^xsd:date) }"),
    ("How many posts have a score of exactly 1000?", "SELECT (COUNT(?p) AS ?n) { ?p ex:score 1000 }"),
    ("How many posts are tagged with the tag named tag-3?", "SELECT (COUNT(DISTINCT ?p) AS ?n) { ?p ex:tag ?t . ?t ex:name \"tag-3\" }"),
    (f"How many different people does {P7[1:-1]} know directly?", f"SELECT (COUNT(DISTINCT ?f) AS ?n) {{ {P7} ex:knows ?f }}"),
    (f"How many people can {P7[1:-1]} reach by following knows links any number of times?", f"SELECT (COUNT(DISTINCT ?f) AS ?n) {{ {P7} ex:knows+ ?f }}"),
    ("What is the name of the city where the most people live?",
     "SELECT ?name { { SELECT ?c (COUNT(?p) AS ?k) { ?p ex:livesIn ?c } GROUP BY ?c ORDER BY DESC(?k) LIMIT 1 } ?c ex:name ?name }"),
    ("How many posts did http://example.org/person/42 create?", "SELECT (COUNT(?post) AS ?n) { <http://example.org/person/42> ex:created ?post }"),
    ("Is there anyone older than 90?", "ASK { ?p a ex:Person ; ex:age ?a FILTER(?a > 90) }"),
    ("How many employees does the company named Company 10 have?", "SELECT (COUNT(?p) AS ?n) { ?p ex:worksFor ?c . ?c ex:name \"Company 10\" }"),
    ("What is the highest score of any post?", "SELECT (MAX(?s) AS ?m) { ?p ex:score ?s }"),
    ("How many tags are there?", "SELECT (COUNT(?t) AS ?n) { ?t a ex:Tag }"),
    ("How many posts were created in March 2024?",
     "SELECT (COUNT(?p) AS ?n) { ?p ex:createdOn ?d FILTER(?d >= \"2024-03-01\"^^xsd:date && ?d < \"2024-04-01\"^^xsd:date) }"),
    ("What is the youngest age among people who work for the company named Company 3?",
     "SELECT (MIN(?a) AS ?m) { ?p ex:worksFor ?c ; ex:age ?a . ?c ex:name \"Company 3\" }"),
]


def norm(v):
    try:
        return f"{float(v):.3f}"
    except ValueError:
        return v


def rows(result):
    if "boolean" in result:
        return [{str(result["boolean"]).lower()}]
    return [{norm(b["value"]) for b in r.values()} for r in result["results"]["bindings"]]


def same(ref, got):
    if len(ref) != len(got):
        return False
    free = list(got)
    for r in ref:
        hit = next((g for g in free if r <= g), None)
        if hit is None:
            return False
        free.remove(hit)
    return True


def main():
    llm_args = sys.argv[1:]
    if not any(a.startswith("--llm-") for a in llm_args):
        sys.exit(__doc__)
    work = tempfile.mkdtemp(prefix="graphrd-nlq-eval-")
    data = os.path.join(work, "data")
    subprocess.run([BIN, "load", "--data", data, "--repo", "bench", "--ruleset", "empty", "--format", "nt", "-"], input=block((0, 1)), check=True, capture_output=True)
    srv = Server(data=data, args=llm_args).start()
    for p, n in [("ex", "http://example.org/"), ("xsd", "http://www.w3.org/2001/XMLSchema#")]:
        srv.req("PUT", f"/repositories/bench/namespaces/{p}", n.encode(), "text/plain")
    ok = 0
    for i, (q, ref) in enumerate(QUESTIONS, 1):
        st, b = srv.req("POST", "/repositories/bench", urllib.parse.urlencode({"query": EX + ref}).encode(), "application/x-www-form-urlencoded", accept="application/sparql-results+json")
        want = rows(json.loads(b))
        t0 = time.time()
        st, b = srv.req("POST", "/rest/repositories/bench/nl-query", json.dumps({"question": q, "run": True}).encode(), "application/json", timeout=600)
        secs = time.time() - t0
        if st != 200:
            print(f"{i:2}. FAIL ({st}) {q}\n    {b.decode()[:300]}")
            continue
        a = json.loads(b)
        got = rows(a["results"]) if "results" in a else None
        good = got is not None and same(want, got)
        ok += good
        print(f"{i:2}. {'ok  ' if good else 'FAIL'} {q}  ({secs:.1f} s, {a['attempts']} attempt{'s' if a['attempts'] > 1 else ''})")
        if not good:
            print("    query:  " + a["sparql"].replace("\n", "\n            "))
            print(f"    want {sorted(map(sorted, want))[:5]}, got {sorted(map(sorted, got))[:5] if got is not None else a.get('error')}")
    srv.stop()
    pct = 100 * ok / len(QUESTIONS)
    print(f"\n{ok}/{len(QUESTIONS)} correct ({pct:.0f}%), target 80%")
    sys.exit(0 if pct >= 80 else 1)


if __name__ == "__main__":
    main()
