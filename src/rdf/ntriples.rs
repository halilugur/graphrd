//! Strict, line-based N-Triples and N-Quads (RDF 1.1, plus RDF-star quoted triples `<< s p o >>`
//! and RDF 1.2 triple terms `<<( s p o )>>`), following the W3C grammars exactly: absolute IRIs
//! only, no prefixes, no abbreviations.

use std::collections::HashMap;

use super::{Literal, Quad, RDF_LANGSTRING, Term, XSD_STRING};

/// Parse N-Triples (`quads` off) or N-Quads; blank node labels kept or renewed.
pub fn parse(text: &str, quads: bool, keep_labels: bool) -> Result<Vec<Quad>, String> {
    let mut out = Vec::new();
    parse_into(text, quads, keep_labels, None, &mut out)?;
    Ok(out)
}

/// Lenient parsing: a statement that does not parse is skipped (to the end of its line)
/// and its error recorded, instead of failing the document.
pub fn parse_lenient(text: &str, quads: bool, skipped: &mut Vec<String>) -> Vec<Quad> {
    let mut out = Vec::new();
    let _ = parse_into(text, quads, false, Some(skipped), &mut out);
    out
}

/// One part of a larger document (parallel bulk loading): blank node labels are kept as
/// written, so the caller can map them consistently across parts. With `skipped`, lines
/// that do not parse are skipped and their errors recorded.
pub fn parse_part(
    text: &str,
    quads: bool,
    skipped: Option<&mut Vec<String>>,
) -> Result<Vec<Quad>, String> {
    let mut out = Vec::new();
    parse_into(text, quads, true, skipped, &mut out)?;
    Ok(out)
}

fn parse_into(
    text: &str,
    quads: bool,
    keep_labels: bool,
    mut skipped: Option<&mut Vec<String>>,
    out: &mut Vec<Quad>,
) -> Result<(), String> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
        line: 1,
        quads,
        keep_labels,
        bnodes: HashMap::new(),
    };
    loop {
        match p.statement(out) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) => match skipped.as_deref_mut() {
                Some(list) => {
                    list.push(e);
                    // resume on the next line
                    while let Some(c) = p.peek() {
                        p.i += 1;
                        if c == b'\n' {
                            p.line += 1;
                            break;
                        }
                    }
                }
                None => return Err(e),
            },
        }
    }
}

impl Parser<'_> {
    /// One statement (or a blank line, comment or directive); false at the end of the input.
    fn statement(&mut self, out: &mut Vec<Quad>) -> Result<bool, String> {
        let p = self;
        p.ws();
        match p.peek() {
            None => return Ok(false),
            Some(b'\n' | b'\r') => {
                p.eol();
                return Ok(true);
            }
            Some(b'#') => {
                p.comment();
                return Ok(true);
            }
            Some(b'V') if p.s[p.i..].starts_with(b"VERSION") => {
                // RDF 1.2 version announcement: accepted, not interpreted
                p.i += 7;
                p.ws();
                if p.peek() != Some(b'"') {
                    return p.err("expected a version string");
                }
                p.literal()?;
                p.ws();
                return Ok(true);
            }
            _ => {}
        }
        let s = p.subject()?;
        p.ws();
        let pr = p.iri()?;
        p.ws();
        let o = p.object()?;
        p.ws();
        let g = if p.quads && matches!(p.peek(), Some(b'<' | b'_')) {
            let g = if p.peek() == Some(b'<') {
                p.iri()?
            } else {
                p.blank()?
            };
            p.ws();
            Some(g)
        } else {
            None
        };
        if p.peek() != Some(b'.') {
            return p.err("expected '.' at the end of the statement");
        }
        p.i += 1;
        p.ws();
        match p.peek() {
            None => {}
            Some(b'#') => p.comment(),
            Some(b'\n' | b'\r') => p.eol(),
            _ => return p.err("one statement per line"),
        }
        out.push(Quad { s, p: pr, o, g });
        Ok(true)
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    line: usize,
    quads: bool,
    keep_labels: bool,
    bnodes: HashMap<String, String>,
}

impl Parser<'_> {
    fn err<T>(&self, msg: &str) -> Result<T, String> {
        Err(format!("line {}: {msg}", self.line))
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.i += 1;
        }
    }

    fn eol(&mut self) {
        while matches!(self.peek(), Some(b'\n' | b'\r')) {
            if self.peek() == Some(b'\n') {
                self.line += 1;
            }
            self.i += 1;
        }
    }

    fn comment(&mut self) {
        while !matches!(self.peek(), None | Some(b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn subject(&mut self) -> Result<Term, String> {
        match self.peek() {
            // the store's own log may hold RDF-star triples as subjects
            Some(b'<') if self.s.get(self.i + 1) == Some(&b'<') && self.keep_labels => {
                self.quoted()
            }
            Some(b'<') if self.s.get(self.i + 1) == Some(&b'<') => {
                self.err("a triple term cannot be a subject")
            }
            Some(b'<') => self.iri(),
            Some(b'_') => self.blank(),
            _ => self.err("expected an IRI or blank node as subject"),
        }
    }

    fn object(&mut self) -> Result<Term, String> {
        match self.peek() {
            Some(b'<') if self.s.get(self.i + 1) == Some(&b'<') => self.quoted(),
            Some(b'<') => self.iri(),
            Some(b'_') => self.blank(),
            Some(b'"') => self.literal(),
            _ => self.err("expected an IRI, blank node or literal as object"),
        }
    }

    /// `<< s p o >>` (RDF-star) or `<<( s p o )>>` (RDF 1.2 triple term).
    fn quoted(&mut self) -> Result<Term, String> {
        self.i += 2;
        let paren = self.peek() == Some(b'(');
        if paren {
            self.i += 1;
        } else if !self.keep_labels {
            return self
                .err("reified triples << >> are not part of N-Triples; use a triple term <<( )>>");
        }
        self.ws();
        let s = self.subject()?;
        self.ws();
        let p = self.iri()?;
        self.ws();
        let o = self.object()?;
        self.ws();
        let close: &[u8] = if paren { b")>>" } else { b">>" };
        if !self.s[self.i..].starts_with(close) {
            return self.err("unterminated quoted triple");
        }
        self.i += close.len();
        Ok(Term::triple(s, p, o))
    }

    fn uchar(&mut self) -> Result<char, String> {
        // at the 'u' or 'U' after a backslash
        let n = if self.peek() == Some(b'u') { 4 } else { 8 };
        let hex = self
            .s
            .get(self.i + 1..self.i + 1 + n)
            .ok_or_else(|| format!("line {}: bad \\u escape", self.line))?;
        let hex =
            std::str::from_utf8(hex).map_err(|_| format!("line {}: bad \\u escape", self.line))?;
        let v = u32::from_str_radix(hex, 16)
            .map_err(|_| format!("line {}: bad \\u escape", self.line))?;
        self.i += 1 + n;
        char::from_u32(v).ok_or_else(|| format!("line {}: invalid code point", self.line))
    }

    fn iri(&mut self) -> Result<Term, String> {
        if self.peek() != Some(b'<') {
            return self.err("expected an IRI");
        }
        self.i += 1;
        let mut out = String::new();
        let start = self.i;
        loop {
            let Some(c) = self.peek() else {
                return self.err("unterminated IRI");
            };
            match c {
                b'>' => {
                    self.i += 1;
                    break;
                }
                b'\\' => {
                    out.push_str(std::str::from_utf8(&self.s[start..self.i]).unwrap_or(""));
                    self.i += 1;
                    if !matches!(self.peek(), Some(b'u' | b'U')) {
                        return self.err("only \\u and \\U escapes are allowed in IRIs");
                    }
                    let ch = self.uchar()?;
                    if (ch as u32) <= 0x20 || "<>\"{}|^`\\".contains(ch) {
                        return self.err("escaped character not allowed in an IRI");
                    }
                    out.push(ch);
                    return self.iri_rest(out);
                }
                0..=0x20 | b'<' | b'"' | b'{' | b'}' | b'|' | b'^' | b'`' => {
                    return self.err("character not allowed in an IRI");
                }
                _ => self.i += 1,
            }
        }
        let iri = std::str::from_utf8(&self.s[start..self.i - 1])
            .map_err(|_| format!("line {}: invalid UTF-8", self.line))?
            .to_string();
        self.absolute(iri)
    }

    /// The rest of an IRI after its first escape.
    fn iri_rest(&mut self, mut out: String) -> Result<Term, String> {
        loop {
            let Some(c) = self.peek() else {
                return self.err("unterminated IRI");
            };
            match c {
                b'>' => {
                    self.i += 1;
                    return self.absolute(out);
                }
                b'\\' => {
                    self.i += 1;
                    if !matches!(self.peek(), Some(b'u' | b'U')) {
                        return self.err("only \\u and \\U escapes are allowed in IRIs");
                    }
                    let ch = self.uchar()?;
                    if (ch as u32) <= 0x20 || "<>\"{}|^`\\".contains(ch) {
                        return self.err("escaped character not allowed in an IRI");
                    }
                    out.push(ch);
                }
                0..=0x20 | b'<' | b'"' | b'{' | b'}' | b'|' | b'^' | b'`' => {
                    return self.err("character not allowed in an IRI");
                }
                _ => {
                    let len = utf8_len(c);
                    out.push_str(
                        std::str::from_utf8(&self.s[self.i..self.i + len])
                            .map_err(|_| format!("line {}: invalid UTF-8", self.line))?,
                    );
                    self.i += len;
                }
            }
        }
    }

    fn absolute(&self, iri: String) -> Result<Term, String> {
        let line = self.line;
        if self.keep_labels {
            // the store's own log may hold relative IRIs inserted without a base
            return Ok(Term::Iri(iri));
        }
        // scheme ":" ... ; N-Triples has no base IRI to resolve against
        let scheme_ok = iri.find(':').is_some_and(|i| {
            i > 0
                && iri.as_bytes()[0].is_ascii_alphabetic()
                && iri[..i]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
        });
        if !scheme_ok {
            return Err(format!(
                "line {line}: relative IRI <{iri}> (N-Triples needs absolute IRIs)"
            ));
        }
        Ok(Term::Iri(iri))
    }

    fn blank(&mut self) -> Result<Term, String> {
        if !self.s[self.i..].starts_with(b"_:") {
            return self.err("expected a blank node");
        }
        self.i += 2;
        let start = self.i;
        // decode only the candidate label (up to the first byte that cannot be part of one),
        // not the rest of the input: that made parsing quadratic in blank-node-heavy files
        let mut limit = start;
        while self
            .s
            .get(limit)
            .is_some_and(|&b| b >= 0x80 || b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
        {
            limit += 1;
        }
        let text = std::str::from_utf8(&self.s[start..limit]).unwrap_or("");
        match text.chars().next() {
            Some(c) if pn_chars_u(c) || c.is_ascii_digit() => {}
            _ => return self.err("bad blank node label"),
        }
        let mut end = 0;
        for (i, c) in text.char_indices() {
            if !(pn_chars(c) || c == '.') {
                break;
            }
            end = i + c.len_utf8();
        }
        // a label cannot end with '.' (that dot ends the statement)
        let end = text[..end].trim_end_matches('.').len();
        let label = &text[..end];
        self.i = start + end;
        Ok(Term::Blank(if self.keep_labels {
            label.to_string()
        } else {
            self.bnodes
                .entry(label.to_string())
                .or_insert_with(super::fresh_blank_id)
                .clone()
        }))
    }

    fn literal(&mut self) -> Result<Term, String> {
        self.i += 1;
        let mut v = String::new();
        loop {
            let Some(c) = self.peek() else {
                return self.err("unterminated string");
            };
            match c {
                b'"' => {
                    self.i += 1;
                    break;
                }
                b'\n' | b'\r' => return self.err("line break in a string"),
                b'\\' => {
                    self.i += 1;
                    match self.peek() {
                        Some(b't') => v.push('\t'),
                        Some(b'b') => v.push('\u{8}'),
                        Some(b'n') => v.push('\n'),
                        Some(b'r') => v.push('\r'),
                        Some(b'f') => v.push('\u{c}'),
                        Some(b'"') => v.push('"'),
                        Some(b'\'') => v.push('\''),
                        Some(b'\\') => v.push('\\'),
                        Some(b'u' | b'U') => {
                            let ch = self.uchar()?;
                            v.push(ch);
                            continue;
                        }
                        _ => return self.err("bad escape in a string"),
                    }
                    self.i += 1;
                }
                _ => {
                    let len = utf8_len(c);
                    v.push_str(
                        std::str::from_utf8(&self.s[self.i..(self.i + len).min(self.s.len())])
                            .map_err(|_| format!("line {}: invalid UTF-8", self.line))?,
                    );
                    self.i += len;
                }
            }
        }
        // N-Triples 1.2 allows whitespace before the language tag or datatype
        let save = self.i;
        self.ws();
        if !matches!(self.peek(), Some(b'@' | b'^')) {
            self.i = save;
        }
        match self.peek() {
            Some(b'@') => {
                self.i += 1;
                let start = self.i;
                while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                    self.i += 1;
                }
                if self.i == start {
                    return self.err("bad language tag");
                }
                while self.peek() == Some(b'-')
                    && self
                        .s
                        .get(self.i + 1)
                        .is_some_and(|c| c.is_ascii_alphanumeric())
                {
                    self.i += 1;
                    while self.peek().is_some_and(|c| c.is_ascii_alphanumeric()) {
                        self.i += 1;
                    }
                }
                let tag = std::str::from_utf8(&self.s[start..self.i])
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !super::valid_lang(&tag) {
                    return self.err("bad language tag");
                }
                // RDF 1.2 base direction: `@lang--ltr` / `@lang--rtl` (lower case)
                if self.s[self.i..].starts_with(b"--") {
                    self.i += 2;
                    let d = self.i;
                    while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                        self.i += 1;
                    }
                    let dir = std::str::from_utf8(&self.s[d..self.i]).unwrap_or("");
                    if dir != "ltr" && dir != "rtl" {
                        return self.err("bad base direction (ltr or rtl)");
                    }
                    return Ok(Term::dir_lang(v, &tag, dir));
                }
                Ok(Term::Literal(Literal {
                    value: v,
                    datatype: RDF_LANGSTRING.into(),
                    lang: Some(tag),
                }))
            }
            Some(b'^') => {
                if self.s.get(self.i + 1) != Some(&b'^') {
                    return self.err("expected '^^'");
                }
                self.i += 2;
                self.ws();
                let Term::Iri(dt) = self.iri()? else {
                    unreachable!()
                };
                if dt == RDF_LANGSTRING || dt == super::RDF_DIRLANGSTRING {
                    return self.err(
                        "rdf:langString and rdf:dirLangString need a language tag, not a datatype",
                    );
                }
                Ok(Term::Literal(Literal {
                    value: v,
                    datatype: dt,
                    lang: None,
                }))
            }
            _ => Ok(Term::Literal(Literal {
                value: v,
                datatype: XSD_STRING.into(),
                lang: None,
            })),
        }
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xF0.. => 4,
        0xE0.. => 3,
        0xC0.. => 2,
        _ => 1,
    }
}

fn pn_chars_base(c: char) -> bool {
    c.is_ascii_alphabetic()
        || matches!(c as u32, 0xC0..=0xD6 | 0xD8..=0xF6 | 0xF8..=0x2FF | 0x370..=0x37D | 0x37F..=0x1FFF | 0x200C..=0x200D | 0x2070..=0x218F | 0x2C00..=0x2FEF | 0x3001..=0xD7FF | 0xF900..=0xFDCF | 0xFDF0..=0xFFFD | 0x10000..=0xEFFFF)
}

fn pn_chars_u(c: char) -> bool {
    pn_chars_base(c) || c == '_'
}

fn pn_chars(c: char) -> bool {
    pn_chars_u(c)
        || c == '-'
        || c.is_ascii_digit()
        || matches!(c as u32, 0xB7 | 0x300..=0x36F | 0x203F..=0x2040)
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::rdf::Term;

    #[test]
    fn many_blank_nodes_parse_in_linear_time() {
        // a blank node label once decoded the whole rest of the input: 60k of them took
        // minutes; linear parsing takes milliseconds
        let text: String = (0..60_000)
            .map(|i| format!("_:b{i} <http://ex/p> _:c{i} .\n"))
            .collect();
        let t = std::time::Instant::now();
        let quads = parse(&text, false, false).unwrap();
        assert_eq!(quads.len(), 60_000);
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            t.elapsed()
        );
        // labels with non-ASCII characters and inner dots still parse whole
        let q = parse("_:é.x <http://ex/p> _:a.b.\n", false, true).unwrap();
        assert_eq!(q[0].s, Term::Blank("é.x".into()));
        assert_eq!(q[0].o, Term::Blank("a.b".into()));
    }

    #[test]
    fn strict_grammar() {
        let ok = "<http://a> <http://b> \"x\\u00e9\\n\"@en-GB . # comment\n_:b1 <http://b> <<( <http://a> <http://b> _:b1 )>> .\n\n";
        let q = parse(ok, false, false).unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].o.value(), "xé\n");
        for bad in [
            "@prefix ex: <http://e/> .",
            "<a> <http://b> <http://c> .",
            "<http://a> <http://b> 1 .",
            "<http://a> <http://b> 'x' .",
            "<http://a> <http://b> <http://c> ; <http://d> <http://e> .",
            "<http://a> <http://b> \"x\"@1 .",
            "<http://a\\u0020b> <http://b> <http://c> .",
            "<http://a> <http://b> <http://c>",
            "<http://a> <http://b> <http://c> . <http://a> <http://b> <http://c> .",
        ] {
            assert!(parse(bad, false, false).is_err(), "accepted {bad}");
        }
        assert!(
            parse(
                "<http://a> <http://b> <http://c> <http://g> .",
                false,
                false
            )
            .is_err()
        );
        assert_eq!(
            parse("<http://a> <http://b> <http://c> <http://g> .", true, false).unwrap()[0]
                .g
                .as_ref()
                .unwrap()
                .value(),
            "http://g"
        );
    }
}
