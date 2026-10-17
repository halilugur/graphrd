"""Minimal N-Triples / N-Quads 1.2 reader (triple terms <<( )>>, base direction) and dataset
isomorphism with blank nodes, including blank nodes inside triple terms. rdflib cannot read
RDF 1.2, so the W3C runner compares results with this."""
import hashlib
import re
from collections import Counter

XSD_STRING = "http://www.w3.org/2001/XMLSchema#string"
ESC = {"t": "\t", "b": "\b", "n": "\n", "r": "\r", "f": "\f", '"': '"', "'": "'", "\\": "\\"}


class Reader:
    def __init__(self, text):
        self.s, self.i = text, 0

    def ws(self):
        while self.i < len(self.s) and self.s[self.i] in " \t":
            self.i += 1

    def term(self):
        self.ws()
        s = self.s
        if s.startswith("<<(", self.i):
            self.i += 3
            t = (self.term(), self.term(), self.term())
            self.ws()
            assert s.startswith(")>>", self.i), "expected )>>"
            self.i += 3
            return ("T",) + t
        if s.startswith("<<", self.i):  # RDF-star style, accepted for comparison
            self.i += 2
            t = (self.term(), self.term(), self.term())
            self.ws()
            assert s.startswith(">>", self.i)
            self.i += 2
            return ("T",) + t
        c = s[self.i]
        if c == "<":
            j = s.index(">", self.i)
            iri = re.sub(r"\\u([0-9A-Fa-f]{4})|\\U([0-9A-Fa-f]{8})", lambda m: chr(int(m.group(1) or m.group(2), 16)), s[self.i + 1:j])
            self.i = j + 1
            return ("I", iri)
        if c == "_":
            m = re.compile(r"_:([^\s.()<>]+(?:\.+[^\s.()<>]+)*)").match(s, self.i)
            self.i = m.end()
            return ("B", m.group(1))
        if c == '"':
            self.i += 1
            out = []
            while s[self.i] != '"':
                if s[self.i] == "\\":
                    n = s[self.i + 1]
                    if n in "uU":
                        k = 4 if n == "u" else 8
                        out.append(chr(int(s[self.i + 2:self.i + 2 + k], 16)))
                        self.i += 2 + k
                        continue
                    out.append(ESC[n])
                    self.i += 2
                    continue
                out.append(s[self.i])
                self.i += 1
            self.i += 1
            self.ws()
            if s.startswith("@", self.i):
                m = re.compile(r"@([A-Za-z0-9-]+)").match(s, self.i)
                self.i = m.end()
                return ("L", "".join(out), None, m.group(1).lower())
            if s.startswith("^^", self.i):
                self.i += 2
                dt = self.term()
                return ("L", "".join(out), dt[1], "")
            return ("L", "".join(out), XSD_STRING, "")
        raise ValueError(f"unexpected {s[self.i:self.i + 20]!r}")


def parse(text):
    quads = []
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or line.startswith("VERSION"):
            continue
        r = Reader(line)
        s, p, o = r.term(), r.term(), r.term()
        r.ws()
        g = r.term() if r.i < len(line) and line[r.i] in "<_" else None
        quads.append((s, p, o, g))
    return quads


def _bnodes(t, out):
    if t is None:
        return
    if t[0] == "B":
        out.add(t[1])
    elif t[0] == "T":
        for x in t[1:]:
            _bnodes(x, out)


def _subst(t, f):
    if t is None:
        return None
    if t[0] == "B":
        return f(t[1])
    if t[0] == "T":
        return ("T",) + tuple(_subst(x, f) for x in t[1:])
    return t


def isomorphic(a, b, multiset=False):
    """Whether two datasets (sets of quads; or multisets of rows) are equal up to a bijective
    renaming of blank nodes."""
    if not multiset:
        a, b = list(set(a)), list(set(b))
    if len(a) != len(b):
        return False
    ba, bb = set(), set()
    for q in a:
        for t in q:
            _bnodes(t, ba)
    for q in b:
        for t in q:
            _bnodes(t, bb)
    if len(ba) != len(bb):
        return False
    if not ba:
        return Counter(a) == Counter(b)

    def colors(quads, nodes):
        col = {n: "0" for n in nodes}
        for _ in range(6):
            sig = {n: [] for n in nodes}
            for q in quads:
                masked = repr(tuple(_subst(t, lambda x: ("B", col[x])) for t in q))
                found = set()
                for t in q:
                    _bnodes(t, found)
                for n in found:
                    sig[n].append(masked + "|" + repr(tuple(_subst(t, lambda x, n=n: ("B", "*" if x == n else col[x])) for t in q)))
            col = {n: hashlib.sha1("".join(sorted(sig[n])).encode()).hexdigest() for n in nodes}
        return col

    ca, cb = colors(a, ba), colors(b, bb)
    if Counter(ca.values()) != Counter(cb.values()):
        return False
    target = Counter(b)
    order = sorted(ba, key=lambda n: ca[n])

    def solve(k, mapping, used):
        if k == len(order):
            mapped = Counter(tuple(_subst(t, lambda x: ("B", mapping[x])) for t in q) for q in a)
            return mapped == target
        n = order[k]
        for m in bb:
            if m in used or cb[m] != ca[n]:
                continue
            mapping[n] = m
            if solve(k + 1, mapping, used | {m}):
                return True
            del mapping[n]
        return False

    return solve(0, {}, frozenset())
