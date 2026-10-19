//! SPARQL operator and function semantics over RDF terms.

use std::cmp::Ordering;

use super::regex::Regex;
use crate::rdf::{
    Literal, RDF_LANGSTRING, Term, XSD, XSD_BOOLEAN, XSD_DATETIME, XSD_DECIMAL, XSD_DOUBLE,
    XSD_STRING, write::format_double,
};

/// `xsd:float`.
pub const XSD_FLOAT: &str = "http://www.w3.org/2001/XMLSchema#float";

const INTEGER_TYPES: [&str; 13] = [
    "integer",
    "int",
    "long",
    "short",
    "byte",
    "nonNegativeInteger",
    "positiveInteger",
    "nonPositiveInteger",
    "negativeInteger",
    "unsignedLong",
    "unsignedInt",
    "unsignedShort",
    "unsignedByte",
];

#[derive(Debug, Clone, Copy)]
/// A numeric operand, in the widest of the SPARQL numeric types.
pub enum Num {
    /// `xsd:integer`.
    Int(i64),
    /// `xsd:decimal`.
    Dec(Dec),
    /// `xsd:float`.
    Float(f64),
    /// `xsd:double`.
    Double(f64),
}

impl Num {
    fn rank(&self) -> u8 {
        match self {
            Num::Int(_) => 0,
            Num::Dec(_) => 1,
            Num::Float(_) => 2,
            Num::Double(_) => 3,
        }
    }
    /// The value as `f64` (exact for integers and decimals within range).
    pub fn f(&self) -> f64 {
        match *self {
            Num::Int(i) => i as f64,
            Num::Dec(d) => d.f(),
            Num::Float(x) | Num::Double(x) => x,
        }
    }
    /// Exact decimal view of an integer or decimal.
    pub fn dec(&self) -> Option<Dec> {
        match *self {
            Num::Int(i) => Some(Dec::from_i64(i)),
            Num::Dec(d) => Some(d),
            _ => None,
        }
    }
    fn with_rank(rank: u8, v: f64) -> Num {
        match rank {
            1 => Dec::from_f64(v).map_or(Num::Double(v), Num::Dec),
            2 => Num::Float(v),
            _ => Num::Double(v),
        }
    }
    /// The value as a literal of its own type, in canonical form.
    pub fn to_term(self) -> Term {
        match self {
            Num::Int(i) => Term::integer(i),
            Num::Dec(d) => Term::typed(d.to_string(), XSD_DECIMAL),
            Num::Float(x) => Term::typed(format_double(x), XSD_FLOAT),
            Num::Double(x) => Term::typed(format_double(x), XSD_DOUBLE),
        }
    }
}

/// Exact xsd:decimal: `m / 10^s`. 128 bits hold 38 significant digits; operations that
/// would overflow fail (a SPARQL error) instead of losing precision silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dec {
    m: i128,
    s: u32,
}

const DEC_DIV_SCALE: u32 = 20;

impl Dec {
    /// An integer as a decimal.
    pub fn from_i64(i: i64) -> Dec {
        Dec { m: i as i128, s: 0 }
    }

    /// Parse an `xsd:decimal` lexical form; `None` when it is not one.
    pub fn parse(v: &str) -> Option<Dec> {
        let v = v.trim();
        let (neg, body) = match v.as_bytes().first()? {
            b'-' => (true, &v[1..]),
            b'+' => (false, &v[1..]),
            _ => (false, v),
        };
        let (int, frac) = body.split_once('.').unwrap_or((body, ""));
        if (int.is_empty() && frac.is_empty())
            || !int.bytes().all(|b| b.is_ascii_digit())
            || !frac.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let frac = frac.trim_end_matches('0');
        let digits = format!("{int}{frac}");
        let m: i128 = if digits.is_empty() {
            0
        } else {
            digits.parse().ok()?
        };
        Some(
            Dec {
                m: if neg { -m } else { m },
                s: frac.len() as u32,
            }
            .norm(),
        )
    }

    /// The nearest decimal to `f` (its shortest representation); `None` for NaN and
    /// infinities.
    pub fn from_f64(f: f64) -> Option<Dec> {
        if !f.is_finite() {
            return None;
        }
        let s = format!("{f}");
        Dec::parse(&s).or_else(|| Dec::parse(format!("{f:.20}").trim_end_matches('0')))
    }

    /// The value as `f64`.
    pub fn f(&self) -> f64 {
        self.m as f64 / 10f64.powi(self.s as i32)
    }

    fn norm(mut self) -> Dec {
        while self.s > 0 && self.m % 10 == 0 {
            self.m /= 10;
            self.s -= 1;
        }
        self
    }

    fn pow10(n: u32) -> Option<i128> {
        10i128.checked_pow(n)
    }

    /// Both mantissas at the larger scale.
    fn align(a: Dec, b: Dec) -> Option<(i128, i128, u32)> {
        let s = a.s.max(b.s);
        Some((
            a.m.checked_mul(Self::pow10(s - a.s)?)?,
            b.m.checked_mul(Self::pow10(s - b.s)?)?,
            s,
        ))
    }

    /// Sum, `None` on overflow.
    pub fn add(a: Dec, b: Dec) -> Option<Dec> {
        let (x, y, s) = Self::align(a, b)?;
        Some(
            Dec {
                m: x.checked_add(y)?,
                s,
            }
            .norm(),
        )
    }

    /// Difference, `None` on overflow.
    pub fn sub(a: Dec, b: Dec) -> Option<Dec> {
        let (x, y, s) = Self::align(a, b)?;
        Some(
            Dec {
                m: x.checked_sub(y)?,
                s,
            }
            .norm(),
        )
    }

    /// Product, `None` on overflow.
    pub fn mul(a: Dec, b: Dec) -> Option<Dec> {
        Some(
            Dec {
                m: a.m.checked_mul(b.m)?,
                s: a.s + b.s,
            }
            .norm(),
        )
    }

    /// Division to DEC_DIV_SCALE fractional digits (truncated), None when dividing by zero.
    pub fn div(a: Dec, b: Dec) -> Option<Dec> {
        if b.m == 0 {
            return None;
        }
        // a/b = (a.m * 10^(k + b.s - a.s)) / b.m / 10^k
        let k = DEC_DIV_SCALE;
        let shift = (k + b.s) as i64 - a.s as i64;
        let (num, den) = if shift >= 0 {
            (a.m.checked_mul(Self::pow10(shift as u32)?)?, b.m)
        } else {
            (a.m, b.m.checked_mul(Self::pow10((-shift) as u32)?)?)
        };
        Some(Dec { m: num / den, s: k }.norm())
    }

    /// Numeric order of the two decimals.
    pub fn cmp(a: Dec, b: Dec) -> Option<Ordering> {
        let (x, y, _) = Self::align(a, b)?;
        Some(x.cmp(&y))
    }

    /// The absolute value.
    pub fn abs(self) -> Dec {
        Dec {
            m: self.m.abs(),
            s: self.s,
        }
    }

    /// Rounded towards negative infinity, at scale 0.
    pub fn floor(self) -> Dec {
        let p = Self::pow10(self.s).unwrap_or(1);
        Dec {
            m: self.m.div_euclid(p),
            s: 0,
        }
    }

    /// Rounded towards positive infinity, at scale 0.
    pub fn ceil(self) -> Dec {
        let p = Self::pow10(self.s).unwrap_or(1);
        Dec {
            m: -(-self.m).div_euclid(p),
            s: 0,
        }
    }

    /// fn:round: halves round towards positive infinity.
    pub fn round(self) -> Dec {
        if self.s == 0 {
            return self;
        }
        let half = Dec { m: 5, s: 1 };
        Dec::add(self, half).map_or(self, |d| d.floor())
    }
}

impl std::fmt::Display for Dec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let neg = self.m < 0;
        let digits = self.m.unsigned_abs().to_string();
        let s = self.s as usize;
        let (int, frac) = if digits.len() > s {
            (
                digits[..digits.len() - s].to_string(),
                digits[digits.len() - s..].to_string(),
            )
        } else {
            (
                "0".to_string(),
                format!("{}{digits}", "0".repeat(s - digits.len())),
            )
        };
        let frac = if frac.is_empty() {
            "0".to_string()
        } else {
            frac
        };
        write!(f, "{}{int}.{frac}", if neg { "-" } else { "" })
    }
}

/// Whether the datatype is one of the derived XML Schema integer types.
pub fn is_integer_type(dt: &str) -> bool {
    dt.strip_prefix(XSD)
        .is_some_and(|l| INTEGER_TYPES.contains(&l))
}

/// The numeric value of a literal, when its datatype is numeric and its lexical
/// form parses; `None` otherwise (an unbound or non-numeric term).
pub fn num(t: &Term) -> Option<Num> {
    let l = t.as_literal()?;
    let v = l.value.trim();
    if is_integer_type(&l.datatype) {
        return v.trim_start_matches('+').parse().ok().map(Num::Int);
    }
    match l.datatype.as_str() {
        XSD_DECIMAL => Dec::parse(v).map(Num::Dec),
        XSD_DOUBLE => parse_float(v).map(Num::Double),
        XSD_FLOAT => parse_float(v).map(Num::Float),
        _ => None,
    }
}

fn parse_float(v: &str) -> Option<f64> {
    match v {
        "INF" | "+INF" => Some(f64::INFINITY),
        "-INF" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        _ => v.parse().ok(),
    }
}

/// SPARQL arithmetic: `+ - * /` with the result in the operands' widest type;
/// `/` on two integers gives a decimal; `None` divides by zero or overflows.
pub fn arith(op: char, a: Num, b: Num) -> Option<Num> {
    let rank = a.rank().max(b.rank());
    if rank == 0 {
        let (Num::Int(x), Num::Int(y)) = (a, b) else {
            return None;
        };
        return match op {
            '+' => x.checked_add(y).map(Num::Int),
            '-' => x.checked_sub(y).map(Num::Int),
            '*' => x.checked_mul(y).map(Num::Int),
            '/' => Dec::div(Dec::from_i64(x), Dec::from_i64(y)).map(Num::Dec),
            _ => None,
        };
    }
    if rank == 1 {
        // integers and decimals: exact decimal arithmetic
        let (x, y) = (a.dec()?, b.dec()?);
        return match op {
            '+' => Dec::add(x, y),
            '-' => Dec::sub(x, y),
            '*' => Dec::mul(x, y),
            '/' => Dec::div(x, y),
            _ => None,
        }
        .map(Num::Dec);
    }
    let (x, y) = (a.f(), b.f());
    let v = match op {
        '+' => x + y,
        '-' => x - y,
        '*' => x * y,
        '/' => {
            if y == 0.0 && rank == 1 {
                return None;
            }
            x / y
        }
        _ => return None,
    };
    Some(Num::with_rank(rank, v))
}

/// String-like literal: (value, language).
pub fn string_lit(t: &Term) -> Option<(&str, Option<&str>)> {
    let l = t.as_literal()?;
    if l.datatype == XSD_STRING
        || l.datatype == RDF_LANGSTRING
        || l.datatype == crate::rdf::RDF_DIRLANGSTRING
    {
        Some((&l.value, l.lang.as_deref()))
    } else {
        None
    }
}

/// A string literal like `like`: same language tag and base direction (kept as `lang--dir`).
fn same_kind_string(value: String, like: Option<&str>) -> Term {
    match like.map(|l| l.split_once("--").map_or((l, None), |(l, d)| (l, Some(d)))) {
        Some((l, Some(d))) => Term::dir_lang(value, l, d),
        Some((l, None)) => Term::lang(value, l),
        None => Term::string(value),
    }
}

/// (language, direction) of a literal's tag.
fn lang_dir(t: &Term) -> Option<(String, String)> {
    let l = t.as_literal()?;
    let tag = l.lang.clone().unwrap_or_default();
    Some(match tag.split_once("--") {
        Some((lang, dir)) => (lang.to_string(), dir.to_string()),
        None => (tag, String::new()),
    })
}

/// The SPARQL *effective boolean value*: booleans by their lexical form, numbers
/// by being non-zero and not-NaN, plain strings by being non-empty; `None` is an
/// error (an invalid lexical form, a language-tagged string, an IRI).
pub fn ebv(t: &Term) -> Option<bool> {
    let l = t.as_literal()?;
    if l.datatype == XSD_BOOLEAN {
        // an invalid lexical form is an error
        return match l.value.as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        };
    }
    if let Some(n) = num(t) {
        let f = n.f();
        return Some(!(f == 0.0 || f.is_nan()));
    }
    // only xsd:string has an EBV among strings; language-tagged strings are an error
    if l.datatype == XSD_STRING {
        return Some(!l.value.is_empty());
    }
    None
}

// ------------------------------------------------------------- dateTime

#[derive(Debug, Clone, Copy, PartialEq)]
/// The parts of an `xsd:dateTime` lexical form.
pub struct DateTime {
    /// The year (may be negative, and 0 as written by XML Schema).
    pub year: i64,
    /// The month, 1-12.
    pub month: u32,
    /// The day, 1-31.
    pub day: u32,
    /// The hour, 0-23.
    pub hour: u32,
    /// The minute, 0-59.
    pub minute: u32,
    /// The second, 0-60 inclusive (a leap second).
    pub second: f64,
    /// The timezone offset in minutes east of UTC; `None` when the form has none.
    pub tz_minutes: Option<i32>,
}

/// Parse an `xsd:dateTime`, `xsd:date` or `xsd:time` lexical form; `None` when it
/// is none of them.
pub fn parse_datetime(s: &str) -> Option<DateTime> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (date, rest) = match s.find('T') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    };
    let mut dp = date.splitn(3, '-');
    let year: i64 = dp.next()?.parse().ok()?;
    let month: u32 = dp.next()?.parse().ok()?;
    let day_tz = dp.next()?;
    let (day_s, mut tz_part) = if rest.is_empty() {
        let cut = day_tz.find(['Z', '+', '-']).unwrap_or(day_tz.len());
        (&day_tz[..cut], &day_tz[cut..])
    } else {
        (day_tz, "")
    };
    let day: u32 = day_s.parse().ok()?;
    let (mut hour, mut minute, mut second) = (0, 0, 0.0);
    if !rest.is_empty() {
        let cut = rest.find(['Z', '+', '-']).unwrap_or(rest.len());
        tz_part = &rest[cut..];
        let mut tp = rest[..cut].splitn(3, ':');
        hour = tp.next()?.parse().ok()?;
        minute = tp.next()?.parse().ok()?;
        second = tp.next()?.parse().ok()?;
    }
    let tz_minutes = match tz_part {
        "" => None,
        "Z" => Some(0),
        t => {
            let sign = if t.starts_with('-') { -1 } else { 1 };
            let (h, m) = t[1..].split_once(':')?;
            Some(sign * (h.parse::<i32>().ok()? * 60 + m.parse::<i32>().ok()?))
        }
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(DateTime {
        year: if neg { -year } else { year },
        month,
        day,
        hour,
        minute,
        second,
        tz_minutes,
    })
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

impl DateTime {
    /// Seconds since the epoch, treating a missing timezone as UTC.
    pub fn epoch_seconds(&self) -> f64 {
        let days = days_from_civil(self.year, self.month, self.day);
        let secs = days as f64 * 86400.0
            + self.hour as f64 * 3600.0
            + self.minute as f64 * 60.0
            + self.second;
        secs - self.tz_minutes.unwrap_or(0) as f64 * 60.0
    }
}

/// The current moment as an `xsd:dateTime` literal, timezone `Z`.
pub fn now_datetime() -> Term {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // civil from days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d_ = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    Term::typed(
        format!(
            "{y:04}-{m:02}-{d_:02}T{:02}:{:02}:{:02}.{:03}Z",
            rem / 3600,
            (rem % 3600) / 60,
            rem % 60,
            d.subsec_millis()
        ),
        XSD_DATETIME,
    )
}

// ------------------------------------------------------------- comparison

const XSD_DATE: &str = "http://www.w3.org/2001/XMLSchema#date";

/// Compare two xsd:date or two xsd:dateTime values (XSD 1.1 order): a value without a
/// timezone is compared under every timezone (-14:00..+14:00); None when that is undecided.
fn temporal_cmp(x: &Literal, y: &Literal) -> Option<Ordering> {
    if x.datatype != y.datatype || !(x.datatype == XSD_DATETIME || x.datatype == XSD_DATE) {
        return None;
    }
    let (p, q) = (parse_datetime(&x.value)?, parse_datetime(&y.value)?);
    let (a, b) = (p.epoch_seconds(), q.epoch_seconds());
    match (p.tz_minutes.is_some(), q.tz_minutes.is_some()) {
        (true, true) | (false, false) => a.partial_cmp(&b),
        (has_a, _) => {
            // the zoneless side can be anywhere in [t - 14h, t + 14h]
            let w = 14.0 * 3600.0;
            let (lo, hi, fixed, flip) = if has_a {
                (b - w, b + w, a, true)
            } else {
                (a - w, a + w, b, false)
            };
            let o = if hi < fixed {
                Ordering::Less
            } else if lo > fixed {
                Ordering::Greater
            } else {
                return None;
            };
            Some(if flip { o.reverse() } else { o })
        }
    }
}

/// Numeric order: exact for integers and decimals, IEEE for float and double.
fn num_cmp(a: &Num, b: &Num) -> Option<Ordering> {
    match (a.dec(), b.dec()) {
        (Some(x), Some(y)) => Dec::cmp(x, y).or_else(|| a.f().partial_cmp(&b.f())),
        _ => a.f().partial_cmp(&b.f()),
    }
}

/// `=` semantics: Some(bool), or None for a type error.
pub fn equals(a: &Term, b: &Term) -> Option<bool> {
    if a == b {
        return Some(true);
    }
    match (a, b) {
        // triple terms are equal when their components are (value-equal for literals)
        (Term::Triple(x), Term::Triple(y)) => {
            let mut all = true;
            for i in 0..3 {
                all &= equals(&x[i], &y[i])?;
            }
            Some(all)
        }
        (Term::Literal(x), Term::Literal(y)) => {
            if let (Some(n), Some(m)) = (num(a), num(b)) {
                return Some(num_cmp(&n, &m) == Some(Ordering::Equal));
            }
            // language-tagged strings are disjoint from every other kind of literal
            match (&x.lang, &y.lang) {
                (Some(l), Some(m)) => return Some(l.eq_ignore_ascii_case(m) && x.value == y.value),
                (Some(_), None) | (None, Some(_)) => return Some(false),
                _ => {}
            }
            if (x.datatype == XSD_DATETIME || x.datatype == XSD_DATE) && x.datatype == y.datatype {
                return temporal_cmp(x, y).map(|o| o == Ordering::Equal);
            }
            if x.datatype == XSD_BOOLEAN && y.datatype == XSD_BOOLEAN {
                return Some(ebv(a) == ebv(b));
            }
            let known = |l: &Literal| {
                l.datatype == XSD_STRING
                    || l.datatype == XSD_BOOLEAN
                    || l.datatype == XSD_DATETIME
                    || l.datatype == XSD_DATE
                    || num(&Term::Literal(l.clone())).is_some()
            };
            if known(x) && known(y) {
                Some(false)
            } else {
                None // unknown datatypes with different lexical forms: error
            }
        }
        _ => Some(false),
    }
}

/// `<` family: comparable values only.
pub fn compare(a: &Term, b: &Term) -> Option<Ordering> {
    if let (Some(n), Some(m)) = (num(a), num(b)) {
        return num_cmp(&n, &m);
    }
    let (x, y) = (a.as_literal()?, b.as_literal()?);
    if (x.datatype == XSD_DATETIME || x.datatype == XSD_DATE) && x.datatype == y.datatype {
        return temporal_cmp(x, y);
    }
    if x.datatype == XSD_BOOLEAN && y.datatype == XSD_BOOLEAN {
        return Some(ebv(a)?.cmp(&ebv(b)?));
    }
    if (x.datatype == XSD_STRING && y.datatype == XSD_STRING)
        || (x.lang.is_some() && x.lang == y.lang)
    {
        return Some(x.value.cmp(&y.value));
    }
    if x.datatype == y.datatype && x.lang.is_none() {
        return Some(x.value.cmp(&y.value));
    }
    None
}

/// Total order for ORDER BY.
pub fn order_cmp(a: Option<&Term>, b: Option<&Term>) -> Ordering {
    fn rank(t: Option<&Term>) -> u8 {
        match t {
            None => 0,
            Some(Term::Blank(_)) => 1,
            Some(Term::Iri(_)) => 2,
            Some(Term::Literal(_)) => 3,
            Some(Term::Triple(_)) => 4,
        }
    }
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        // plain strings (the common case when sorting labels) compare by code point directly
        (Some(Term::Literal(l)), Some(Term::Literal(m)))
            if l.lang.is_none()
                && m.lang.is_none()
                && l.datatype == XSD_STRING
                && m.datatype == XSD_STRING =>
        {
            l.value.cmp(&m.value)
        }
        (Some(x), Some(y)) => {
            if let Some(o) = compare(x, y) {
                if o != Ordering::Equal {
                    return o;
                }
            }
            match (x, y) {
                // triple terms order by subject, then predicate, then object
                (Term::Triple(p), Term::Triple(q)) => (0..3)
                    .map(|i| order_cmp(Some(&p[i]), Some(&q[i])))
                    .find(|o| *o != Ordering::Equal)
                    .unwrap_or(Ordering::Equal),
                (Term::Literal(l), Term::Literal(m)) => l
                    .value
                    .cmp(&m.value)
                    .then_with(|| l.datatype.cmp(&m.datatype))
                    .then_with(|| l.lang.cmp(&m.lang)),
                _ => x.value().cmp(y.value()),
            }
        }
        _ => Ordering::Equal,
    }
}

// ------------------------------------------------------------- casts

/// fn:string of an xs:double / xs:float: plain decimal notation for magnitudes in
/// [1e-6, 1e6), scientific notation otherwise.
fn xpath_float_string(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "INF".into() } else { "-INF".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    if (1e-6..1e6).contains(&f.abs()) {
        let s = format!("{f}");
        return s.strip_suffix(".0").map(String::from).unwrap_or(s);
    }
    format_double(f)
}

/// XSD Casting: `v` to the datatype `dt` as the spec's cast table says; `None`
/// when the cast does not exist or the lexical form is invalid.
pub fn cast(dt: &str, v: &Term) -> Option<Term> {
    let lex = match v {
        Term::Iri(i) if dt == XSD_STRING => return Some(Term::string(i.clone())),
        Term::Literal(l) => l.value.trim().to_string(),
        _ => return None,
    };
    let as_num = num(v);
    let as_bool = v
        .as_literal()
        .filter(|l| l.datatype == XSD_BOOLEAN)
        .map(|l| l.value == "true" || l.value == "1");
    Some(match dt {
        XSD_STRING => Term::string(match (as_num, as_bool) {
            // XPath casting: numbers and booleans become their canonical form
            (_, Some(b)) => b.to_string(),
            (Some(Num::Int(i)), _) => i.to_string(),
            (Some(Num::Dec(d)), _) => d.to_string().trim_end_matches(".0").to_string(),
            (Some(n), _) => xpath_float_string(n.f()),
            _ => v.as_literal()?.value.clone(),
        }),
        XSD_BOOLEAN => {
            if let Some(b) = as_bool {
                Term::boolean(b)
            } else if let Some(n) = as_num {
                Term::boolean(n.f() != 0.0 && !n.f().is_nan())
            } else {
                match lex.as_str() {
                    "true" | "1" => Term::boolean(true),
                    "false" | "0" => Term::boolean(false),
                    _ => return None,
                }
            }
        }
        XSD_DOUBLE | XSD_FLOAT => {
            let f = match (as_num, as_bool) {
                (Some(n), _) => n.f(),
                (_, Some(b)) => b as i64 as f64,
                _ => parse_float(&lex)?,
            };
            if dt == XSD_DOUBLE {
                Num::Double(f).to_term()
            } else {
                Num::Float(f).to_term()
            }
        }
        XSD_DECIMAL => {
            let f = match (as_num, as_bool) {
                (Some(n), _) if n.f().is_finite() => n.f(),
                (Some(_), _) => return None,
                (_, Some(b)) => b as i64 as f64,
                _ => {
                    if lex.contains(['e', 'E']) {
                        return None;
                    }
                    lex.parse().ok()?
                }
            };
            match (as_num, as_bool) {
                (Some(n), _) => match n.dec() {
                    Some(d) => Num::Dec(d).to_term(),
                    None => Num::Dec(Dec::from_f64(f)?).to_term(),
                },
                _ => Num::Dec(Dec::parse(&lex).or_else(|| Dec::from_f64(f))?).to_term(),
            }
        }
        XSD_DATETIME => {
            parse_datetime(&lex)?;
            Term::typed(lex, XSD_DATETIME)
        }
        d if is_integer_type(d) => {
            let i = match (as_num, as_bool) {
                (Some(Num::Int(i)), _) => i,
                (Some(n), _) if n.f().is_finite() => n.f().trunc() as i64,
                (Some(_), _) => return None,
                (_, Some(b)) => b as i64,
                _ => lex.trim_start_matches('+').parse().ok()?,
            };
            Term::typed(i.to_string(), d)
        }
        _ => return None,
    })
}

// ------------------------------------------------------------- built-ins

fn str_arg(t: &Term) -> Option<(&str, Option<&str>)> {
    string_lit(t)
}

/// Strings are argument-compatible if the second has no language or the same one.
fn compatible(a: Option<&str>, b: Option<&str>) -> bool {
    b.is_none() || a == b
}

fn rand_u64() -> u64 {
    use std::cell::Cell;
    thread_local!(static STATE: Cell<u64> = Cell::new(
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(88172645463325252, |d| d.as_nanos() as u64) | 1
    ));
    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}

fn uuid() -> String {
    let (a, b) = (rand_u64(), rand_u64());
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        a >> 32,
        (a >> 16) & 0xffff,
        a & 0xfff,
        0x8000 | (b >> 48) & 0x3fff,
        b & 0xffff_ffff_ffff
    )
}

/// A compiled pattern from a small per-thread cache: a FILTER runs REGEX once per row with
/// the same pattern, which should be compiled once.
fn cached_regex(pat: &str, flags: &str) -> Option<std::rc::Rc<Regex>> {
    thread_local! {
        static CACHE: std::cell::RefCell<Vec<(String, String, std::rc::Rc<Regex>)>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(i) = c.iter().position(|(p, f, _)| p == pat && f == flags) {
            let hit = c.remove(i);
            let r = hit.2.clone();
            c.push(hit);
            return Some(r);
        }
        let r = std::rc::Rc::new(Regex::new(pat, flags).ok()?);
        if c.len() >= 32 {
            c.remove(0);
        }
        c.push((pat.to_string(), flags.to_string(), r.clone()));
        Some(r)
    })
}

/// Whether CONTAINS, STRSTARTS, STRENDS or REGEX holds for the string `s` and the other
/// arguments (None for an error, or another function). Shared by `call` and the evaluator,
/// which tests a term where it lies instead of copying it into an argument list.
pub(crate) fn string_test(name: &str, s: &Term, rest: [Option<&Term>; 2]) -> Option<bool> {
    let (s, l1) = str_arg(s)?;
    match name {
        "CONTAINS" | "STRSTARTS" | "STRENDS" => {
            let (t, l2) = str_arg(rest[0]?)?;
            if !compatible(l1, l2) {
                return None;
            }
            Some(match name {
                "CONTAINS" => s.contains(t),
                "STRSTARTS" => s.starts_with(t),
                _ => s.ends_with(t),
            })
        }
        "REGEX" => {
            let pat = str_arg(rest[0]?)?.0;
            let flags = rest[1].and_then(str_arg).map_or("", |f| f.0);
            Some(cached_regex(pat, flags)?.is_match(s))
        }
        _ => None,
    }
}

/// Evaluate an eager built-in or cast. `args` are already evaluated
/// (None = error/unbound). Returns None on error.
pub fn call(name: &str, args: &[Option<Term>], base: &str) -> Option<Term> {
    let a = |i: usize| args.get(i).and_then(|x| x.as_ref());
    if name.starts_with(XSD) {
        return cast(name, a(0)?);
    }
    if let Some(local) = name.strip_prefix(crate::geo::GEOF) {
        return crate::geo::sparql_call(local, args);
    }
    Some(match name {
        "STR" => match a(0)? {
            Term::Blank(_) => return None,
            t => Term::string(t.value().to_string()),
        },
        "LANG" => Term::string(lang_dir(a(0)?)?.0),
        // SPARQL 1.2 base direction
        "LANGDIR" => Term::string(lang_dir(a(0)?)?.1),
        "HASLANG" => Term::boolean(lang_dir(a(0)?).is_some_and(|(l, _)| !l.is_empty())),
        "HASLANGDIR" => Term::boolean(lang_dir(a(0)?).is_some_and(|(_, d)| !d.is_empty())),
        "STRLANGDIR" => {
            let (s, l) = str_arg(a(0)?)?;
            let lang = str_arg(a(1)?)?.0;
            let dir = str_arg(a(2)?)?.0;
            if l.is_some() || !crate::rdf::valid_lang(lang) || (dir != "ltr" && dir != "rtl") {
                return None;
            }
            Term::dir_lang(s.to_string(), lang, dir)
        }
        "DATATYPE" => Term::Iri(a(0)?.as_literal()?.datatype.clone()),
        "LANGMATCHES" => {
            let tag = str_arg(a(0)?)?.0.to_ascii_lowercase();
            let range = str_arg(a(1)?)?.0.to_ascii_lowercase();
            Term::boolean(if range == "*" {
                !tag.is_empty()
            } else {
                tag == range || tag.starts_with(&format!("{range}-"))
            })
        }
        "IRI" | "URI" => match a(0)? {
            Term::Iri(i) => Term::Iri(i.clone()),
            t => Term::Iri(crate::rdf::resolve_iri(base, &string_lit(t)?.0.to_string())),
        },
        "BNODE" => match a(0) {
            Some(_) if args.is_empty() => return None,
            _ => Term::fresh_blank(),
        },
        "RAND" => Num::Double((rand_u64() >> 11) as f64 / (1u64 << 53) as f64).to_term(),
        "NOW" => now_datetime(),
        "UUID" => Term::Iri(format!("urn:uuid:{}", uuid())),
        "STRUUID" => Term::string(uuid()),
        "ABS" | "CEIL" | "FLOOR" | "ROUND" => {
            let n = num(a(0)?)?;
            match n {
                Num::Int(i) => Term::integer(if name == "ABS" { i.abs() } else { i }),
                Num::Dec(d) => Num::Dec(match name {
                    "ABS" => d.abs(),
                    "CEIL" => d.ceil(),
                    "FLOOR" => d.floor(),
                    _ => d.round(),
                })
                .to_term(),
                other => {
                    let f = other.f();
                    let r = match name {
                        "ABS" => f.abs(),
                        "CEIL" => f.ceil(),
                        "FLOOR" => f.floor(),
                        _ => (f + 0.5).floor(),
                    };
                    Num::with_rank(other.rank(), r).to_term()
                }
            }
        }
        "STRLEN" => Term::integer(str_arg(a(0)?)?.0.chars().count() as i64),
        "UCASE" => {
            let (s, l) = str_arg(a(0)?)?;
            same_kind_string(s.to_uppercase(), l)
        }
        "LCASE" => {
            let (s, l) = str_arg(a(0)?)?;
            same_kind_string(s.to_lowercase(), l)
        }
        "ENCODE_FOR_URI" => Term::string(crate::http::percent_encode(str_arg(a(0)?)?.0)),
        "CONTAINS" | "STRSTARTS" | "STRENDS" | "REGEX" => {
            Term::boolean(string_test(name, a(0)?, [a(1), a(2)])?)
        }
        "STRBEFORE" | "STRAFTER" => {
            let (s, l1) = str_arg(a(0)?)?;
            let (t, l2) = str_arg(a(1)?)?;
            if !compatible(l1, l2) {
                return None;
            }
            match s.find(t) {
                None => Term::string(""),
                Some(i) => {
                    let part = if name == "STRBEFORE" {
                        &s[..i]
                    } else {
                        &s[i + t.len()..]
                    };
                    if part.is_empty() && t.is_empty() && name == "STRAFTER" {
                        same_kind_string(s.to_string(), l1)
                    } else {
                        same_kind_string(part.to_string(), l1)
                    }
                }
            }
        }
        "CONCAT" => {
            let mut out = String::new();
            let mut lang: Option<Option<&str>> = None;
            for x in args {
                let (s, l) = str_arg(x.as_ref()?)?;
                out.push_str(s);
                lang = match lang {
                    None => Some(l),
                    Some(prev) if prev == l => Some(l),
                    Some(_) => Some(None),
                };
            }
            same_kind_string(out, lang.flatten())
        }
        "SUBSTR" => {
            let (s, l) = str_arg(a(0)?)?;
            let start = num(a(1)?)?.f().round() as i64;
            let chars: Vec<char> = s.chars().collect();
            let len = match a(2) {
                Some(t) => num(t)?.f().round() as i64,
                None => chars.len() as i64 + 1,
            };
            let from = start.max(1);
            let to = (start + len).min(chars.len() as i64 + 1);
            let out: String = if to > from {
                chars[(from - 1) as usize..(to - 1) as usize]
                    .iter()
                    .collect()
            } else {
                String::new()
            };
            same_kind_string(out, l)
        }
        "REPLACE" => {
            let (s, l) = str_arg(a(0)?)?;
            let pat = str_arg(a(1)?)?.0;
            let rep = str_arg(a(2)?)?.0;
            let flags = a(3).and_then(|f| str_arg(f)).map_or("", |f| f.0);
            same_kind_string(cached_regex(pat, flags)?.replace_all(s, rep), l)
        }
        "STRLANG" => {
            let (s, l) = str_arg(a(0)?)?;
            if l.is_some() || !crate::rdf::valid_lang(str_arg(a(1)?)?.0) {
                return None;
            }
            Term::lang(s.to_string(), str_arg(a(1)?)?.0)
        }
        "STRDT" => {
            let (s, l) = str_arg(a(0)?)?;
            if l.is_some() {
                return None;
            }
            match a(1)? {
                Term::Iri(dt) => Term::typed(s.to_string(), dt),
                _ => return None,
            }
        }
        "SAMETERM" => Term::boolean(a(0)? == a(1)?),
        "TRIPLE" => {
            let (s, p, o) = (a(0)?.clone(), a(1)?.clone(), a(2)?.clone());
            if !(s.is_iri() || s.is_blank()) || !p.is_iri() {
                return None;
            }
            Term::triple(s, p, o)
        }
        "SUBJECT" | "PREDICATE" | "OBJECT" => match a(0)? {
            Term::Triple(t) => t[match name {
                "SUBJECT" => 0,
                "PREDICATE" => 1,
                _ => 2,
            }]
            .clone(),
            _ => return None,
        },
        "ISTRIPLE" => Term::boolean(a(0)?.is_triple()),
        "ISIRI" | "ISURI" => Term::boolean(a(0)?.is_iri()),
        "ISBLANK" => Term::boolean(a(0)?.is_blank()),
        "ISLITERAL" => Term::boolean(a(0)?.is_literal()),
        "ISNUMERIC" => Term::boolean(num(a(0)?).is_some()),
        "YEAR" | "MONTH" | "DAY" | "HOURS" | "MINUTES" | "SECONDS" | "TIMEZONE" | "TZ" => {
            let l = a(0)?.as_literal()?;
            let dt = parse_datetime(&l.value)?;
            match name {
                "YEAR" => Term::integer(dt.year),
                "MONTH" => Term::integer(dt.month as i64),
                "DAY" => Term::integer(dt.day as i64),
                "HOURS" => Term::integer(dt.hour as i64),
                "MINUTES" => Term::integer(dt.minute as i64),
                "SECONDS" => Num::Dec(Dec::from_f64(dt.second)?).to_term(),
                "TZ" => Term::string(match dt.tz_minutes {
                    None => String::new(),
                    Some(0) => "Z".into(),
                    Some(m) => format!(
                        "{}{:02}:{:02}",
                        if m < 0 { '-' } else { '+' },
                        m.abs() / 60,
                        m.abs() % 60
                    ),
                }),
                _ => {
                    let m = dt.tz_minutes?;
                    let sign = if m < 0 { "-" } else { "" };
                    let (h, mm) = (m.abs() / 60, m.abs() % 60);
                    let body = match (h, mm) {
                        (0, 0) => "T0S".to_string(),
                        (h, 0) => format!("T{h}H"),
                        (0, mm) => format!("T{mm}M"),
                        (h, mm) => format!("T{h}H{mm}M"),
                    };
                    Term::typed(
                        format!("{sign}P{body}"),
                        "http://www.w3.org/2001/XMLSchema#dayTimeDuration",
                    )
                }
            }
        }
        "MD5" | "SHA1" | "SHA256" | "SHA384" | "SHA512" => {
            let s = str_arg(a(0)?)?.0;
            Term::string(super::hash::hex_digest(name, s.as_bytes())?)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numerics_and_strings() {
        let r = arith('/', Num::Int(7), Num::Int(2)).unwrap();
        assert_eq!(r.to_term(), Term::typed("3.5", XSD_DECIMAL));
        assert_eq!(
            equals(&Term::integer(1), &Term::typed("1.0", XSD_DECIMAL)),
            Some(true)
        );
        let s = call(
            "SUBSTR",
            &[Some(Term::string("foobar")), Some(Term::integer(4))],
            "",
        )
        .unwrap();
        assert_eq!(s, Term::string("bar"));
        let dt = parse_datetime("2024-02-29T12:30:00Z").unwrap();
        assert_eq!(dt.year, 2024);
        assert_eq!(
            call(
                "YEAR",
                &[Some(Term::typed("2024-02-29T12:30:00Z", XSD_DATETIME))],
                ""
            )
            .unwrap(),
            Term::integer(2024)
        );
        assert_eq!(
            cast(crate::rdf::XSD_INTEGER, &Term::string("42")).unwrap(),
            Term::integer(42)
        );
    }
}
