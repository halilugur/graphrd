//! GeoSPARQL 1.1 geometry functions (OGC 22-047r1) without dependencies: WKT and GeoJSON
//! literals (CRS84), the DE-9IM `relate` matrix, the Simple Features, Egenhofer and RCC8
//! topology families, boolean set operations, constructors (envelope, convex hull, centroid,
//! boundary), accessors and metric functions (distance, area, length, perimeter).
//!
//! Topology and set operations work on a planar arrangement of the two inputs: every segment
//! is split at every intersection ("noding"); each piece, each node and sample points beside
//! each piece are then located (interior / boundary / exterior) in both geometries.

use crate::json::Json;
use crate::rdf::{Term, XSD_DOUBLE};

/// The GeoSPARQL ontology namespace.
pub const GEO: &str = "http://www.opengis.net/ont/geosparql#";
/// The GeoSPARQL function namespace.
pub const GEOF: &str = "http://www.opengis.net/def/function/geosparql/";
/// The datatype of a WKT literal.
pub const WKT_LITERAL: &str = "http://www.opengis.net/ont/geosparql#wktLiteral";
/// The datatype of a GeoJSON literal.
pub const GEOJSON_LITERAL: &str = "http://www.opengis.net/ont/geosparql#geoJSONLiteral";
const CRS84: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
const UOM: &str = "http://www.opengis.net/def/uom/OGC/1.0/";
const EARTH_RADIUS: f64 = 6_371_008.8;
const EPS: f64 = 1e-12;

/// A coordinate: longitude first, latitude second (CRS84).
pub type C = (f64, f64);

#[derive(Clone, Debug, PartialEq)]
/// A geometry in coordinate space.
pub enum Geom {
    /// A single point.
    Point(C),
    /// A line through the given points.
    Line(Vec<C>),
    /// Exterior ring first, then holes; rings are closed (first == last).
    Polygon(Vec<Vec<C>>),
    /// Several points.
    MultiPoint(Vec<C>),
    /// Several lines.
    MultiLine(Vec<Vec<C>>),
    /// Several polygons, each with its rings.
    MultiPolygon(Vec<Vec<Vec<C>>>),
    /// Geometries of any kind together.
    Collection(Vec<Geom>),
    /// An empty geometry of the given kind (POINT EMPTY ...).
    Empty(&'static str),
}

// ------------------------------------------------------------------ parsing

/// A WKT literal: optional `<crs>` prefix, then the geometry. Coordinates come back in CRS84
/// (longitude, latitude) whatever the literal's CRS, so literals in different systems compare
/// directly. Also returns the coordinate dimension written (2 for XY, 3 for Z or M, 4 for ZM).
pub fn parse_wkt(s: &str) -> Result<(Geom, u8), String> {
    let (crs, s) = split_crs(s)?;
    let mut p = WktParser {
        s: s.as_bytes(),
        i: 0,
        dim: 2,
    };
    let g = p.geometry()?;
    p.ws();
    if p.i != p.s.len() {
        return Err("trailing characters in WKT".into());
    }
    Ok((
        if crs == Crs::Crs84 {
            g
        } else {
            map_coords(g, &|c| crs.to_crs84(c))
        },
        p.dim,
    ))
}

/// The CRS IRI of a WKT literal (CRS84 when it names none).
pub fn wkt_srid(s: &str) -> &str {
    match s.trim().strip_prefix('<').and_then(|r| r.split_once('>')) {
        Some((crs, _)) => crs,
        None => CRS84,
    }
}

fn split_crs(s: &str) -> Result<(Crs, &str), String> {
    let s = s.trim();
    let Some(rest) = s.strip_prefix('<') else {
        return Ok((Crs::Crs84, s));
    };
    let (iri, body) = rest.split_once('>').ok_or("bad CRS in WKT literal")?;
    let crs = Crs::from_iri(iri).ok_or_else(|| format!("unsupported CRS <{iri}> (CRS84, EPSG:4326, 4258, 4269, 3857 and WGS 84 / UTM 326xx, 327xx are)"))?;
    Ok((crs, body.trim()))
}

// ------------------------------------------------------------------ coordinate systems

const EPSG: &str = "http://www.opengis.net/def/crs/EPSG/0/";
/// WGS 84 ellipsoid.
const WGS84_A: f64 = 6_378_137.0;
const WGS84_F: f64 = 1.0 / 298.257_223_563;
const UTM_K0: f64 = 0.9996;

/// The coordinate systems a WKT literal may name. Everything is converted to CRS84 on parsing;
/// ETRS89 and NAD83 are taken as WGS 84 (they differ by about a metre).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Crs {
    /// Longitude, latitude (WGS 84).
    Crs84,
    /// Latitude, longitude: EPSG:4326 (WGS 84), 4258 (ETRS89), 4269 (NAD83).
    LatLong,
    /// EPSG:3857, spherical "Web Mercator" metres.
    WebMercator,
    /// EPSG:326zz (north) / 327zz (south): WGS 84 / UTM zone zz, metres.
    Utm(u8, bool),
}

impl Crs {
    /// The coordinate reference system an IRI names; `None` when the IRI names none.
    /// Unknown CRSes are rejected at parse time.
    pub fn from_iri(iri: &str) -> Option<Crs> {
        if iri == CRS84 {
            return Some(Crs::Crs84);
        }
        let code: u32 = iri.strip_prefix(EPSG)?.parse().ok()?;
        Some(match code {
            4326 | 4258 | 4269 => Crs::LatLong,
            3857 | 900913 => Crs::WebMercator,
            32601..=32660 => Crs::Utm((code - 32600) as u8, true),
            32701..=32760 => Crs::Utm((code - 32700) as u8, false),
            _ => return None,
        })
    }

    /// One coordinate in this system -> (longitude, latitude) degrees.
    pub fn to_crs84(self, (x, y): C) -> C {
        match self {
            Crs::Crs84 => (x, y),
            Crs::LatLong => (y, x),
            Crs::WebMercator => (
                (x / WGS84_A).to_degrees(),
                (2.0 * (y / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees(),
            ),
            Crs::Utm(zone, north) => utm_inverse(x, if north { y } else { y - 10_000_000.0 }, zone),
        }
    }
}

fn utm_meridian(zone: u8) -> f64 {
    (zone as f64 * 6.0 - 183.0).to_radians()
}

/// Transverse Mercator easting/northing (WGS 84, UTM scale) -> (longitude, latitude), after
/// Snyder, "Map Projections: A Working Manual" (1987), eqs. 8-18 to 8-25; millimetre accuracy
/// inside a zone.
fn utm_inverse(e: f64, n: f64, zone: u8) -> C {
    let e2 = WGS84_F * (2.0 - WGS84_F);
    let ep2 = e2 / (1.0 - e2);
    let e1 = (1.0 - (1.0 - e2).sqrt()) / (1.0 + (1.0 - e2).sqrt());
    let x = e - 500_000.0;
    let mu = n
        / UTM_K0
        / (WGS84_A * (1.0 - e2 / 4.0 - 3.0 * e2 * e2 / 64.0 - 5.0 * e2 * e2 * e2 / 256.0));
    let phi1 = mu
        + (3.0 * e1 / 2.0 - 27.0 * e1.powi(3) / 32.0) * (2.0 * mu).sin()
        + (21.0 * e1 * e1 / 16.0 - 55.0 * e1.powi(4) / 32.0) * (4.0 * mu).sin()
        + (151.0 * e1.powi(3) / 96.0) * (6.0 * mu).sin()
        + (1097.0 * e1.powi(4) / 512.0) * (8.0 * mu).sin();
    let (s, c, t) = (phi1.sin(), phi1.cos(), phi1.tan());
    let c1 = ep2 * c * c;
    let t1 = t * t;
    let n1 = WGS84_A / (1.0 - e2 * s * s).sqrt();
    let r1 = WGS84_A * (1.0 - e2) / (1.0 - e2 * s * s).powf(1.5);
    let d = x / (n1 * UTM_K0);
    let lat = phi1
        - (n1 * t / r1)
            * (d * d / 2.0
                - (5.0 + 3.0 * t1 + 10.0 * c1 - 4.0 * c1 * c1 - 9.0 * ep2) * d.powi(4) / 24.0
                + (61.0 + 90.0 * t1 + 298.0 * c1 + 45.0 * t1 * t1 - 252.0 * ep2 - 3.0 * c1 * c1)
                    * d.powi(6)
                    / 720.0);
    let lon = utm_meridian(zone)
        + (d - (1.0 + 2.0 * t1 + c1) * d.powi(3) / 6.0
            + (5.0 - 2.0 * c1 + 28.0 * t1 - 3.0 * c1 * c1 + 8.0 * ep2 + 24.0 * t1 * t1)
                * d.powi(5)
                / 120.0)
            / c;
    (lon.to_degrees(), lat.to_degrees())
}

/// (longitude, latitude) degrees -> UTM easting/northing (northern-hemisphere false northing);
/// the inverse of `utm_inverse`, used by the tests.
#[cfg(test)]
fn utm_forward((lon, lat): C, zone: u8) -> C {
    let e2 = WGS84_F * (2.0 - WGS84_F);
    let ep2 = e2 / (1.0 - e2);
    let phi = lat.to_radians();
    let (s, c) = (phi.sin(), phi.cos());
    let n = WGS84_A / (1.0 - e2 * s * s).sqrt();
    let t = phi.tan().powi(2);
    let cc = ep2 * c * c;
    let a = c * (lon.to_radians() - utm_meridian(zone));
    let m = WGS84_A
        * ((1.0 - e2 / 4.0 - 3.0 * e2 * e2 / 64.0 - 5.0 * e2.powi(3) / 256.0) * phi
            - (3.0 * e2 / 8.0 + 3.0 * e2 * e2 / 32.0 + 45.0 * e2.powi(3) / 1024.0)
                * (2.0 * phi).sin()
            + (15.0 * e2 * e2 / 256.0 + 45.0 * e2.powi(3) / 1024.0) * (4.0 * phi).sin()
            - (35.0 * e2.powi(3) / 3072.0) * (6.0 * phi).sin());
    let east = UTM_K0
        * n
        * (a + (1.0 - t + cc) * a.powi(3) / 6.0
            + (5.0 - 18.0 * t + t * t + 72.0 * cc - 58.0 * ep2) * a.powi(5) / 120.0)
        + 500_000.0;
    let north = UTM_K0
        * (m + n
            * phi.tan()
            * (a * a / 2.0
                + (5.0 - t + 9.0 * cc + 4.0 * cc * cc) * a.powi(4) / 24.0
                + (61.0 - 58.0 * t + t * t + 600.0 * cc - 330.0 * ep2) * a.powi(6) / 720.0));
    (east, north)
}

fn map_coords(g: Geom, f: &dyn Fn(C) -> C) -> Geom {
    let line = |l: Vec<C>| l.into_iter().map(f).collect::<Vec<C>>();
    let poly = |p: Vec<Vec<C>>| p.into_iter().map(line).collect::<Vec<_>>();
    match g {
        Geom::Point(c) => Geom::Point(f(c)),
        Geom::Line(l) => Geom::Line(line(l)),
        Geom::Polygon(p) => Geom::Polygon(poly(p)),
        Geom::MultiPoint(l) => Geom::MultiPoint(line(l)),
        Geom::MultiLine(p) => Geom::MultiLine(poly(p)),
        Geom::MultiPolygon(ps) => Geom::MultiPolygon(ps.into_iter().map(poly).collect()),
        Geom::Collection(gs) => {
            Geom::Collection(gs.into_iter().map(|g| map_coords(g, f)).collect())
        }
        e @ Geom::Empty(_) => e,
    }
}

struct WktParser<'a> {
    s: &'a [u8],
    i: usize,
    dim: u8,
}

impl WktParser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn word(&mut self) -> String {
        self.ws();
        let start = self.i;
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_alphabetic()) {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).to_ascii_uppercase()
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(format!("expected '{}' in WKT", c as char))
        }
    }

    /// `EMPTY` (true) or an opening parenthesis (false).
    fn empty_or_open(&mut self) -> Result<bool, String> {
        self.ws();
        let save = self.i;
        if self.word() == "EMPTY" {
            return Ok(true);
        }
        self.i = save;
        self.expect(b'(')?;
        Ok(false)
    }

    fn number(&mut self) -> Result<f64, String> {
        self.ws();
        let start = self.i;
        while self
            .s
            .get(self.i)
            .is_some_and(|c| c.is_ascii_digit() || b"+-.eE".contains(c))
        {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i])
            .ok()
            .and_then(|t| t.parse().ok())
            .ok_or_else(|| "bad number in WKT".to_string())
    }

    fn coord(&mut self) -> Result<C, String> {
        let x = self.number()?;
        let y = self.number()?;
        // extra ordinates (Z, M) are read and dropped: functions work in 2D
        let mut n = 2;
        loop {
            self.ws();
            if self
                .s
                .get(self.i)
                .is_some_and(|c| c.is_ascii_digit() || b"+-.".contains(c))
            {
                self.number()?;
                n += 1;
            } else {
                break;
            }
        }
        self.dim = self.dim.max(n);
        Ok((x, y))
    }

    fn coords(&mut self) -> Result<Vec<C>, String> {
        let mut out = vec![self.coord()?];
        while self.eat(b',') {
            out.push(self.coord()?);
        }
        self.expect(b')')?;
        Ok(out)
    }

    fn rings(&mut self) -> Result<Vec<Vec<C>>, String> {
        let mut out = Vec::new();
        loop {
            self.expect(b'(')?;
            let mut ring = self.coords()?;
            if ring.first() != ring.last() {
                ring.push(ring[0]);
            }
            if ring.len() < 4 {
                return Err("a polygon ring needs at least 3 distinct points".into());
            }
            out.push(ring);
            if !self.eat(b',') {
                break;
            }
        }
        self.expect(b')')?;
        Ok(out)
    }

    fn geometry(&mut self) -> Result<Geom, String> {
        let kind = self.word();
        // dimension modifiers: Z, M, ZM
        let save = self.i;
        let m = self.word();
        match m.as_str() {
            "Z" | "M" => self.dim = self.dim.max(3),
            "ZM" => self.dim = 4,
            _ => self.i = save,
        }
        Ok(match kind.as_str() {
            "POINT" => {
                if self.empty_or_open()? {
                    return Ok(Geom::Empty("Point"));
                }
                let c = self.coord()?;
                self.expect(b')')?;
                Geom::Point(c)
            }
            "LINESTRING" => {
                if self.empty_or_open()? {
                    return Ok(Geom::Empty("LineString"));
                }
                Geom::Line(self.coords()?)
            }
            "POLYGON" => {
                if self.empty_or_open()? {
                    return Ok(Geom::Empty("Polygon"));
                }
                self.i -= 1; // rings() reads the per-ring parentheses after the outer one
                self.expect(b'(')?;
                Geom::Polygon(self.rings()?)
            }
            "MULTIPOINT" => {
                if self.empty_or_open()? {
                    return Ok(Geom::Empty("MultiPoint"));
                }
                let mut pts = Vec::new();
                loop {
                    // both MULTIPOINT ((1 2), (3 4)) and MULTIPOINT (1 2, 3 4)
                    if self.eat(b'(') {
                        pts.push(self.coord()?);
                        self.expect(b')')?;
                    } else {
                        pts.push(self.coord()?);
                    }
                    if !self.eat(b',') {
                        break;
                    }
                }
                self.expect(b')')?;
                Geom::MultiPoint(pts)
            }
            "MULTILINESTRING" => {
                if self.empty_or_open()? {
                    return Ok(Geom::Empty("MultiLineString"));
                }
                let mut lines = Vec::new();
                loop {
                    self.expect(b'(')?;
                    lines.push(self.coords()?);
                    if !self.eat(b',') {
                        break;
                    }
                }
                self.expect(b')')?;
                Geom::MultiLine(lines)
            }
            "MULTIPOLYGON" => {
                if self.empty_or_open()? {
                    return Ok(Geom::Empty("MultiPolygon"));
                }
                let mut polys = Vec::new();
                loop {
                    self.expect(b'(')?;
                    polys.push(self.rings()?);
                    if !self.eat(b',') {
                        break;
                    }
                }
                self.expect(b')')?;
                Geom::MultiPolygon(polys)
            }
            "GEOMETRYCOLLECTION" => {
                if self.empty_or_open()? {
                    return Ok(Geom::Empty("GeometryCollection"));
                }
                let mut parts = vec![self.geometry()?];
                while self.eat(b',') {
                    parts.push(self.geometry()?);
                }
                self.expect(b')')?;
                Geom::Collection(parts)
            }
            other => return Err(format!("unknown WKT geometry type {other}")),
        })
    }
}

/// Parse a GeoJSON geometry object.
pub fn parse_geojson(s: &str) -> Result<Geom, String> {
    fn pos(v: &Json) -> Result<C, String> {
        match v.as_array() {
            Some([Json::Num(x), Json::Num(y), ..]) => Ok((*x, *y)),
            _ => Err("bad GeoJSON position".into()),
        }
    }
    fn list(v: Option<&Json>) -> Result<Vec<C>, String> {
        v.and_then(Json::as_array)
            .ok_or("bad GeoJSON coordinates")?
            .iter()
            .map(pos)
            .collect()
    }
    fn rings(v: Option<&Json>) -> Result<Vec<Vec<C>>, String> {
        v.and_then(Json::as_array)
            .ok_or("bad GeoJSON coordinates")?
            .iter()
            .map(|r| list(Some(r)))
            .collect()
    }
    fn geom(j: &Json) -> Result<Geom, String> {
        let c = j.get("coordinates");
        Ok(match j.str_at(&["type"]) {
            "Point" => match c.and_then(Json::as_array) {
                Some([]) => Geom::Empty("Point"),
                _ => Geom::Point(pos(c.ok_or("bad GeoJSON")?)?),
            },
            "LineString" => Geom::Line(list(c)?),
            "Polygon" => Geom::Polygon(rings(c)?),
            "MultiPoint" => Geom::MultiPoint(list(c)?),
            "MultiLineString" => Geom::MultiLine(rings(c)?),
            "MultiPolygon" => Geom::MultiPolygon(
                c.and_then(Json::as_array)
                    .ok_or("bad GeoJSON")?
                    .iter()
                    .map(|p| rings(Some(p)))
                    .collect::<Result<_, _>>()?,
            ),
            "GeometryCollection" => Geom::Collection(
                j.get("geometries")
                    .and_then(Json::as_array)
                    .ok_or("bad GeoJSON")?
                    .iter()
                    .map(geom)
                    .collect::<Result<_, _>>()?,
            ),
            "Feature" => geom(
                j.get("geometry")
                    .ok_or("GeoJSON feature without geometry")?,
            )?,
            other => return Err(format!("unknown GeoJSON type {other}")),
        })
    }
    geom(&Json::parse(s)?)
}

// ------------------------------------------------------------------ writing

fn fmt_num(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn wkt_coords(cs: &[C]) -> String {
    cs.iter()
        .map(|(x, y)| format!("{} {}", fmt_num(*x), fmt_num(*y)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn wkt_rings(rs: &[Vec<C>]) -> String {
    rs.iter()
        .map(|r| format!("({})", wkt_coords(r)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The geometry in WKT, numbers trimmed.
pub fn to_wkt(g: &Geom) -> String {
    match g {
        Geom::Point((x, y)) => format!("POINT({} {})", fmt_num(*x), fmt_num(*y)),
        Geom::Line(cs) => format!("LINESTRING({})", wkt_coords(cs)),
        Geom::Polygon(rs) => format!("POLYGON({})", wkt_rings(rs)),
        Geom::MultiPoint(ps) => format!(
            "MULTIPOINT({})",
            ps.iter()
                .map(|p| format!("({})", wkt_coords(&[*p])))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Geom::MultiLine(ls) => format!("MULTILINESTRING({})", wkt_rings(ls)),
        Geom::MultiPolygon(ps) => format!(
            "MULTIPOLYGON({})",
            ps.iter()
                .map(|p| format!("({})", wkt_rings(p)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Geom::Collection(gs) => format!(
            "GEOMETRYCOLLECTION({})",
            gs.iter().map(to_wkt).collect::<Vec<_>>().join(", ")
        ),
        Geom::Empty(k) => format!("{} EMPTY", k.to_ascii_uppercase()),
    }
}

/// The geometry as a GeoJSON object.
pub fn to_geojson(g: &Geom) -> Json {
    let pos = |(x, y): &C| Json::Arr(vec![Json::Num(*x), Json::Num(*y)]);
    let line = |cs: &[C]| Json::Arr(cs.iter().map(pos).collect());
    let rings = |rs: &[Vec<C>]| Json::Arr(rs.iter().map(|r| line(r)).collect());
    match g {
        Geom::Point(c) => Json::obj()
            .with("type", "Point")
            .with("coordinates", pos(c)),
        Geom::Line(cs) => Json::obj()
            .with("type", "LineString")
            .with("coordinates", line(cs)),
        Geom::Polygon(rs) => Json::obj()
            .with("type", "Polygon")
            .with("coordinates", rings(rs)),
        Geom::MultiPoint(ps) => Json::obj()
            .with("type", "MultiPoint")
            .with("coordinates", line(ps)),
        Geom::MultiLine(ls) => Json::obj()
            .with("type", "MultiLineString")
            .with("coordinates", rings(ls)),
        Geom::MultiPolygon(ps) => Json::obj().with("type", "MultiPolygon").with(
            "coordinates",
            Json::Arr(ps.iter().map(|p| rings(p)).collect()),
        ),
        Geom::Collection(gs) => Json::obj()
            .with("type", "GeometryCollection")
            .with("geometries", Json::Arr(gs.iter().map(to_geojson).collect())),
        Geom::Empty(k) => Json::obj()
            .with("type", *k)
            .with("coordinates", Json::Arr(vec![])),
    }
}

// ------------------------------------------------------------------ structure

/// Flattened parts: points (dim 0), lines (dim 1), polygons (dim 2).
#[derive(Default, Clone)]
struct Parts {
    points: Vec<C>,
    lines: Vec<Vec<C>>,
    polys: Vec<Vec<Vec<C>>>,
}

fn parts(g: &Geom) -> Parts {
    let mut p = Parts::default();
    fn go(g: &Geom, p: &mut Parts) {
        match g {
            Geom::Point(c) => p.points.push(*c),
            Geom::MultiPoint(cs) => p.points.extend(cs),
            Geom::Line(cs) => p.lines.push(cs.clone()),
            Geom::MultiLine(ls) => p.lines.extend(ls.iter().cloned()),
            Geom::Polygon(rs) => p.polys.push(rs.clone()),
            Geom::MultiPolygon(ps) => p.polys.extend(ps.iter().cloned()),
            Geom::Collection(gs) => gs.iter().for_each(|g| go(g, p)),
            Geom::Empty(_) => {}
        }
    }
    go(g, &mut p);
    p
}

/// The geometry's dimension: 0 for points, 1 for lines, 2 for areas; a
/// collection's largest. -1 when empty.
pub fn dimension(g: &Geom) -> i64 {
    let p = parts(g);
    if !p.polys.is_empty() {
        2
    } else if !p.lines.is_empty() {
        1
    } else if !p.points.is_empty() {
        0
    } else {
        match g {
            Geom::Empty("LineString" | "MultiLineString") => 1,
            Geom::Empty("Polygon" | "MultiPolygon") => 2,
            _ => 0,
        }
    }
}

/// Whether the geometry has no coordinates.
pub fn is_empty(g: &Geom) -> bool {
    let p = parts(g);
    p.points.is_empty() && p.lines.is_empty() && p.polys.is_empty()
}

/// Line boundary under the "mod 2" rule: endpoints that occur an odd number of times.
fn line_boundary(lines: &[Vec<C>]) -> Vec<C> {
    let mut ends: Vec<C> = Vec::new();
    for l in lines {
        if l.len() < 2 || l.first() == l.last() {
            continue;
        }
        for e in [l[0], l[l.len() - 1]] {
            match ends.iter().position(|x| near(*x, e)) {
                Some(i) => {
                    ends.remove(i);
                }
                None => ends.push(e),
            }
        }
    }
    ends
}

fn near(a: C, b: C) -> bool {
    (a.0 - b.0).abs() <= EPS * (1.0 + a.0.abs()) && (a.1 - b.1).abs() <= EPS * (1.0 + a.1.abs())
}

// ------------------------------------------------------------------ location

#[derive(Clone, Copy, PartialEq, Debug)]
enum Loc {
    Interior,
    Boundary,
    Exterior,
}

fn cross(o: C, a: C, b: C) -> f64 {
    (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
}

fn on_segment(p: C, a: C, b: C) -> bool {
    let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
    // well below the side-probe offset (1e-9 relative) used to classify faces
    let tol = 1e-11 * (1.0 + len + a.0.abs().max(a.1.abs()));
    if cross(a, b, p).abs() > tol * len.max(1e-300) {
        return false;
    }
    p.0 >= a.0.min(b.0) - tol
        && p.0 <= a.0.max(b.0) + tol
        && p.1 >= a.1.min(b.1) - tol
        && p.1 <= a.1.max(b.1) + tol
}

/// Point in ring: Some(true) inside, Some(false) outside, None on the ring.
fn in_ring(p: C, ring: &[C]) -> Option<bool> {
    let mut inside = false;
    for w in ring.windows(2) {
        let (a, b) = (w[0], w[1]);
        if on_segment(p, a, b) {
            return None;
        }
        if (a.1 > p.1) != (b.1 > p.1) {
            let x = a.0 + (p.1 - a.1) / (b.1 - a.1) * (b.0 - a.0);
            if x > p.0 {
                inside = !inside;
            }
        }
    }
    Some(inside)
}

fn locate_poly(p: C, rings: &[Vec<C>]) -> Loc {
    match in_ring(p, &rings[0]) {
        None => return Loc::Boundary,
        Some(false) => return Loc::Exterior,
        Some(true) => {}
    }
    for hole in &rings[1..] {
        match in_ring(p, hole) {
            None => return Loc::Boundary,
            Some(true) => return Loc::Exterior,
            Some(false) => {}
        }
    }
    Loc::Interior
}

/// Location of a point in a geometry (SFS semantics for collections: union of the parts,
/// with interiors of higher-dimensional parts taking precedence).
fn locate(p: C, g: &Parts) -> Loc {
    let mut boundary = false;
    for poly in &g.polys {
        match locate_poly(p, poly) {
            Loc::Interior => return Loc::Interior,
            Loc::Boundary => boundary = true,
            Loc::Exterior => {}
        }
    }
    if boundary {
        // on a polygon's edge: interior if another polygon of the collection covers it
        return Loc::Boundary;
    }
    let ends = line_boundary(&g.lines);
    let mut on_line = false;
    for l in &g.lines {
        if l.windows(2).any(|w| on_segment(p, w[0], w[1])) {
            on_line = true;
        }
    }
    if on_line {
        return if ends.iter().any(|e| near(*e, p)) {
            Loc::Boundary
        } else {
            Loc::Interior
        };
    }
    if g.points.iter().any(|q| near(*q, p)) {
        return Loc::Interior;
    }
    Loc::Exterior
}

// ------------------------------------------------------------------ arrangement

fn segments(g: &Parts) -> Vec<(C, C)> {
    let mut out = Vec::new();
    for l in &g.lines {
        out.extend(l.windows(2).filter(|w| w[0] != w[1]).map(|w| (w[0], w[1])));
    }
    for poly in &g.polys {
        for r in poly {
            out.extend(r.windows(2).filter(|w| w[0] != w[1]).map(|w| (w[0], w[1])));
        }
    }
    out
}

/// Intersection point(s) of two segments: none, one point, or the two ends of an overlap.
fn seg_intersections(a: C, b: C, c: C, d: C) -> Vec<C> {
    let r = (b.0 - a.0, b.1 - a.1);
    let s = (d.0 - c.0, d.1 - c.1);
    let denom = r.0 * s.1 - r.1 * s.0;
    let qp = (c.0 - a.0, c.1 - a.1);
    let mut out = Vec::new();
    if denom.abs() < 1e-18 * (r.0.hypot(r.1) * s.0.hypot(s.1)).max(1e-300) {
        // parallel: collinear overlap contributes its endpoints
        for p in [a, b] {
            if on_segment(p, c, d) {
                out.push(p);
            }
        }
        for p in [c, d] {
            if on_segment(p, a, b) {
                out.push(p);
            }
        }
        return out;
    }
    let t = (qp.0 * s.1 - qp.1 * s.0) / denom;
    let u = (qp.0 * r.1 - qp.1 * r.0) / denom;
    let tol = 1e-12;
    if (-tol..=1.0 + tol).contains(&t) && (-tol..=1.0 + tol).contains(&u) {
        out.push((a.0 + t * r.0, a.1 + t * r.1));
    }
    out
}

/// Snaps nearly equal coordinates to the first one seen, so that a crossing computed from
/// two different segment pairs yields one node (bucketed by a coarse grid).
#[derive(Default)]
struct Snap {
    nodes: std::collections::HashMap<(i64, i64), Vec<C>>,
}

impl Snap {
    fn tol(c: C) -> f64 {
        1e-9 * (1.0 + c.0.abs().max(c.1.abs()))
    }

    fn key(c: C) -> (i64, i64) {
        let cell = 1e-6;
        ((c.0 / cell).floor() as i64, (c.1 / cell).floor() as i64)
    }

    fn snap(&mut self, c: C) -> C {
        let (kx, ky) = Snap::key(c);
        let t = Snap::tol(c);
        for dx in -1..=1 {
            for dy in -1..=1 {
                if let Some(n) = self.nodes.get(&(kx + dx, ky + dy)).and_then(|v| {
                    v.iter()
                        .find(|n| (n.0 - c.0).abs() <= t && (n.1 - c.1).abs() <= t)
                }) {
                    return *n;
                }
            }
        }
        self.nodes.entry((kx, ky)).or_default().push(c);
        c
    }
}

/// All segments of both inputs, split at every intersection with any other segment.
fn noded_edges(a: &Parts, b: &Parts) -> Vec<(C, C)> {
    let mut snap = Snap::default();
    let mut segs = segments(a);
    segs.extend(segments(b));
    // input vertices first, so they win over computed crossings
    for s in segs.iter_mut() {
        *s = (snap.snap(s.0), snap.snap(s.1));
    }
    let points: Vec<C> = a
        .points
        .iter()
        .chain(&b.points)
        .map(|p| snap.snap(*p))
        .collect();
    let bbox = |&(p, q): &(C, C)| (p.0.min(q.0), p.1.min(q.1), p.0.max(q.0), p.1.max(q.1));
    let boxes: Vec<_> = segs.iter().map(bbox).collect();
    let mut out = Vec::new();
    for (i, &(p, q)) in segs.iter().enumerate() {
        let mut cuts = vec![p, q];
        let bi = boxes[i];
        let t = Snap::tol(p).max(Snap::tol(q));
        for (j, &(r, s)) in segs.iter().enumerate() {
            let bj = boxes[j];
            if i != j
                && bj.0 <= bi.2 + t
                && bi.0 <= bj.2 + t
                && bj.1 <= bi.3 + t
                && bi.1 <= bj.3 + t
            {
                cuts.extend(
                    seg_intersections(p, q, r, s)
                        .into_iter()
                        .map(|c| snap.snap(c)),
                );
            }
        }
        for pt in &points {
            if on_segment(*pt, p, q) {
                cuts.push(*pt);
            }
        }
        let len2 = (q.0 - p.0).powi(2) + (q.1 - p.1).powi(2);
        let t = |c: &C| ((c.0 - p.0) * (q.0 - p.0) + (c.1 - p.1) * (q.1 - p.1)) / len2;
        cuts.sort_by(|x, y| t(x).partial_cmp(&t(y)).unwrap_or(std::cmp::Ordering::Equal));
        cuts.dedup();
        for w in cuts.windows(2) {
            out.push((w[0], w[1]));
        }
    }
    out
}

/// Points just left and right of an edge's midpoint.
fn side_points(a: C, b: C) -> (C, C) {
    let m = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx.hypot(dy);
    let off = (len * 1e-6).max(1e-9 * (1.0 + m.0.abs().max(m.1.abs())));
    let (nx, ny) = (-dy / len * off, dx / len * off);
    ((m.0 + nx, m.1 + ny), (m.0 - nx, m.1 - ny))
}

/// A point strictly inside a polygon (for polygons that no edge test reaches).
fn interior_point(poly: &[Vec<C>]) -> Option<C> {
    let ring = &poly[0];
    let (y0, y1) = ring.iter().fold((f64::MAX, f64::MIN), |(lo, hi), c| {
        (lo.min(c.1), hi.max(c.1))
    });
    // scan a horizontal line through the middle and take the midpoint of the widest inside span
    for frac in [0.5, 0.37, 0.63, 0.21, 0.79] {
        let y = y0 + (y1 - y0) * frac;
        let mut xs: Vec<f64> = Vec::new();
        for r in poly {
            for w in r.windows(2) {
                let (a, b) = (w[0], w[1]);
                if (a.1 > y) != (b.1 > y) {
                    xs.push(a.0 + (y - a.1) / (b.1 - a.1) * (b.0 - a.0));
                }
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let best = xs
            .chunks(2)
            .filter(|c| c.len() == 2)
            .max_by(|a, b| (a[1] - a[0]).partial_cmp(&(b[1] - b[0])).unwrap());
        if let Some(c) = best {
            let p = ((c[0] + c[1]) / 2.0, y);
            if locate_poly(p, poly) == Loc::Interior {
                return Some(p);
            }
        }
    }
    None
}

// ------------------------------------------------------------------ DE-9IM

/// The DE-9IM intersection matrix of `a` and `b` as a 9-character string over F, 0, 1, 2
/// (rows: interior, boundary, exterior of a; columns: the same of b).
pub fn relate(ga: &Geom, gb: &Geom) -> String {
    let (a, b) = (parts(ga), parts(gb));
    // a point and polygons (the common spatial filter): the point's location decides it
    let only_polys = |g: &Parts| !g.polys.is_empty() && g.lines.is_empty() && g.points.is_empty();
    let at = |l: Loc, want: Loc| if l == want { '0' } else { 'F' };
    if let (Geom::Point(p), true) = (ga, only_polys(&b)) {
        let l = locate(*p, &b);
        return [
            at(l, Loc::Interior),
            at(l, Loc::Boundary),
            at(l, Loc::Exterior),
            'F',
            'F',
            'F',
            '2',
            '1',
            '2',
        ]
        .iter()
        .collect();
    }
    if let (true, Geom::Point(p)) = (only_polys(&a), gb) {
        let l = locate(*p, &a);
        return [
            at(l, Loc::Interior),
            'F',
            '2',
            at(l, Loc::Boundary),
            'F',
            '1',
            at(l, Loc::Exterior),
            'F',
            '2',
        ]
        .iter()
        .collect();
    }
    let mut m = [[-1i8; 3]; 3];
    let idx = |l: Loc| match l {
        Loc::Interior => 0,
        Loc::Boundary => 1,
        Loc::Exterior => 2,
    };
    let mut set = |la: Loc, lb: Loc, d: i8| {
        let c = &mut m[idx(la)][idx(lb)];
        *c = (*c).max(d);
    };
    set(Loc::Exterior, Loc::Exterior, 2);
    // points
    for p in &a.points {
        set(locate(*p, &a), locate(*p, &b), 0);
    }
    for p in &b.points {
        set(locate(*p, &a), locate(*p, &b), 0);
    }
    // nodes and edges of the arrangement
    let edges = noded_edges(&a, &b);
    for &(p, q) in &edges {
        for n in [p, q] {
            set(locate(n, &a), locate(n, &b), 0);
        }
        let mid = ((p.0 + q.0) / 2.0, (p.1 + q.1) / 2.0);
        let (la, lb) = (locate(mid, &a), locate(mid, &b));
        // an edge lies in a's boundary or interior (or b's) - that location has dimension 1
        set(la, lb, 1);
        // areas beside the edge
        let (l, r) = side_points(p, q);
        for s in [l, r] {
            let (sa, sb) = (locate(s, &a), locate(s, &b));
            if sa != Loc::Boundary
                && sb != Loc::Boundary
                && (sa == Loc::Interior && !a.polys.is_empty()
                    || sb == Loc::Interior && !b.polys.is_empty())
            {
                let da = if sa == Loc::Interior && !a.polys.is_empty() {
                    2
                } else {
                    -1
                };
                let db = if sb == Loc::Interior && !b.polys.is_empty() {
                    2
                } else {
                    -1
                };
                // a 2D cell needs both sides' locations to be areal (or exterior)
                let ok_a = sa == Loc::Exterior || da == 2;
                let ok_b = sb == Loc::Exterior || db == 2;
                if ok_a && ok_b {
                    set(sa, sb, 2);
                }
            }
        }
    }
    // polygons not reached by any edge test (e.g. one inside the other with no crossings)
    for (own, other, flip) in [(&a, &b, false), (&b, &a, true)] {
        for poly in &own.polys {
            if let Some(p) = interior_point(poly) {
                // a 2D cell of `own`'s interior lies in `other`'s areal interior or, around
                // any lower-dimensional part of `other`, in its exterior
                let lo = locate(p, own);
                let lt = if other
                    .polys
                    .iter()
                    .any(|q| locate_poly(p, q) == Loc::Interior)
                {
                    Loc::Interior
                } else {
                    Loc::Exterior
                };
                if flip { set(lt, lo, 2) } else { set(lo, lt, 2) }
            }
        }
    }
    let mut out = String::with_capacity(9);
    for row in m {
        for c in row {
            out.push(match c {
                -1 => 'F',
                d => (b'0' + d as u8) as char,
            });
        }
    }
    out
}

/// DE-9IM pattern test: `T` (non-empty), `F` (empty), `*` (any), or `0`/`1`/`2`.
pub fn matches(matrix: &str, pattern: &str) -> bool {
    matrix
        .chars()
        .zip(pattern.chars())
        .all(|(m, p)| match p.to_ascii_uppercase() {
            '*' => true,
            'T' => m != 'F',
            'F' => m == 'F',
            d => m == d,
        })
}

/// The topology functions that hold only for geometries that meet (all but the disjointness
/// ones).
const MEETING: [&str; 21] = [
    "sfEquals",
    "ehEquals",
    "rcc8eq",
    "sfIntersects",
    "sfTouches",
    "ehMeet",
    "sfCrosses",
    "sfWithin",
    "sfContains",
    "sfOverlaps",
    "ehOverlap",
    "ehCovers",
    "ehCoveredBy",
    "ehInside",
    "ehContains",
    "rcc8ec",
    "rcc8po",
    "rcc8tppi",
    "rcc8tpp",
    "rcc8ntpp",
    "rcc8ntppi",
];

/// Topology functions of the three GeoSPARQL families, by local name.
pub fn topology(name: &str, a: &Geom, b: &Geom) -> Option<bool> {
    // geometries whose boxes are apart are disjoint: no need for the matrix
    if let (Some(x), Some(y)) = (coord_bounds(a), coord_bounds(b)) {
        let pad = 1e-9;
        if x.0 > y.2 + pad || y.0 > x.2 + pad || x.1 > y.3 + pad || y.1 > x.3 + pad {
            match name {
                "sfDisjoint" | "ehDisjoint" => return Some(true),
                "rcc8dc" => {}
                _ if MEETING.contains(&name) => return Some(false),
                _ => {}
            }
        }
    }
    let m = relate(a, b);
    let any = |ps: &[&str]| ps.iter().any(|p| matches(&m, p));
    let (da, db) = (dimension(a), dimension(b));
    Some(match name {
        "sfEquals" | "ehEquals" | "rcc8eq" => any(&["TFFFTFFFT"]),
        "sfDisjoint" | "ehDisjoint" => any(&["FF*FF****"]),
        "sfIntersects" => !any(&["FF*FF****"]),
        "sfTouches" | "ehMeet" => any(&["FT*******", "F**T*****", "F***T****"]),
        "sfCrosses" => {
            if da < db {
                any(&["T*T******"])
            } else if da > db {
                any(&["T*****T**"])
            } else if da == 1 {
                any(&["0********"])
            } else {
                false
            }
        }
        "sfWithin" => any(&["T*F**F***"]),
        "sfContains" => any(&["T*****FF*"]),
        "sfOverlaps" => {
            if da != db {
                false
            } else if da == 1 {
                any(&["1*T***T**"])
            } else {
                any(&["T*T***T**"])
            }
        }
        "ehOverlap" => any(&["T*T***T**"]),
        "ehCovers" => any(&["T*TFT*FF*"]),
        "ehCoveredBy" => any(&["TFF*TFT**"]),
        "ehInside" => any(&["TFF*FFT**"]),
        "ehContains" => any(&["T*TFF*FF*"]),
        "rcc8dc" => any(&["FFTFFTTTT"]),
        "rcc8ec" => any(&["FFTFTTTTT"]),
        "rcc8po" => any(&["TTTTTTTTT"]),
        "rcc8tppi" => any(&["TTTFTTFFT"]),
        "rcc8tpp" => any(&["TFFTTFTTT"]),
        "rcc8ntpp" => any(&["TFFTFFTTT"]),
        "rcc8ntppi" => any(&["TTTFFTFFT"]),
        _ => return None,
    })
}

// ------------------------------------------------------------------ set operations

#[derive(Clone, Copy, PartialEq)]
/// A set operation on two geometries.
pub enum SetOp {
    /// The shared area.
    Intersection,
    /// Both together.
    Union,
    /// The first without the second.
    Difference,
    /// What belongs to exactly one of the two.
    SymDifference,
}

fn inside(op: SetOp, ia: bool, ib: bool) -> bool {
    match op {
        SetOp::Intersection => ia && ib,
        SetOp::Union => ia || ib,
        SetOp::Difference => ia && !ib,
        SetOp::SymDifference => ia != ib,
    }
}

/// Polygon overlay on the arrangement: keep the edges where membership of the result
/// changes from one side to the other, oriented with the result on the left, and link them
/// into rings; then the lines and points of the result.
pub fn set_op(op: SetOp, ga: &Geom, gb: &Geom) -> Geom {
    let (a, b) = (parts(ga), parts(gb));
    let area_in = |p: C, g: &Parts| {
        g.polys
            .iter()
            .any(|poly| locate_poly(p, poly) == Loc::Interior)
    };
    let edges = noded_edges(
        &Parts {
            polys: a.polys.clone(),
            ..Default::default()
        },
        &Parts {
            polys: b.polys.clone(),
            ..Default::default()
        },
    );
    let mut kept: Vec<(C, C)> = Vec::new();
    for &(p, q) in &edges {
        let (l, r) = side_points(p, q);
        let li = inside(op, area_in(l, &a), area_in(l, &b));
        let ri = inside(op, area_in(r, &a), area_in(r, &b));
        let e = if li && !ri {
            (p, q)
        } else if ri && !li {
            (q, p)
        } else {
            continue;
        };
        if !kept.iter().any(|k| near(k.0, e.0) && near(k.1, e.1)) {
            kept.push(e);
        }
    }
    let rings = link_rings(kept);
    // shells are counter-clockwise (result on the left), holes clockwise
    let mut shells: Vec<Vec<Vec<C>>> = Vec::new();
    let mut holes: Vec<Vec<C>> = Vec::new();
    for r in rings {
        if signed_area(&r) > 0.0 {
            shells.push(vec![r])
        } else {
            holes.push(r)
        }
    }
    for h in holes {
        // the smallest shell around the whole hole (an island inside the hole is smaller)
        let hole_area = signed_area(&h).abs();
        if let Some(s) = shells
            .iter_mut()
            .filter(|s| {
                signed_area(&s[0]) > hole_area
                    && h.iter().all(|p| in_ring(*p, &s[0]) != Some(false))
            })
            .min_by(|x, y| signed_area(&x[0]).partial_cmp(&signed_area(&y[0])).unwrap())
        {
            s.push(h);
        }
    }
    let mut out: Vec<Geom> = shells.into_iter().map(Geom::Polygon).collect();
    // lines and points: arrangement pieces and nodes in the result (closed-set membership)
    // that no resulting area already covers - shared edges, crossings, touching vertices
    let in_result = |p: C| {
        inside(
            op,
            locate(p, &a) != Loc::Exterior,
            locate(p, &b) != Loc::Exterior,
        )
    };
    let area_side = |p: C| inside(op, area_in(p, &a), area_in(p, &b));
    let all = Parts {
        points: a.points.iter().chain(&b.points).copied().collect(),
        lines: a.lines.iter().chain(&b.lines).cloned().collect(),
        polys: a.polys.iter().chain(&b.polys).cloned().collect(),
    };
    let pieces_all = noded_edges(&all, &Parts::default());
    let mut pieces: Vec<(C, C)> = Vec::new();
    for &(p, q) in &pieces_all {
        let mid = ((p.0 + q.0) / 2.0, (p.1 + q.1) / 2.0);
        let (l, r) = side_points(p, q);
        if in_result(mid)
            && !area_side(l)
            && !area_side(r)
            && !pieces
                .iter()
                .any(|x| (near(x.0, p) && near(x.1, q)) || (near(x.0, q) && near(x.1, p)))
        {
            pieces.push((p, q));
        }
    }
    for l in merge_lines(pieces) {
        out.push(Geom::Line(l));
    }
    let mut covered = parts(&simplify_collection(out.clone()));
    let nodes: Vec<C> = all
        .points
        .iter()
        .copied()
        .chain(pieces_all.iter().flat_map(|e| [e.0, e.1]))
        .collect();
    for p in nodes {
        if in_result(p) && locate(p, &covered) == Loc::Exterior {
            out.push(Geom::Point(p));
            covered.points.push(p);
        }
    }
    simplify_collection(out)
}

fn signed_area(r: &[C]) -> f64 {
    r.windows(2)
        .map(|w| w[0].0 * w[1].1 - w[1].0 * w[0].1)
        .sum::<f64>()
        / 2.0
}

/// Link directed edges into closed rings, turning as far left as possible at each node.
fn link_rings(mut edges: Vec<(C, C)>) -> Vec<Vec<C>> {
    let mut rings = Vec::new();
    while let Some(first) = edges.pop() {
        let mut ring = vec![first.0, first.1];
        let mut guard = 0;
        while !near(*ring.last().unwrap(), ring[0]) && guard < 1_000_000 {
            guard += 1;
            let cur = *ring.last().unwrap();
            let prev = ring[ring.len() - 2];
            let heading = (cur.1 - prev.1).atan2(cur.0 - prev.0);
            let candidates: Vec<usize> = (0..edges.len())
                .filter(|&i| near(edges[i].0, cur))
                .collect();
            let Some(&best) = candidates.iter().max_by(|&&i, &&j| {
                let turn = |k: usize| {
                    let e = edges[k];
                    let h = (e.1.1 - e.0.1).atan2(e.1.0 - e.0.0);
                    let mut d = h - heading;
                    while d <= -std::f64::consts::PI {
                        d += 2.0 * std::f64::consts::PI;
                    }
                    while d > std::f64::consts::PI {
                        d -= 2.0 * std::f64::consts::PI;
                    }
                    d
                };
                turn(i).partial_cmp(&turn(j)).unwrap()
            }) else {
                break;
            };
            ring.push(edges.remove(best).1);
        }
        if ring.len() >= 4 && near(*ring.last().unwrap(), ring[0]) {
            let n = ring.len();
            ring[n - 1] = ring[0];
            rings.push(ring);
        }
    }
    rings
}

/// Join line pieces that share endpoints into maximal lines.
fn merge_lines(mut pieces: Vec<(C, C)>) -> Vec<Vec<C>> {
    let mut out = Vec::new();
    while let Some((a, b)) = pieces.pop() {
        let mut line = vec![a, b];
        loop {
            let end = *line.last().unwrap();
            let start = line[0];
            if let Some(i) = pieces.iter().position(|p| near(p.0, end) || near(p.1, end)) {
                let p = pieces.remove(i);
                line.push(if near(p.0, end) { p.1 } else { p.0 });
            } else if let Some(i) = pieces
                .iter()
                .position(|p| near(p.0, start) || near(p.1, start))
            {
                let p = pieces.remove(i);
                line.insert(0, if near(p.1, start) { p.0 } else { p.1 });
            } else {
                break;
            }
        }
        out.push(line);
    }
    out
}

fn simplify_collection(mut gs: Vec<Geom>) -> Geom {
    match gs.len() {
        0 => Geom::Empty("GeometryCollection"),
        1 => gs.pop().unwrap(),
        _ => {
            if gs.iter().all(|g| matches!(g, Geom::Polygon(_))) {
                Geom::MultiPolygon(
                    gs.into_iter()
                        .map(|g| {
                            if let Geom::Polygon(p) = g {
                                p
                            } else {
                                unreachable!()
                            }
                        })
                        .collect(),
                )
            } else if gs.iter().all(|g| matches!(g, Geom::Line(_))) {
                Geom::MultiLine(
                    gs.into_iter()
                        .map(|g| {
                            if let Geom::Line(l) = g {
                                l
                            } else {
                                unreachable!()
                            }
                        })
                        .collect(),
                )
            } else if gs.iter().all(|g| matches!(g, Geom::Point(_))) {
                Geom::MultiPoint(
                    gs.into_iter()
                        .map(|g| {
                            if let Geom::Point(p) = g {
                                p
                            } else {
                                unreachable!()
                            }
                        })
                        .collect(),
                )
            } else {
                Geom::Collection(gs)
            }
        }
    }
}

// ------------------------------------------------------------------ constructors

fn all_coords(g: &Geom) -> Vec<C> {
    let p = parts(g);
    let mut out = p.points.clone();
    for l in &p.lines {
        out.extend(l);
    }
    for poly in &p.polys {
        for r in poly {
            out.extend(r);
        }
    }
    out
}

/// The smallest axis-aligned rectangle containing the geometry.
pub fn envelope(g: &Geom) -> Geom {
    let cs = all_coords(g);
    if cs.is_empty() {
        return Geom::Empty("Polygon");
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (x, y) in cs {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    if x0 == x1 && y0 == y1 {
        return Geom::Point((x0, y0));
    }
    if x0 == x1 || y0 == y1 {
        return Geom::Line(vec![(x0, y0), (x1, y1)]);
    }
    Geom::Polygon(vec![vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]])
}

/// The convex hull (Andrew's monotone chain).
pub fn convex_hull(g: &Geom) -> Geom {
    let mut pts = all_coords(g);
    pts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    pts.dedup_by(|a, b| near(*a, *b));
    if pts.is_empty() {
        return Geom::Empty("Polygon");
    }
    if pts.len() < 3 {
        return if pts.len() == 1 {
            Geom::Point(pts[0])
        } else {
            Geom::Line(pts)
        };
    }
    let mut lower: Vec<C> = Vec::new();
    for &p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<C> = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    let mut ring = lower;
    ring.extend(upper);
    if ring.len() < 3 {
        return Geom::Line(ring);
    }
    ring.push(ring[0]);
    Geom::Polygon(vec![ring])
}

/// Centroid of the highest-dimensional parts (area-, length- or count-weighted).
pub fn centroid(g: &Geom) -> Geom {
    let p = parts(g);
    if !p.polys.is_empty() {
        let (mut ax, mut ay, mut aa) = (0.0, 0.0, 0.0);
        for poly in &p.polys {
            for (k, r) in poly.iter().enumerate() {
                let sign = if k == 0 { 1.0 } else { -1.0 };
                let a = signed_area(r).abs() * sign;
                let (mut cx, mut cy, mut s) = (0.0, 0.0, 0.0);
                for w in r.windows(2) {
                    let c = w[0].0 * w[1].1 - w[1].0 * w[0].1;
                    cx += (w[0].0 + w[1].0) * c;
                    cy += (w[0].1 + w[1].1) * c;
                    s += c;
                }
                if s.abs() > 0.0 {
                    ax += cx / (3.0 * s) * a;
                    ay += cy / (3.0 * s) * a;
                    aa += a;
                }
            }
        }
        if aa.abs() > 0.0 {
            return Geom::Point((ax / aa, ay / aa));
        }
    }
    if !p.lines.is_empty() {
        let (mut x, mut y, mut len) = (0.0, 0.0, 0.0);
        for l in &p.lines {
            for w in l.windows(2) {
                let d = (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1);
                x += (w[0].0 + w[1].0) / 2.0 * d;
                y += (w[0].1 + w[1].1) / 2.0 * d;
                len += d;
            }
        }
        if len > 0.0 {
            return Geom::Point((x / len, y / len));
        }
    }
    if !p.points.is_empty() {
        let n = p.points.len() as f64;
        return Geom::Point((
            p.points.iter().map(|c| c.0).sum::<f64>() / n,
            p.points.iter().map(|c| c.1).sum::<f64>() / n,
        ));
    }
    Geom::Empty("Point")
}

/// The geometry's boundary: a polygon's rings as lines, a line's ends as points.
pub fn boundary(g: &Geom) -> Geom {
    let p = parts(g);
    let mut out = Vec::new();
    for poly in &p.polys {
        for r in poly {
            out.push(Geom::Line(r.clone()));
        }
    }
    for e in line_boundary(&p.lines) {
        out.push(Geom::Point(e));
    }
    simplify_collection(out)
}

/// No self-intersections in lines (other than closing) and valid rings.
pub fn is_simple(g: &Geom) -> bool {
    let p = parts(g);
    for l in &p.lines {
        let segs: Vec<(C, C)> = l.windows(2).map(|w| (w[0], w[1])).collect();
        for i in 0..segs.len() {
            for j in i + 1..segs.len() {
                let pts = seg_intersections(segs[i].0, segs[i].1, segs[j].0, segs[j].1);
                let adjacent = j == i + 1 && pts.len() == 1 && near(pts[0], segs[i].1);
                let closing = i == 0
                    && j == segs.len() - 1
                    && near(l[0], *l.last().unwrap())
                    && pts.len() == 1
                    && near(pts[0], l[0]);
                if !pts.is_empty() && !adjacent && !closing {
                    return false;
                }
            }
        }
    }
    true
}

// ------------------------------------------------------------------ metrics (CRS84, metres)

fn haversine(a: C, b: C) -> f64 {
    let (la1, la2) = (a.1.to_radians(), b.1.to_radians());
    let dlat = la2 - la1;
    let dlon = (b.0 - a.0).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS * h.sqrt().asin()
}

fn closest_on_segment(p: C, a: C, b: C) -> C {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    if len2 == 0.0 {
        return a;
    }
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0);
    (a.0 + t * dx, a.1 + t * dy)
}

/// Distance in metres (great-circle between the closest points in coordinate space).
pub fn distance_m(ga: &Geom, gb: &Geom) -> f64 {
    if topology("sfIntersects", ga, gb) == Some(true) {
        return 0.0;
    }
    let (a, b) = (parts(ga), parts(gb));
    let mut best = f64::MAX;
    let pts_a = all_coords(ga);
    let pts_b = all_coords(gb);
    let segs_a = segments(&a);
    let segs_b = segments(&b);
    for p in &pts_a {
        for q in &pts_b {
            best = best.min(haversine(*p, *q));
        }
        for &(s, t) in &segs_b {
            best = best.min(haversine(*p, closest_on_segment(*p, s, t)));
        }
    }
    for q in &pts_b {
        for &(s, t) in &segs_a {
            best = best.min(haversine(*q, closest_on_segment(*q, s, t)));
        }
    }
    best
}

/// Length of lines (or perimeter of polygons) in metres.
pub fn length_m(g: &Geom, perimeter: bool) -> f64 {
    let p = parts(g);
    let line = |cs: &[C]| cs.windows(2).map(|w| haversine(w[0], w[1])).sum::<f64>();
    if perimeter {
        p.polys.iter().flatten().map(|r| line(r)).sum()
    } else {
        p.lines.iter().map(|l| line(l)).sum()
    }
}

/// Area in square metres on the sphere.
pub fn area_m2(g: &Geom) -> f64 {
    let ring_area = |r: &[C]| {
        let mut s = 0.0;
        for w in r.windows(2) {
            let (a, b) = (w[0], w[1]);
            s += (b.0 - a.0).to_radians() * (2.0 + a.1.to_radians().sin() + b.1.to_radians().sin());
        }
        (s * EARTH_RADIUS * EARTH_RADIUS / 2.0).abs()
    };
    parts(g)
        .polys
        .iter()
        .map(|poly| ring_area(&poly[0]) - poly[1..].iter().map(|h| ring_area(h)).sum::<f64>())
        .sum()
}

/// Convert metres (or square metres) to the unit IRI.
pub fn in_units(metres: f64, unit: &str, squared: bool) -> Option<f64> {
    let local = unit.strip_prefix(UOM).unwrap_or(unit);
    let f = match local {
        "metre" | "meter" | "http://qudt.org/vocab/unit/M" => 1.0,
        "kilometre" | "kilometer" => 1000.0,
        "degree" => EARTH_RADIUS * std::f64::consts::PI / 180.0,
        "radian" => EARTH_RADIUS,
        _ => return None,
    };
    Some(if squared {
        metres / (f * f)
    } else {
        metres / f
    })
}

// ------------------------------------------------------------------ spatial index

/// Bounding box in CRS84 degrees: (min lon, min lat, max lon, max lat).
pub type Bbox = (f64, f64, f64, f64);

/// Depth of the quadtree over the CRS84 plane; a cell at the deepest level is about
/// 0.5 x 0.3 metres at the equator.
pub const CELL_LEVELS: u32 = 26;

/// The bounding box of a geometry literal's value (WKT or GeoJSON), None when the term is not
/// one or the geometry is empty.
pub fn literal_bbox(t: &Term) -> Option<Bbox> {
    coord_bounds(&geom_arg(Some(t))?.0)
}

/// Whether the term is a WKT or GeoJSON literal.
pub fn is_geometry_literal(t: &Term) -> bool {
    t.as_literal()
        .is_some_and(|l| l.datatype == WKT_LITERAL || l.datatype == GEOJSON_LITERAL)
}

/// Degrees -> the unit square, clamped (so out-of-range coordinates still land in a cell) and
/// NaN-safe (NaN goes to the whole plane).
fn unit(b: Bbox) -> (f64, f64, f64, f64) {
    let ok = !(b.0.is_nan() || b.1.is_nan() || b.2.is_nan() || b.3.is_nan());
    let x = |v: f64| ((v + 180.0) / 360.0).clamp(0.0, 1.0);
    let y = |v: f64| ((v + 90.0) / 180.0).clamp(0.0, 1.0);
    if ok {
        (x(b.0), y(b.1), x(b.2), y(b.3))
    } else {
        (0.0, 0.0, 1.0, 1.0)
    }
}

fn cell_of(v: f64, level: u32) -> u64 {
    let n = 1u64 << level;
    ((v * n as f64) as u64).min(n - 1)
}

fn interleave(i: u64, j: u64) -> u64 {
    let mut z = 0;
    for b in 0..CELL_LEVELS {
        z |= ((i >> b) & 1) << (2 * b) | ((j >> b) & 1) << (2 * b + 1);
    }
    z
}

/// A cell (level, column, row) as its first position on the deepest level's Z-order curve:
/// a cell's descendants then occupy one contiguous range of positions.
fn cell_base(level: u32, i: u64, j: u64) -> u64 {
    interleave(i, j) << (2 * (CELL_LEVELS - level))
}

/// Where a geometry with this box is indexed: (Z position, level) of the at most 2 x 2 cells
/// covering the box on the deepest level whose cells are at least as large as the box.
pub fn index_cells(b: Bbox) -> Vec<(u64, u32)> {
    let (x0, y0, x1, y1) = unit(b);
    let size = (x1 - x0).max(y1 - y0);
    let level = if size <= 0.0 {
        CELL_LEVELS
    } else {
        ((-size.log2()).floor().max(0.0) as u32).min(CELL_LEVELS)
    };
    let mut out = Vec::new();
    for i in cell_of(x0, level)..=cell_of(x1, level) {
        for j in cell_of(y0, level)..=cell_of(y1, level) {
            out.push((cell_base(level, i, j), level));
        }
    }
    out
}

/// What to read from the index to find every geometry whose box meets `b`: contiguous Z ranges
/// [from, to) holding entries of any level, and single cells (Z position, level) holding
/// entries of exactly that level (the coarser cells around the query).
pub fn query_cells(b: Bbox) -> (Vec<(u64, u64)>, Vec<(u64, u32)>) {
    let (x0, y0, x1, y1) = unit(b);
    let size = (x1 - x0).max(y1 - y0);
    // cells of a sixteenth to an eighth of the query's size: at most 17 x 17 of them cover
    // it, and the area they cover is at most 1.6 times the query's
    let depth = if size <= 0.0 {
        CELL_LEVELS
    } else {
        ((-size.log2()).floor().max(0.0) as u32 + 4).min(CELL_LEVELS)
    };
    let (i0, i1, j0, j1) = (
        cell_of(x0, depth),
        cell_of(x1, depth),
        cell_of(y0, depth),
        cell_of(y1, depth),
    );
    let span = 1u64 << (2 * (CELL_LEVELS - depth));
    let mut ranges = Vec::new();
    let mut coarse = std::collections::BTreeSet::new();
    for i in i0..=i1 {
        for j in j0..=j1 {
            let z = cell_base(depth, i, j);
            ranges.push((z, z + span));
            for level in 0..depth {
                let up = depth - level;
                coarse.insert((cell_base(level, i >> up, j >> up), level));
            }
        }
    }
    ranges.sort_unstable();
    (ranges, coarse.into_iter().collect())
}

/// The box a FILTER over a geometry variable and a constant restricts that variable to, so the
/// spatial index can supply its candidates: a topology function other than the disjointness
/// ones (`geof:sfWithin(?g, <const>)`, either order), or `geof:distance(?g, <const>[, unit]) <
/// r` (also `<=`, and `r >` / `r >=`). `var_arg` says whether an argument is the variable.
/// Every geometry satisfying the call has a box meeting the returned one.
pub fn filter_box(
    name: &str,
    args: &[Option<Term>],
    var_arg: &dyn Fn(usize) -> bool,
    radius: Option<f64>,
) -> Option<Bbox> {
    if !indexable(name, radius) {
        return None;
    }
    let constant = |i: usize| -> Option<Bbox> {
        if var_arg(1 - i) && !var_arg(i) {
            coord_bounds(&geom_arg(args[i].as_ref())?.0)
        } else {
            None
        }
    };
    if args.len() < 2 {
        return None;
    }
    let b = constant(1).or_else(|| constant(0))?;
    // a hair wider, for the snapping tolerance of the topology code
    let pad = 1e-9;
    match radius {
        Some(r) => {
            let unit = match args.get(2) {
                None => format!("{UOM}metre"),
                Some(t) => t.as_ref()?.value().to_string(),
            };
            // `distance` is the haversine distance of a pair of points, one on each geometry.
            // Within r: their latitudes differ by at most r/R radians, and since
            // hav(d) >= cos(lat1) cos(lat2) hav(dlon), dlon <= 2 asin(sin(r/2R) / cos(far))
            // with `far` the latitude farthest from the equator either point can have.
            let metres = r / in_units(1.0, &unit, false)?;
            if !(metres >= 0.0) {
                return None;
            }
            let rad = metres / EARTH_RADIUS;
            let dlat = rad.to_degrees() + pad;
            let far = (b.1 - dlat)
                .abs()
                .max((b.3 + dlat).abs())
                .min(90.0)
                .to_radians();
            let s = (rad / 2.0).sin() / far.cos();
            let dlon = if !(s < 1.0) || rad >= std::f64::consts::PI {
                360.0
            } else {
                (2.0 * s.asin()).to_degrees() + pad
            };
            // near the antimeridian the circle wraps around: take every longitude
            let (w, e) = if b.0 - dlon < -180.0 || b.2 + dlon > 180.0 {
                (-180.0, 180.0)
            } else {
                (b.0 - dlon, b.2 + dlon)
            };
            Some((w, b.1 - dlat, e, b.3 + dlat))
        }
        None if args.len() == 2 => Some((b.0 - pad, b.1 - pad, b.2 + pad, b.3 + pad)),
        None => None,
    }
}

/// Whether `filter_box` can answer this function: `distance` under a comparison, or a topology
/// function that implies the geometries meet (all but the disjointness ones).
pub fn indexable(name: &str, radius: Option<f64>) -> bool {
    match (name.strip_prefix(GEOF), radius) {
        (Some("distance"), Some(_)) => true,
        (Some(local), None) => MEETING.contains(&local),
        _ => false,
    }
}

// ------------------------------------------------------------------ SPARQL

fn geom_arg(t: Option<&Term>) -> Option<(Geom, u8)> {
    let l = t?.as_literal()?;
    if l.datatype != WKT_LITERAL && l.datatype != GEOJSON_LITERAL {
        return None;
    }
    // a filter compares many geometries with the same constant: keep recent parses of the
    // longer literals (short ones, like most points, parse faster than they hash)
    let cached = l.value.len() >= 48;
    thread_local! {
        static PARSED: std::cell::RefCell<std::collections::HashMap<String, Option<(Geom, u8)>>> = Default::default();
    }
    if let Some(hit) = PARSED.with(|c| {
        if cached {
            c.borrow().get(&l.value).cloned()
        } else {
            None
        }
    }) {
        return hit;
    }
    let g = match l.datatype.as_str() {
        WKT_LITERAL => parse_wkt(&l.value).ok(),
        _ => parse_geojson(&l.value).ok().map(|g| (g, 2)),
    };
    PARSED.with(|c| {
        if !cached {
            return;
        }
        let mut c = c.borrow_mut();
        if c.len() >= 64 {
            c.clear();
        }
        c.insert(l.value.clone(), g.clone());
    });
    g
}

fn wkt_term(g: &Geom) -> Term {
    Term::typed(to_wkt(g), WKT_LITERAL)
}

fn double(v: f64) -> Term {
    Term::typed(crate::rdf::write::format_double(v), XSD_DOUBLE)
}

fn coord_bounds(g: &Geom) -> Option<(f64, f64, f64, f64)> {
    fn go(g: &Geom, b: &mut (f64, f64, f64, f64)) {
        let mut add = |cs: &[C]| {
            for p in cs {
                *b = (b.0.min(p.0), b.1.min(p.1), b.2.max(p.0), b.3.max(p.1));
            }
        };
        match g {
            Geom::Point(c) => add(std::slice::from_ref(c)),
            Geom::Line(cs) | Geom::MultiPoint(cs) => add(cs),
            Geom::Polygon(rs) | Geom::MultiLine(rs) => rs.iter().for_each(|r| add(r)),
            Geom::MultiPolygon(ps) => ps.iter().flatten().for_each(|r| add(r)),
            Geom::Collection(gs) => gs.iter().for_each(|g| go(g, b)),
            Geom::Empty(_) => {}
        }
    }
    let mut b = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    go(g, &mut b);
    (b.0 <= b.2 || b.0.is_nan()).then_some(b)
}

/// A `geof:` function call from SPARQL (`name` is the local name). None = error.
pub fn sparql_call(name: &str, args: &[Option<Term>]) -> Option<Term> {
    let a = |i: usize| args.get(i).and_then(|x| x.as_ref());
    let unit = |i: usize| match a(i) {
        None if args.len() <= i => Some(format!("{UOM}metre")),
        t => Some(t?.value().to_string()),
    };
    if let Some(b) = (|| {
        let (x, _) = geom_arg(a(0))?;
        let (y, _) = geom_arg(a(1))?;
        topology(name, &x, &y)
    })() {
        return Some(Term::boolean(b));
    }
    Some(match name {
        "relate" => {
            let (x, _) = geom_arg(a(0))?;
            let (y, _) = geom_arg(a(1))?;
            let pattern = &a(2)?.as_literal()?.value;
            if pattern.len() != 9
                || !pattern
                    .chars()
                    .all(|c| "TF*012".contains(c.to_ascii_uppercase()))
            {
                return None;
            }
            Term::boolean(matches(&relate(&x, &y), pattern))
        }
        "intersection" | "union" | "difference" | "symDifference" => {
            let (x, _) = geom_arg(a(0))?;
            let (y, _) = geom_arg(a(1))?;
            let op = match name {
                "intersection" => SetOp::Intersection,
                "union" => SetOp::Union,
                "difference" => SetOp::Difference,
                _ => SetOp::SymDifference,
            };
            wkt_term(&set_op(op, &x, &y))
        }
        "distance" => {
            let (x, _) = geom_arg(a(0))?;
            let (y, _) = geom_arg(a(1))?;
            double(in_units(distance_m(&x, &y), &unit(2)?, false)?)
        }
        "area" => double(in_units(area_m2(&geom_arg(a(0))?.0), &unit(1)?, true)?),
        "length" => double(in_units(
            length_m(&geom_arg(a(0))?.0, false),
            &unit(1)?,
            false,
        )?),
        "perimeter" => double(in_units(
            length_m(&geom_arg(a(0))?.0, true),
            &unit(1)?,
            false,
        )?),
        "envelope" => wkt_term(&envelope(&geom_arg(a(0))?.0)),
        "convexHull" => wkt_term(&convex_hull(&geom_arg(a(0))?.0)),
        "centroid" => wkt_term(&centroid(&geom_arg(a(0))?.0)),
        "boundary" => wkt_term(&boundary(&geom_arg(a(0))?.0)),
        "dimension" => Term::integer(dimension(&geom_arg(a(0))?.0)),
        "coordinateDimension" => Term::integer(geom_arg(a(0))?.1 as i64),
        "spatialDimension" => Term::integer(geom_arg(a(0))?.1.min(3) as i64),
        "isEmpty" => Term::boolean(is_empty(&geom_arg(a(0))?.0)),
        "isSimple" => Term::boolean(is_simple(&geom_arg(a(0))?.0)),
        "asWKT" => wkt_term(&geom_arg(a(0))?.0),
        "asGeoJSON" => Term::typed(to_geojson(&geom_arg(a(0))?.0).to_string(), GEOJSON_LITERAL),
        "getSRID" => {
            geom_arg(a(0))?;
            let l = a(0)?.as_literal()?;
            let crs = if l.datatype == WKT_LITERAL {
                wkt_srid(&l.value)
            } else {
                CRS84
            };
            Term::typed(crs, "http://www.w3.org/2001/XMLSchema#anyURI")
        }
        "minX" | "minY" | "maxX" | "maxY" => {
            let (x0, y0, x1, y1) = coord_bounds(&geom_arg(a(0))?.0)?;
            double(match name {
                "minX" => x0,
                "minY" => y0,
                "maxX" => x1,
                _ => y1,
            })
        }
        "numGeometries" => Term::integer(match geom_arg(a(0))?.0 {
            Geom::Empty(_) => 0,
            Geom::MultiPoint(v) => v.len() as i64,
            Geom::MultiLine(v) => v.len() as i64,
            Geom::MultiPolygon(v) => v.len() as i64,
            Geom::Collection(v) => v.len() as i64,
            _ => 1,
        }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(s: &str) -> Geom {
        parse_wkt(s).unwrap().0
    }

    #[test]
    fn topology_matrix() {
        let sq = g("POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))");
        let inner = g("POLYGON((1 1, 2 1, 2 2, 1 2, 1 1))");
        let right = g("POLYGON((4 0, 6 0, 6 4, 4 4, 4 0))");
        let over = g("POLYGON((3 3, 5 3, 5 5, 3 5, 3 3))");
        let far = g("POLYGON((10 10, 11 10, 11 11, 10 11, 10 10))");
        assert_eq!(relate(&sq, &sq), "2FFF1FFF2");
        assert_eq!(relate(&sq, &inner), "212FF1FF2");
        assert_eq!(relate(&inner, &sq), "2FF1FF212");
        assert_eq!(relate(&sq, &right), "FF2F11212");
        assert_eq!(relate(&sq, &over), "212101212");
        assert_eq!(relate(&sq, &far), "FF2FF1212");
        assert_eq!(topology("sfContains", &sq, &inner), Some(true));
        assert_eq!(topology("sfWithin", &inner, &sq), Some(true));
        assert_eq!(topology("sfTouches", &sq, &right), Some(true));
        assert_eq!(topology("sfOverlaps", &sq, &over), Some(true));
        assert_eq!(topology("sfDisjoint", &sq, &far), Some(true));
        let pt = g("POINT(2 2)");
        let edge_pt = g("POINT(4 2)");
        assert_eq!(relate(&pt, &sq), "0FFFFF212");
        assert_eq!(topology("sfTouches", &edge_pt, &sq), Some(true));
        let line = g("LINESTRING(-1 2, 5 2)");
        assert_eq!(topology("sfCrosses", &line, &sq), Some(true));
        assert_eq!(
            relate(&g("LINESTRING(0 0, 2 2)"), &g("LINESTRING(0 2, 2 0)")),
            "0F1FF0102"
        );
    }

    #[test]
    fn set_operations_and_metrics() {
        let a = g("POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))");
        let b = g("POLYGON((1 1, 3 1, 3 3, 1 3, 1 1))");
        let i = set_op(SetOp::Intersection, &a, &b);
        assert_eq!(
            topology("sfEquals", &i, &g("POLYGON((1 1, 2 1, 2 2, 1 2, 1 1))")),
            Some(true),
            "{}",
            to_wkt(&i)
        );
        let u = set_op(SetOp::Union, &a, &b);
        assert!(
            (signed_area(&parts(&u).polys[0][0]).abs() - 7.0).abs() < 1e-9,
            "{}",
            to_wkt(&u)
        );
        let d = set_op(SetOp::Difference, &a, &b);
        assert!(
            (signed_area(&parts(&d).polys[0][0]).abs() - 3.0).abs() < 1e-9,
            "{}",
            to_wkt(&d)
        );
        // Paris - London ~ 344 km
        let km = distance_m(&g("POINT(2.3522 48.8566)"), &g("POINT(-0.1276 51.5072)")) / 1000.0;
        assert!((km - 343.5).abs() < 2.0, "{km}");
        assert_eq!(
            to_wkt(&envelope(&g("LINESTRING(0 0, 3 1, 1 4)"))),
            "POLYGON((0 0, 3 0, 3 4, 0 4, 0 0))"
        );
        assert_eq!(to_wkt(&centroid(&a)), "POINT(1 1)");
        assert!(
            parse_geojson(r#"{"type":"Point","coordinates":[1,2]}"#).unwrap()
                == Geom::Point((1.0, 2.0))
        );
    }
}

#[cfg(test)]
mod spatial {
    use super::*;

    #[test]
    fn coordinate_systems() {
        // reference values from PROJ (pyproj 3, EPSG:4326 -> target, always_xy)
        let cases = [
            (
                32635,
                (666370.5050168498, 4541552.4871906955),
                (28.9784, 41.0082),
            ),
            (
                32631,
                (452482.5327026278, 5411717.176868899),
                (2.3522, 48.8566),
            ),
            (
                32756,
                (334368.633648097, 6250948.345385009),
                (151.2093, -33.8688),
            ),
            (
                3857,
                (3225860.7320037987, 5013551.237222597),
                (28.9784, 41.0082),
            ),
        ];
        for (code, (x, y), (lon, lat)) in cases {
            let (g, _) = parse_wkt(&format!("<{EPSG}{code}> POINT({x} {y})")).unwrap();
            let Geom::Point((a, b)) = g else { panic!() };
            assert!(
                (a - lon).abs() < 1e-7 && (b - lat).abs() < 1e-7,
                "{code}: {a} {b}"
            );
        }
        let (fx, fy) = utm_forward((28.9784, 41.0082), 35);
        assert!(
            (fx - 666370.505).abs() < 0.01 && (fy - 4541552.487).abs() < 0.01,
            "{fx} {fy}"
        );
        // EPSG:4326 is written latitude first
        assert_eq!(
            parse_wkt(&format!("<{EPSG}4326> POINT(41 29)")).unwrap().0,
            Geom::Point((29.0, 41.0))
        );
        assert_eq!(
            wkt_srid(&format!("<{EPSG}3857> POINT(0 0)")),
            format!("{EPSG}3857")
        );
        assert!(parse_wkt(&format!("<{EPSG}2154> POINT(0 0)")).is_err());
        // the same place in two systems
        let a = parse_wkt("POLYGON((28.9 41.0, 29.1 41.0, 29.1 41.1, 28.9 41.1, 28.9 41.0))")
            .unwrap()
            .0;
        let b = parse_wkt(&format!("<{EPSG}32635> POINT(666370.505 4541552.487)"))
            .unwrap()
            .0;
        assert_eq!(topology("sfWithin", &b, &a), Some(true));
    }

    /// Every box meeting the query box is found through the cells the query reads.
    #[test]
    fn cells_cover_every_meeting_box() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut boxes = Vec::new();
        for i in 0..3000 {
            // points, small and large boxes, some at the edges of the plane
            let size = [0.0, 1e-6, 1e-3, 0.5, 20.0, 200.0][i % 6];
            let (x, y) = (rnd() * 380.0 - 190.0, rnd() * 190.0 - 95.0);
            boxes.push((x, y, x + size * rnd(), y + size * rnd()));
        }
        let meets = |a: Bbox, b: Bbox| a.0 <= b.2 && b.0 <= a.2 && a.1 <= b.3 && b.1 <= a.3;
        let index: Vec<(u64, u32, usize)> = boxes
            .iter()
            .enumerate()
            .flat_map(|(i, b)| index_cells(*b).into_iter().map(move |(z, l)| (z, l, i)))
            .collect();
        for q in 0..300 {
            let size = [0.0, 1e-5, 0.01, 1.0, 30.0][q % 5];
            let (x, y) = (rnd() * 360.0 - 180.0, rnd() * 180.0 - 90.0);
            let qb = (x, y, x + size * rnd(), y + size * rnd());
            let (ranges, coarse) = query_cells(qb);
            assert!(ranges.len() <= 17 * 17, "{}", ranges.len());
            let found: std::collections::HashSet<usize> = index
                .iter()
                .filter(|(z, l, _)| {
                    ranges.iter().any(|(a, b)| a <= z && z < b) || coarse.contains(&(*z, *l))
                })
                .map(|e| e.2)
                .collect();
            for (i, b) in boxes.iter().enumerate() {
                if meets(*b, qb) {
                    assert!(
                        found.contains(&i),
                        "box {b:?} meets {qb:?} but was not found"
                    );
                }
            }
        }
    }

    #[test]
    fn filter_boxes() {
        let wkt = |s: &str| Some(Term::typed(s, WKT_LITERAL));
        let sq = wkt("POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))");
        let within = format!("{GEOF}sfWithin");
        let b = filter_box(&within, &[None, sq.clone()], &|i| i == 0, None).unwrap();
        assert!(b.0 < 0.0 && b.0 > -1e-6 && b.2 > 1.0 && b.2 < 1.0 + 1e-6);
        assert!(
            filter_box(
                &format!("{GEOF}sfDisjoint"),
                &[None, sq.clone()],
                &|i| i == 0,
                None
            )
            .is_none()
        );
        assert!(filter_box(&within, &[sq.clone(), sq.clone()], &|_| false, None).is_none());
        // 10 km around a point at 60 N: ~0.09 degrees of latitude, ~0.18 of longitude
        let d = filter_box(
            &format!("{GEOF}distance"),
            &[
                None,
                wkt("POINT(10 60)"),
                Some(Term::iri(format!("{UOM}kilometre"))),
            ],
            &|i| i == 0,
            Some(10.0),
        )
        .unwrap();
        assert!(
            (d.3 - 60.0 - 0.0899).abs() < 0.001 && (d.2 - 10.0 - 0.1799).abs() < 0.002,
            "{d:?}"
        );
        // every point the box leaves out is farther than the radius
        for (lon, lat) in [(10.0, d.3 + 1e-4), (d.2 + 1e-4, 60.09), (d.0 - 1e-4, 59.91)] {
            assert!(
                haversine((lon, lat), (10.0, 60.0)) > 10_000.0,
                "{lon} {lat}"
            );
        }
    }
}

#[cfg(test)]
mod differential {
    /// Cases from `GEO_CASES` (one `wkt_a<TAB>wkt_b` per line) -> `GEO_OUT` lines of
    /// `relate<TAB>intersection<TAB>union<TAB>difference<TAB>symDifference`. Driven by
    /// tests/prod/geo_diff.py against GEOS.
    #[test]
    #[ignore]
    fn dump() {
        let (Ok(cases), Ok(out)) = (std::env::var("GEO_CASES"), std::env::var("GEO_OUT")) else {
            return;
        };
        let mut lines = Vec::new();
        for line in std::fs::read_to_string(cases).unwrap().lines() {
            let (a, b) = line.split_once('\t').unwrap();
            let (a, b) = (
                super::parse_wkt(a).unwrap().0,
                super::parse_wkt(b).unwrap().0,
            );
            let ops = [
                super::SetOp::Intersection,
                super::SetOp::Union,
                super::SetOp::Difference,
                super::SetOp::SymDifference,
            ];
            let mut cols = vec![super::relate(&a, &b)];
            cols.extend(
                ops.iter()
                    .map(|op| super::to_wkt(&super::set_op(*op, &a, &b))),
            );
            lines.push(cols.join("\t"));
        }
        std::fs::write(out, lines.join("\n") + "\n").unwrap();
    }
}
