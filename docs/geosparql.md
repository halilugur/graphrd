# GeoSPARQL

GraphRD implements the GeoSPARQL 1.1 functions over geometry literals with a geometry engine of
its own (no GEOS or PROJ). Its topology and set operations are checked against GEOS on random
geometries: DE-9IM matrices and overlays agree on 10,000 pairs (`tests/prod/geo_diff.py`).

## Geometry literals

| Datatype | Example |
|---|---|
| `geo:wktLiteral` | `"POINT(28.97 41.01)"^^geo:wktLiteral`, optionally with a CRS IRI first: `"<http://www.opengis.net/def/crs/EPSG/0/32635> POINT(666370 4541552)"` |
| `geo:geoJSONLiteral` | `"{\"type\":\"Point\",\"coordinates\":[28.97,41.01]}"^^geo:geoJSONLiteral` |

Geometry types: `POINT`, `LINESTRING`, `POLYGON` (with holes), `MULTIPOINT`, `MULTILINESTRING`,
`MULTIPOLYGON`, `GEOMETRYCOLLECTION`, and `EMPTY` forms.

### Coordinate reference systems

A WKT literal without a CRS IRI is in CRS84 (longitude, latitude; the GeoSPARQL default), and so
is every GeoJSON literal. These CRS IRIs are understood as well:

| CRS IRI | System | Axis order |
|---|---|---|
| `http://www.opengis.net/def/crs/OGC/1.3/CRS84` | WGS 84 | longitude, latitude |
| `http://www.opengis.net/def/crs/EPSG/0/4326` (also `4258` ETRS89, `4269` NAD83) | WGS 84 | latitude, longitude, as EPSG defines it |
| `http://www.opengis.net/def/crs/EPSG/0/3857` | Web Mercator | x, y in metres |
| `http://www.opengis.net/def/crs/EPSG/0/326zz`, `327zz` | WGS 84 / UTM zone zz north, south | easting, northing in metres |

Coordinates are converted to CRS84 when a literal is read, so geometries in different systems can
be compared directly; constructed geometries (`envelope`, `intersection`…) come back in CRS84,
while `getSRID` reports the literal's own CRS. ETRS89 and NAD83 are taken as WGS 84 (they differ by
about a metre); the UTM conversion agrees with PROJ to within a millimetre inside a zone.

The prefixes are predefined: `geo:` = `http://www.opengis.net/ont/geosparql#`,
`geof:` = `http://www.opengis.net/def/function/geosparql/`, `uom:` =
`http://www.opengis.net/def/uom/OGC/1.0/`.

## Functions

| Family | Functions |
|---|---|
| Simple Features relations | `sfEquals`, `sfDisjoint`, `sfIntersects`, `sfTouches`, `sfCrosses`, `sfWithin`, `sfContains`, `sfOverlaps` |
| Egenhofer relations | `ehEquals`, `ehDisjoint`, `ehMeet`, `ehOverlap`, `ehCovers`, `ehCoveredBy`, `ehInside`, `ehContains` |
| RCC8 relations | `rcc8eq`, `rcc8dc`, `rcc8ec`, `rcc8po`, `rcc8tppi`, `rcc8tpp`, `rcc8ntpp`, `rcc8ntppi` |
| DE-9IM | `relate(a, b, "T*F**FFF*")` |
| Set operations | `intersection`, `union`, `difference`, `symDifference` |
| Constructions | `envelope`, `convexHull`, `centroid`, `boundary` |
| Properties | `dimension`, `coordinateDimension`, `spatialDimension`, `isEmpty`, `isSimple`, `getSRID`, `numGeometries`, `minX`, `minY`, `maxX`, `maxY` |
| Serialization | `asWKT`, `asGeoJSON` |
| Measures | `distance(a, b, uom)`, `area(g, uom)`, `length(g, uom)`, `perimeter(g, uom)` |

Relations are computed in coordinate space (planar), as in most GeoSPARQL implementations.
Measures are computed on the sphere and returned in the unit given: `uom:metre` (the default),
`uom:kilometre`, `uom:degree` or `uom:radian`. `area` uses the square of the unit (`uom:metre` gives
square metres).

```sparql
PREFIX geo:  <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX uom:  <http://www.opengis.net/def/uom/OGC/1.0/>
PREFIX ex:   <http://example.org/>

SELECT ?place ?km WHERE {
  ?place geo:hasGeometry/geo:asWKT ?wkt .
  FILTER(geof:sfWithin(?wkt, "POLYGON((26 36, 45 36, 45 42, 26 42, 26 36))"^^geo:wktLiteral))
  BIND(geof:distance(?wkt, "POINT(32.85 39.93)"^^geo:wktLiteral, uom:kilometre) AS ?km)
}
ORDER BY ?km
```

## Spatial index

Every repository indexes its geometry literals (WKT and GeoJSON objects) in a quadtree over the
CRS84 plane, kept up to date on every write and by bulk loads; a repository created by an earlier
version builds it once when it is opened. No setting is needed.

A `FILTER` over a basic graph pattern uses the index when one of its conjuncts (`&&`) relates the
geometry variable to a constant geometry:

- any relation function except the disjointness ones (`sfDisjoint`, `ehDisjoint`, `rcc8dc`), with
  the variable on either side: `geof:sfWithin(?wkt, "POLYGON(…)"^^geo:wktLiteral)`;
- `geof:distance(?wkt, <geometry>[, unit]) < r` (also `<=`, or written `r > …`).

The constant can also be a variable bound by another part of the same pattern (for example
`ex:district geo:asWKT ?area` next to `FILTER(geof:sfWithin(?wkt, ?area))`). Only the geometries
whose cells meet the constant's bounding box (widened by the distance) are then tried; the filter
still decides each of them exactly. A filter the index cannot answer (a disjointness test, a
relation under `||` or `!`, a box wider than half the world) is evaluated geometry by geometry
as before.

Measured with `bench/geo.py` (100,000 points and small polygons over Europe, on the development Mac,
median of 10 runs over HTTP):

| Query | Index | Without index (same build) | Before the index |
|---|---|---|---|
| `sfWithin`, 2 x 1 km area (6 rows) | 0.43 ms | 108 ms | 502 ms |
| `sfWithin`, 17 x 11 km area (COUNT) | 2.9 ms | 114 ms | 512 ms |
| `distance < 5 km` (321 rows) | 2.0 ms | 246 ms | 394 ms |
| `sfWithin` of an area read from the data | 2.8 ms | 116 ms | 508 ms |

"Without index" hides the call from the rewrite (`FILTER((…) || false)`); it is faster than before
because geometries whose bounding boxes are apart are now decided without the full DE-9IM matrix,
and a point against polygons is located directly.

## Limits

- Query rewriting of the GeoSPARQL relation properties (`?a geo:sfWithin ?b` as a triple pattern
  computed from geometries) is not implemented; use the functions in `FILTER`.
- Relations are planar in degrees; the index does not wrap across the antimeridian (a `distance`
  filter near it falls back to all longitudes).
