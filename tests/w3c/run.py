"""W3C conformance runner: the official RDF and SPARQL test suites (https://github.com/w3c/rdf-tests)
against GraphRD. Usage:

    git clone --depth 1 https://github.com/w3c/rdf-tests /path/to/rdf-tests
    python3 tests/w3c/run.py /path/to/rdf-tests [suite ...] [--verbose] [--only NAME]

Suites: turtle ntriples nquads trig rdfxml sparql-query sparql-update sparql-syntax (default: all).
RDF syntax tests run through `graphrd convert`; SPARQL tests run against a GraphRD server, one fresh
repository per test, with a separate (non-union) default graph as the test suites expect.
rdflib is used only to read manifests and expected results."""
import csv, io, json, os, re, subprocess, sys, tempfile, urllib.parse
from collections import Counter

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "prod"))
from common import BIN, Server  # noqa: E402

import rdflib  # noqa: E402

rdflib.NORMALIZE_LITERALS = False  # keep lexical forms exactly as written in the expected files
from rdflib import BNode, Graph, Literal, Namespace, URIRef  # noqa: E402
from rdflib.compare import isomorphic  # noqa: E402
from rdflib.namespace import RDF, RDFS  # noqa: E402

MF = Namespace("http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#")
QT = Namespace("http://www.w3.org/2001/sw/DataAccess/tests/test-query#")
UT = Namespace("http://www.w3.org/2009/sparql/tests/test-update#")
RDFT = Namespace("http://www.w3.org/ns/rdftest#")
SD = Namespace("http://www.w3.org/ns/sparql-service-description#")
DAWGT = Namespace("http://www.w3.org/2001/sw/DataAccess/tests/test-dawg#")
RS = Namespace("http://www.w3.org/2001/sw/DataAccess/tests/result-set#")
BASE = "https://w3c.github.io/rdf-tests/"

SUITES = {
    "service": "sparql/sparql11/service/manifest.ttl",
    "entailment": "sparql/sparql11/entailment/manifest.ttl",
    "ntriples": "rdf/rdf11/rdf-n-triples/manifest.ttl",
    "nquads": "rdf/rdf11/rdf-n-quads/manifest.ttl",
    "turtle": "rdf/rdf11/rdf-turtle/manifest.ttl",
    "trig": "rdf/rdf11/rdf-trig/manifest.ttl",
    "rdfxml": "rdf/rdf11/rdf-xml/manifest.ttl",
    "sparql-query": "sparql/sparql11/manifest-sparql11-query.ttl",
    "sparql-update": "sparql/sparql11/manifest-sparql11-update.ttl",
    "sparql10": "sparql/sparql10/manifest-evaluation.ttl",
    "sparql-syntax": "sparql/sparql10/manifest-syntax.ttl",
    "sparql-results": "sparql/sparql11/manifest-sparql11-results.ttl",
    # RDF 1.2 / SPARQL 1.2 (W3C drafts)
    "ntriples12": "rdf/rdf12/rdf-n-triples/manifest.ttl",
    "nquads12": "rdf/rdf12/rdf-n-quads/manifest.ttl",
    "turtle12": "rdf/rdf12/rdf-turtle/manifest.ttl",
    "trig12": "rdf/rdf12/rdf-trig/manifest.ttl",
    "rdfxml12": "rdf/rdf12/rdf-xml/manifest.ttl",
    "sparql12": "sparql/sparql12/manifest.ttl",
}
RDFC = "rdfc10"
N3 = "n3"
JSONLD = "jsonld-toRdf"


class Runner:
    def __init__(self, root, verbose=False, only=None):
        self.root, self.verbose, self.only = root, verbose, only
        self.results = {}  # suite -> Counter
        self.failures = []
        self.server = None
        self.n = 0

    # ---------------------------------------------------------------- manifests
    def local(self, iri):
        iri = str(iri)
        return os.path.join(self.root, iri[len(BASE):]) if iri.startswith(BASE) else None

    def tests(self, manifest_rel):
        """(test node, graph) for every entry of a manifest and the manifests it includes."""
        seen, todo = set(), [BASE + manifest_rel]
        while todo:
            iri = todo.pop(0)
            if iri in seen:
                continue
            seen.add(iri)
            path = self.local(iri)
            if not path or not os.path.exists(path):
                continue
            g = Graph()
            g.parse(path, format="turtle", publicID=iri)
            for m in g.subjects(RDF.type, MF.Manifest):
                for inc in g.objects(m, MF.include):
                    todo.extend(str(x) for x in rdflib.collection.Collection(g, inc))
                for lst in g.objects(m, MF.entries):
                    for t in rdflib.collection.Collection(g, lst):
                        yield t, g

    # ---------------------------------------------------------------- reporting
    def record(self, suite, name, outcome, detail=""):
        self.results.setdefault(suite, Counter())[outcome] += 1
        if outcome == "fail":
            self.failures.append((suite, name, detail))
            if self.verbose:
                print(f"  FAIL {suite} {name}: {detail[:400]}", flush=True)

    # ---------------------------------------------------------------- RDF syntax
    def convert(self, path, base, fmt):
        out_fmt = "nq" if fmt in ("nq", "trig") else "nt"
        p = subprocess.run([BIN, "convert", "--from", fmt, "--to", out_fmt, "--base", base, path], capture_output=True, text=True, timeout=30)
        return p.returncode == 0, p.stdout, p.stderr

    def rdfc_suite(self, root):
        """RDF Dataset Canonicalization (w3c/rdf-canon tests, cloned next to rdf-tests)."""
        base = "https://w3c.github.io/rdf-canon/tests/"
        g = Graph()
        g.parse(os.path.join(root, "manifest.ttl"), format="turtle", publicID=base + "manifest.ttl")
        man = next(g.subjects(RDF.type, MF.Manifest))
        for t in rdflib.collection.Collection(g, g.value(man, MF.entries)):
            name = str(g.value(t, MF.name) or t)
            kind = str(g.value(t, RDF.type)).rsplit("#", 1)[-1]
            path = lambda iri: os.path.join(root, str(iri)[len(base):])
            action = path(g.value(t, MF.action))
            mode = "rdfc-map" if kind == "RDFC10MapTest" else "rdfc"
            alg = g.value(t, URIRef("https://w3c.github.io/rdf-canon/tests/vocab#hashAlgorithm"))
            p = subprocess.run([BIN, "convert", "--from", "nq", "--to", mode, "--hash", str(alg or "SHA256"), action], capture_output=True, text=True, timeout=120)
            if kind == "RDFC10NegativeEvalTest":
                self.record(RDFC, name, "pass" if p.returncode != 0 else "fail", "accepted a poison dataset")
                continue
            if p.returncode != 0:
                self.record(RDFC, name, "fail", p.stderr)
                continue
            expected = open(path(g.value(t, MF.result)), encoding="utf-8").read()
            same = json.loads(p.stdout) == json.loads(expected) if mode == "rdfc-map" else p.stdout == expected
            self.record(RDFC, name, "pass" if same else "fail", f"ours:\n{p.stdout[:800]}\nexpected:\n{expected[:800]}")

    def jsonld_suite(self, root):
        """JSON-LD 1.1 toRdf tests (w3c/json-ld-api, cloned next to rdf-tests)."""
        import nq12
        base = "https://w3c.github.io/json-ld-api/tests/"
        env = dict(os.environ, GRAPHRD_JSONLD_MAP=f"{base}={root}/")
        man = json.load(open(os.path.join(root, "toRdf-manifest.jsonld")))
        for t in man["sequence"]:
            name = t["@id"] + " " + t.get("name", "")
            if self.only and self.only not in name:
                continue
            opt = t.get("option", {})
            if opt.get("specVersion") == "json-ld-1.0":
                self.record(JSONLD, name, "skip")  # JSON-LD 1.0 only
                continue
            cmd = [BIN, "convert", "--from", "jsonld", "--to", "nq", "--base", opt.get("base", base + t["input"]), os.path.join(root, t["input"])]
            for k in ("processingMode", "rdfDirection"):
                if k in opt:
                    cmd += ["--jsonld", f"{k}={opt[k]}"]
            if opt.get("produceGeneralizedRdf"):
                cmd += ["--jsonld", "produceGeneralizedRdf=true"]
            if "expandContext" in opt:
                cmd += ["--jsonld", f"expandContext={os.path.join(root, opt['expandContext'])}"]
            p = subprocess.run(cmd, capture_output=True, text=True, timeout=60, env=env)
            types = t["@type"]
            if "jld:NegativeEvaluationTest" in types:
                self.record(JSONLD, name, "pass" if p.returncode != 0 else "fail", f"accepted; expected {t.get('expectErrorCode')}\n{p.stdout[:300]}")
                continue
            if p.returncode != 0:
                self.record(JSONLD, name, "fail", p.stderr[:500])
                continue
            if "jld:PositiveSyntaxTest" in types:
                self.record(JSONLD, name, "pass")
                continue
            expected = open(os.path.join(root, t["expect"]), encoding="utf-8").read()
            same = nq12.isomorphic(nq12.parse(p.stdout), nq12.parse(expected))
            self.record(JSONLD, name, "pass" if same else "fail", f"ours:\n{p.stdout[:700]}\nexpected:\n{expected[:700]}")

    def n3_suite(self, root):
        """W3C N3 parser tests (grammar and extended grammar); formulas are blank-node graphs."""
        base = "https://w3c.github.io/N3/tests/N3Tests/"
        for manifest in ("manifest-parser.ttl", "manifest-extended.ttl"):
            g = Graph()
            g.parse(os.path.join(root, manifest), format="turtle", publicID=base + manifest)
            for t in g.subjects(RDF.type, None):
                kind = str(g.value(t, RDF.type)).rsplit("#", 1)[-1]
                if not kind.startswith("TestN3") or "Reason" in kind:
                    continue
                name = f"{manifest.split('-')[1].split('.')[0]}/{g.value(t, MF.name) or t}"
                if self.only and self.only not in name:
                    continue
                if g.value(t, RDFT.approval) == RDFT.Rejected:
                    self.record(N3, name, "skip")
                    continue
                action = str(g.value(t, MF.action))
                path = os.path.join(root, action[len(base):])
                p = subprocess.run([BIN, "convert", "--from", "n3", "--to", "nq", "--n3", "variables=true", "--base", action, path], capture_output=True, text=True, timeout=30)
                if "Negative" in kind:
                    self.record(N3, name, "pass" if p.returncode != 0 else "fail", f"accepted invalid input:\n{p.stdout[:300]}")
                elif "Positive" in kind:
                    self.record(N3, name, "pass" if p.returncode == 0 else "fail", p.stderr)
                else:
                    if p.returncode != 0:
                        self.record(N3, name, "fail", p.stderr)
                        continue
                    result = str(g.value(t, MF.result))
                    rpath = os.path.join(root, result[len(base):])
                    if rpath.endswith(".n3"):
                        q = subprocess.run([BIN, "convert", "--from", "n3", "--to", "nq", "--n3", "variables=true", "--base", result, rpath], capture_output=True, text=True, timeout=30)
                        expected = q.stdout
                    else:
                        expected = open(rpath, encoding="utf-8").read()
                    import nq12
                    try:
                        same = nq12.isomorphic(nq12.parse(p.stdout), nq12.parse(expected))
                    except Exception as e:
                        same = False
                    self.record(N3, name, "pass" if same else "fail", f"ours:\n{p.stdout[:700]}\nexpected:\n{expected[:700]}")

    def rdf_suite(self, suite):
        fmt = {"ntriples": "nt", "nquads": "nq", "turtle": "ttl", "trig": "trig", "rdfxml": "rdf"}[suite.removesuffix("12")]
        for t, g in self.tests(SUITES[suite]):
            name = str(g.value(t, MF.name) or t)
            if self.only and self.only not in name:
                continue
            if g.value(t, RDFT.approval) == RDFT.Rejected:
                self.record(suite, name, "skip")
                continue
            kind = str(g.value(t, RDF.type)).rsplit("#", 1)[-1]
            action = g.value(t, MF.action)
            path = self.local(action)
            ok, out, err = self.convert(path, str(action), fmt)
            if "Negative" in kind:
                self.record(suite, name, "pass" if not ok else "fail", f"accepted invalid input:\n{out[:300]}")
            elif "Positive" in kind:
                self.record(suite, name, "pass" if ok else "fail", err)
            elif "Eval" in kind:
                if not ok:
                    self.record(suite, name, "fail", err)
                    continue
                expected = self.local(g.value(t, MF.result))
                same, why = self.same_dataset(out, "nquads", expected)
                self.record(suite, name, "pass" if same else "fail", why)
            else:
                self.record(suite, name, "skip")

    def to_nquads(self, path):
        """Read an expected RDF file with GraphRD's own (W3C-conformant) parsers: rdflib cannot
        read RDF 1.2."""
        if path.endswith((".nt", ".nq")):
            return open(path, encoding="utf-8").read()
        fmt = {"ttl": "ttl", "trig": "trig", "rdf": "rdf", "nq": "nq", "nt": "nt"}.get(path.rsplit(".", 1)[-1], "ttl")
        p = subprocess.run([BIN, "convert", "--from", fmt, "--to", "nq", "--base", BASE + os.path.relpath(path, self.root), path], capture_output=True, text=True, timeout=30)
        if p.returncode != 0:
            raise RuntimeError(p.stderr)
        return p.stdout

    def same_dataset(self, ours_text, ours_fmt, expected_path):
        if ours_fmt in ("nquads", "nt") and ("<<(" in ours_text or not expected_path.endswith((".nt", ".nq"))):
            import nq12
            try:
                ours = nq12.parse(ours_text)
                exp = nq12.parse(self.to_nquads(expected_path))
            except Exception as e:
                return False, f"cannot compare: {e}"
            if nq12.isomorphic(ours, exp):
                return True, ""
            return False, f"ours={sorted(map(repr, ours))[:4]}\nexpected={sorted(map(repr, exp))[:4]}"
        if expected_path.endswith((".nt", ".nq")) and ours_fmt in ("nquads", "nt"):
            # our own reader: rdflib cannot read RDF 1.2 triple terms
            import nq12
            try:
                ours = nq12.parse(ours_text)
                exp = nq12.parse(open(expected_path, encoding="utf-8").read())
            except Exception as e:
                return False, f"cannot compare: {e}"
            if nq12.isomorphic(ours, exp):
                return True, ""
            return False, f"ours={sorted(map(repr, ours))[:4]}\nexpected={sorted(map(repr, exp))[:4]}"
        return self.same_dataset_rdflib(ours_text, ours_fmt, expected_path)

    def same_dataset_rdflib(self, ours_text, ours_fmt, expected_path):
        # language tags are case-insensitive (RDF 1.1 allows normalising them to lower case)
        ours_text = re.sub(r'"@([A-Za-z0-9-]+)', lambda m: '"@' + m.group(1).lower(), ours_text)
        try:
            ours = rdflib.Dataset()
            ours.parse(data=ours_text, format=ours_fmt)
            exp = rdflib.Dataset()
            fmt = "nquads" if expected_path.endswith(".nq") else ("nt" if expected_path.endswith(".nt") else rdflib.util.guess_format(expected_path))
            text = open(expected_path, encoding="utf-8").read()
            if fmt in ("nt", "nquads"):
                text = re.sub(r'"@([A-Za-z0-9-]+)', lambda m: '"@' + m.group(1).lower(), text)
            exp.parse(data=text, format=fmt, publicID=BASE + os.path.relpath(expected_path, self.root))
        except Exception as e:
            return False, f"cannot compare: {e}"
        def graphs(ds):
            out = {}
            for s, p, o, c in ds.quads((None, None, None, None)):
                key = None if c is None or str(getattr(c, "identifier", c)) == "urn:x-rdflib:default" else str(getattr(c, "identifier", c))
                out.setdefault(key, Graph()).add((s, p, o))
            return out
        a, b = graphs(ours), graphs(exp)
        # graphs named by blank nodes match by content, not by label
        ba = [g for k, g in a.items() if k and k.startswith("N") and len(k) > 30]
        bb = [g for k, g in b.items() if k and k.startswith("N") and len(k) > 30]
        a = {k: v for k, v in a.items() if not (k and k.startswith("N") and len(k) > 30)}
        b = {k: v for k, v in b.items() if not (k and k.startswith("N") and len(k) > 30)}
        if len(ba) != len(bb) or not all(any(isomorphic(x, y) for y in bb) for x in ba):
            return False, f"blank-node-named graphs differ: {len(ba)} vs {len(bb)}"
        if set(a) != set(b):
            return False, f"graphs differ: {sorted(map(str, a))} vs {sorted(map(str, b))}"
        for k in a:
            if not isomorphic(a[k], b[k]):
                ta = sorted(l for l in a[k].serialize(format="nt").splitlines() if l)
                tb = sorted(l for l in b[k].serialize(format="nt").splitlines() if l)
                return False, f"graph {k}: ours={ta[:4]} expected={tb[:4]}"
        return True, ""

    # ---------------------------------------------------------------- server helpers
    def fresh_repo(self, ruleset="empty"):
        self.n += 1
        repo = f"t{self.n}"
        cfg = {"id": repo, "params": {"unionDefaultGraph": {"value": "false"}, "ruleset": {"value": ruleset},
                                      "disableSameAs": {"value": "false" if ruleset.startswith("owl") else "true"}}}
        st, b = self.server.req("POST", "/rest/repositories", json.dumps(cfg).encode(), "application/json")
        assert st in (200, 201), b
        return repo

    def drop_repo(self, repo):
        self.server.req("DELETE", f"/rest/repositories/{repo}")

    def load(self, repo, iri, graph=None):
        path = self.local(iri)
        ext = path.rsplit(".", 1)[-1]
        ctype = {"ttl": "text/turtle", "nt": "application/n-triples", "nq": "application/n-quads", "rdf": "application/rdf+xml", "trig": "application/trig", "jsonld": "application/ld+json"}.get(ext, "text/turtle")
        q = {"baseURI": str(iri)}
        if graph:
            q["context"] = f"<{graph}>"
        with open(path, "rb") as f:
            st, b = self.server.req("POST", f"/repositories/{repo}/statements?" + urllib.parse.urlencode(q), f.read(), ctype)
        if st >= 300:
            raise RuntimeError(f"loading {iri}: {st} {b[:200]!r}")

    # ---------------------------------------------------------------- SPARQL
    def sparql_suite(self, suite):
        for t, g in self.tests(SUITES[suite]):
            name = str(g.value(t, MF.name) or t)
            if self.only and self.only not in name:
                continue
            kind = str(g.value(t, RDF.type)).rsplit("#", 1)[-1]
            if g.value(t, DAWGT.approval) in (DAWGT.Withdrawn, DAWGT.Rejected) or g.value(t, MF.approval) in (DAWGT.Withdrawn, DAWGT.Rejected):
                self.record(suite, name, "skip")
                continue
            if "/entailment/" in str(t) or "/service/" in str(t) or "/service-description/" in str(t) or "/protocol/" in str(t) or "/http-rdf-update/" in str(t) or "/graph-store-protocol/" in str(t):
                self.record(suite, name, "skip")  # need a reasoner config, remote endpoints or a protocol harness
                continue
            try:
                if kind in ("PositiveSyntaxTest", "PositiveSyntaxTest11", "NegativeSyntaxTest", "NegativeSyntaxTest11", "PositiveUpdateSyntaxTest11", "NegativeUpdateSyntaxTest11", "PositiveUpdateSyntaxTest", "NegativeUpdateSyntaxTest"):
                    self.syntax_test(suite, name, kind, g, t)
                elif kind == "QueryEvaluationTest":
                    self.query_test(suite, name, g, t)
                elif kind == "UpdateEvaluationTest":
                    self.update_test(suite, name, g, t)
                elif kind in ("CSVResultFormatTest",):
                    self.query_test(suite, name, g, t)
                else:
                    self.record(suite, name, "skip")
            except Exception as e:
                self.record(suite, name, "fail", f"runner error: {e}")

    def syntax_test(self, suite, name, kind, g, t):
        action = g.value(t, MF.action)
        text = open(self.local(action), encoding="utf-8", errors="replace").read()
        repo = self.fresh_repo()
        try:
            if "Update" in kind:
                st, b = self.server.req("POST", f"/repositories/{repo}/statements", urllib.parse.urlencode({"update": text, "baseURI": str(action)}).encode(), "application/x-www-form-urlencoded")
            else:
                st, b = self.server.req("POST", f"/repositories/{repo}", urllib.parse.urlencode({"query": text, "baseURI": str(action)}).encode(), "application/x-www-form-urlencoded", "application/sparql-results+json")
        finally:
            self.drop_repo(repo)
        body = b.decode(errors="replace")
        syntax_error = st == 400 and ("syntax" in body.lower() or "prefix" in body.lower() or "nested" in body or "complex" in body)
        if "Negative" in kind:
            self.record(suite, name, "pass" if syntax_error else "fail", f"accepted ({st}): {text[:200]}")
        else:
            self.record(suite, name, "pass" if not syntax_error else "fail", f"{body[:200]}\n{text[:300]}")

    # ---------------------------------------------------------------- federation (SERVICE)
    def service_suite(self, suite):
        """SPARQL 1.1 Federated Query: every qt:serviceData endpoint becomes a repository of this
        server holding the endpoint's data, and the endpoint IRIs in the query and the data are
        rewritten to that repository's URL."""
        for t, g in self.tests(SUITES[suite]):
            name = str(g.value(t, MF.name) or t)
            if self.only and self.only not in name:
                continue
            action = g.value(t, MF.action)
            endpoints = {}
            for sd in g.objects(action, QT.serviceData):
                repo = self.fresh_repo()
                endpoints[str(g.value(sd, QT.endpoint))] = (repo, g.value(sd, QT.data))
            subst = {e: f"{self.server.url}/repositories/{r}" for e, (r, _) in endpoints.items()}

            def rewrite(text):
                for e, url in subst.items():
                    text = text.replace(e, url)
                return text
            try:
                for e, (repo, data) in endpoints.items():
                    self.load_text(repo, rewrite(open(self.local(data), encoding="utf-8").read()), data)
                text = rewrite(open(self.local(g.value(action, QT.query)), encoding="utf-8").read())
                repo = self.fresh_repo()
                for d in g.objects(action, QT.data):
                    self.load_text(repo, rewrite(open(self.local(d), encoding="utf-8").read()), d)
                st, b = self.server.req("POST", f"/repositories/{repo}", urllib.parse.urlencode({"query": text}).encode(), "application/x-www-form-urlencoded", "application/sparql-results+json")
                self.drop_repo(repo)
                if st != 200:
                    self.record(suite, name, "fail", f"HTTP {st}: {b[:300]!r}\n{text[:300]}")
                    continue
                answer = b.decode()
                for e, url in subst.items():
                    answer = answer.replace(url, e)  # endpoint IRIs in the answers, as in the test
                ours, exp = parse_json_results(answer), read_expected(self.local(g.value(t, MF.result)))
                same = compare_results(ours, exp, False)
                self.record(suite, name, "pass" if same else "fail", f"ours={show(ours)} expected={show(exp)}\n{text[:400]}")
            except Exception as e:
                self.record(suite, name, "fail", f"runner error: {e}")
            finally:
                for repo, _ in endpoints.values():
                    self.drop_repo(repo)

    def load_text(self, repo, text, iri):
        ext = str(iri).rsplit(".", 1)[-1]
        ctype = {"ttl": "text/turtle", "nt": "application/n-triples", "rdf": "application/rdf+xml"}.get(ext, "text/turtle")
        st, b = self.server.req("POST", f"/repositories/{repo}/statements?" + urllib.parse.urlencode({"baseURI": str(iri)}), text.encode(), ctype)
        if st >= 300:
            raise RuntimeError(f"loading {iri}: {st} {b[:200]!r}")

    # ---------------------------------------------------------------- entailment regimes
    def entailment_suite(self, suite):
        """SPARQL 1.1 Entailment Regimes: each test runs under the weakest regime it lists that a
        GraphRD ruleset implements (RDF, RDFS, D, OWL 2 RL for OWL-RDF-Based); tests that only
        list OWL Direct Semantics or RIF are skipped."""
        rulesets = [("RDF", "rdf"), ("RDFS", "rdfs"), ("D", "rdfs"), ("OWL-RDF-Based", "owl2-rl")]
        for t, g in self.tests(SUITES[suite]):
            name = str(g.value(t, MF.name) or t)
            if self.only and self.only not in name:
                continue
            if g.value(t, DAWGT.approval) in (DAWGT.Withdrawn, DAWGT.Rejected):
                self.record(suite, name, "skip")
                continue
            action = g.value(t, MF.action)
            reg = g.value(action, SD.entailmentRegime)
            listed = [str(x).rsplit("/", 1)[-1] for x in (rdflib.collection.Collection(g, reg) if (reg, RDF.first, None) in g else [reg])]
            ruleset = next((r for k, r in rulesets if k in listed), None)
            if ruleset is None:
                self.record(suite, name, "skip")
                continue
            try:
                self.query_test(suite, name, g, t, ruleset)
            except Exception as e:
                self.record(suite, name, "fail", f"runner error: {e}")

    def query_test(self, suite, name, g, t, ruleset="empty"):
        action = g.value(t, MF.action)
        qiri = g.value(action, QT.query)
        text = open(self.local(qiri), encoding="utf-8").read()
        repo = self.fresh_repo(ruleset)
        try:
            loaded = set()
            for d in g.objects(action, QT.data):
                self.load(repo, d)
            for d in g.objects(action, QT.graphData):
                self.load(repo, d, d)
                loaded.add(str(d))
            # FROM / FROM NAMED documents are loaded as graphs named by their IRI
            for m in re.finditer(r"FROM\s+(?:NAMED\s+)?<([^>]*)>", text, re.I):
                iri = urllib.parse.urljoin(str(qiri), m.group(1))
                if iri not in loaded and self.local(iri) and os.path.exists(self.local(iri)):
                    self.load(repo, URIRef(iri), iri)
                    loaded.add(iri)
            result = g.value(t, MF.result)
            rpath = self.local(result) if result else None
            form = query_form(text)
            if rpath and rpath.endswith((".csv", ".tsv")):
                accept = "text/csv" if rpath.endswith(".csv") else "text/tab-separated-values"
                st, b = self.server.req("POST", f"/repositories/{repo}", urllib.parse.urlencode({"query": text, "baseURI": str(qiri)}).encode(), "application/x-www-form-urlencoded", accept)
                if st != 200:
                    return self.record(suite, name, "fail", f"HTTP {st}: {b[:300]!r}")
                same = text_table(b.decode(), accept) == text_table(open(rpath, encoding="utf-8").read(), accept)
                ours_t, exp_t = text_table(b.decode(), accept), text_table(open(rpath, encoding="utf-8").read(), accept)
                return self.record(suite, name, "pass" if same else "fail", f"header ours={ours_t[0]} expected={exp_t[0]}\nonly ours: {list((ours_t[1] - exp_t[1]).elements())}\nonly expected: {list((exp_t[1] - ours_t[1]).elements())}")
            accept = "application/n-triples" if form in ("CONSTRUCT", "DESCRIBE") else "application/sparql-results+json"
            st, b = self.server.req("POST", f"/repositories/{repo}", urllib.parse.urlencode({"query": text, "baseURI": str(qiri)}).encode(), "application/x-www-form-urlencoded", accept)
            if st != 200:
                return self.record(suite, name, "fail", f"HTTP {st}: {b[:300]!r}\n{text[:300]}")
            if form in ("CONSTRUCT", "DESCRIBE"):
                same, why = self.same_dataset(b.decode(), "nt", rpath)
                return self.record(suite, name, "pass" if same else "fail", why + "\n" + text[:300])
            ours = parse_json_results(b.decode())
            exp = read_expected(rpath)
            ordered = bool(re.search(r"ORDER\s+BY", text, re.I)) and not isinstance(exp, bool)
            same = compare_results(ours, exp, ordered)
            if not same and re.search(r"SELECT\s+REDUCED", text, re.I) and not isinstance(exp, bool):
                # REDUCED may drop any number of duplicates
                same = set(map(normal, ours)) == set(map(normal, exp)) and len(set(map(normal, exp))) <= len(ours) <= len(exp)
            if isinstance(ours, bool) or isinstance(exp, bool):
                detail = f"ours={ours} expected={exp}"
            else:
                co, ce = Counter(map(normal, ours)), Counter(map(normal, exp))
                detail = f"ours={len(ours)} expected={len(exp)}\nonly ours: {[dict(r) for r in (co - ce)][:4]}\nonly expected: {[dict(r) for r in (ce - co)][:4]}"
            self.record(suite, name, "pass" if same else "fail", f"{detail}\n{text[:500]}")
        finally:
            self.drop_repo(repo)

    def update_test(self, suite, name, g, t):
        action = g.value(t, MF.action)
        req = g.value(action, UT.request)
        text = open(self.local(req), encoding="utf-8").read()
        repo = self.fresh_repo()
        try:
            for d in g.objects(action, UT.data):
                self.load(repo, d)
            for gd in g.objects(action, UT.graphData):
                self.load(repo, g.value(gd, UT.graph), g.value(gd, RDFS.label))
            st, b = self.server.req("POST", f"/repositories/{repo}/statements", urllib.parse.urlencode({"update": text, "baseURI": str(req)}).encode(), "application/x-www-form-urlencoded")
            result = g.value(t, MF.result)
            expect_ok = g.value(result, UT.result) != UT.Failure if result is not None else True
            if st >= 300:
                return self.record(suite, name, "pass" if not expect_ok else "fail", f"HTTP {st}: {b[:300]!r}\n{text[:300]}")
            st, dump = self.server.req("GET", f"/repositories/{repo}/statements?infer=false", accept="application/n-quads")
            if "<<(" in dump.decode() or "reif" in text.lower() or "<<" in text:
                import nq12
                exp_text = ""
                for d in g.objects(result, UT.data):
                    exp_text += self.to_nquads(self.local(d))
                for gd in g.objects(result, UT.graphData):
                    gname = str(g.value(gd, RDFS.label))
                    for line in self.to_nquads(self.local(g.value(gd, UT.graph))).splitlines():
                        q = nq12.parse(line)
                        if q:
                            exp_text += line.rstrip().rstrip(".").rstrip() + f" <{gname}> .\n"
                same = nq12.isomorphic(nq12.parse(dump.decode()), nq12.parse(exp_text))
                return self.record(suite, name, "pass" if same else "fail", f"ours:\n{dump.decode()[:600]}\nexpected:\n{exp_text[:600]}")
            ours = rdflib.Dataset()
            ours.parse(data=dump.decode(), format="nquads")
            exp = rdflib.Dataset()
            for d in g.objects(result, UT.data):
                exp.default_context.parse(self.local(d), publicID=str(d), format=rdflib.util.guess_format(self.local(d)))
            for gd in g.objects(result, UT.graphData):
                exp.graph(URIRef(str(g.value(gd, RDFS.label)))).parse(self.local(g.value(gd, UT.graph)), publicID=str(g.value(gd, UT.graph)), format=rdflib.util.guess_format(self.local(g.value(gd, UT.graph))))
            same, why = compare_datasets(ours, exp)
            self.record(suite, name, "pass" if same else "fail", why + "\n" + text[:300])
        finally:
            self.drop_repo(repo)


def query_form(text):
    t = re.sub(r"#[^\n]*", "", text)
    t = re.sub(r"(?is)^\s*((BASE\s*<[^>]*>|PREFIX\s+[^:\s]*:\s*<[^>]*>)\s*)*", "", t)
    m = re.match(r"\s*(SELECT|ASK|CONSTRUCT|DESCRIBE)", t, re.I)
    return m.group(1).upper() if m else "SELECT"


XSD = "http://www.w3.org/2001/XMLSchema#"
NUMERIC = {XSD + t for t in ("integer", "decimal", "float", "double", "int", "long", "short", "byte", "nonNegativeInteger", "positiveInteger", "negativeInteger", "nonPositiveInteger", "unsignedInt", "unsignedLong", "unsignedShort", "unsignedByte")}


def literal_key(value, dt, lang):
    """Numbers compare by datatype and value: computed results may use any valid lexical form."""
    if dt in NUMERIC:
        try:
            from decimal import Decimal
            v = value.strip()
            if dt in (XSD + "float", XSD + "double"):
                num = float(v.replace("INF", "inf")) if v not in ("NaN",) else "NaN"
            else:
                num = Decimal(v)
            return ("N", dt, str(num) if isinstance(num, str) else (num if not isinstance(num, Decimal) else num.normalize()))
        except Exception:
            pass
    return ("L", value, dt, lang)


def term_key(t):
    if t is None:
        return None
    if isinstance(t, BNode):
        return ("B", str(t))
    if isinstance(t, URIRef):
        return ("I", str(t))
    if isinstance(t, Literal):
        return literal_key(str(t), str(t.datatype) if t.datatype else (None if t.language else XSD + "string"), (t.language or "").lower())
    return ("?", str(t))


def json_term(x):
    if x["type"] == "uri":
        return ("I", x["value"])
    if x["type"] == "bnode":
        return ("B", x["value"])
    if x["type"] == "triple":
        v = x["value"]
        return ("T", json_term(v["subject"]), json_term(v["predicate"]), json_term(v["object"]))
    dt = x.get("datatype")
    lang = x.get("xml:lang", "").lower()
    if x.get("its:dir"):
        lang += "--" + x["its:dir"]  # RDF 1.2 base direction, compared as part of the tag
    lang_types = ("http://www.w3.org/1999/02/22-rdf-syntax-ns#langString", "http://www.w3.org/1999/02/22-rdf-syntax-ns#dirLangString")
    return literal_key(x["value"], dt if dt and dt not in lang_types else (None if lang else XSD + "string"), lang)


def parse_json_results(text):
    doc = json.loads(text)
    if "boolean" in doc:
        return doc["boolean"]
    return [{v: json_term(x) for v, x in b.items()} for b in doc["results"]["bindings"]]


ROOT = None


def rebase(rows):
    """Relative IRIs in result files resolve against the test's published URL, not the disk."""
    if isinstance(rows, bool) or ROOT is None:
        return rows
    local = "file://" + os.path.realpath(ROOT) + "/"
    local2 = "file://" + ROOT.rstrip("/") + "/"
    def fix(v):
        if v and v[0] == "I":
            for l in (local, local2):
                if v[1].startswith(l):
                    return ("I", BASE + v[1][len(l):])
        return v
    return [{k: fix(v) for k, v in r.items()} for r in rows]


def read_expected(path):
    return rebase(read_expected_raw(path))


def read_expected_raw(path):
    if path.endswith(".srj"):
        return parse_json_results(open(path, encoding="utf-8").read())
    if path.endswith(".srx"):
        return parse_srx(path)
    elif path.endswith((".ttl", ".rdf", ".nt")):
        g = Graph()
        g.parse(path, format=rdflib.util.guess_format(path))
        rs = g.value(None, RDF.type, RS.ResultSet, any=True) or next(g.subjects(RS.resultVariable, None), None)
        b = g.value(rs, RS.boolean) if rs is not None else None
        if b is not None:
            return bool(b.toPython())
        rows = []
        for sol in g.objects(rs, RS.solution):
            row = {}
            for bind in g.objects(sol, RS.binding):
                row[str(g.value(bind, RS.variable))] = term_key(g.value(bind, RS.value))
            idx = g.value(sol, RS["index"])  # RS.index would be str.index
            rows.append((int(str(idx)) if idx is not None else 0, row))
        # rs:index gives the order of ordered results
        return [r for _, r in sorted(rows, key=lambda x: x[0])]
    else:
        raise RuntimeError(f"unknown result format {path}")
    if res.type == "ASK":
        return bool(res.askAnswer)
    return [{str(v): term_key(row[v]) for v in res.vars if row[v] is not None} for row in res]


def parse_srx(path):
    """SPARQL XML results, read exactly (rdflib drops <result/> rows with no bindings)."""
    import xml.etree.ElementTree as ET
    ns = "{http://www.w3.org/2005/sparql-results#}"
    root = ET.parse(path).getroot()
    b = root.find(f"{ns}boolean")
    if b is not None:
        return b.text.strip() == "true"
    def term(el):
        if el.tag == f"{ns}uri":
            return ("I", el.text or "")
        if el.tag == f"{ns}bnode":
            return ("B", el.text or "")
        if el.tag == f"{ns}literal":
            lang = el.get("{http://www.w3.org/XML/1998/namespace}lang", "").lower()
            d = el.get("{http://www.w3.org/2005/11/its}dir")
            if d:
                lang += "--" + d
            dt = el.get("datatype")
            lang_types = ("http://www.w3.org/1999/02/22-rdf-syntax-ns#langString", "http://www.w3.org/1999/02/22-rdf-syntax-ns#dirLangString")
            return literal_key(el.text or "", dt if dt and dt not in lang_types else (None if lang else XSD + "string"), lang)
        if el.tag == f"{ns}triple":
            part = lambda n: term(list(el.find(f"{ns}{n}"))[0])
            return ("T", part("subject"), part("predicate"), part("object"))
        raise RuntimeError(f"unknown term {el.tag}")
    rows = []
    for r in root.iter(f"{ns}result"):
        row = {}
        for bnd in r.findall(f"{ns}binding"):
            row[bnd.get("name")] = term(list(bnd)[0])
        rows.append(row)
    return rows


def normal(row):
    """Compare numbers and booleans by value where the lexical form may legitimately differ."""
    return tuple(sorted(row.items()))


def has_bnode(t):
    return t is not None and (t[0] == "B" or (t[0] == "T" and any(has_bnode(x) for x in t[1:])))


def compare_results(a, b, ordered):
    if isinstance(a, bool) or isinstance(b, bool):
        return a == b
    if len(a) != len(b):
        return False
    if any(has_bnode(v) for r in a + b for v in r.values()):
        # blank nodes (also inside triple terms): multiset isomorphism over rows
        import nq12
        names = sorted({k for r in a + b for k in r})
        rows = lambda rs: [tuple(r.get(k) for k in names) for r in rs]
        if not nq12.isomorphic(rows(a), rows(b), multiset=True):
            return False
        if ordered:
            mask = lambda t: ("B",) if t and t[0] == "B" else t
            return [tuple(mask(x) for x in r) for r in rows(a)] == [tuple(mask(x) for x in r) for r in rows(b)]
        return True
    has_bnodes = any(v[0] == "B" for r in a + b for v in r.values() if v)
    if not has_bnodes:
        return [normal(r) for r in a] == [normal(r) for r in b] if ordered else Counter(map(normal, a)) == Counter(map(normal, b))
    # blank nodes: look for a consistent renaming (backtracking over candidate rows)
    def strip(r):
        return tuple(sorted((k, v if v[0] != "B" else ("B",)) for k, v in r.items()))
    if Counter(map(strip, a)) != Counter(map(strip, b)):
        return False
    rows_b = list(b)
    def solve(i, mapping, used):
        if i == len(a):
            return True
        for j, rb in enumerate(rows_b):
            if j in used or (ordered and j != i):
                continue
            if strip(a[i]) != strip(rb):
                continue
            m = dict(mapping)
            ok = True
            for k, va in a[i].items():
                vb = rb.get(k)
                if va[0] == "B":
                    if m.get(va[1], vb[1]) != vb[1] or (va[1] not in m and vb[1] in m.values()):
                        ok = False
                        break
                    m[va[1]] = vb[1]
            if ok and solve(i + 1, m, used | {j}):
                return True
        return False
    return solve(0, {}, frozenset())


def compare_datasets(a, b):
    def graphs(ds):
        out = {}
        for s, p, o, c in ds.quads((None, None, None, None)):
            cid = getattr(c, "identifier", c)
            key = None if cid is None or str(cid) == "urn:x-rdflib:default" or isinstance(cid, BNode) else str(cid)
            out.setdefault(key, Graph()).add((s, p, o))
        return {k: v for k, v in out.items() if len(v)}
    ga, gb = graphs(a), graphs(b)
    if set(ga) != set(gb):
        return False, f"graphs: ours={sorted(map(str, ga))} expected={sorted(map(str, gb))}"
    for k in ga:
        if not isomorphic(ga[k], gb[k]):
            ta = sorted(ga[k].serialize(format="nt").splitlines())
            tb = sorted(gb[k].serialize(format="nt").splitlines())
            return False, f"graph {k}: only ours={sorted(set(ta) - set(tb))[:3]} only expected={sorted(set(tb) - set(ta))[:3]}"
    return True, ""


def text_table(text, accept):
    rows = list(csv.reader(io.StringIO(text), delimiter="," if accept == "text/csv" else "\t"))
    head, body = rows[0] if rows else [], rows[1:]
    def norm(c):
        c = re.sub(r"_:\S+", "_:b", c)
        # bare numbers (TSV abbreviates them as in Turtle) compare by value
        if re.fullmatch(r"[+-]?\d+", c):
            return ("int", int(c))
        if re.fullmatch(r"[+-]?\d*\.\d+", c):
            return ("dec", float(c))
        if re.fullmatch(r"[+-]?(\d+\.?\d*|\.\d+)[eE][+-]?\d+", c):
            return ("dbl", float(c))
        return c
    return head, Counter(tuple(norm(c) for c in r) for r in body if r)


def show(r):
    if isinstance(r, bool):
        return r
    return [dict(sorted(x.items())) for x in r[:4]] + (["..."] if len(r) > 4 else [])


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    verbose = "--verbose" in sys.argv
    only = sys.argv[sys.argv.index("--only") + 1] if "--only" in sys.argv else None
    if only in args:
        args.remove(only)
    global ROOT
    root, suites = args[0], args[1:] or ["ntriples", "nquads", "turtle", "trig", "rdfxml", "sparql-syntax", "sparql10", "sparql-query", "sparql-update", "sparql-results",
                                         "ntriples12", "nquads12", "turtle12", "trig12", "rdfxml12", "sparql12"]
    ROOT = root
    r = Runner(root, verbose, only)
    if any(s.startswith("sparql") or s in ("entailment", "service") for s in suites):
        r.server = Server(args=["--sync", "off"]).start()
    try:
        for s in suites:
            if s == JSONLD:
                r.jsonld_suite(os.path.join(os.path.dirname(root.rstrip("/")), "json-ld-api", "tests"))
                c = r.results.get(s, Counter())
                print(f"{s:16s} {c['pass']:5d} / {c['pass'] + c['fail']:5d} passed ({100 * c['pass'] / max(c['pass'] + c['fail'], 1):5.1f}%)  {c['skip']} skipped", flush=True)
                continue
            if s == N3:
                r.n3_suite(os.path.join(os.path.dirname(root.rstrip("/")), "N3", "tests", "N3Tests"))
                c = r.results.get(s, Counter())
                print(f"{s:16s} {c['pass']:5d} / {c['pass'] + c['fail']:5d} passed ({100 * c['pass'] / max(c['pass'] + c['fail'], 1):5.1f}%)  {c['skip']} skipped", flush=True)
                continue
            if s == RDFC:
                r.rdfc_suite(os.path.join(os.path.dirname(root.rstrip("/")), "rdf-canon", "tests"))
                c = r.results.get(s, Counter())
                print(f"{s:16s} {c['pass']:5d} / {c['pass'] + c['fail']:5d} passed ({100 * c['pass'] / max(c['pass'] + c['fail'], 1):5.1f}%)  {c['skip']} skipped", flush=True)
                continue
            (r.entailment_suite if s == "entailment" else r.service_suite if s == "service" else r.sparql_suite if s.startswith("sparql") else r.rdf_suite)(s)
            c = r.results.get(s, Counter())
            total = c["pass"] + c["fail"]
            print(f"{s:16s} {c['pass']:5d} / {total:5d} passed ({100 * c['pass'] / max(total, 1):5.1f}%)  {c['skip']} skipped", flush=True)
    finally:
        if r.server:
            r.server.stop()
    with open(os.path.join(tempfile.gettempdir(), "graphrd-w3c-failures.txt"), "w") as f:
        for s, n, d in r.failures:
            f.write(f"=== {s} :: {n}\n{d}\n\n")
    print(f"failure details: {os.path.join(tempfile.gettempdir(), 'graphrd-w3c-failures.txt')}")
    sys.exit(1 if r.failures else 0)


if __name__ == "__main__":
    main()
