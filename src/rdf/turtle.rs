//! Parser for Turtle, TriG, N-Triples and N-Quads (one recursive-descent
//! grammar; the line-based formats are subsets).

use std::collections::HashMap;

use super::*;

#[derive(Clone, Copy, PartialEq)]
/// Which syntax the Turtle family parser reads.
pub enum Mode {
/// Turtle.
    Turtle,
/// TriG: Turtle plus named graphs.
    TriG,
/// N-Quads (reused for N-Triples by the same reader).
    NQuads,
/// Notation3 (formulas become blank-node named graphs).
    N3,
}

/// Parse a document of the mode's syntax (strict; see `parse_lenient`).
pub fn parse(text: &str, base: &str, mode: Mode) -> Result<Vec<Quad>, String> {
    parse_opts(text, base, mode, false)
}

/// Notation3: formulas `{ ... }` become blank-node named graphs (an empty formula is the
/// literal `true`). N3 variables (`?x`) and literal subjects have no RDF equivalent: they are
/// errors unless `allow_variables`, which maps variables to blank nodes (for syntax checks).
pub fn parse_n3(text: &str, base: &str, allow_variables: bool) -> Result<Vec<Quad>, String> {
    let mut p = TurtleParser::new(text, base, Mode::N3, false);
    p.n3_vars = allow_variables;
    p.document()?;
    Ok(p.out)
}

/// Prefix declarations (`@prefix p: <ns> .` / `PREFIX p: <ns>`) of a
/// Turtle/TriG document, in order of appearance.
pub fn prefixes(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim_start();
        let rest = if l.get(..7).is_some_and(|x| x.eq_ignore_ascii_case("@prefix")) {
            &l[7..]
        } else if l.get(..6).is_some_and(|x| x.eq_ignore_ascii_case("prefix")) {
            &l[6..]
        } else {
            continue;
        };
        let Some((name, after)) = rest.trim_start().split_once(':') else { continue };
        let after = after.trim_start();
        if let (Some(after), true) = (after.strip_prefix('<'), !name.contains(char::is_whitespace)) {
            if let Some(end) = after.find('>') {
                out.push((name.to_string(), after[..end].to_string()));
            }
        }
    }
    out
}

/// N-Quads keeping blank node labels as written (for the store's own files).
pub fn parse_nquads_raw(text: &str) -> Result<Vec<Quad>, String> {
    super::ntriples::parse(text, true, true)
}

/// Lenient parsing: a statement that does not parse (in TriG, the graph block holding it)
/// is skipped, up to the next `.` that ends a line, and its error recorded.
pub fn parse_lenient(text: &str, base: &str, mode: Mode, skipped: &mut Vec<String>) -> Vec<Quad> {
    let mut p = TurtleParser::new(text, base, mode, false);
    p.skipped = Some(std::mem::take(skipped));
    let _ = p.document();
    *skipped = p.skipped.take().unwrap_or_default();
    p.out
}

fn parse_opts(text: &str, base: &str, mode: Mode, keep_labels: bool) -> Result<Vec<Quad>, String> {
    let mut p = TurtleParser::new(text, base, mode, keep_labels);
    p.document()?;
    Ok(p.out)
}

struct TurtleParser<'a> {
    s: &'a [u8],
    i: usize,
    base: String,
    prefixes: HashMap<String, String>,
    bnodes: HashMap<String, String>,
    keep_labels: bool,
    graph: Option<Term>,
    mode: Mode,
    out: Vec<Quad>,
    /// Nesting of blank-node lists, collections and quoted triples.
    depth: usize,
    /// N3: accept variables and literal subjects.
    n3_vars: bool,
    /// Lenient mode: errors of skipped statements.
    skipped: Option<Vec<String>>,
}

const LOG: &str = "http://www.w3.org/2000/10/swap/log#";
const OWL_SAME_AS: &str = "http://www.w3.org/2002/07/owl#sameAs";

/// Parsing recurses into nested terms; this keeps hostile input from exhausting the stack.
const MAX_NESTING: usize = 256;

impl<'a> TurtleParser<'a> {
    fn new(text: &'a str, base: &str, mode: Mode, keep_labels: bool) -> TurtleParser<'a> {
        TurtleParser {
            keep_labels,
            s: text.as_bytes(),
            i: 0,
            base: base.to_string(),
            prefixes: HashMap::new(),
            bnodes: HashMap::new(),
            graph: None,
            mode,
            out: Vec::new(),
            depth: 0,
            n3_vars: false,
            skipped: None,
        }
    }

    fn nest(&mut self, f: impl FnOnce(&mut Self) -> Result<Term, String>) -> Result<Term, String> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            return self.err("terms nested too deeply");
        }
        let r = f(self);
        self.depth -= 1;
        r
    }

    fn err<T>(&self, msg: &str) -> Result<T, String> {
        let line = self.s[..self.i.min(self.s.len())].iter().filter(|&&b| b == b'\n').count() + 1;
        let ctx: String = String::from_utf8_lossy(&self.s[self.i.min(self.s.len())..(self.i + 30).min(self.s.len())]).into_owned();
        Err(format!("{msg} at line {line} near {ctx:?}"))
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.i += 1;
            } else if c == b'#' {
                while let Some(c) = self.peek() {
                    if c == b'\n' {
                        break;
                    }
                    self.i += 1;
                }
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        self.ws();
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            self.err(&format!("expected '{}'", c as char))
        }
    }

    fn keyword(&mut self, kw: &str) -> bool {
        let end = self.i + kw.len();
        if end <= self.s.len()
            && self.s[self.i..end].eq_ignore_ascii_case(kw.as_bytes())
            && self.s.get(end).is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_' || *c == b':' || *c == b'-'))
        {
            self.i = end;
            true
        } else {
            false
        }
    }

    fn emit(&mut self, s: Term, p: Term, o: Term) {
        self.out.push(Quad { s, p, o, g: self.graph.clone() });
    }

    fn document(&mut self) -> Result<(), String> {
        loop {
            self.ws();
            let Some(c) = self.peek() else { return Ok(()) };
            let (mark, graph, start) = (self.out.len(), self.graph.clone(), self.i);
            if let Err(e) = self.statement(c) {
                let Some(list) = self.skipped.as_mut() else { return Err(e) };
                list.push(e);
                // drop what the statement had produced, and resume after the next line-ending '.'
                self.out.truncate(mark);
                self.graph = graph;
                self.depth = 0;
                // from the start of the line the error is on (an unterminated string has
                // already run past the statement's end), but never before the statement
                let at = self.i.min(self.s.len());
                let line_start = self.s[..at].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
                self.i = line_start.max(start);
                self.skip_statement();
                if self.i <= start {
                    self.i = start + 1;
                }
            }
        }
    }

    fn skip_statement(&mut self) {
        while self.i < self.s.len() {
            let c = self.s[self.i];
            self.i += 1;
            if c == b'.' {
                let mut j = self.i;
                while j < self.s.len() && (self.s[j] == b' ' || self.s[j] == b'\t' || self.s[j] == b'\r') {
                    j += 1;
                }
                if j >= self.s.len() || self.s[j] == b'\n' || self.s[j] == b'#' {
                    self.i = j;
                    return;
                }
            }
        }
    }

    fn statement(&mut self, c: u8) -> Result<(), String> {
        {
            if self.mode == Mode::N3 {
                if !self.n3_directive()? {
                    self.n3_triples()?;
                    self.expect(b'.')?;
                }
                return Ok(());
            }
            if c == b'@' {
                self.i += 1;
                // @prefix and @base are lower case; PREFIX and BASE are case-insensitive
                if self.s[self.i..].starts_with(b"prefix") && self.s.get(self.i + 6).is_some_and(|c| c.is_ascii_whitespace() || *c == b':') {
                    self.i += 6;
                    self.prefix_decl()?;
                    self.expect(b'.')?;
                } else if self.s[self.i..].starts_with(b"version") && self.s.get(self.i + 7).is_some_and(|c| c.is_ascii_whitespace() || *c == b'"' || *c == b'\'') {
                    // RDF 1.2 version announcement: accepted, not interpreted
                    self.i += 7;
                    self.version_string()?;
                    self.expect(b'.')?;
                } else if self.s[self.i..].starts_with(b"base") && self.s.get(self.i + 4).is_some_and(|c| c.is_ascii_whitespace() || *c == b'<') {
                    self.i += 4;
                    self.ws();
                    let iri = self.iriref()?;
                    self.base = iri;
                    self.expect(b'.')?;
                } else {
                    return self.err("unknown directive");
                }
            } else if self.keyword("VERSION") {
                self.version_string()?;
            } else if self.keyword("PREFIX") {
                self.prefix_decl()?;
            } else if self.keyword("BASE") {
                self.ws();
                self.base = self.iriref()?;
            } else if self.mode == Mode::TriG && self.keyword("GRAPH") {
                self.ws();
                let g = self.term_for_graph()?;
                self.graph_block(Some(g))?;
            } else if self.mode == Mode::TriG && c == b'{' {
                self.graph_block(None)?;
            } else {
                self.triples_or_graph()?;
            }
        }
        Ok(())
    }

    fn graph_block(&mut self, g: Option<Term>) -> Result<(), String> {
        self.expect(b'{')?;
        let saved = std::mem::replace(&mut self.graph, g);
        loop {
            self.ws();
            match self.peek() {
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                None => return self.err("unterminated graph block"),
                _ => {
                    self.triples()?;
                    self.ws();
                    if self.peek() == Some(b'.') {
                        self.i += 1;
                    } else if self.peek() != Some(b'}') {
                        return self.err("expected '.' or '}'");
                    }
                }
            }
        }
        self.graph = saved;
        Ok(())
    }

    fn term_for_graph(&mut self) -> Result<Term, String> {
        match self.peek() {
            Some(b'[') => {
                self.i += 1;
                self.ws();
                self.expect(b']')?;
                Ok(Term::fresh_blank())
            }
            _ => self.iri_or_blank(),
        }
    }

    fn prefix_decl(&mut self) -> Result<(), String> {
        self.ws();
        let start = self.i;
        while let Some(c) = self.peek() {
            if c == b':' {
                break;
            }
            if c.is_ascii_whitespace() {
                return self.err("bad prefix name");
            }
            self.i += 1;
        }
        let name = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
        // PN_PREFIX: starts with a letter, does not end with '.'
        if !name.is_empty() && (!name.starts_with(|c: char| c.is_alphabetic()) || name.ends_with('.')) {
            return self.err(&format!("bad prefix name '{name}'"));
        }
        self.expect(b':')?;
        self.ws();
        let iri = self.iriref()?;
        self.prefixes.insert(name, iri);
        Ok(())
    }

    /// In TriG, a statement may be `label { ... }`.
    fn triples_or_graph(&mut self) -> Result<(), String> {
        if self.mode == Mode::TriG {
            let save = self.i;
            if matches!(self.peek(), Some(b'<') | Some(b'_')) || self.looks_like_pname() || self.peek() == Some(b'[') {
                let g = self.term_for_graph();
                if let Ok(g) = g {
                    self.ws();
                    if self.peek() == Some(b'{') {
                        return self.graph_block(Some(g));
                    }
                }
                self.i = save;
            }
        }
        self.triples()?;
        self.expect(b'.')
    }

    fn looks_like_pname(&self) -> bool {
        let mut j = self.i;
        while j < self.s.len() {
            let c = self.s[j];
            if c == b':' {
                return true;
            }
            if c.is_ascii_whitespace() || b"<>{}()[];,.\"'".contains(&c) {
                return false;
            }
            j += 1;
        }
        false
    }

    fn triples(&mut self) -> Result<(), String> {
        self.ws();
        if self.peek() == Some(b'[') {
            let subj = self.blank_node_property_list()?;
            self.ws();
            if !matches!(self.peek(), Some(b'.') | Some(b'}') | None) {
                self.predicate_object_list(&subj)?;
            }
            return Ok(());
        }
        let reified = self.starts_quoted() && !self.starts_triple_term();
        let subj = self.subject()?;
        self.ws();
        if reified && matches!(self.peek(), Some(b'.') | Some(b'}') | None) {
            return Ok(()); // a reified triple on its own: `<< s p o >> .`
        }
        self.predicate_object_list(&subj)
    }

    fn starts_quoted(&self) -> bool {
        self.s.get(self.i..self.i + 2) == Some(b"<<")
    }

    /// RDF-star quoted triple: `<< subject predicate object >>`.
    fn quoted(&mut self) -> Result<Term, String> {
        self.nest(Self::quoted_inner)
    }

    /// RDF 1.2 reified triple `<< s p o ~ id? >>`: the reifier (named, or a fresh blank node),
    /// with `reifier rdf:reifies <<( s p o )>>` emitted.
    fn quoted_inner(&mut self) -> Result<Term, String> {
        self.i += 2;
        self.ws();
        let s = if self.starts_quoted() && !self.starts_triple_term() {
            self.quoted()?
        } else if self.s.get(self.i..self.i + 2) == Some(b"[]") || (self.peek() == Some(b'[') && self.empty_brackets()) {
            self.anon()?
        } else {
            match self.peek() {
                Some(b'<') if !self.starts_quoted() => Term::Iri(self.iriref()?),
                Some(b'_') => self.blank_label()?,
                Some(b'<' | b'[' | b'(' | b'"' | b'\'') => return self.err("bad subject in a reified triple"),
                _ => Term::Iri(self.pname()?),
            }
        };
        self.ws();
        let p = self.verb()?;
        self.ws();
        let o = self.rt_object()?;
        self.ws();
        let reifier = if self.peek() == Some(b'~') { self.reifier()? } else { Term::fresh_blank() };
        self.ws();
        if self.s.get(self.i..self.i + 2) != Some(b">>") {
            return self.err("expected '>>' to close the reified triple");
        }
        self.i += 2;
        self.emit(reifier.clone(), Term::iri(RDF_REIFIES), Term::triple(s, p, o));
        Ok(reifier)
    }

    /// Object of a reified triple or triple term: no collections or blank node property lists.
    fn rt_object(&mut self) -> Result<Term, String> {
        self.ws();
        match self.peek() {
            Some(b'(') => self.err("a collection cannot be part of a reified triple"),
            Some(b'[') if !self.empty_brackets() => self.err("a blank node property list cannot be part of a reified triple"),
            Some(b'[') => self.anon(),
            _ => self.object(),
        }
    }

    fn empty_brackets(&self) -> bool {
        let mut j = self.i + 1;
        while self.s.get(j).is_some_and(|c| c.is_ascii_whitespace()) {
            j += 1;
        }
        self.s.get(j) == Some(&b']')
    }

    fn anon(&mut self) -> Result<Term, String> {
        self.expect(b'[')?;
        self.expect(b']')?;
        Ok(Term::fresh_blank())
    }

    fn verb(&mut self) -> Result<Term, String> {
        if self.peek() == Some(b'a') && self.s.get(self.i + 1).is_none_or(|c| c.is_ascii_whitespace() || b"<\"[_(".contains(c)) {
            self.i += 1;
            return Ok(Term::iri(RDF_TYPE));
        }
        match self.peek() {
            Some(b'<') => Ok(Term::Iri(self.iriref()?)),
            Some(b'_') | Some(b'[') | Some(b'(') | Some(b'"') => self.err("a predicate must be an IRI"),
            _ => Ok(Term::Iri(self.pname()?)),
        }
    }

    /// `~ id?`: the reifier named by an IRI or blank node, or a fresh blank node.
    fn reifier(&mut self) -> Result<Term, String> {
        self.i += 1; // '~'
        self.ws();
        match self.peek() {
            Some(b'<') => Ok(Term::Iri(self.iriref()?)),
            Some(b'_') if self.s.get(self.i + 1) == Some(&b':') => self.blank_label(),
            Some(b'[') if self.empty_brackets() => self.anon(),
            Some(c) if c.is_ascii_alphabetic() || c == b':' || c >= 0x80 => Ok(Term::Iri(self.pname()?)),
            _ => Ok(Term::fresh_blank()),
        }
    }

    fn starts_triple_term(&self) -> bool {
        self.s.get(self.i..self.i + 3) == Some(b"<<(")
    }

    /// RDF 1.2 triple term `<<( s p o )>>` (object position only).
    fn triple_term(&mut self) -> Result<Term, String> {
        self.nest(|p| {
            p.i += 3;
            p.ws();
            let s = match p.peek() {
                Some(b'<') if !p.starts_quoted() => Term::Iri(p.iriref()?),
                Some(b'_') => p.blank_label()?,
                Some(b'[') if p.empty_brackets() => p.anon()?,
                Some(b'<' | b'[' | b'(' | b'"' | b'\'') => return p.err("bad subject in a triple term"),
                _ => Term::Iri(p.pname()?),
            };
            p.ws();
            let pr = p.verb()?;
            p.ws();
            let o = match p.peek() {
                Some(b'<') if p.starts_quoted() && !p.starts_triple_term() => return p.err("a reified triple cannot be inside a triple term"),
                _ => p.rt_object()?,
            };
            p.ws();
            if p.s.get(p.i..p.i + 3) != Some(b")>>") {
                return p.err("expected ')>>' to close the triple term");
            }
            p.i += 3;
            Ok(Term::triple(s, pr, o))
        })
    }

    // ------------------------------------------------------------ Notation3

    /// `@prefix`, `@base`, `PREFIX`, `BASE`, and the legacy `@forAll`, `@forSome` and
    /// `@keywords` (parsed, not interpreted). False when the input is not a directive.
    fn n3_directive(&mut self) -> Result<bool, String> {
        let at = |p: &Self, kw: &str| p.s[p.i..].starts_with(kw.as_bytes()) && p.s.get(p.i + kw.len()).is_none_or(|c| !c.is_ascii_alphanumeric());
        if at(self, "@prefix") {
            self.i += 7;
            self.prefix_decl()?;
            self.expect(b'.')?;
        } else if at(self, "@base") {
            self.i += 5;
            self.ws();
            self.base = self.iriref()?;
            self.expect(b'.')?;
        } else if at(self, "@forAll") || at(self, "@forSome") || at(self, "@keywords") {
            let keywords = at(self, "@keywords");
            self.i += if at(self, "@forAll") { 7 } else if keywords { 9 } else { 8 };
            loop {
                self.ws();
                if matches!(self.peek(), Some(b'.') | None) {
                    break;
                }
                if keywords {
                    while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') {
                        self.i += 1;
                    }
                } else {
                    self.n3_item()?;
                }
                self.ws();
                if self.peek() != Some(b',') {
                    break;
                }
                self.i += 1;
            }
            self.expect(b'.')?;
        } else if self.keyword("PREFIX") {
            self.prefix_decl()?;
        } else if self.keyword("BASE") {
            self.ws();
            self.base = self.iriref()?;
        } else {
            return Ok(false);
        }
        Ok(true)
    }

    fn n3_emit(&mut self, s: Term, p: Term, o: Term) -> Result<(), String> {
        if !self.n3_vars {
            if s.is_literal() {
                return self.err("literals are not allowed as subjects in regular RDF");
            }
            if !p.is_iri() {
                return self.err("predicates must be IRIs in regular RDF");
            }
        }
        self.emit(s, p, o);
        Ok(())
    }

    fn n3_triples(&mut self) -> Result<(), String> {
        let subj = self.n3_expr()?;
        self.ws();
        if matches!(self.peek(), Some(b'.') | Some(b'}') | None) {
            return Ok(());
        }
        self.n3_pol(&subj)
    }

    fn n3_pol(&mut self, subj: &Term) -> Result<(), String> {
        loop {
            self.ws();
            let (pred, inverse) = self.n3_verb()?;
            loop {
                let obj = self.n3_expr()?;
                if inverse {
                    self.n3_emit(obj, pred.clone(), subj.clone())?;
                } else {
                    self.n3_emit(subj.clone(), pred.clone(), obj)?;
                }
                self.ws();
                if self.peek() != Some(b',') {
                    break;
                }
                self.i += 1;
            }
            self.ws();
            if self.peek() != Some(b';') {
                return Ok(());
            }
            while self.peek() == Some(b';') {
                self.i += 1;
                self.ws();
            }
            if matches!(self.peek(), Some(b'.') | Some(b']') | Some(b'}') | None) {
                return Ok(());
            }
        }
    }

    /// The verb and whether it is inverted (`is p of`, `<- p`).
    fn n3_verb(&mut self) -> Result<(Term, bool), String> {
        self.ws();
        let two = self.s.get(self.i..self.i + 2);
        if two == Some(b"=>") {
            self.i += 2;
            return Ok((Term::iri(format!("{LOG}implies")), false));
        }
        if two == Some(b"<=") {
            self.i += 2;
            return Ok((Term::iri(format!("{LOG}isImpliedBy")), false));
        }
        if two == Some(b"<-") {
            self.i += 2;
            return Ok((self.n3_expr()?, true));
        }
        if self.peek() == Some(b'=') {
            self.i += 1;
            return Ok((Term::iri(OWL_SAME_AS), false));
        }
        if self.keyword("a") || self.keyword("@a") {
            return Ok((Term::iri(RDF_TYPE), false));
        }
        if self.keyword("has") || self.keyword("@has") {
            return Ok((self.n3_expr()?, false));
        }
        if self.keyword("is") || self.keyword("@is") {
            let p = self.n3_expr()?;
            self.ws();
            if !(self.keyword("of") || self.keyword("@of")) {
                return self.err("expected 'of' after 'is ...'");
            }
            return Ok((p, true));
        }
        Ok((self.n3_expr()?, false))
    }

    /// A path: `item ! p` (an object of item) and `item ^ p` (a subject of item), left to right.
    fn n3_expr(&mut self) -> Result<Term, String> {
        self.nest(|p| {
            let mut item = p.n3_item()?;
            loop {
                let save = p.i;
                p.ws();
                let forward = match p.peek() {
                    Some(b'!') => true,
                    Some(b'^') if p.s.get(p.i + 1) != Some(&b'^') => false,
                    _ => {
                        p.i = save;
                        return Ok(item);
                    }
                };
                p.i += 1;
                let pred = p.n3_item()?;
                let node = Term::fresh_blank();
                if forward {
                    p.n3_emit(item, pred, node.clone())?;
                } else {
                    p.n3_emit(node.clone(), pred, item)?;
                }
                item = node;
            }
        })
    }

    fn n3_item(&mut self) -> Result<Term, String> {
        self.ws();
        match self.peek() {
            Some(b'{') => self.n3_formula(),
            Some(b'?') => self.n3_variable(),
            Some(b'[') => {
                self.i += 1;
                self.ws();
                // `[ id <iri> ... ]`: properties of a named resource
                if self.s[self.i..].starts_with(b"id") && self.s.get(self.i + 2).is_some_and(|c| c.is_ascii_whitespace() || *c == b'<') {
                    self.i += 2;
                    self.ws();
                    let node = match self.peek() {
                        Some(b'<') => Term::Iri(self.iriref()?),
                        Some(b'_' | b'[' | b'(' | b'{' | b'?' | b'"' | b'\'') => return self.err("`[ id ... ]` needs an IRI"),
                        _ => Term::Iri(self.pname()?),
                    };
                    self.ws();
                    if self.peek() != Some(b']') {
                        self.n3_pol(&node)?;
                    }
                    self.expect(b']')?;
                    return Ok(node);
                }
                let node = Term::fresh_blank();
                if self.peek() != Some(b']') {
                    self.n3_pol(&node)?;
                }
                self.expect(b']')?;
                Ok(node)
            }
            Some(b'(') => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    self.ws();
                    match self.peek() {
                        Some(b')') => {
                            self.i += 1;
                            break;
                        }
                        None => return self.err("unterminated collection"),
                        _ => items.push(self.n3_expr()?),
                    }
                }
                if items.is_empty() {
                    return Ok(Term::iri(RDF_NIL));
                }
                let head = Term::fresh_blank();
                let mut cur = head.clone();
                let n = items.len();
                for (k, item) in items.into_iter().enumerate() {
                    self.emit(cur.clone(), Term::iri(RDF_FIRST), item);
                    let next = if k + 1 == n { Term::iri(RDF_NIL) } else { Term::fresh_blank() };
                    self.emit(cur.clone(), Term::iri(RDF_REST), next.clone());
                    cur = next;
                }
                Ok(head)
            }
            Some(b'<') => Ok(Term::Iri(self.iriref()?)),
            Some(b'_') if self.s.get(self.i + 1) == Some(&b':') => self.blank_label(),
            Some(b'"') | Some(b'\'') => self.literal(),
            Some(c) if c.is_ascii_digit() || c == b'+' || c == b'-' || c == b'.' => self.number(),
            None => self.err("unexpected end of input"),
            _ => {
                if self.keyword("true") || self.keyword("@true") {
                    return Ok(Term::boolean(true));
                }
                if self.keyword("false") || self.keyword("@false") {
                    return Ok(Term::boolean(false));
                }
                Ok(Term::Iri(self.pname()?))
            }
        }
    }

    fn n3_variable(&mut self) -> Result<Term, String> {
        self.i += 1; // '?'
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c >= 0x80) {
            self.i += 1;
        }
        if self.i == start {
            return self.err("empty variable name");
        }
        if !self.n3_vars {
            return self.err("N3 variables are not allowed in regular RDF");
        }
        let name = format!("?{}", String::from_utf8_lossy(&self.s[start..self.i]));
        let id = self.bnodes.entry(name).or_insert_with(fresh_blank_id).clone();
        Ok(Term::Blank(id))
    }

    /// `{ ... }`: the statements go into a fresh blank-node graph, which is the term's value.
    fn n3_formula(&mut self) -> Result<Term, String> {
        self.nest(|p| {
            p.i += 1;
            let g = Term::fresh_blank();
            let saved = std::mem::replace(&mut p.graph, Some(g.clone()));
            let before = p.out.len();
            loop {
                p.ws();
                match p.peek() {
                    Some(b'}') => {
                        p.i += 1;
                        break;
                    }
                    None => return p.err("unterminated formula"),
                    _ => {
                        if p.n3_directive()? {
                            continue;
                        }
                        p.n3_triples()?;
                        p.ws();
                        if p.peek() == Some(b'.') {
                            p.i += 1;
                        } else if p.peek() != Some(b'}') {
                            return p.err("expected '.' or '}' in a formula");
                        }
                    }
                }
            }
            p.graph = saved;
            // the empty formula is `true`
            if p.out.len() == before {
                return Ok(Term::boolean(true));
            }
            Ok(g)
        })
    }

    fn version_string(&mut self) -> Result<(), String> {
        self.ws();
        match self.peek() {
            Some(q @ (b'"' | b'\'')) => {
                if self.s.get(self.i..self.i + 3) == Some(&[q, q, q][..]) {
                    return self.err("the version must be a short string");
                }
                self.literal().map(|_| ())
            }
            _ => self.err("expected a version string"),
        }
    }

    fn subject(&mut self) -> Result<Term, String> {
        self.nest(Self::subject_inner)
    }

    fn subject_inner(&mut self) -> Result<Term, String> {
        self.ws();
        if self.starts_triple_term() {
            return self.err("a triple term cannot be a subject");
        }
        if self.starts_quoted() {
            return self.quoted();
        }
        match self.peek() {
            Some(b'(') => self.collection(),
            _ => self.iri_or_blank(),
        }
    }

    fn iri_or_blank(&mut self) -> Result<Term, String> {
        self.ws();
        match self.peek() {
            Some(b'<') => Ok(Term::Iri(self.iriref()?)),
            Some(b'_') if self.s.get(self.i + 1) == Some(&b':') => self.blank_label(),
            _ => Ok(Term::Iri(self.pname()?)),
        }
    }

    fn predicate_object_list(&mut self, subj: &Term) -> Result<(), String> {
        loop {
            self.ws();
            let pred = if self.peek() == Some(b'a')
                && self.s.get(self.i + 1).is_none_or(|c| c.is_ascii_whitespace() || *c == b'<' || *c == b'"' || *c == b'[' || *c == b'_')
            {
                self.i += 1;
                Term::iri(RDF_TYPE)
            } else {
                match self.peek() {
                    Some(b'<') => Term::Iri(self.iriref()?),
                    Some(b'_') | Some(b'[') | Some(b'(') | Some(b'"') => return self.err("a predicate must be an IRI"),
                    _ => Term::Iri(self.pname()?),
                }
            };
            self.object_list(subj, &pred)?;
            self.ws();
            if self.peek() != Some(b';') {
                return Ok(());
            }
            while self.peek() == Some(b';') {
                self.i += 1;
                self.ws();
            }
            if matches!(self.peek(), Some(b'.') | Some(b']') | Some(b'}') | None) || self.s.get(self.i..self.i + 2) == Some(b"|}") {
                return Ok(());
            }
        }
    }

    fn object_list(&mut self, subj: &Term, pred: &Term) -> Result<(), String> {
        loop {
            let obj = self.object()?;
            if self.mode == Mode::NQuads {
                self.ws();
                if self.peek() != Some(b'.') {
                    let g = self.iri_or_blank()?;
                    self.out.push(Quad { s: subj.clone(), p: pred.clone(), o: obj, g: Some(g) });
                    return Ok(());
                }
            }
            self.emit(subj.clone(), pred.clone(), obj.clone());
            self.ws();
            // RDF 1.2 annotations: `~ id` names a reifier of the asserted triple; `{| ... |}`
            // describes the preceding reifier, or a fresh blank node when there is none
            let triple = Term::triple(subj.clone(), pred.clone(), obj);
            let mut reifier: Option<Term> = None;
            loop {
                if self.peek() == Some(b'~') {
                    let r = self.reifier()?;
                    self.emit(r.clone(), Term::iri(RDF_REIFIES), triple.clone());
                    reifier = Some(r);
                } else if self.s.get(self.i..self.i + 2) == Some(b"{|") {
                    self.i += 2;
                    let r = match reifier.take() {
                        Some(r) => r,
                        None => {
                            let r = Term::fresh_blank();
                            self.emit(r.clone(), Term::iri(RDF_REIFIES), triple.clone());
                            r
                        }
                    };
                    self.predicate_object_list(&r)?;
                    self.ws();
                    if self.s.get(self.i..self.i + 2) != Some(b"|}") {
                        return self.err("expected '|}' to close the annotation");
                    }
                    self.i += 2;
                } else {
                    break;
                }
                self.ws();
            }
            if self.peek() == Some(b',') {
                self.i += 1;
            } else {
                return Ok(());
            }
        }
    }

    fn object(&mut self) -> Result<Term, String> {
        self.nest(Self::object_inner)
    }

    fn object_inner(&mut self) -> Result<Term, String> {
        self.ws();
        if self.starts_triple_term() {
            return self.triple_term();
        }
        if self.starts_quoted() {
            return self.quoted();
        }
        match self.peek() {
            Some(b'<') => Ok(Term::Iri(self.iriref()?)),
            Some(b'_') if self.s.get(self.i + 1) == Some(&b':') => self.blank_label(),
            Some(b'[') => self.blank_node_property_list(),
            Some(b'(') => self.collection(),
            Some(b'"') | Some(b'\'') => self.literal(),
            Some(c) if c.is_ascii_digit() || c == b'+' || c == b'-' || c == b'.' => self.number(),
            _ => {
                if self.keyword("true") {
                    return Ok(Term::boolean(true));
                }
                if self.keyword("false") {
                    return Ok(Term::boolean(false));
                }
                Ok(Term::Iri(self.pname()?))
            }
        }
    }

    fn blank_node_property_list(&mut self) -> Result<Term, String> {
        self.expect(b'[')?;
        let node = Term::fresh_blank();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(node);
        }
        self.predicate_object_list(&node)?;
        self.expect(b']')?;
        Ok(node)
    }

    fn collection(&mut self) -> Result<Term, String> {
        self.expect(b'(')?;
        let mut items = Vec::new();
        loop {
            self.ws();
            if self.peek() == Some(b')') {
                self.i += 1;
                break;
            }
            if self.peek().is_none() {
                return self.err("unterminated collection");
            }
            items.push(self.object()?);
        }
        if items.is_empty() {
            return Ok(Term::iri(RDF_NIL));
        }
        let head = Term::fresh_blank();
        let mut cur = head.clone();
        let n = items.len();
        for (k, item) in items.into_iter().enumerate() {
            self.emit(cur.clone(), Term::iri(RDF_FIRST), item);
            let next = if k + 1 == n { Term::iri(RDF_NIL) } else { Term::fresh_blank() };
            self.emit(cur.clone(), Term::iri(RDF_REST), next.clone());
            cur = next;
        }
        Ok(head)
    }

    fn blank_label(&mut self) -> Result<Term, String> {
        self.i += 2; // _:
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c >= 0x80 || (c == b'.' && self.is_name_continue_after_dot()) {
                self.i += 1;
            } else {
                break;
            }
        }
        if self.i == start {
            return self.err("empty blank node label");
        }
        let label = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
        if self.keep_labels {
            return Ok(Term::Blank(label));
        }
        let id = self.bnodes.entry(label).or_insert_with(fresh_blank_id).clone();
        Ok(Term::Blank(id))
    }

    /// A '.' inside a name is allowed only if more name characters follow.
    fn is_name_continue_after_dot(&self) -> bool {
        self.s.get(self.i + 1).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-' || *c == b':' || *c == b'%' || *c >= 0x80)
    }

    fn iriref(&mut self) -> Result<String, String> {
        if self.peek() != Some(b'<') {
            return self.err("expected IRI");
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let start = self.i;
            while let Some(c) = self.peek() {
                if c == b'>' || c == b'\\' || c == b'\n' {
                    break;
                }
                if c <= 0x20 || b"<\"{}|^`".contains(&c) {
                    return self.err("character not allowed in an IRI");
                }
                self.i += 1;
            }
            out.push_str(&String::from_utf8_lossy(&self.s[start..self.i]));
            match self.peek() {
                Some(b'>') => {
                    self.i += 1;
                    break;
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = self.unicode_escape()?;
                    if (c as u32) <= 0x20 || "<>\"{}|^`\\".contains(c) {
                        return self.err("escaped character not allowed in an IRI");
                    }
                    out.push(c);
                }
                _ => return self.err("unterminated IRI"),
            }
        }
        Ok(resolve_iri(&self.base, &out))
    }

    fn unicode_escape(&mut self) -> Result<char, String> {
        let n = match self.peek() {
            Some(b'u') => 4,
            Some(b'U') => 8,
            _ => return self.err("bad escape"),
        };
        self.i += 1;
        let hex = self.s.get(self.i..self.i + n).ok_or("short unicode escape")?;
        let v = u32::from_str_radix(std::str::from_utf8(hex).unwrap_or("x"), 16).map_err(|_| "bad unicode escape".to_string())?;
        self.i += n;
        char::from_u32(v).ok_or_else(|| "invalid code point".to_string())
    }

    /// PNAME_NS / PNAME_LN with the Turtle 1.1 rules: the prefix starts with a letter and
    /// does not end with '.'; the local part may not start with '-' or '.' nor end with '.';
    /// '%' must start a two-digit hex escape; '\\' escapes only the reserved characters.
    fn pname(&mut self) -> Result<String, String> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c == b':' {
                break;
            }
            let first = self.i == start;
            if c.is_ascii_alphabetic() || c >= 0x80 || (!first && (c.is_ascii_digit() || c == b'_' || c == b'-')) || (!first && c == b'.' && self.dots_then_name()) {
                self.i += 1;
            } else {
                return self.err("expected prefixed name");
            }
        }
        if self.peek() != Some(b':') {
            return self.err("expected prefixed name");
        }
        let prefix = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
        self.i += 1;
        let mut local = String::new();
        let local_start = self.i;
        while let Some(c) = self.peek() {
            let first = self.i == local_start;
            if c == b'\\' {
                match self.s.get(self.i + 1) {
                    Some(&n) if b"_~.-!$&'()*+,;=/?#@%".contains(&n) => {
                        local.push(n as char);
                        self.i += 2;
                        continue;
                    }
                    _ => return self.err("bad escape in a local name"),
                }
            }
            if c == b'%' {
                let hex = self.s.get(self.i + 1..self.i + 3);
                if !hex.is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) {
                    return self.err("'%' in a local name must be followed by two hex digits");
                }
                local.push_str(&String::from_utf8_lossy(&self.s[self.i..self.i + 3]));
                self.i += 3;
                continue;
            }
            let name_char = c.is_ascii_alphanumeric() || c == b'_' || c == b':' || c >= 0x80 || (!first && c == b'-');
            if name_char || (!first && c == b'.' && self.dots_then_name()) {
                let ch_start = self.i;
                self.i += 1;
                while self.peek().is_some_and(|b| (0x80..0xC0).contains(&b)) {
                    self.i += 1;
                }
                local.push_str(&String::from_utf8_lossy(&self.s[ch_start..self.i]));
            } else if first && c == b'-' {
                return self.err("a local name cannot start with '-'");
            } else {
                break;
            }
        }
        match self.prefixes.get(&prefix) {
            Some(ns) => Ok(format!("{ns}{local}")),
            // N3: the empty prefix defaults to the document, `<#>`
            None if self.mode == Mode::N3 && prefix.is_empty() => Ok(format!("{}#{local}", self.base.split('#').next().unwrap_or(""))),
            None => self.err(&format!("undefined prefix '{prefix}:'")),
        }
    }

    /// At a '.': whether a run of dots is followed by another name character (so the dots
    /// belong to the name, not end the statement).
    fn dots_then_name(&self) -> bool {
        let mut j = self.i;
        while self.s.get(j) == Some(&b'.') {
            j += 1;
        }
        self.s.get(j).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-' || *c == b':' || *c == b'%' || *c == b'\\' || *c >= 0x80)
    }

    fn literal(&mut self) -> Result<Term, String> {
        let q = self.peek().unwrap();
        let long = self.s.get(self.i..self.i + 3) == Some(&[q, q, q][..]);
        self.i += if long { 3 } else { 1 };
        let mut val = String::new();
        loop {
            let start = self.i;
            while let Some(c) = self.peek() {
                if c == q || c == b'\\' || (!long && (c == b'\n' || c == b'\r')) {
                    break;
                }
                self.i += 1;
            }
            val.push_str(&String::from_utf8_lossy(&self.s[start..self.i]));
            match self.peek() {
                None => return self.err("unterminated string"),
                Some(b'\\') => {
                    self.i += 1;
                    let c = match self.peek() {
                        Some(b't') => '\t',
                        Some(b'b') => '\u{8}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b'f') => '\u{c}',
                        Some(b'"') => '"',
                        Some(b'\'') => '\'',
                        Some(b'\\') => '\\',
                        Some(b'u') | Some(b'U') => {
                            let ch = self.unicode_escape()?;
                            val.push(ch);
                            continue;
                        }
                        _ => return self.err("bad string escape"),
                    };
                    self.i += 1;
                    val.push(c);
                }
                Some(c) if c == q => {
                    if long {
                        if self.s.get(self.i..self.i + 3) == Some(&[q, q, q][..]) {
                            self.i += 3;
                            break;
                        }
                        val.push(q as char);
                        self.i += 1;
                    } else {
                        self.i += 1;
                        break;
                    }
                }
                _ => return self.err("newline in string"),
            }
        }
        if self.peek() == Some(b'@') {
            // LANGTAG ::= '@' [a-zA-Z]+ ('-' [a-zA-Z0-9]+)*
            self.i += 1;
            let start = self.i;
            while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                self.i += 1;
            }
            if self.i == start {
                return self.err("bad language tag");
            }
            while self.peek() == Some(b'-') && self.s.get(self.i + 1).is_some_and(|c| c.is_ascii_alphanumeric()) {
                self.i += 1;
                while self.peek().is_some_and(|c| c.is_ascii_alphanumeric()) {
                    self.i += 1;
                }
            }
            let lang = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
            if !valid_lang(&lang) {
                return self.err(&format!("bad language tag '{lang}'"));
            }
            // RDF 1.2 base direction: `@lang--ltr` / `@lang--rtl` (lower case)
            if self.s.get(self.i..self.i + 2) == Some(b"--") {
                self.i += 2;
                let d = self.i;
                while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                    self.i += 1;
                }
                let dir = String::from_utf8_lossy(&self.s[d..self.i]).into_owned();
                if dir != "ltr" && dir != "rtl" {
                    return self.err(&format!("bad base direction '{dir}' (ltr or rtl)"));
                }
                return Ok(Term::dir_lang(val, &lang, &dir));
            }
            return Ok(Term::lang(val, &lang));
        }
        if self.s.get(self.i..self.i + 2) == Some(b"^^") {
            self.i += 2;
            let dt = match self.peek() {
                Some(b'<') => self.iriref()?,
                _ => self.pname()?,
            };
            if dt == RDF_LANGSTRING || dt == RDF_DIRLANGSTRING {
                return self.err("rdf:langString and rdf:dirLangString need a language tag, not a datatype");
            }
            return Ok(Term::typed(val, &dt));
        }
        Ok(Term::string(val))
    }

    fn number(&mut self) -> Result<Term, String> {
        let start = self.i;
        if matches!(self.peek(), Some(b'+') | Some(b'-')) {
            self.i += 1;
        }
        let digits = |p: &mut Self| {
            let s = p.i;
            while p.peek().is_some_and(|c| c.is_ascii_digit()) {
                p.i += 1;
            }
            p.i - s
        };
        let int_digits = digits(self);
        let mut dt = XSD_INTEGER;
        let dot_exponent = int_digits > 0 && self.peek() == Some(b'.') && matches!(self.s.get(self.i + 1), Some(b'e' | b'E'));
        if self.peek() == Some(b'.') && (self.s.get(self.i + 1).is_some_and(|c| c.is_ascii_digit()) || dot_exponent) {
            self.i += 1;
            digits(self);
            dt = XSD_DECIMAL;
        } else if int_digits == 0 && !matches!(self.peek(), Some(b'e') | Some(b'E')) {
            return self.err("bad number");
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            if digits(self) == 0 {
                return self.err("bad exponent");
            }
            dt = XSD_DOUBLE;
        }
        let lex = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
        Ok(Term::typed(lex, dt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn n3_formulas_paths_and_variables() {
        let doc = "@prefix : <http://ex/> . { ?x :p ?y } => { ?y :q ?x } . :a!:b :c [ id :d :e 1 ] . :s is :p of :o ; = :t . :k :v {} .";
        assert!(parse_n3(doc, "http://ex/doc", false).unwrap_err().contains("variables"));
        let q = parse_n3(doc, "http://ex/doc", true).unwrap();
        let implies = q.iter().find(|q| q.p == Term::iri(format!("{LOG}implies"))).unwrap();
        assert_eq!(q.iter().filter(|x| x.g.as_ref() == Some(&implies.s)).count(), 1, "antecedent formula is a graph");
        assert!(q.iter().any(|x| x.s == Term::iri("http://ex/a") && x.p == Term::iri("http://ex/b") && x.g.is_none()));
        assert!(q.iter().any(|x| x.s == Term::iri("http://ex/d") && x.p == Term::iri("http://ex/e")));
        assert!(q.iter().any(|x| x.s == Term::iri("http://ex/o") && x.p == Term::iri("http://ex/p") && x.o == Term::iri("http://ex/s")));
        assert!(q.iter().any(|x| x.p == Term::iri(OWL_SAME_AS) && x.o == Term::iri("http://ex/t")));
        assert!(q.iter().any(|x| x.p == Term::iri("http://ex/v") && x.o == Term::boolean(true)));
        let q = parse_n3(":x :y :z .", "http://ex/doc", false).unwrap();
        assert_eq!(q[0].s, Term::iri("http://ex/doc#x"), "the empty prefix defaults to <#>");
    }

    #[test]
    fn turtle_basics() {
        let ttl = r#"
            @prefix ex: <http://ex.org/> .
            ex:a a ex:C ; ex:p "x"@en, 1, 2.5, 1e3, true ;
                 ex:list (ex:b "c") ;
                 ex:q [ ex:r ex:s ] .
            ex:b ex:p """multi
line""" .
        "#;
        let q = parse(ttl, "http://base/", Mode::Turtle).unwrap();
        assert_eq!(q.len(), 14);
        assert!(q.iter().any(|q| q.o == Term::typed("2.5", XSD_DECIMAL)));
        assert!(q.iter().any(|q| q.o == Term::lang("x", "en")));
    }

    #[test]
    fn turtle_rdf12() {
        let reifies = Term::iri(RDF_REIFIES);
        // reified triple: a reifier node described by rdf:reifies <<( s p o )>>
        let q = parse("@prefix ex: <http://ex.org/> .\n<< ex:alice ex:knows ex:bob ~ ex:r1 >> ex:since 2020 .", "", Mode::Turtle).unwrap();
        assert_eq!(q.len(), 2);
        assert!(q.iter().any(|q| q.s == Term::iri("http://ex.org/r1") && q.p == reifies && q.o.is_triple()));
        assert!(q.iter().any(|q| q.s == Term::iri("http://ex.org/r1") && q.p == Term::iri("http://ex.org/since")));
        // annotation: asserted triple, one reifier per block, a named reifier for `~`
        let q = parse("@prefix ex: <http://ex.org/> .\nex:a ex:b ex:c ~ ex:r {| ex:source ex:doc |} {| ex:conf 0.9 |} .", "", Mode::Turtle).unwrap();
        assert_eq!(q.iter().filter(|q| q.p == reifies).count(), 2);
        assert!(q.iter().any(|q| q.s == Term::iri("http://ex.org/r") && q.p == Term::iri("http://ex.org/source")));
        assert!(q.iter().any(|q| q.s == Term::iri("http://ex.org/a") && q.o == Term::iri("http://ex.org/c")));
        // triple terms only as objects; base direction
        let q = parse("<http://s> <http://p> <<( <http://a> <http://b> \"x\"@en--rtl )>> .", "", Mode::Turtle).unwrap();
        assert!(matches!(&q[0].o, Term::Triple(t) if t[2] == Term::dir_lang("x", "en", "rtl")));
        assert!(parse("<<( <http://a> <http://b> <http://c> )>> <http://p> 1 .", "", Mode::Turtle).is_err());
        assert!(parse("<http://s> <http://p> \"x\"@en--LTR .", "", Mode::Turtle).is_err());
        assert!(parse("VERSION \"1.2\"\n@version \"1.2\" .\n<http://s> <http://p> 1 .", "", Mode::Turtle).is_ok());
        // the store's own log keeps RDF-star triples as subjects
        assert!(parse_nquads_raw("<< <http://a> <http://b> <http://c> >> <http://p> \"x\" .\n").unwrap()[0].s.is_triple());
    }

    #[test]
    fn prefix_scan() {
        let p = prefixes("@prefix ex: <http://ex.org/> .\nPREFIX foaf:<http://xmlns.com/foaf/0.1/>\n@prefix : <http://d/> .");
        assert_eq!(p, vec![("ex".into(), "http://ex.org/".into()), ("foaf".into(), "http://xmlns.com/foaf/0.1/".into()), ("".into(), "http://d/".into())]);
    }

    #[test]
    fn trig_and_nquads() {
        let trig = "@prefix ex: <http://ex.org/> . ex:g { ex:a ex:b ex:c . } GRAPH ex:h { ex:a ex:b ex:d }";
        let q = parse(trig, "", Mode::TriG).unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[1].g, Some(Term::iri("http://ex.org/h")));
        let nq = "<http://a> <http://b> \"c\" <http://g> .\n<http://a> <http://b> _:x .\n";
        let q = parse(nq, "", Mode::NQuads).unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].g, Some(Term::iri("http://g")));
        assert_eq!(q[1].g, None);
    }
}
