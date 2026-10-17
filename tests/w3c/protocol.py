"""W3C SPARQL 1.1 Protocol and Service Description tests against GraphRD's standard endpoint
(/sparql). Usage:

    git clone --depth 1 https://github.com/w3c/rdf-tests /path/to/rdf-tests
    uv run --with rdflib python3 tests/w3c/protocol.py /path/to/rdf-tests [--verbose]

Each test gets a fresh repository named "default" (answered at /sparql), loaded with the
test's ut:graphData; its HTTP requests are then sent in order and every response is checked
against mf:expectedStatus, mf:expectedFormat and mf:expectedBoolean."""
import http.client, json, os, sys, urllib.parse

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "prod"))
from common import Server  # noqa: E402

import rdflib  # noqa: E402
from rdflib import Graph, Namespace, URIRef  # noqa: E402
from rdflib.collection import Collection  # noqa: E402
from rdflib.namespace import RDF, RDFS  # noqa: E402

MF = Namespace("http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#")
UT = Namespace("http://www.w3.org/2009/sparql/tests/test-update#")
HT = Namespace("http://www.w3.org/2011/http#")
CNT = Namespace("http://www.w3.org/2011/content#")
HTS = Namespace("http://www.w3.org/2011/http-statusCodes#")
SD = Namespace("http://www.w3.org/ns/sparql-service-description#")

FORMATS = {
    "boolean": ("application/sparql-results+json", "application/sparql-results+xml"),
    "tabular": ("application/sparql-results+json", "application/sparql-results+xml", "text/csv", "text/tab-separated-values"),
    "RDF": ("application/rdf+xml", "text/turtle", "application/n-triples", "text/plain", "application/ld+json", "application/n-quads", "application/trig"),
}


def request(server, method, path, body=None, headers=None):
    # http.client sends exactly the headers given (urllib would add a form Content-Type)
    c = http.client.HTTPConnection("127.0.0.1", server.port, timeout=60)
    c.request(method, path, body=body, headers=headers or {})
    r = c.getresponse()
    out = r.status, r.getheader("content-type", ""), r.read()
    c.close()
    return out


def fresh_repo(server, graph_data, base_dir):
    request(server, "DELETE", "/rest/repositories/default")
    cfg = {"id": "default", "params": {"unionDefaultGraph": {"value": "false"}, "ruleset": {"value": "empty"}}}
    st, _, b = request(server, "POST", "/rest/repositories", json.dumps(cfg).encode(), {"Content-Type": "application/json"})
    assert st in (200, 201), b
    for file, label in graph_data:
        data = open(os.path.join(base_dir, os.path.basename(file)), "rb").read()
        ctx = urllib.parse.quote(f"<{label}>", safe="")
        st, _, b = request(server, "POST", f"/repositories/default/statements?context={ctx}", data, {"Content-Type": "application/n-triples"})
        assert st == 204, b


def boolean_of(ct, body):
    text = body.decode("utf-8")
    if "json" in ct:
        return json.loads(text)["boolean"]
    return "<boolean>true</boolean>" in text


def run_request(server, m, req):
    method = str(m.value(req, HT.methodName))
    path = str(m.value(req, HT.absolutePath)).replace("/sparql/", "/sparql", 1)
    headers = {}
    hl = m.value(req, HT.headers)
    if hl is not None:
        for h in Collection(m, hl):
            headers[str(m.value(h, HT.fieldName))] = str(m.value(h, HT.fieldValue))
    body = None
    b = m.value(req, HT.body)
    if b is not None:
        enc = str(m.value(b, CNT.characterEncoding) or "UTF-8")
        body = str(m.value(b, CNT.chars)).encode("utf-16" if enc.upper() == "UTF-16" else enc)
    status, ct, out = request(server, method, path, body, headers)
    resp = m.value(req, HT.resp)
    want = {str(s).rsplit("StatusCode", 1)[1] for s in m.objects(resp, MF.expectedStatus)}
    if f"{status // 100}xx" not in want:
        return f"{method} {path[:60]} -> {status}, want {sorted(want)}: {out[:200]!r}"
    fmt = m.value(resp, MF.expectedFormat)
    if fmt is not None and not ct.split(";")[0].strip().startswith(FORMATS[str(fmt)]):
        return f"content type {ct!r} is not {fmt}"
    expected = m.value(resp, MF.expectedBoolean)
    if expected is not None and boolean_of(ct, out) != expected.toPython():
        return f"boolean {not expected.toPython()}, want {expected.toPython()}"
    return None


def service_description(server):
    st, ct, body = request(server, "GET", "/sparql", headers={"Accept": "text/turtle"})
    if st != 200:
        return {"returns-rdf": f"status {st}"}
    g = Graph()
    try:
        g.parse(data=body.decode(), format="turtle")
    except Exception as e:  # noqa: BLE001
        return {"returns-rdf": f"not Turtle: {e}"}
    endpoint = URIRef(server.url + "/sparql")
    services = set(g.subjects(RDF.type, SD.Service))
    fails = {}
    if not any((s, SD.endpoint, endpoint) in g for s in services):
        fails["has-endpoint-triple"] = f"no sd:endpoint <{endpoint}>"
    if not services or not all(g.value(s, SD.supportedLanguage) for s in services):
        fails["conforms-to-schema"] = "no sd:Service with sd:supportedLanguage"
    return fails


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    verbose = "--verbose" in sys.argv
    root = os.path.join(args[0], "sparql", "sparql11")
    pdir = os.path.join(root, "protocol")
    m = Graph().parse(os.path.join(pdir, "manifest.ttl"), format="turtle")
    manifest = next(m.subjects(RDF.type, MF.Manifest))
    tests = list(Collection(m, m.value(manifest, MF.entries)))
    server = Server().start()
    passed, failed = 0, []
    for t in tests:
        name = str(t).rsplit("#", 1)[1]
        graph_data = [(str(m.value(gd, UT.graph)), str(m.value(gd, RDFS.label))) for gd in m.objects(t, UT.graphData)]
        fresh_repo(server, graph_data, pdir)
        conn = m.value(t, MF.action)
        err = None
        for req in Collection(m, m.value(conn, HT.requests)):
            err = run_request(server, m, req)
            if err:
                break
        if err:
            failed.append((name, err))
        else:
            passed += 1
    sd_fails = service_description(server)
    for name in ["returns-rdf", "has-endpoint-triple", "conforms-to-schema"]:
        if name in sd_fails:
            failed.append(("sd:" + name, sd_fails[name]))
        else:
            passed += 1
    server.stop()
    for name, err in failed:
        print(f"FAIL {name}: {err}" if verbose else f"FAIL {name}")
    print(f"protocol + service description: {passed}/{passed + len(failed)} passed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    rdflib.NORMALIZE_LITERALS = False
    main()
