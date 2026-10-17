"""W3C SHACL test suite (https://github.com/w3c/data-shapes, data-shapes-test-suite) against
`graphrd validate`. Usage:

    git clone --depth 1 https://github.com/w3c/data-shapes /path/to/data-shapes
    python3 tests/w3c/shacl.py /path/to/data-shapes [core|sparql ...] [--verbose] [--only NAME]

Each sht:Validate test runs `graphrd validate` on its data and shapes graphs. A report matches when
sh:conforms agrees and the results agree as a multiset of (focus node, result path, value, source
shape, constraint component, severity), plus sh:sourceConstraint where the expected report gives one.
Blank nodes compare as equal to each other (their labels differ between the files and the report);
paths compare by structure. sh:resultMessage and sh:detail are not compared. sht:Failure tests pass
when validation fails. rdflib reads the manifests and the reports."""
import logging, os, subprocess, sys, urllib.parse, warnings
from collections import Counter

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "prod"))
from common import BIN  # noqa: E402

import rdflib  # noqa: E402

rdflib.NORMALIZE_LITERALS = False
# the suite has ill-typed literals on purpose; rdflib logs each one it cannot convert
logging.getLogger("rdflib").setLevel(logging.CRITICAL)
warnings.filterwarnings("ignore")
from rdflib import BNode, Graph, Namespace, URIRef  # noqa: E402
from rdflib.collection import Collection  # noqa: E402
from rdflib.namespace import RDF  # noqa: E402

MF = Namespace("http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#")
SHT = Namespace("http://www.w3.org/ns/shacl-test#")
SH = Namespace("http://www.w3.org/ns/shacl#")


def path_of(uri):
    return urllib.parse.unquote(urllib.parse.urlparse(str(uri)).path)


def load(path):
    g = Graph()
    g.parse(path, format="turtle", publicID="file://" + path)
    return g


def term_key(t):
    return "_:" if isinstance(t, BNode) else (t.n3() if t is not None else None)


def path_key(g, p, depth=0):
    if p is None or depth > 20:
        return None
    if not isinstance(p, BNode):
        return p.n3()
    if (p, RDF.first, None) in g:
        return "(" + " ".join(path_key(g, i, depth + 1) or "?" for i in Collection(g, p)) + ")"
    for kind, mark in [(SH.inversePath, "^"), (SH.zeroOrMorePath, "*"), (SH.oneOrMorePath, "+"), (SH.zeroOrOnePath, "?")]:
        inner = g.value(p, kind)
        if inner is not None:
            return f"{mark}[{path_key(g, inner, depth + 1)}]"
    alt = g.value(p, SH.alternativePath)
    if alt is not None:
        return "|(" + " ".join(path_key(g, i, depth + 1) or "?" for i in Collection(g, alt)) + ")"
    return "_:"


def results(g, report, with_constraint):
    out = Counter()
    for r in g.objects(report, SH.result):
        key = (
            term_key(g.value(r, SH.focusNode)),
            path_key(g, g.value(r, SH.resultPath)),
            term_key(g.value(r, SH.value)),
            term_key(g.value(r, SH.sourceShape)),
            term_key(g.value(r, SH.sourceConstraintComponent)),
            term_key(g.value(r, SH.resultSeverity)),
            term_key(g.value(r, SH.sourceConstraint)) if with_constraint else None,
        )
        out[key] += 1
    return out


def conforms(g, report):
    v = g.value(report, SH.conforms)
    return str(v).lower() in ("true", "1")


def tests_in(manifest_path, seen):
    """(name, test node, graph) for every entry under a manifest, following mf:include."""
    if manifest_path in seen:
        return
    seen.add(manifest_path)
    g = load(manifest_path)
    for m in g.subjects(RDF.type, MF.Manifest):
        for inc in g.objects(m, MF.include):
            yield from tests_in(os.path.realpath(path_of(inc)), seen)
        for lst in g.objects(m, MF.entries):
            for t in Collection(g, lst):
                yield str(t).rsplit("/", 1)[-1], t, g


def run_test(g, t, verbose):
    action = g.value(t, MF.action)
    data = path_of(g.value(action, SHT.dataGraph))
    shapes = path_of(g.value(action, SHT.shapesGraph))
    files = [data] if data == shapes else [data, shapes]
    p = subprocess.run([BIN, "validate", "--shapes", shapes, *files], capture_output=True, text=True, timeout=60)
    expected = g.value(t, MF.result)
    if expected == SHT.Failure:
        return p.returncode != 0, "expected a failure" if p.returncode == 0 else ""
    if p.returncode != 0:
        return False, p.stderr.strip()
    got = Graph()
    got.parse(data=p.stdout, format="turtle")
    report = next(got.subjects(RDF.type, SH.ValidationReport), None)
    with_constraint = any(g.value(r, SH.sourceConstraint) is not None for r in g.objects(expected, SH.result))
    want = results(g, expected, with_constraint)
    have = results(got, report, with_constraint)
    if conforms(g, expected) != conforms(got, report):
        return False, f"conforms: expected {conforms(g, expected)}"
    if want != have:
        missing = want - have
        extra = have - want
        return False, f"missing {sorted(missing.elements(), key=str)}\n      extra {sorted(extra.elements(), key=str)}"
    return True, ""


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    verbose = "--verbose" in sys.argv
    only = sys.argv[sys.argv.index("--only") + 1] if "--only" in sys.argv else None
    if only in args:
        args.remove(only)
    root = args[0]
    tests_dir = os.path.join(root, "data-shapes-test-suite", "tests") if os.path.isdir(os.path.join(root, "data-shapes-test-suite")) else root
    suites = args[1:] or ["core", "sparql"]
    total_pass = total = 0
    for suite in suites:
        passed = failed = 0
        for name, t, g in tests_in(os.path.realpath(os.path.join(tests_dir, suite, "manifest.ttl")), set()):
            if only and only not in name:
                continue
            ok, why = run_test(g, t, verbose)
            if ok:
                passed += 1
            else:
                failed += 1
                print(f"FAIL {suite}/{name}" + (f"\n      {why}" if verbose and why else ""))
        print(f"{suite}: {passed}/{passed + failed} passed")
        total_pass += passed
        total += passed + failed
    print(f"total: {total_pass}/{total} passed ({100 * total_pass / max(total, 1):.1f}%)")
    sys.exit(0 if total_pass == total else 1)


if __name__ == "__main__":
    main()
