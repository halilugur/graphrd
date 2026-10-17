"""Differential test of GraphRD's GeoSPARQL geometry engine against GEOS (via shapely).

    uv run --with shapely python3 tests/prod/geo_diff.py [N]

Random points, lines and polygons on a small integer grid (so touching, shared edges and
collinear overlaps are common) are compared on the DE-9IM matrix and on the areas of the
four set operations.
"""
import os, random, subprocess, sys, tempfile
from shapely import wkt
from shapely.geometry import Point, LineString, Polygon, MultiPolygon, MultiLineString, MultiPoint

N = int(sys.argv[1]) if len(sys.argv) > 1 else 2000
rnd = random.Random(7)

FLOAT = False

def pt():
    if FLOAT:
        return (round(rnd.uniform(0, 6), 3), round(rnd.uniform(0, 6), 3))
    return (rnd.randint(0, 6), rnd.randint(0, 6))

def holed():
    x, y = pt()
    w, h = rnd.randint(3, 5), rnd.randint(3, 5)
    return Polygon([(x, y), (x + w, y), (x + w, y + h), (x, y + h)], [[(x + 1, y + 1), (x + 1, y + h - 1), (x + w - 1, y + h - 1), (x + w - 1, y + 1)]])

def geom():
    k = rnd.random()
    if k < 0.1:
        return holed()
    if k < 0.2:
        parts = [simple() for _ in range(2)]
        if all(p.geom_type == "Polygon" for p in parts) and not parts[0].intersects(parts[1]):
            return MultiPolygon(parts)
        if all(p.geom_type == "LineString" for p in parts):
            return MultiLineString(parts)
        if all(p.geom_type == "Point" for p in parts) and not parts[0].equals(parts[1]):
            return MultiPoint(parts)
    return simple()

def simple():
    k = rnd.random()
    if k < 0.15:
        return Point(pt())
    if k < 0.4:
        while True:
            g = LineString([pt() for _ in range(rnd.randint(2, 4))])
            if g.length > 0:
                return g
    if k < 0.7:
        x, y = pt()
        return Polygon([(x, y), (x + rnd.randint(1, 4), y), (x + rnd.randint(1, 4), y + rnd.randint(1, 4)), (x, y + rnd.randint(1, 4))]).convex_hull
    while True:
        g = Polygon([pt() for _ in range(rnd.randint(3, 6))])
        if g.is_valid and g.area > 0:
            return g

def same(g, ref):
    """Point-set equality (tolerant): nothing of either is outside the other."""
    if g.is_empty or ref.is_empty:
        return g.is_empty and ref.is_empty
    try:
        if g.equals(ref):
            return True
    except Exception:
        pass
    return g.buffer(1e-7).covers(ref) and ref.buffer(1e-7).covers(g) and abs(g.area - ref.area) < 1e-6

cases = []
while len(cases) < N:
    FLOAT = len(cases) >= N * 2 // 3
    a, b = geom(), geom()
    if a.geom_type == "Polygon" and not a.is_valid or b.geom_type == "Polygon" and not b.is_valid:
        continue
    cases.append((a, b))

tmp = tempfile.mkdtemp()
cin, cout = os.path.join(tmp, "cases"), os.path.join(tmp, "out")
with open(cin, "w") as f:
    for a, b in cases:
        f.write(f"{a.wkt}\t{b.wkt}\n")
root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
subprocess.run(["cargo", "test", "--release", "--lib", "geo::differential::dump", "--", "--ignored", "--quiet"],
               cwd=root, env={**os.environ, "GEO_CASES": cin, "GEO_OUT": cout}, check=True, capture_output=True)
ours = open(cout).read().splitlines()
bad = {"relate": 0, "set ops": 0}
degenerate = 0

def near_degenerate(a, b):
    """A vertex of one within 1e-9 of the other's boundary without touching it in binary
    floating point: GEOS decides exactly, GraphRD with a tolerance (and calls it a touch)."""
    from shapely.geometry import MultiPoint
    for x, y in ((a, b), (b, a)):
        pts = MultiPoint([c for g in getattr(x, "geoms", [x]) for c in (g.exterior.coords if g.geom_type == "Polygon" else g.coords)])
        d = pts.distance(y.boundary if y.geom_type != "Point" else y)
        if 0 < d < 1e-9 or (d == 0 and not pts.touches(y.boundary) and not pts.intersects(y.boundary)):
            return True
        for p in pts.geoms:
            if 0 < p.distance(y.boundary if y.geom_type != "Point" else y) < 1e-9:
                return True
    return False
shown = 0
for (a, b), line in zip(cases, ours):
    cols = line.split("\t")
    want = a.relate(b)
    problems = []
    if cols[0] != want and near_degenerate(a, b):
        degenerate += 1
        continue
    if cols[0] != want and shown < 15:
        print(f"RELATE A={a.wkt} B={b.wkt} got {cols[0]} want {want}")
    if cols[0] != want:
        bad["relate"] += 1
        problems.append(f"relate {cols[0]} want {want}")
    for name, got, ref in zip(["intersection", "union", "difference", "symDifference"], cols[1:],
                              [a.intersection(b), a.union(b), a.difference(b), a.symmetric_difference(b)]):
        g = wkt.loads(got)
        if not same(g, ref):
            bad["set ops"] += 1
            problems.append(f"{name}: got {got} want {ref.wkt}")
    if problems and shown < 15:
        shown += 1
        print(f"A={a.wkt}\nB={b.wkt}\n  " + "\n  ".join(problems))
print(f"{N} pairs: relate mismatches {bad['relate']}, set-op mismatches {bad['set ops']}, "
      f"near-degenerate (tolerance vs exact binary) {degenerate}")
sys.exit(1 if any(bad.values()) else 0)
