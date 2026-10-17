"""Search connector against a real OpenSearch (Docker): a repository's people indexed through a
connector, kept in step with inserts, updates and deletes, searched from SPARQL, kept across a
server restart, and removed with its index.

    python3 tests/prod/opensearch.py            (needs Docker; pulls opensearchproject/opensearch)
"""
import json, os, socket, subprocess, sys, time, urllib.error, urllib.parse, urllib.request

sys.path.insert(0, os.path.dirname(__file__))
from common import Server  # noqa: E402

IMAGE = os.environ.get("OPENSEARCH_IMAGE", "opensearchproject/opensearch:2.17.1")
checks = 0


def check(cond, what):
    global checks
    checks += 1
    if not cond:
        print(f"  FAIL {what}")
        sys.exit(1)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def http(method, url, body=None, ctype="application/json"):
    req = urllib.request.Request(url, data=body.encode() if isinstance(body, str) else body, method=method, headers={"Content-Type": ctype} if body is not None else {})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def main():
    print("=== search connector: OpenSearch")
    port = free_port()
    name = f"graphrd-opensearch-{os.getpid()}"
    subprocess.run(["docker", "run", "-d", "--rm", "--name", name, "-p", f"{port}:9200", "-e", "discovery.type=single-node",
                    "-e", "DISABLE_SECURITY_PLUGIN=true", "-e", "OPENSEARCH_JAVA_OPTS=-Xms512m -Xmx512m", IMAGE], check=True, capture_output=True)
    os_url = f"http://127.0.0.1:{port}"
    try:
        for _ in range(180):
            try:
                if http("GET", f"{os_url}/_cluster/health")[0] == 200:
                    break
            except Exception:
                pass
            time.sleep(1)
        else:
            check(False, "OpenSearch started")
        srv = Server().start()
        st, _ = http("POST", f"{srv.url}/rest/repositories", '{"id":"people","params":{"ruleset":{"value":"rdfs-optimized"}}}')
        check(st in (200, 201), "create repository")
        ttl = """@prefix ex: <http://example.org/> . @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
            ex:Student rdfs:subClassOf ex:Person .
            ex:ann a ex:Person ; ex:name "Ann Smith" ; ex:city "Ankara" .
            ex:bob a ex:Student ; ex:name "Bob Stone" ; ex:city "Izmir" .
            ex:acme a ex:Company ; ex:name "Acme Smith Ltd" ."""
        check(http("POST", f"{srv.url}/repositories/people/statements", ttl, "text/turtle")[0] == 204, "load data")
        conn = {"type": "opensearch", "url": os_url, "index": "people-test", "types": ["http://example.org/Person"],
                "fields": [{"name": "name", "property": "http://example.org/name"}, {"name": "city", "property": "http://example.org/city"}]}
        st, body = http("PUT", f"{srv.url}/rest/repositories/people/connectors/people", json.dumps(conn))
        check(st == 201, f"create connector: {st} {body}")

        def search(q):
            sparql = f'SELECT ?e WHERE {{ ?e <http://graphrd.org/connector/people> {json.dumps(q)} }}'
            st, b = http("POST", f"{srv.url}/repositories/people", urllib.parse.urlencode({"query": sparql}), "application/x-www-form-urlencoded")
            check(st == 200, f"search {q}: {st} {b[:200]}")
            return sorted(r["e"]["value"].rsplit("/", 1)[1] for r in json.loads(b)["results"]["bindings"])

        def eventually(q, want):
            got = None
            for _ in range(60):
                got = search(q)
                if got == want:
                    return
                time.sleep(0.25)
            check(False, f"search {q!r}: {got} != {want}")

        eventually("smith", ["ann"])                  # the company is not a Person
        eventually("izmir", ["bob"])                  # a Student is a Person by inference
        eventually("name:stone AND city:izmir", ["bob"])
        upd = 'PREFIX ex: <http://example.org/> DELETE { ex:ann ex:city ?c } INSERT { ex:ann ex:city "Izmir" } WHERE { ex:ann ex:city ?c }'
        check(http("POST", f"{srv.url}/repositories/people/statements", urllib.parse.urlencode({"update": upd}), "application/x-www-form-urlencoded")[0] == 204, "update")
        eventually("izmir", ["ann", "bob"])
        http("POST", f"{srv.url}/repositories/people/statements", 'PREFIX ex: <http://example.org/> INSERT DATA { ex:cem a ex:Person ; ex:name "Cem Ak" }', "application/sparql-update")
        eventually("cem", ["cem"])
        http("POST", f"{srv.url}/repositories/people/statements", "PREFIX ex: <http://example.org/> DELETE WHERE { ex:bob ?p ?o }", "application/sparql-update")
        eventually("izmir", ["ann"])
        # joined with the graph
        sparql = 'SELECT ?n WHERE { ?e <http://graphrd.org/connector/people> "ankara OR izmir" . ?e <http://example.org/name> ?n }'
        st, b = http("POST", f"{srv.url}/repositories/people", urllib.parse.urlencode({"query": sparql}), "application/x-www-form-urlencoded")
        check([r["n"]["value"] for r in json.loads(b)["results"]["bindings"]] == ["Ann Smith"], f"join: {b[:200]}")
        st, b = http("GET", f"{srv.url}/rest/repositories/people/connectors/people")
        status = json.loads(b)
        check(status["indexed"] >= 3 and status["lastError"] is None, f"status: {status}")
        # a restart keeps the connector, and changes made meanwhile are caught up
        srv.stop()
        srv.start()
        http("POST", f"{srv.url}/repositories/people/statements", 'PREFIX ex: <http://example.org/> INSERT DATA { ex:dee a ex:Person ; ex:name "Dee Fox" }', "application/sparql-update")
        eventually("fox", ["dee"])
        check(http("DELETE", f"{srv.url}/rest/repositories/people/connectors/people?dropIndex=true")[0] == 204, "delete connector")
        check(http("GET", f"{os_url}/people-test")[0] == 404, "index dropped")
        st, _ = http("POST", f"{srv.url}/repositories/people", urllib.parse.urlencode({"query": 'SELECT * { ?e <http://graphrd.org/connector/people> "x" }'}), "application/x-www-form-urlencoded")
        check(st >= 400, "a removed connector cannot be queried")
        bad = dict(conn, url="http://127.0.0.1:9")
        check(http("PUT", f"{srv.url}/rest/repositories/people/connectors/broken", json.dumps(bad))[0] == 502, "an unreachable engine is refused")
        srv.cleanup()
        print(f"--- search connector (OpenSearch): PASS, {checks} checks")
    finally:
        subprocess.run(["docker", "rm", "-f", name], capture_output=True)


if __name__ == "__main__":
    main()
