"""Differential SPARQL test: the same data and queries run on GraphRD and on rdflib (the reference);
results are compared as multisets (or as ordered lists when the query fully orders them)."""
import random, sys, os
from decimal import Decimal, InvalidOperation
sys.path.insert(0, os.path.dirname(__file__))
from common import Server, Report
import rdflib
from rdflib import Dataset, URIRef, Literal, BNode
from rdflib.namespace import XSD

EX = "http://ex.org/"
XS = "http://www.w3.org/2001/XMLSchema#"
rng = random.Random(42)


def gen_data(n=300):
    quads = []
    first = ["Ann", "Bob", "Çağla", "Dmitri", "Eve", "Fatma", "Gus", "Hans", "Ida", "Jo"]
    cities = ["Paris", "Ankara", "Berlin", "Tokyo", "Lima"]
    for i in range(n):
        s = f"<{EX}p{i}>"
        g = f"<{EX}g{i % 3}>" if i % 4 else ""
        def q(p, o):
            quads.append(f"{s} {p} {o} {g} .")
        q(f"<{EX}type>", f"<{EX}{['Person','Robot','Alien'][i % 3]}>")
        q(f"<{EX}name>", f'"{first[i % 10]} {i}"' if i % 5 else f'"{first[i % 10]}"@en')
        if i % 7:
            q(f"<{EX}age>", f'"{rng.randint(1, 90)}"^^<{XS}integer>')
        if i % 3 == 0:
            q(f"<{EX}height>", f'"{rng.randint(100, 220) / 100}"^^<{XS}decimal>')
        if i % 11 == 0:
            q(f"<{EX}score>", f'"{rng.random() * 10:.3f}E0"^^<{XS}double>')
        q(f"<{EX}city>", f'"{cities[i % 5]}"')
        for _ in range(rng.randint(0, 3)):
            q(f"<{EX}knows>", f"<{EX}p{rng.randrange(n)}>")
        if i % 13 == 0:
            q(f"<{EX}born>", f'"19{rng.randint(10, 99)}-0{rng.randint(1, 9)}-1{rng.randint(0, 9)}"^^<{XS}date>')
        if i % 17 == 0:
            q(f"<{EX}note>", '"line\\nbreak \\"quoted\\" tab\\t ünïcödé"')
        if i % 19 == 0:
            q(f"<{EX}flag>", f'"{"true" if i % 2 else "false"}"^^<{XS}boolean>')
    quads.append(f"<{EX}Person> <{EX}label> \"Person\"@en .")
    quads.append(f"<{EX}Person> <{EX}label> \"Personne\"@fr .")
    quads.append(f"<{EX}Robot> <{EX}sub> <{EX}Machine> .")
    quads.append(f"<{EX}Machine> <{EX}sub> <{EX}Thing> .")
    quads.append(f"<{EX}Person> <{EX}sub> <{EX}Thing> .")
    return "\n".join(quads) + "\n"


P = f"PREFIX ex: <{EX}> PREFIX xsd: <{XS}> "
QUERIES = [
    # basic graph patterns
    "SELECT ?s ?n WHERE { ?s ex:name ?n }",
    "SELECT ?s WHERE { ?s ex:type ex:Robot }",
    "SELECT ?a ?b WHERE { ?a ex:knows ?b . ?b ex:knows ?a }",
    "SELECT ?a ?c WHERE { ?a ex:knows ?b . ?b ex:knows ?c . ?c ex:type ex:Alien }",
    "SELECT * WHERE { ?s ?p ?o } LIMIT 0",
    "SELECT ?p (COUNT(*) AS ?c) WHERE { ?s ?p ?o } GROUP BY ?p",
    # named graphs
    "SELECT ?g (COUNT(*) AS ?c) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g",
    "SELECT ?s WHERE { GRAPH ex:g1 { ?s ex:type ex:Person } }",
    "SELECT ?s ?g WHERE { GRAPH ?g { ?s ex:age ?a FILTER(?a > 80) } }",
    # filters
    "SELECT ?s ?a WHERE { ?s ex:age ?a FILTER(?a >= 30 && ?a < 40) }",
    "SELECT ?s WHERE { ?s ex:name ?n FILTER(lang(?n) = 'en') }",
    "SELECT ?s WHERE { ?s ex:name ?n FILTER(langMatches(lang(?n), 'EN')) }",
    "SELECT ?s ?n WHERE { ?s ex:name ?n FILTER(regex(?n, '^b', 'i')) }",
    "SELECT ?s ?n WHERE { ?s ex:name ?n FILTER(CONTAINS(?n, '1')) }",
    "SELECT ?s ?n WHERE { ?s ex:name ?n FILTER(STRSTARTS(?n, 'Ç')) }",
    "SELECT ?s WHERE { ?s ex:height ?h FILTER(?h > 1.5) }",
    "SELECT ?s WHERE { ?s ex:score ?h FILTER(?h < 5) }",
    "SELECT ?s WHERE { ?s ex:born ?d FILTER(?d < '1950-01-01'^^xsd:date) }",
    "SELECT ?s WHERE { ?s ex:flag true }",
    "SELECT ?s WHERE { ?s ex:age 42 }",
    "SELECT ?s ?o WHERE { ?s ex:knows ?o FILTER(?s = ?o) }",
    "SELECT ?s WHERE { ?s ex:type ?t FILTER(?t IN (ex:Robot, ex:Alien)) }",
    "SELECT ?s WHERE { ?s ex:type ?t FILTER(?t NOT IN (ex:Robot, ex:Alien)) }",
    "SELECT ?s ?n WHERE { ?s ex:name ?n FILTER(isLiteral(?n) && !isIRI(?n) && STRLEN(?n) > 6) }",
    "SELECT ?s WHERE { ?s ex:age ?a FILTER(datatype(?a) = xsd:integer && ?a / 2 = 10) }",
    # optional / union / minus / exists
    "SELECT ?s ?a WHERE { ?s ex:type ex:Person OPTIONAL { ?s ex:age ?a } }",
    "SELECT ?s ?a ?h WHERE { ?s ex:type ex:Robot OPTIONAL { ?s ex:age ?a } OPTIONAL { ?s ex:height ?h } }",
    "SELECT ?s ?a WHERE { ?s ex:type ex:Person OPTIONAL { ?s ex:age ?a FILTER(?a > 50) } }",
    "SELECT ?s WHERE { ?s ex:type ex:Person OPTIONAL { ?s ex:age ?a } FILTER(!BOUND(?a)) }",
    "SELECT ?x WHERE { { ?x ex:type ex:Robot } UNION { ?x ex:height ?h } }",
    "SELECT ?x ?y WHERE { { ?x ex:age ?y } UNION { ?x ex:height ?y } }",
    "SELECT ?s WHERE { ?s ex:type ex:Person MINUS { ?s ex:age ?a } }",
    "SELECT ?s WHERE { ?s ex:type ex:Person FILTER NOT EXISTS { ?s ex:knows ?o } }",
    "SELECT ?s WHERE { ?s ex:type ex:Alien FILTER EXISTS { ?s ex:knows ?o . ?o ex:type ex:Robot } }",
    # bind / values / expressions
    "SELECT ?s ?u WHERE { ?s ex:name ?n BIND(UCASE(?n) AS ?u) }",
    "SELECT ?s ?l WHERE { ?s ex:name ?n BIND(STRLEN(?n) AS ?l) FILTER(?l > 7) }",
    "SELECT ?s ?c WHERE { ?s ex:city ?c0 BIND(CONCAT(?c0, '-', STR(?s)) AS ?c) }",
    "SELECT ?s ?x WHERE { ?s ex:age ?a BIND(IF(?a > 45, 'old', 'young') AS ?x) }",
    "SELECT ?s ?x WHERE { ?s ex:type ex:Robot OPTIONAL { ?s ex:age ?a } BIND(COALESCE(?a, -1) AS ?x) }",
    "SELECT ?s ?y WHERE { ?s ex:born ?d BIND(YEAR(?d) AS ?y) }",
    "SELECT ?s ?r WHERE { ?s ex:name ?n BIND(REPLACE(?n, '[0-9]+', '#') AS ?r) }",
    "SELECT ?s ?r WHERE { ?s ex:name ?n BIND(SUBSTR(?n, 2, 3) AS ?r) }",
    "SELECT ?s ?a WHERE { VALUES ?s { ex:p1 ex:p2 ex:p3 ex:p999 } ?s ex:age ?a }",
    "SELECT ?s ?c WHERE { VALUES (?s ?c) { (ex:p0 'Paris') (ex:p1 'Paris') (ex:p5 UNDEF) } ?s ex:city ?c }",
    "SELECT ?s ?b WHERE { ?s ex:age ?a BIND(?a > 30 AS ?b) }",
    "SELECT ?s ?x WHERE { ?s ex:height ?h BIND(ROUND(?h * 10) AS ?x) }",
    "SELECT ?s ?x WHERE { ?s ex:age ?a BIND(ABS(?a - 50) AS ?x) FILTER(?x < 3) }",
    "SELECT ?s ?x WHERE { ?s ex:name ?n BIND(STRBEFORE(?n, ' ') AS ?x) }",
    "SELECT ?s ?x WHERE { ?s ex:name ?n BIND(STRAFTER(?n, ' ') AS ?x) }",
    "SELECT ?s ?x WHERE { ?s ex:name ?n BIND(LCASE(?n) AS ?x) FILTER(?x = 'bob') }",
    "SELECT ?x WHERE { ex:p0 ex:name ?n BIND(STRLANG(STR(?n), 'de') AS ?x) }",
    "SELECT ?x WHERE { ex:p1 ex:age ?a BIND(STRDT(STR(?a), xsd:string) AS ?x) }",
    "SELECT ?x WHERE { ex:p1 ex:age ?a BIND(xsd:double(?a) AS ?x) }",
    "SELECT ?x WHERE { ex:p1 ex:age ?a BIND(xsd:string(?a) AS ?x) }",
    "SELECT ?x WHERE { ex:p3 ex:height ?a BIND(xsd:integer(FLOOR(?a)) AS ?x) }",
    "SELECT ?s ?x WHERE { ?s ex:note ?n BIND(ENCODE_FOR_URI(?n) AS ?x) }",
    "SELECT ?s ?x WHERE { ?s ex:city ?n BIND(MD5(?n) AS ?x) }",
    "SELECT ?s ?x WHERE { ?s ex:city ?n BIND(SHA1(?n) AS ?x) }",
    "SELECT ?s ?x WHERE { ?s ex:city ?n BIND(SHA256(?n) AS ?x) }",
    # aggregates
    "SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }",
    "SELECT (COUNT(DISTINCT ?s) AS ?c) WHERE { ?s ex:knows ?o }",
    "SELECT ?t (COUNT(?s) AS ?c) (AVG(?a) AS ?avg) (MIN(?a) AS ?mn) (MAX(?a) AS ?mx) (SUM(?a) AS ?sm) WHERE { ?s ex:type ?t ; ex:age ?a } GROUP BY ?t",
    "SELECT ?c (COUNT(*) AS ?n) WHERE { ?s ex:city ?c } GROUP BY ?c HAVING(COUNT(*) > 50)",
    "SELECT ?c (SAMPLE(?c) AS ?x) WHERE { ?s ex:city ?c } GROUP BY ?c",
    "SELECT ?t (GROUP_CONCAT(DISTINCT ?c; SEPARATOR='|') AS ?x) WHERE { ?s ex:type ?t ; ex:city ?c } GROUP BY ?t",
    "SELECT (SUM(?h) AS ?x) WHERE { ?s ex:height ?h }",
    "SELECT (AVG(?h) AS ?x) WHERE { ?s ex:height ?h }",
    "SELECT ?s (COUNT(?o) AS ?k) WHERE { ?s ex:type ex:Alien OPTIONAL { ?s ex:knows ?o } } GROUP BY ?s",
    "SELECT (COUNT(?a) AS ?x) WHERE { ?s ex:type ex:Robot OPTIONAL { ?s ex:age ?a } }",
    "SELECT ?k (COUNT(*) AS ?n) WHERE { ?s ex:age ?a BIND(?a > 45 AS ?k) } GROUP BY ?k",
    # subqueries / distinct / order / limit
    "SELECT DISTINCT ?c WHERE { ?s ex:city ?c }",
    "SELECT DISTINCT ?t ?c WHERE { ?s ex:type ?t ; ex:city ?c }",
    "SELECT REDUCED ?c WHERE { ?s ex:city ?c } ORDER BY ?c",
    "SELECT ?s ?a WHERE { ?s ex:age ?a } ORDER BY DESC(?a) ?s LIMIT 10",
    "SELECT ?s ?a WHERE { ?s ex:age ?a } ORDER BY ?a ?s LIMIT 5 OFFSET 20",
    "SELECT ?s ?n WHERE { ?s ex:name ?n FILTER(lang(?n) = '') } ORDER BY ?n ?s",
    "SELECT ?s ?n WHERE { ?s ex:name ?n FILTER(lang(?n) != '') } ORDER BY DESC(?n) ?s",
    "SELECT (MAX(?n) AS ?x) (MIN(?n) AS ?y) WHERE { ?s ex:name ?n FILTER(lang(?n) = '') }",
    "SELECT ?s ?h WHERE { ?s ex:height ?h } ORDER BY ?h ?s",
    "SELECT ?t ?c WHERE { { SELECT ?t (COUNT(*) AS ?c) WHERE { ?s ex:type ?t } GROUP BY ?t } } ORDER BY DESC(?c)",
    # property paths
    "SELECT ?x WHERE { ex:Robot ex:sub+ ?x }",
    "SELECT ?x WHERE { ex:Robot ex:sub* ?x }",
    "SELECT ?x WHERE { ?x ex:sub/ex:sub ex:Thing }",
    "SELECT ?x WHERE { ex:Thing ^ex:sub ?x }",
    "SELECT ?x WHERE { ex:p0 ex:knows/ex:knows ?x }",
    "SELECT DISTINCT ?x WHERE { ex:p0 (ex:knows|^ex:knows) ?x }",
    "SELECT ?x WHERE { ex:p0 ex:knows? ?x }",
    "SELECT (COUNT(DISTINCT ?x) AS ?c) WHERE { ex:p0 ex:knows+ ?x }",
    "SELECT ?x WHERE { ex:p1 !(ex:knows|ex:type) ?x }",
    "SELECT ?a ?b WHERE { ?a ex:knows{1} ?b . FILTER(?a = ex:p2) }".replace("{1}", ""),
    # ASK / CONSTRUCT-ish (via SELECT) / misc
    "ASK { ex:p0 ex:type ex:Person }",
    "ASK { ex:p0 ex:type ex:Robot }",
    "ASK { ?s ex:age ?a FILTER(?a > 1000) }",
    "SELECT ?l WHERE { ex:Person ex:label ?l FILTER(lang(?l) = 'fr') }",
    "SELECT ?s WHERE { ?s ex:name 'Bob'@en }",
    "SELECT ?s WHERE { ?s ex:city 'paris' }",
    "SELECT ?s ?n WHERE { ?s ex:note ?n }",
    "SELECT ?s WHERE { ?s ex:height 1.5 }",
    "SELECT ?x WHERE { BIND(7 / 2 AS ?x) }",
    "SELECT ?x WHERE { BIND('a' < 'b' AS ?x) }",
    "SELECT ?x WHERE { BIND(1 = 1.0 AS ?x) }",
    "SELECT ?x WHERE { BIND(STR(ex:p1) AS ?x) }",
    "SELECT ?x WHERE { BIND(IRI(CONCAT(STR(ex:), 'z')) AS ?x) }",
    "SELECT ?x WHERE { BIND(isNumeric(1.5) AS ?x) }",
    "SELECT ?x WHERE { BIND(sameTerm(1, 1.0) AS ?x) }",
]


# Queries where rdflib 7 itself is wrong; GraphRD's answer is checked against the SPARQL 1.1 spec instead.
SPEC = {
    # integer op integer is xsd:integer (SPARQL 1.1 §17.3, XPath op:numeric-add); rdflib returns decimal
    "SELECT ?x WHERE { BIND(1 + 2 * 3 AS ?x) }": [(("N", "int", 7.0),)],
    # a BIND error leaves the variable unbound but keeps the row (§18.6 Extend); rdflib drops the row
    "SELECT ?x WHERE { BIND(1/0 AS ?x) }": [(None,)],
    # "Paris" and "Paris"^^xsd:string are the same term in RDF 1.1; rdflib treats them as different
    "SELECT (COUNT(*) AS ?c) WHERE { ?s ex:city 'Paris'^^xsd:string }": [(("N", "int", 60.0),)],
    # comparing a string with a number is a type error, so FILTER removes every row (§17.3); rdflib keeps them
    "SELECT ?s WHERE { ?s ex:name ?n FILTER(?n > 5) }": [],
    # MAX over IRIs returns the IRI (§18.5.1.6); rdflib turns it into a string literal
    "SELECT ?s ?m WHERE { { SELECT ?s (MAX(?o) AS ?m) WHERE { ?s ex:knows ?o } GROUP BY ?s } FILTER(?s = ex:p0) }": "max-iri",
    # the subquery result joins on ?a (§18.2.1 scope); rdflib ignores the join and returns every person
    "SELECT ?s WHERE { ?s ex:age ?a { SELECT (MAX(?x) AS ?a) WHERE { ?y ex:age ?x } } }": "max-join",
    # integer arithmetic stays integer; rdflib types ?a*2+1 as decimal
    "SELECT (COUNT(*) AS ?c) WHERE { ?s ex:age ?a BIND(?a * 2 + 1 AS ?d) FILTER(datatype(?d) = xsd:integer) }": [(("N", "int", 257.0),)],
}
# Results the spec leaves open: compared after normalising the open part.
UNORDERED_CONCAT = "GROUP_CONCAT"


def norm_term(t):
    """A comparable form of a term (rdflib node or SPARQL-JSON dict)."""
    if t is None:
        return None
    if isinstance(t, dict):
        if t["type"] == "uri":
            return ("I", t["value"])
        if t["type"] == "bnode":
            return ("B",)
        dt = t.get("datatype") or ("http://www.w3.org/1999/02/22-rdf-syntax-ns#langString" if "xml:lang" in t else XS + "string")
        return lit(t["value"], dt, t.get("xml:lang"))
    if isinstance(t, URIRef):
        return ("I", str(t))
    if isinstance(t, BNode):
        return ("B",)
    dt = str(t.datatype) if t.datatype else ("http://www.w3.org/1999/02/22-rdf-syntax-ns#langString" if t.language else XS + "string")
    return lit(str(t), dt, t.language)


NUMERIC = {XS + x for x in ("integer", "decimal", "double", "float", "int", "long")}


def lit(v, dt, lang):
    if dt in NUMERIC:
        try:
            # compare numbers by value with a tolerance-friendly rounding; the type family matters
            fam = "int" if dt in (XS + "integer", XS + "int", XS + "long") else ("dec" if dt == XS + "decimal" else "dbl")
            return ("N", fam, round(float(Decimal(v.replace("E", "e"))), 9))
        except (InvalidOperation, ValueError):
            return ("L", v, dt)
    return ("L", v, dt, (lang or "").lower())


UPDATES = [
    "INSERT DATA { ex:new1 ex:name 'New' ; ex:age 5 }",
    "INSERT DATA { GRAPH ex:g9 { ex:new2 ex:knows ex:new1 } }",
    "DELETE DATA { ex:p0 ex:city 'Paris' }",
    "DELETE WHERE { ?s ex:type ex:Alien ; ex:city 'Lima' }",
    "DELETE { ?s ex:age ?a } INSERT { ?s ex:age ?b } WHERE { ?s ex:age ?a FILTER(?a < 10) BIND(?a + 100 AS ?b) }",
    "INSERT { GRAPH ex:old { ?s ex:isOld true } } WHERE { ?s ex:age ?a FILTER(?a > 85) }",
    "WITH ex:g1 DELETE { ?s ex:city ?c } WHERE { ?s ex:city ?c }",
    "DELETE { GRAPH ?g { ?s ex:knows ?o } } WHERE { GRAPH ?g { ?s ex:knows ?o FILTER(?o = ex:p5) } }",
    "COPY ex:g2 TO ex:g7",
    "MOVE ex:g7 TO ex:g8",
    "ADD ex:g8 TO ex:g9",
    "CLEAR GRAPH ex:g8",
    "DROP SILENT GRAPH ex:nonexistent",
    "INSERT DATA { ex:s ex:p 'a'@en, 'a'@EN, 'a', 1, 1.0, '01'^^xsd:integer }",
    "DELETE DATA { ex:s ex:p 'a'@en }",
    "INSERT { ?s ex:count ?c } WHERE { { SELECT ?s (COUNT(?o) AS ?c) WHERE { ?s ex:knows ?o } GROUP BY ?s } }",
    "DELETE { ?s ?p ?o } WHERE { ?s ?p ?o FILTER(?s = ex:p1) }",
    "CLEAR DEFAULT",
]
# checked after the updates: g2's triples now live in g2 and g9, but the default graph is a set
AFTER = [
    "SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }",
    "SELECT ?s (COUNT(?o) AS ?c) WHERE { ?s ex:knows ?o } GROUP BY ?s",
    "SELECT (COUNT(*) AS ?c) WHERE { GRAPH ?g { ?s ?p ?o } }",
    "SELECT (COUNT(*) AS ?c) FROM ex:g2 FROM ex:g9 WHERE { ?s ?p ?o }",
    "SELECT (COUNT(*) AS ?c) WHERE { ?s ex:knows/ex:knows ?o }",
]


def dump_ours(srv, repo):
    res = srv.query(repo, "SELECT ?s ?p ?o ?g WHERE { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }")
    rows, _ = ours_rows(res)
    return sorted(map(repr, rows))


def dump_ref(ds):
    rows = []
    for s, p, o, g in ds.quads((None, None, None, None)):
        gid = getattr(g, "identifier", g)
        named = isinstance(gid, URIRef) and str(gid) != "urn:x-rdflib:default"
        rows.append((norm_term(s), norm_term(p), norm_term(o), norm_term(URIRef(str(gid))) if named else None))
    ext = [r for r in rows if r[3] is None]
    # the union default graph shows each named-graph triple once without a graph, too
    ext += [(a, b, c, None) for a, b, c, g in rows if g is not None]
    return sorted(set(map(repr, ext)) | set(map(repr, rows)))


def ours_rows(res):
    if "boolean" in res:
        return [res["boolean"]], None
    vars_ = res["head"]["vars"]
    return [tuple(norm_term(b.get(v)) for v in vars_) for b in res["results"]["bindings"]], vars_


def ref_rows(r, vars_):
    if r.type == "ASK":
        return [r.askAnswer]
    vs = vars_ or [str(v) for v in r.vars]
    return [tuple(norm_term(row[v]) if v in row.labels else None for v in vs) for row in r]


def main():
    rep = Report("differential SPARQL vs rdflib")
    data = gen_data()
    ds = Dataset(default_union=True)
    ds.parse(data=data, format="nquads")
    srv = Server().start()
    try:
        srv.create_repo("diff")
        srv.load("diff", data)
        rep.check(srv.size("diff") == len(ds), f"size {srv.size('diff')} vs {len(ds)}")
        for q in QUERIES:
            full = P + q
            try:
                ours, vars_ = ours_rows(srv.query("diff", full))
            except Exception as e:
                rep.check(False, f"{q}\n      GraphRD error: {e}")
                continue
            try:
                ref = ref_rows(ds.query(full), vars_)
            except Exception as e:
                print(f"  skip (rdflib error {type(e).__name__}): {q}")
                continue
            if UNORDERED_CONCAT in q:
                fix = lambda rows: [tuple((t[0], "|".join(sorted(t[1].split("|"))), *t[2:]) if t and t[0] == "L" else t for t in r) for r in rows]
                ours, ref = fix(ours), fix(ref)
            ordered = "ORDER BY" in q and "LIMIT" not in q.split("ORDER BY")[0]
            same = ours == ref if ordered else sorted(map(repr, ours)) == sorted(map(repr, ref))
            if not same:
                so, sr = set(map(repr, ours)), set(map(repr, ref))
                rep.check(False, f"{q}\n      ours={len(ours)} ref={len(ref)}\n      only ours: {sorted(so - sr)[:3]}\n      only ref:  {sorted(sr - so)[:3]}")
            else:
                rep.check(True, q)
        srv.create_repo("upd")
        srv.load("upd", data)
        ds2 = Dataset(default_union=True)
        ds2.parse(data=data, format="nquads")
        for u in UPDATES:
            st, b = srv.update("upd", P + u)
            if not rep.check(st in (200, 204), f"update {u}: HTTP {st} {b[:200]}"):
                continue
            if u == "CLEAR DEFAULT":
                # union mode in a reference library clears every graph; the spec clears only the default graph
                for gr in list(ds2.contexts()):
                    if not (isinstance(gr.identifier, URIRef) and str(gr.identifier) != "urn:x-rdflib:default"):
                        ds2.remove_graph(gr)
            else:
                ds2.update(P + u)
            a, r = set(dump_ours(srv, "upd")), set(dump_ref(ds2))
            rep.check(a == r, f"after {u}\n      only ours: {sorted(a - r)[:3]}\n      only ref: {sorted(r - a)[:3]}")
        for q in AFTER:
            ours, vars_ = ours_rows(srv.query("upd", P + q))
            ref = ref_rows(ds2.query(P + q), vars_)
            rep.check(sorted(map(repr, ours)) == sorted(map(repr, ref)), f"after updates: {q}\n      ours={sorted(map(repr, ours))[:3]}\n      ref={sorted(map(repr, ref))[:3]}")
        maxes = {}
        for q, want in SPEC.items():
            try:
                ours, _ = ours_rows(srv.query("diff", P + q))
            except Exception as e:
                rep.check(False, f"{q}\n      GraphRD error: {e}")
                continue
            if want == "max-iri":
                knows = [o for (s, p, o, g) in ds.quads((URIRef(EX + "p0"), URIRef(EX + "knows"), None, None))]
                want = [(("I", EX + "p0"), ("I", max(map(str, knows))))] if knows else []
            elif want == "max-join":
                top = max(int(o) for (s, p, o, g) in ds.quads((None, URIRef(EX + "age"), None, None)))
                want = sorted({(("I", str(s)),) for (s, p, o, g) in ds.quads((None, URIRef(EX + "age"), None, None)) if int(o) == top})
                ours = sorted(set(ours))
            rep.check(ours == want, f"spec: {q}\n      got {ours[:3]} want {want[:3]}")
    finally:
        srv.stop()
    rep.done()


if __name__ == "__main__":
    main()
