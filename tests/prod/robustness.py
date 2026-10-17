"""Robustness: hostile and malformed input must never crash or wedge the server.
Every case must answer (any status but 5xx for client mistakes) or close the connection,
and the server must keep serving normal requests afterwards."""
import os, random, socket, sys, threading, time
sys.path.insert(0, os.path.dirname(__file__))
from common import Server, Report, free_port

rng = random.Random(3)
EX = "http://ex.org/"


def raw(srv, data, read=True, timeout=10):
    s = socket.create_connection(("127.0.0.1", srv.port), timeout=timeout)
    try:
        s.sendall(data)
        if not read:
            return b""
        s.shutdown(socket.SHUT_WR)
        out = b""
        while True:
            try:
                chunk = s.recv(65536)
            except (ConnectionResetError, socket.timeout):
                break
            if not chunk:
                break
            out += chunk
        return out
    finally:
        s.close()


def post_json(body):
    return b"POST /rest/repositories HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: %d\r\n\r\n%s" % (len(body), body)


def status(resp):
    try:
        return int(resp.split(b" ", 2)[1])
    except Exception:
        return None


def healthy(srv):
    try:
        return srv.query("r", "ASK { ?s ?p ?o }")["boolean"] is True
    except Exception:
        return False


VALID = [
    f"PREFIX ex: <{EX}> SELECT ?s (COUNT(*) AS ?n) WHERE {{ ?s ex:p ?o OPTIONAL {{ ?o ex:q ?z }} FILTER(?o > 3 && regex(str(?s), 'a')) }} GROUP BY ?s HAVING(COUNT(*) > 1) ORDER BY DESC(?n) LIMIT 5",
    f"PREFIX ex: <{EX}> SELECT * WHERE {{ {{ ?s ex:p+ ?o }} UNION {{ ?s ^ex:q/ex:p* ?o }} MINUS {{ ?s ex:r ?o }} BIND(CONCAT(STR(?s), 'x') AS ?c) VALUES ?o {{ 1 2 }} }}",
    f"PREFIX ex: <{EX}> CONSTRUCT {{ ?s ex:x ?o }} WHERE {{ GRAPH ?g {{ ?s ?p ?o }} FILTER NOT EXISTS {{ ?o ?p ?s }} }}",
    f"PREFIX ex: <{EX}> INSERT {{ ?s ex:y ?o }} WHERE {{ ?s ex:p ?o FILTER(?o < 0) }}",
]
TURTLE = f"""@prefix ex: <{EX}> . @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:a ex:p 1, 2.5, "x"@en, "y"^^xsd:string ; ex:q [ ex:r ex:b ; ex:s ( 1 2 3 ) ] .
<< ex:a ex:p 1 >> ex:since "2020"^^xsd:gYear .
"""


def mutate(s):
    s = list(s)
    for _ in range(rng.randint(1, 6)):
        op = rng.random()
        i = rng.randrange(len(s) + 1)
        if op < 0.35 and s:
            del s[min(i, len(s) - 1)]
        elif op < 0.7:
            s.insert(i, rng.choice('{}()<>"\'?.;,*+^|!/\\@#_:-[]0aZ\n\x00é퟿'))
        elif s:
            j = min(i, len(s) - 1)
            s[j:j] = s[max(0, j - 5):j]
    return "".join(s)


def main():
    rep = Report("robustness against hostile input")
    pg = free_port()
    srv = Server(args=["--pg-port", str(pg)]).start()
    srv.create_repo("r")
    srv.load("r", f"<{EX}a> <{EX}p> \"5\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n", "application/n-triples")

    # --- protocol abuse -----------------------------------------------------------------
    cases = {
        "garbage bytes": bytes(rng.randrange(256) for _ in range(4096)),
        "no version": b"GET /\r\n\r\n",
        "bad method": b"BREW /coffee HTTP/1.1\r\nHost: x\r\n\r\n",
        "huge header": b"GET /protocol HTTP/1.1\r\nX: " + b"a" * 2_000_000 + b"\r\n\r\n",
        "many headers": b"GET /protocol HTTP/1.1\r\n" + b"".join(b"H%d: v\r\n" % i for i in range(20000)) + b"\r\n",
        "negative length": b"POST /repositories/r HTTP/1.1\r\nContent-Length: -5\r\n\r\nabc",
        "absurd length": b"POST /repositories/r HTTP/1.1\r\nContent-Length: 99999999999999999999\r\n\r\nabc",
        "length larger than body": b"POST /repositories/r HTTP/1.1\r\nContent-Length: 1000\r\nContent-Type: application/sparql-query\r\n\r\nASK {}",
        "bad chunked": b"POST /repositories/r HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Type: application/sparql-query\r\n\r\nzz\r\nASK\r\n0\r\n\r\n",
        "huge chunk size": b"POST /repositories/r HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\nASK\r\n",
        "bad percent encoding": b"GET /repositories/r?query=%ZZ%%%G1 HTTP/1.1\r\n\r\n",
        "invalid utf8 query": b"GET /repositories/r?query=%FF%FE%80 HTTP/1.1\r\n\r\n",
        "path traversal": b"GET /../../../../etc/passwd HTTP/1.1\r\n\r\n",
        "encoded traversal": b"GET /%2e%2e/%2e%2e/%2e%2e/etc/passwd HTTP/1.1\r\n\r\n",
        "backslash traversal": b"GET /..\\..\\..\\etc\\passwd HTTP/1.1\r\n\r\n",
        "nul in path": b"GET /index.html%00.js HTTP/1.1\r\n\r\n",
        "very long url": b"GET /" + b"a" * 1_000_000 + b" HTTP/1.1\r\n\r\n",
        "pipelined": b"GET /protocol HTTP/1.1\r\n\r\nGET /protocol HTTP/1.1\r\n\r\nGET /nope HTTP/1.1\r\nConnection: close\r\n\r\n",
        "bad json repo config": b"POST /rest/repositories HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 9\r\n\r\n{\"id\":[1,",
        "deep json": b"POST /rest/repositories HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 200000\r\n\r\n" + b"[" * 200000,
        "bad multipart": b"POST /rest/repositories HTTP/1.1\r\nContent-Type: multipart/form-data; boundary=xx\r\nContent-Length: 20\r\n\r\n--xx\r\nbroken--xx--",
        "repo id traversal": post_json(b'{"id":"../../../tmp/evil"}'),
    }
    for name, data in cases.items():
        try:
            resp = raw(srv, data)
        except Exception as e:
            resp = b""
        st = status(resp)
        if "traversal" in name or "nul" in name:
            rep.check(b"root:" not in resp and st != 200 or (st == 200 and b"<html" in resp.lower()[:2000]), f"{name}: leaked a file (status {st})")
        if name == "repo id traversal":
            rep.check(st is not None and 400 <= st < 500, f"{name}: status {st}")
            rep.check(not os.path.exists("/tmp/evil"), f"{name}: created a directory outside the data dir")
        rep.check(st is None or st < 500, f"{name}: server error {st} {resp[:120]!r}")
        rep.check(srv.alive() and healthy(srv), f"server down after: {name}")

    # --- slow clients must not block others ----------------------------------------------
    slow = []
    for _ in range(64):
        s = socket.create_connection(("127.0.0.1", srv.port))
        s.sendall(b"POST /repositories/r HTTP/1.1\r\nContent-Length: 100\r\n\r\nASK")
        slow.append(s)
    t0 = time.time()
    rep.check(healthy(srv), "server unresponsive while 64 clients hold half-sent requests")
    rep.check(time.time() - t0 < 2, f"normal request took {time.time() - t0:.1f}s next to slow clients")
    idle = [socket.create_connection(("127.0.0.1", srv.port)) for _ in range(200)]
    rep.check(healthy(srv), "server unresponsive with 200 idle connections")
    for s in slow + idle:
        s.close()

    # --- resource exhaustion through queries ---------------------------------------------
    deep = {
        "nested groups": "SELECT * WHERE " + "{ " * 50000 + "?s ?p ?o" + " }" * 50000,
        "nested parens": "SELECT * WHERE { ?s ?p ?o FILTER(" + "(" * 100000 + "1" + ")" * 100000 + ") }",
        "nested not": "SELECT * WHERE { ?s ?p ?o FILTER(" + "!" * 100000 + "true) }",
        "long union chain": "SELECT * WHERE { " + " UNION ".join(["{ ?s ?p ?o }"] * 20000) + " }",
        "long path": f"SELECT * WHERE {{ ?s " + "/".join([f"<{EX}p>"] * 20000) + " ?o }",
        "nested path parens": f"SELECT * WHERE {{ ?s " + "(" * 50000 + f"<{EX}p>" + ")" * 50000 + " ?o }",
        "nested subselect": "SELECT * WHERE { " + "{ SELECT * WHERE " * 20000 + "{ ?s ?p ?o }" + " }" * 40000,
        "nested exists": "SELECT * WHERE { ?s ?p ?o " + "FILTER EXISTS { " * 20000 + "?s ?p ?o" + " }" * 20000 + " }",
        "nested collections": "SELECT * WHERE { ?s ?p " + "(" * 50000 + "1" + ")" * 50000 + " }",
        "nested blank nodes": "SELECT * WHERE { ?s ?p " + "[ <http://x> " * 50000 + "1" + " ]" * 50000 + " }",
        "nested quoted triples": "SELECT * WHERE { " + "<< " * 30000 + "?s ?p ?o" + " >> ?p ?o" * 30000 + " }",
        "huge values": "SELECT * WHERE { VALUES ?x { " + " ".join(str(i) for i in range(200000)) + " } }",
        "huge regex": "SELECT * WHERE { ?s ?p ?o FILTER(regex(str(?o), '" + "(a*)*" * 2000 + "b')) }",
        "catastrophic regex": "SELECT * WHERE { BIND('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa!' AS ?x) FILTER(regex(?x, '^(a+)+$')) }",
        "deep turtle nesting": None,
    }
    for name, q in deep.items():
        t0 = time.time()
        try:
            if q is None:
                body = f"<{EX}a> <{EX}p> " + "[ <http://x> " * 50000 + "1" + " ]" * 50000 + " ."
                st, b = srv.req("POST", "/repositories/r/statements", body.encode(), "text/turtle", timeout=30)
            else:
                st, b = srv.req("POST", "/repositories/r", q.encode(), "application/sparql-query", "application/sparql-results+json", timeout=30)
        except Exception as e:
            st, b = None, str(e).encode()
        rep.check(srv.alive(), f"{name}: server crashed")
        if not srv.alive():
            srv.start()
            continue
        rep.check(st is not None and st < 500 or st == 503, f"{name}: status {st} {b[:150]!r} after {time.time() - t0:.1f}s")
        rep.check(healthy(srv), f"server unhealthy after: {name}")

    # --- fuzzed SPARQL and Turtle ---------------------------------------------------------
    crashes = errors = 0
    for i in range(1500):
        q = mutate(rng.choice(VALID))
        try:
            ctype = "application/sparql-update" if "INSERT" in q else "application/sparql-query"
            path = "/repositories/r/statements" if "INSERT" in q else "/repositories/r"
            st, b = srv.req("POST", path, q.encode("utf-8", "surrogatepass"), ctype, timeout=10)
            if st >= 500:
                errors += 1
                if errors <= 5:
                    rep.check(False, f"fuzzed query -> {st}: {q!r} {b[:150]!r}")
        except Exception as e:
            if not srv.alive():
                crashes += 1
                rep.check(False, f"server crashed on fuzzed query {q!r}")
                srv.start()
    for i in range(800):
        t = mutate(TURTLE)
        try:
            st, b = srv.req("POST", "/repositories/r/statements", t.encode("utf-8", "surrogatepass"), "text/turtle", timeout=10)
            if st >= 500:
                errors += 1
                if errors <= 10:
                    rep.check(False, f"fuzzed turtle -> {st}: {t!r} {b[:150]!r}")
        except Exception:
            if not srv.alive():
                crashes += 1
                rep.check(False, f"server crashed on fuzzed turtle {t!r}")
                srv.start()
    print(f"  fuzzing: 2300 inputs, {crashes} crashes, {errors} server errors")
    rep.check(crashes == 0 and errors == 0, "fuzzing clean")

    # --- PostgreSQL port ------------------------------------------------------------------
    for name, data in {"pg garbage": bytes(rng.randrange(256) for _ in range(5000)),
                       "pg huge length": b"\x7f\xff\xff\xff" + b"\x00" * 100,
                       "pg zero length": b"\x00\x00\x00\x00",
                       "pg startup then garbage": b"\x00\x00\x00\x08\x04\xd2\x16\x2f" + b"\x00\x00\x00\x29\x00\x03\x00\x00user\x00a\x00database\x00r\x00\x00" + b"Q\x7f\xff\xff\xffSELECT"}.items():
        try:
            s = socket.create_connection(("127.0.0.1", pg), timeout=5)
            s.sendall(data)
            try:
                s.recv(1000)
            except Exception:
                pass
            s.close()
        except Exception:
            pass
        rep.check(srv.alive() and healthy(srv), f"server down after: {name}")
    srv.stop()
    rep.done()


if __name__ == "__main__":
    main()
