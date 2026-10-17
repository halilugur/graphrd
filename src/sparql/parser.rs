//! SPARQL 1.1 Query and Update parser (lexer + recursive descent) producing
//! algebra (spec section 18.2) directly.

use std::collections::{HashMap, HashSet};

use super::ast::*;
use crate::rdf::{self, Term, resolve_iri};

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Iri(String),
    PName(String, String),
    Blank(String),
    Var(String),
    Str(String),
    Lang(String),
    Int(String),
    Dec(String),
    Dbl(String),
    Word(String),
    Punct(&'static str),
    Eof,
}

const PUNCTS: [&str; 33] = [
    "<<(", ")>>", "<<", ">>", "~", "{|", "|}", "^^", "&&", "||", "!=", "<=", ">=", "{", "}", "(",
    ")", "[", "]", ".", ";", ",", "*", "/", "|", "^", "?", "+", "-", "!", "=", "<", ">",
];

/// Tokens with their (start, end) positions in the source, in chars.
fn lex(src: &str) -> Result<(Vec<Tok>, Vec<(usize, usize)>), String> {
    let c: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut tok_start = 0;
    let is_name = |ch: char| ch.is_alphanumeric() || ch == '_' || ch == '-' || ch as u32 >= 0x80;
    while i < c.len() {
        if out.len() > spans.len() {
            spans.push((tok_start, i));
        }
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        if ch == '#' {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
            continue;
        }
        tok_start = i;
        // IRIREF vs '<' operator
        if ch == '<' {
            let mut j = i + 1;
            let mut ok = false;
            while j < c.len() {
                let d = c[j];
                if d == '>' {
                    ok = true;
                    break;
                }
                if d.is_whitespace() || "<\"{}|^`".contains(d) {
                    break;
                }
                j += 1;
            }
            if ok {
                out.push(Tok::Iri(unescape_iri(
                    &c[i + 1..j].iter().collect::<String>(),
                )?));
                i = j + 1;
                continue;
            }
        }
        if (ch == '?' || ch == '$') && c.get(i + 1).is_some_and(|&n| is_name(n) && n != '-') {
            let s = i + 1;
            i += 1;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] as u32 >= 0x80) {
                i += 1;
            }
            out.push(Tok::Var(c[s..i].iter().collect()));
            continue;
        }
        if ch == '"' || ch == '\'' {
            let long = c.get(i + 1) == Some(&ch) && c.get(i + 2) == Some(&ch);
            i += if long { 3 } else { 1 };
            let mut s = String::new();
            loop {
                let Some(&d) = c.get(i) else {
                    return Err("unterminated string".into());
                };
                if d == '\\' {
                    let e = *c.get(i + 1).ok_or("bad escape")?;
                    i += 2;
                    match e {
                        't' => s.push('\t'),
                        'n' => s.push('\n'),
                        'r' => s.push('\r'),
                        'b' => s.push('\u{8}'),
                        'f' => s.push('\u{c}'),
                        '"' => s.push('"'),
                        '\'' => s.push('\''),
                        '\\' => s.push('\\'),
                        'u' | 'U' => {
                            let n = if e == 'u' { 4 } else { 8 };
                            let hex: String =
                                c.get(i..i + n).ok_or("short escape")?.iter().collect();
                            let v = u32::from_str_radix(&hex, 16).map_err(|_| "bad \\u escape")?;
                            s.push(char::from_u32(v).ok_or("bad code point")?);
                            i += n;
                        }
                        _ => return Err(format!("bad escape \\{e}")),
                    }
                    continue;
                }
                if d == ch {
                    if !long {
                        i += 1;
                        break;
                    }
                    if c.get(i + 1) == Some(&ch) && c.get(i + 2) == Some(&ch) {
                        i += 3;
                        // allow up to 2 extra quotes at the end of a long string
                        while c.get(i) == Some(&ch) {
                            s.push(ch);
                            i += 1;
                        }
                        break;
                    }
                }
                if !long && (d == '\n' || d == '\r') {
                    return Err("newline in string".into());
                }
                s.push(d);
                i += 1;
            }
            out.push(Tok::Str(s));
            continue;
        }
        if ch == '@' {
            let s = i + 1;
            i += 1;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '-') {
                i += 1;
            }
            out.push(Tok::Lang(
                c[s..i].iter().collect::<String>().to_ascii_lowercase(),
            ));
            continue;
        }
        if ch.is_ascii_digit() || (ch == '.' && c.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let s = i;
            while i < c.len() && c[i].is_ascii_digit() {
                i += 1;
            }
            let mut kind = 0; // 0 int 1 dec 2 dbl
            if c.get(i) == Some(&'.') && c.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
                i += 1;
                while i < c.len() && c[i].is_ascii_digit() {
                    i += 1;
                }
                kind = 1;
            } else if c.get(i) == Some(&'.') && matches!(c.get(i + 1), Some('e' | 'E')) {
                i += 1;
            }
            if matches!(c.get(i), Some('e' | 'E')) {
                let save = i;
                i += 1;
                if matches!(c.get(i), Some('+' | '-')) {
                    i += 1;
                }
                if c.get(i).is_some_and(|d| d.is_ascii_digit()) {
                    while i < c.len() && c[i].is_ascii_digit() {
                        i += 1;
                    }
                    kind = 2;
                } else {
                    i = save;
                }
            }
            let lex: String = c[s..i].iter().collect();
            out.push(match kind {
                0 => Tok::Int(lex),
                1 => Tok::Dec(lex),
                _ => Tok::Dbl(lex),
            });
            continue;
        }
        if ch == '_' && c.get(i + 1) == Some(&':') {
            let s = i + 2;
            i += 2;
            while i < c.len()
                && (is_name(c[i]) || (c[i] == '.' && c.get(i + 1).is_some_and(|&n| is_name(n))))
            {
                i += 1;
            }
            out.push(Tok::Blank(c[s..i].iter().collect()));
            continue;
        }
        if ch.is_alphabetic() || ch == '_' || ch as u32 >= 0x80 || ch == ':' {
            let s = i;
            while i < c.len()
                && (is_name(c[i]) || (c[i] == '.' && c.get(i + 1).is_some_and(|&n| is_name(n))))
            {
                i += 1;
            }
            let word: String = c[s..i].iter().collect();
            if c.get(i) == Some(&':') {
                i += 1;
                let mut local = String::new();
                while i < c.len() {
                    let d = c[i];
                    if d == '\\' && i + 1 < c.len() {
                        // PN_LOCAL_ESC: only these characters may be escaped
                        if !"_~.-!$&'()*+,;=/?#@%".contains(c[i + 1]) {
                            return Err(format!("bad escape '\\{}' in a prefixed name", c[i + 1]));
                        }
                        local.push(c[i + 1]);
                        i += 2;
                    } else if is_name(d)
                        || d == ':'
                        || d == '%'
                        || (d == '.'
                            && c.get(i + 1)
                                .is_some_and(|&n| is_name(n) || n == ':' || n == '%'))
                    {
                        local.push(d);
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push(Tok::PName(word, local));
            } else {
                out.push(Tok::Word(word));
            }
            continue;
        }
        let rest: String = c[i..(i + 3).min(c.len())].iter().collect();
        match PUNCTS.iter().find(|p| rest.starts_with(**p)) {
            Some(p) => {
                out.push(Tok::Punct(p));
                i += p.chars().count();
            }
            None => return Err(format!("unexpected character '{ch}'")),
        }
    }
    if out.len() > spans.len() {
        spans.push((tok_start, i));
    }
    out.push(Tok::Eof);
    spans.push((c.len(), c.len()));
    Ok((out, spans))
}

fn unescape_iri(s: &str) -> Result<String, String> {
    if !s.contains('\\') {
        return Ok(s.to_string());
    }
    let c: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '\\' && matches!(c.get(i + 1), Some('u' | 'U')) {
            let n = if c[i + 1] == 'u' { 4 } else { 8 };
            let hex: String = c
                .get(i + 2..i + 2 + n)
                .ok_or("bad IRI escape")?
                .iter()
                .collect();
            out.push(
                char::from_u32(u32::from_str_radix(&hex, 16).map_err(|_| "bad IRI escape")?)
                    .ok_or("bad code point")?,
            );
            i += 2 + n;
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    Ok(out)
}

struct Parser<'a> {
    t: Vec<Tok>,
    /// Source positions of the tokens, and the source itself (for SERVICE).
    spans: Vec<(usize, usize)>,
    src: Vec<char>,
    i: usize,
    base: String,
    prefixes: HashMap<String, String>,
    fallback_prefixes: &'a HashMap<String, String>,
    vars: VarTable,
    bnode_vars: HashMap<String, Var>,
    template: bool,
    aggs: Vec<Aggregate>,
    agg_vars: Vec<Var>,
    allow_aggs: bool,
    /// Current nesting of groups, expressions, paths and nodes (bounded by MAX_NESTING).
    depth: usize,
    /// Left-deep operators built so far (OPTIONAL, MINUS, BIND, arithmetic...; MAX_CHAIN).
    chain: usize,
    /// The basic graph pattern being parsed, and where each blank node label was used: a
    /// label may not appear in two different basic graph patterns.
    bgp: usize,
    bnode_bgp: HashMap<String, usize>,
    /// Inside INSERT DATA / DELETE DATA: no variables.
    data_block: bool,
    /// Inside DELETE templates and DELETE DATA / WHERE: no blank nodes.
    no_bnodes: bool,
    /// Projected variables neither grouped nor aggregated, pending the trailing VALUES
    /// (which may bind them, since it joins after grouping).
    ungrouped: Vec<Var>,
    /// Blank node labels of earlier INSERT DATA operations in the same request.
    data_labels: HashSet<String>,
    op_labels: HashSet<String>,
}

/// Bounds on query shape. Parsing and evaluation recurse over the query tree, so these keep
/// hostile queries from exhausting the stack; real queries stay far below them.
const MAX_NESTING: usize = 256;
const MAX_CHAIN: usize = 5_000;

/// Variables an expression reads (aggregates appear as their placeholder variables).
fn expr_vars(e: &Expr, out: &mut Vec<Var>) {
    match e {
        Expr::Var(v) | Expr::Bound(v) => out.push(*v),
        Expr::Exists(..) => {}
        _ => e.for_each_child(&mut |c| expr_vars(c, out)),
    }
}

/// Combine `items` (non-empty) into a balanced tree with an associative operator, so that
/// long UNION / && / || chains stay shallow.
fn balanced<T>(mut items: Vec<T>, f: &dyn Fn(T, T) -> T) -> T {
    if items.len() == 1 {
        return items.pop().unwrap();
    }
    let right = items.split_off(items.len() / 2);
    f(balanced(items, f), balanced(right, f))
}

/// Parse a query (known prefixes fill in undeclared ones).
pub fn parse_query(
    src: &str,
    base: &str,
    known: &HashMap<String, String>,
) -> Result<ParsedQuery, String> {
    let mut p = Parser::new(src, base, known)?;
    p.prologue()?;
    let mut query = p.query_body()?;
    if p.word("VALUES") {
        let values = p.data_block()?;
        if let Form::Select {
            projection: Some(proj),
            ..
        } = &query.form
        {
            let bound = crate::sparql::eval::visible_vars(&values, &p.vars);
            if let Some((_, v)) = proj.iter().find(|(e, v)| e.is_some() && bound.contains(v)) {
                return p.err(&format!(
                    "?{} is bound by VALUES and by a SELECT expression",
                    p.vars.names[*v]
                ));
            }
        }
        p.check_grouping(Some(&values))?;
        query.values = Some(values);
    } else {
        p.check_grouping(None)?;
    }
    if p.peek() != &Tok::Eof {
        return p.err("unexpected trailing input");
    }
    Ok(ParsedQuery {
        query,
        vars: p.vars,
        base: p.base,
    })
}

/// Parse an update request.
pub fn parse_update(
    src: &str,
    base: &str,
    known: &HashMap<String, String>,
) -> Result<ParsedUpdate, String> {
    let mut p = Parser::new(src, base, known)?;
    let mut ops = Vec::new();
    loop {
        p.prologue()?;
        if p.peek() == &Tok::Eof {
            break;
        }
        ops.push(p.update_op()?);
        let labels = std::mem::take(&mut p.op_labels);
        p.data_labels.extend(labels);
        if !p.punct(";") {
            break;
        }
    }
    p.prologue()?;
    if p.peek() != &Tok::Eof {
        return p.err("unexpected trailing input");
    }
    Ok(ParsedUpdate {
        ops,
        vars: p.vars,
        base: p.base,
    })
}

impl<'a> Parser<'a> {
    fn new(src: &str, base: &str, known: &'a HashMap<String, String>) -> Result<Self, String> {
        let (t, spans) = lex(src).map_err(|e| format!("SPARQL syntax error: {e}"))?;
        Ok(Parser {
            t,
            spans,
            src: src.chars().collect(),
            i: 0,
            base: base.to_string(),
            prefixes: HashMap::new(),
            fallback_prefixes: known,
            vars: VarTable::default(),
            bnode_vars: HashMap::new(),
            template: false,
            aggs: Vec::new(),
            agg_vars: Vec::new(),
            allow_aggs: false,
            depth: 0,
            chain: 0,
            bgp: 0,
            bnode_bgp: HashMap::new(),
            data_block: false,
            ungrouped: Vec::new(),
            no_bnodes: false,
            data_labels: HashSet::new(),
            op_labels: HashSet::new(),
        })
    }

    /// BASE and the PREFIX declarations `body` uses, for a query sent to a SERVICE endpoint.
    fn prologue_text(&self, body: &str) -> String {
        let mut out = String::new();
        if !self.base.is_empty() {
            out.push_str(&format!("BASE <{}>\n", self.base));
        }
        for (p, iri) in self
            .fallback_prefixes
            .iter()
            .filter(|(p, _)| !self.prefixes.contains_key(*p))
            .chain(&self.prefixes)
        {
            if body.contains(&format!("{p}:")) {
                out.push_str(&format!("PREFIX {p}: <{iri}>\n"));
            }
        }
        out
    }

    /// Fail if a projected variable is neither grouped, aggregated, nor bound by `values`.
    fn check_grouping(&mut self, values: Option<&Pattern>) -> Result<(), String> {
        let bound = values
            .map(|v| crate::sparql::eval::visible_vars(v, &self.vars))
            .unwrap_or_default();
        let pending = std::mem::take(&mut self.ungrouped);
        match pending.into_iter().find(|v| !bound.contains(v)) {
            Some(v) => self.err(&format!(
                "?{} is neither grouped nor aggregated",
                self.vars.names[v]
            )),
            None => Ok(()),
        }
    }

    fn nest<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, String>) -> Result<T, String> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            return self.err("query is nested too deeply");
        }
        let r = f(self);
        self.depth -= 1;
        r
    }

    fn link(&mut self) -> Result<(), String> {
        self.chain += 1;
        if self.chain > MAX_CHAIN {
            return self.err("query is too complex");
        }
        Ok(())
    }

    fn err<T>(&self, msg: &str) -> Result<T, String> {
        Err(format!(
            "SPARQL syntax error: {msg} (at token {:?})",
            self.t.get(self.i).unwrap_or(&Tok::Eof)
        ))
    }

    fn peek(&self) -> &Tok {
        &self.t[self.i]
    }
    fn peek_at(&self, k: usize) -> &Tok {
        self.t.get(self.i + k).unwrap_or(&Tok::Eof)
    }
    fn next(&mut self) -> Tok {
        let t = self.t[self.i].clone();
        if t != Tok::Eof {
            self.i += 1;
        }
        t
    }
    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Tok::Word(x) if x.eq_ignore_ascii_case(w))
    }
    fn word(&mut self, w: &str) -> bool {
        if self.is_word(w) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect_word(&mut self, w: &str) -> Result<(), String> {
        if self.word(w) {
            Ok(())
        } else {
            self.err(&format!("expected {w}"))
        }
    }
    fn is_punct(&self, p: &str) -> bool {
        matches!(self.peek(), Tok::Punct(x) if *x == p)
    }
    fn punct(&mut self, p: &str) -> bool {
        if self.is_punct(p) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, p: &str) -> Result<(), String> {
        if self.punct(p) {
            Ok(())
        } else {
            self.err(&format!("expected '{p}'"))
        }
    }

    fn prologue(&mut self) -> Result<(), String> {
        loop {
            if self.word("VERSION") {
                // SPARQL 1.2 version declaration: a short string, accepted and not interpreted
                // a short string: not """...""" or '''...'''
                let start = self.spans[self.i].0;
                let long = self.src.get(start..start + 3).is_some_and(|c| {
                    c.iter().all(|&ch| ch == '"') || c.iter().all(|&ch| ch == '\'')
                });
                match self.next() {
                    Tok::Str(_) if !long => {}
                    _ => return self.err("expected a short version string after VERSION"),
                }
            } else if self.word("BASE") {
                match self.next() {
                    Tok::Iri(i) => self.base = resolve_iri(&self.base, &i),
                    _ => return self.err("expected IRI after BASE"),
                }
            } else if self.word("PREFIX") {
                let (prefix, local) = match self.next() {
                    Tok::PName(p, l) => (p, l),
                    _ => return self.err("expected prefix name"),
                };
                if !local.is_empty() {
                    return self.err("bad prefix declaration");
                }
                match self.next() {
                    Tok::Iri(i) => {
                        let iri = resolve_iri(&self.base, &i);
                        self.prefixes.insert(prefix, iri);
                    }
                    _ => return self.err("expected IRI in PREFIX"),
                }
            } else {
                return Ok(());
            }
        }
    }

    fn iri_tok(&self, t: &Tok) -> Result<Option<String>, String> {
        Ok(match t {
            Tok::Iri(i) => Some(resolve_iri(&self.base, i)),
            Tok::PName(p, l) => {
                let ns = self
                    .prefixes
                    .get(p)
                    .or_else(|| self.fallback_prefixes.get(p))
                    .ok_or_else(|| format!("SPARQL: undefined prefix '{p}:'"))?;
                Some(format!("{ns}{l}"))
            }
            _ => None,
        })
    }

    fn iri(&mut self) -> Result<String, String> {
        let t = self.next();
        match self.iri_tok(&t)? {
            Some(i) => Ok(i),
            None => {
                self.i -= 1;
                self.err("expected IRI")
            }
        }
    }

    fn is_iri(&self) -> bool {
        matches!(self.peek(), Tok::Iri(_) | Tok::PName(..))
    }

    fn var(&mut self, name: &str) -> Var {
        self.vars.slot(name)
    }

    // ------------------------------------------------------------ queries

    fn query_body(&mut self) -> Result<Query, String> {
        if self.word("SELECT") {
            let form = self.select_clause()?;
            let dataset = self.dataset_clause()?;
            self.word("WHERE");
            let pattern = self.group_graph_pattern()?;
            self.finish_query(form, dataset, pattern)
        } else if self.word("CONSTRUCT") {
            if self.is_punct("{") {
                let template = self.template_block()?;
                let dataset = self.dataset_clause()?;
                self.word("WHERE");
                let pattern = self.group_graph_pattern()?;
                self.finish_query(Form::Construct(template), dataset, pattern)
            } else {
                let dataset = self.dataset_clause()?;
                self.expect_word("WHERE")?;
                self.expect("{")?;
                let mut triples = Vec::new();
                if !self.is_punct("}") {
                    self.triples_same_subject_path(&mut triples)?;
                    while self.punct(".") {
                        if self.is_punct("}") {
                            break;
                        }
                        self.triples_same_subject_path(&mut triples)?;
                    }
                }
                self.expect("}")?;
                let pattern = Pattern::Bgp(triples.clone());
                // Variables-as-template: blank-node vars cannot appear here.
                self.finish_query(Form::Construct(triples), dataset, pattern)
            }
        } else if self.word("ASK") {
            let dataset = self.dataset_clause()?;
            self.word("WHERE");
            let pattern = self.group_graph_pattern()?;
            self.finish_query(Form::Ask, dataset, pattern)
        } else if self.word("DESCRIBE") {
            let mut targets = Vec::new();
            if !self.punct("*") {
                loop {
                    match self.peek().clone() {
                        Tok::Var(v) => {
                            self.i += 1;
                            targets.push(VarOrTerm::Var(self.var(&v)));
                        }
                        _ if self.is_iri() => targets.push(VarOrTerm::Term(Term::Iri(self.iri()?))),
                        _ => break,
                    }
                }
            }
            let dataset = self.dataset_clause()?;
            let pattern = if self.word("WHERE") || self.is_punct("{") {
                self.group_graph_pattern()?
            } else {
                Pattern::empty()
            };
            if targets.is_empty() {
                // DESCRIBE * : every variable in the pattern
                targets = (0..self.vars.len())
                    .filter(|&v| !self.vars.is_hidden(v))
                    .map(VarOrTerm::Var)
                    .collect();
            }
            self.finish_query(Form::Describe(targets), dataset, pattern)
        } else {
            self.err("expected SELECT, CONSTRUCT, ASK or DESCRIBE")
        }
    }

    fn select_clause(&mut self) -> Result<Form, String> {
        let distinct = self.word("DISTINCT");
        let reduced = !distinct && self.word("REDUCED");
        if self.punct("*") {
            return Ok(Form::Select {
                distinct,
                reduced,
                projection: None,
            });
        }
        let mut proj = Vec::new();
        self.allow_aggs = true;
        loop {
            match self.peek().clone() {
                Tok::Var(v) => {
                    self.i += 1;
                    let slot = self.var(&v);
                    proj.push((None, slot));
                }
                Tok::Punct("(") => {
                    self.i += 1;
                    let e = self.expr()?;
                    self.expect_word("AS")?;
                    let v = match self.next() {
                        Tok::Var(v) => self.var(&v),
                        _ => return self.err("expected variable after AS"),
                    };
                    self.expect(")")?;
                    proj.push((Some(e), v));
                }
                _ => break,
            }
        }
        self.allow_aggs = false;
        if proj.is_empty() {
            return self.err("empty SELECT clause");
        }
        Ok(Form::Select {
            distinct,
            reduced,
            projection: Some(proj),
        })
    }

    fn dataset_clause(&mut self) -> Result<Option<Dataset>, String> {
        let mut ds = Dataset::default();
        let mut any = false;
        while self.word("FROM") {
            any = true;
            if self.word("NAMED") {
                ds.named.push(self.iri()?);
            } else {
                ds.default.push(self.iri()?);
            }
        }
        Ok(any.then_some(ds))
    }

    fn finish_query(
        &mut self,
        form: Form,
        dataset: Option<Dataset>,
        pattern: Pattern,
    ) -> Result<Query, String> {
        let mut group_by = Vec::new();
        let mut having = Vec::new();
        let mut order_by = Vec::new();
        let (mut limit, mut offset) = (None, 0);
        if self.word("GROUP") {
            self.expect_word("BY")?;
            loop {
                if self.punct("(") {
                    let e = self.expr()?;
                    let alias = if self.word("AS") {
                        match self.next() {
                            Tok::Var(v) => Some(self.var(&v)),
                            _ => return self.err("expected variable"),
                        }
                    } else {
                        None
                    };
                    self.expect(")")?;
                    group_by.push((e, alias));
                } else if let Tok::Var(v) = self.peek().clone() {
                    self.i += 1;
                    let slot = self.var(&v);
                    group_by.push((Expr::Var(slot), None));
                } else if self.is_call_start() || self.is_iri() {
                    group_by.push((self.primary()?, None));
                } else {
                    break;
                }
            }
        }
        self.allow_aggs = true;
        if self.word("HAVING") {
            while self.is_punct("(") || self.is_call_start() || self.is_iri() {
                having.push(self.primary()?);
            }
        }
        if self.word("ORDER") {
            self.expect_word("BY")?;
            loop {
                if self.word("ASC") {
                    order_by.push((self.bracketted()?, false));
                } else if self.word("DESC") {
                    order_by.push((self.bracketted()?, true));
                } else if let Tok::Var(v) = self.peek().clone() {
                    self.i += 1;
                    let slot = self.var(&v);
                    order_by.push((Expr::Var(slot), false));
                } else if self.is_punct("(") || self.is_call_start() || self.is_iri() {
                    order_by.push((self.primary()?, false));
                } else {
                    break;
                }
            }
        }
        self.allow_aggs = false;
        loop {
            if self.word("LIMIT") {
                match self.next() {
                    Tok::Int(n) => limit = Some(n.parse().map_err(|_| "bad LIMIT")?),
                    _ => return self.err("expected integer"),
                }
            } else if self.word("OFFSET") {
                match self.next() {
                    Tok::Int(n) => offset = n.parse().map_err(|_| "bad OFFSET")?,
                    _ => return self.err("expected integer"),
                }
            } else {
                break;
            }
        }
        // SELECT expressions may not rebind a variable that is already in scope, and with
        // grouping only group keys and aggregates may be projected
        if let Form::Select { projection, .. } = &form {
            let grouped = !group_by.is_empty() || !self.aggs.is_empty();
            match projection {
                None if grouped => {
                    return self.err("SELECT * is not allowed with GROUP BY or aggregates");
                }
                None => {}
                Some(proj) => {
                    // with grouping only the group keys are in scope for SELECT expressions
                    let in_scope = if grouped {
                        group_by
                            .iter()
                            .flat_map(|(e, alias)| match (e, alias) {
                                (_, Some(a)) => vec![*a],
                                (Expr::Var(v), None) => vec![*v],
                                _ => vec![],
                            })
                            .collect()
                    } else {
                        crate::sparql::eval::visible_vars(&pattern, &self.vars)
                    };
                    let keys: Vec<Var> = group_by
                        .iter()
                        .flat_map(|(e, alias)| match (e, alias) {
                            (_, Some(a)) => vec![*a],
                            (Expr::Var(v), None) => vec![*v],
                            _ => vec![],
                        })
                        .collect();
                    let mut projected: Vec<Var> = Vec::new();
                    for (e, v) in proj {
                        match e {
                            Some(e) => {
                                if in_scope.contains(v) || projected.contains(v) {
                                    return self.err(&format!(
                                        "?{} is already in scope",
                                        self.vars.names[*v]
                                    ));
                                }
                                if grouped {
                                    let mut used = Vec::new();
                                    expr_vars(e, &mut used);
                                    let bad: Vec<Var> = used
                                        .into_iter()
                                        .filter(|u| {
                                            !keys.contains(u)
                                                && !self.agg_vars.contains(u)
                                                && !projected.contains(u)
                                        })
                                        .collect();
                                    self.ungrouped.extend(bad);
                                }
                            }
                            None if grouped && !keys.contains(v) => self.ungrouped.push(*v),
                            None => {}
                        }
                        projected.push(*v);
                    }
                }
            }
        }
        Ok(Query {
            form,
            dataset,
            pattern,
            group_by,
            having,
            aggregates: std::mem::take(&mut self.aggs),
            agg_vars: std::mem::take(&mut self.agg_vars),
            order_by,
            limit,
            offset,
            values: None,
        })
    }

    /// A built-in call like `STR(` - but not a clause keyword followed by '('.
    fn is_call_start(&self) -> bool {
        match self.peek() {
            Tok::Word(w) => {
                self.peek_at(1) == &Tok::Punct("(")
                    && !["HAVING", "ORDER", "LIMIT", "OFFSET", "VALUES", "GROUP"]
                        .iter()
                        .any(|k| w.eq_ignore_ascii_case(k))
            }
            _ => false,
        }
    }

    fn bracketted(&mut self) -> Result<Expr, String> {
        self.expect("(")?;
        let e = self.expr()?;
        self.expect(")")?;
        Ok(e)
    }

    // ------------------------------------------------------------ patterns

    fn join(a: Pattern, b: Pattern) -> Pattern {
        match (a, b) {
            (Pattern::Bgp(x), b) if x.is_empty() => b,
            (a, Pattern::Bgp(y)) if y.is_empty() => a,
            (Pattern::Bgp(mut x), Pattern::Bgp(y)) => {
                x.extend(y);
                Pattern::Bgp(x)
            }
            (a, b) => Pattern::Join(Box::new(a), Box::new(b)),
        }
    }

    fn group_graph_pattern(&mut self) -> Result<Pattern, String> {
        self.nest(Self::group_graph_pattern_inner)
    }

    fn group_graph_pattern_inner(&mut self) -> Result<Pattern, String> {
        self.expect("{")?;
        if self.is_word("SELECT") {
            let saved_aggs = std::mem::take(&mut self.aggs);
            let saved_agg_vars = std::mem::take(&mut self.agg_vars);
            self.i += 1;
            let form = self.select_clause()?;
            self.word("WHERE");
            let pattern = self.group_graph_pattern()?;
            let mut q = self.finish_query(form, None, pattern)?;
            if self.word("VALUES") {
                let values = self.data_block()?;
                self.check_grouping(Some(&values))?;
                q.values = Some(values);
            } else {
                self.check_grouping(None)?;
            }
            self.aggs = saved_aggs;
            self.agg_vars = saved_agg_vars;
            self.expect("}")?;
            return Ok(Pattern::SubSelect(Box::new(q)));
        }
        let mut g = Pattern::empty();
        let mut filters = Vec::new();
        self.bgp += 1;
        // '.' may follow a triples block (consumed there) or a non-triples pattern, once
        let mut dot_ok = false;
        loop {
            if self.punct("}") {
                break;
            }
            if self.is_punct(".") {
                if !dot_ok {
                    return self.err("unexpected '.'");
                }
                self.i += 1;
                dot_ok = false;
                continue;
            }
            let triples_next = !(self.is_punct("{") || self.starts_pattern_keyword());
            if !triples_next {
                dot_ok = true;
                // anything but FILTER ends the current basic graph pattern
                if !self.is_word("FILTER") {
                    self.bgp += 1;
                }
            }
            if !self.is_punct("}") {
                self.link()?;
            }
            if self.word("FILTER") {
                filters.push(self.constraint()?);
            } else if self.word("OPTIONAL") {
                let inner = self.group_graph_pattern()?;
                self.bgp += 1;
                g = match inner {
                    Pattern::Filter(e, x) => Pattern::LeftJoin(Box::new(g), x, Some(e)),
                    other => Pattern::LeftJoin(Box::new(g), Box::new(other), None),
                };
            } else if self.word("MINUS") {
                let inner = self.group_graph_pattern()?;
                self.bgp += 1;
                g = Pattern::Minus(Box::new(g), Box::new(inner));
            } else if self.word("GRAPH") {
                let name = self.var_or_iri()?;
                let inner = self.group_graph_pattern()?;
                self.bgp += 1;
                g = Self::join(g, Pattern::Graph(name, Box::new(inner)));
            } else if self.word("SERVICE") {
                let silent = self.word("SILENT");
                let name = self.var_or_iri()?;
                let from = self.spans[self.i].0;
                let inner = self.group_graph_pattern()?;
                let to = self.spans[self.i - 1].1;
                let body: String = self.src[from..to].iter().collect();
                let text = format!("{}SELECT * WHERE {body}", self.prologue_text(&body));
                g = Self::join(g, Pattern::Service(name, Box::new(inner), silent, text));
            } else if self.word("BIND") {
                self.expect("(")?;
                let e = self.expr()?;
                self.expect_word("AS")?;
                let v = match self.next() {
                    Tok::Var(v) => self.var(&v),
                    _ => return self.err("expected variable"),
                };
                self.expect(")")?;
                if crate::sparql::eval::visible_vars(&g, &self.vars).contains(&v) {
                    return self.err(&format!(
                        "BIND: ?{} is already in scope",
                        self.vars.names[v]
                    ));
                }
                g = Pattern::Extend(Box::new(g), v, e);
            } else if self.word("VALUES") {
                let vals = self.data_block()?;
                g = Self::join(g, vals);
            } else if self.is_punct("{") {
                let mut alts = vec![self.group_graph_pattern()?];
                while self.word("UNION") {
                    alts.push(self.group_graph_pattern()?);
                }
                let p = balanced(alts, &|a, b| Pattern::Union(Box::new(a), Box::new(b)));
                // keep a nested group's FILTER inside it: `OPTIONAL { { … FILTER } }` must not
                // turn the filter into the OPTIONAL's condition, which sees outer variables
                g = match (&g, &p) {
                    (Pattern::Bgp(x), Pattern::Filter(..)) if x.is_empty() => {
                        Pattern::Join(Box::new(g), Box::new(p))
                    }
                    _ => Self::join(g, p),
                };
                self.bgp += 1;
            } else {
                let mut triples = Vec::new();
                self.triples_same_subject_path(&mut triples)?;
                g = Self::join(g, Pattern::Bgp(triples));
                dot_ok = false;
                if !self.punct(".")
                    && !self.is_punct("}")
                    && !self.is_punct("{")
                    && !self.starts_pattern_keyword()
                {
                    return self.err("expected '.' or '}'");
                }
            }
        }
        Ok(match filters.len() {
            0 => g,
            _ => Pattern::Filter(
                balanced(filters, &|a, b| Expr::And(Box::new(a), Box::new(b))),
                Box::new(g),
            ),
        })
    }

    fn starts_pattern_keyword(&self) -> bool {
        [
            "FILTER", "OPTIONAL", "MINUS", "GRAPH", "SERVICE", "BIND", "VALUES",
        ]
        .iter()
        .any(|w| self.is_word(w))
            || self.is_punct("{")
    }

    fn data_block(&mut self) -> Result<Pattern, String> {
        let mut vars = Vec::new();
        let single = if let Tok::Var(v) = self.peek().clone() {
            self.i += 1;
            vars.push(self.var(&v));
            true
        } else {
            self.expect("(")?;
            while let Tok::Var(v) = self.peek().clone() {
                self.i += 1;
                let slot = self.var(&v);
                if vars.contains(&slot) {
                    return self.err(&format!("?{v} appears twice in VALUES"));
                }
                vars.push(slot);
            }
            self.expect(")")?;
            false
        };
        self.expect("{")?;
        let mut rows = Vec::new();
        while !self.punct("}") {
            if single {
                rows.push(vec![self.data_value()?]);
            } else {
                self.expect("(")?;
                let mut row = Vec::new();
                while !self.punct(")") {
                    row.push(self.data_value()?);
                }
                if row.len() != vars.len() {
                    return self.err("VALUES row has wrong arity");
                }
                rows.push(row);
            }
        }
        Ok(Pattern::Values(vars, rows))
    }

    fn data_value(&mut self) -> Result<Option<Term>, String> {
        if self.word("UNDEF") {
            return Ok(None);
        }
        Ok(Some(self.graph_term()?))
    }

    fn var_or_iri(&mut self) -> Result<VarOrTerm, String> {
        match self.peek().clone() {
            Tok::Var(v) => {
                self.i += 1;
                Ok(VarOrTerm::Var(self.var(&v)))
            }
            _ => Ok(VarOrTerm::Term(Term::Iri(self.iri()?))),
        }
    }

    /// IRI, literal, or blank node constant.
    fn graph_term(&mut self) -> Result<Term, String> {
        if self.is_punct("<<(") {
            self.check_constant_triple_term()?;
            let mut scratch = Vec::new();
            return match self.triple_term_pattern(&mut scratch)? {
                VarOrTerm::Term(t) if scratch.is_empty() => Ok(t),
                _ => self.err("variables and blank nodes are not allowed in this triple term"),
            };
        }
        let t = self.next();
        if let Some(i) = self.iri_tok(&t)? {
            return Ok(Term::Iri(i));
        }
        match t {
            Tok::Str(s) => self.literal_rest(s),
            Tok::Int(n) => Ok(Term::typed(n, rdf::XSD_INTEGER)),
            Tok::Dec(n) => Ok(Term::typed(n, rdf::XSD_DECIMAL)),
            Tok::Dbl(n) => Ok(Term::typed(n, rdf::XSD_DOUBLE)),
            Tok::Punct(sign @ ("+" | "-")) => match self.next() {
                Tok::Int(n) => Ok(Term::typed(format!("{sign}{n}"), rdf::XSD_INTEGER)),
                Tok::Dec(n) => Ok(Term::typed(format!("{sign}{n}"), rdf::XSD_DECIMAL)),
                Tok::Dbl(n) => Ok(Term::typed(format!("{sign}{n}"), rdf::XSD_DOUBLE)),
                _ => self.err("expected number"),
            },
            Tok::Word(w) if w.eq_ignore_ascii_case("true") => Ok(Term::boolean(true)),
            Tok::Word(w) if w.eq_ignore_ascii_case("false") => Ok(Term::boolean(false)),
            Tok::Blank(b) => Ok(Term::Blank(b)),
            Tok::Punct("[") if self.punct("]") => Ok(Term::fresh_blank()),
            Tok::Punct("(") if self.punct(")") => Ok(Term::iri(rdf::RDF_NIL)),
            _ => {
                self.i -= 1;
                self.err("expected RDF term")
            }
        }
    }

    fn literal_rest(&mut self, s: String) -> Result<Term, String> {
        if let Tok::Lang(l) = self.peek().clone() {
            self.i += 1;
            // SPARQL 1.2: "text"@lang--ltr / --rtl
            if let Some((lang, dir)) = l.split_once("--") {
                if !rdf::valid_lang(lang) || (dir != "ltr" && dir != "rtl") {
                    return self.err(&format!("bad language tag or direction '@{l}'"));
                }
                return Ok(Term::dir_lang(s, lang, dir));
            }
            if !rdf::valid_lang(&l) {
                return self.err(&format!("bad language tag '@{l}'"));
            }
            return Ok(Term::lang(s, &l));
        }
        if self.punct("^^") {
            let dt = self.iri()?;
            return Ok(Term::typed(s, &dt));
        }
        Ok(Term::string(s))
    }

    fn blank_var(&mut self, label: Option<&str>) -> Result<VarOrTerm, String> {
        if self.no_bnodes {
            return self.err("blank nodes are not allowed in DELETE");
        }
        if self.template {
            if let Some(l) = label {
                if self.data_block {
                    if self.data_labels.contains(l) {
                        return self.err(&format!(
                            "blank node _:{l} is already used by an earlier INSERT DATA"
                        ));
                    }
                    self.op_labels.insert(l.to_string());
                }
            }
            return Ok(VarOrTerm::Term(match label {
                Some(l) => Term::Blank(l.to_string()),
                None => Term::fresh_blank(),
            }));
        }
        Ok(match label {
            Some(l) => {
                if let Some(&b) = self.bnode_bgp.get(l) {
                    if b != self.bgp {
                        return self.err(&format!(
                            "blank node _:{l} is used in two different graph patterns"
                        ));
                    }
                }
                self.bnode_bgp.insert(l.to_string(), self.bgp);
                if let Some(&v) = self.bnode_vars.get(l) {
                    return Ok(VarOrTerm::Var(v));
                }
                let v = self.vars.hidden();
                self.bnode_vars.insert(l.to_string(), v);
                VarOrTerm::Var(v)
            }
            None => VarOrTerm::Var(self.vars.hidden()),
        })
    }

    /// VarOrTerm, or a blank node property list / collection (emitting triples).
    /// `<< s p o >>` in a pattern; ground triples become constants.
    /// Predicate of a triple term or reified triple: a variable, an IRI or `a`.
    fn tt_predicate(&mut self) -> Result<VarOrTerm, String> {
        Ok(match self.peek().clone() {
            Tok::Var(v) => {
                self.i += 1;
                VarOrTerm::Var(self.var(&v))
            }
            _ if self.word("a") => VarOrTerm::Term(Term::iri(rdf::RDF_TYPE)),
            _ => VarOrTerm::Term(Term::Iri(self.iri()?)),
        })
    }

    fn tt(s: VarOrTerm, p: VarOrTerm, o: VarOrTerm) -> VarOrTerm {
        match (s, p, o) {
            (VarOrTerm::Term(s), VarOrTerm::Term(p), VarOrTerm::Term(o)) => {
                VarOrTerm::Term(Term::triple(s, p, o))
            }
            (s, p, o) => VarOrTerm::Triple(Box::new([s, p, o])),
        }
    }

    /// In VALUES and expressions a triple term may not have a triple term subject, nor
    /// contain blank nodes (checked on the tokens of the term, which starts at `<<(`).
    fn check_constant_triple_term(&self) -> Result<(), String> {
        let mut depth = 0;
        let mut j = self.i;
        loop {
            match self.t.get(j) {
                Some(Tok::Punct("<<(")) => {
                    // a nested triple term right after an opening `<<(` is in subject position
                    if depth > 0 && matches!(self.t.get(j - 1), Some(Tok::Punct("<<("))) {
                        return self
                            .err("a triple term cannot be the subject of a triple term here");
                    }
                    depth += 1;
                }
                Some(Tok::Punct(")>>")) => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                Some(Tok::Blank(_)) | Some(Tok::Punct("[")) => {
                    return self.err("blank nodes are not allowed in a triple term here");
                }
                Some(Tok::Eof) | None => return Ok(()),
                _ => {}
            }
            j += 1;
        }
    }

    /// SPARQL 1.2 triple term `<<( s p o )>>`.
    fn triple_term_pattern(&mut self, out: &mut Vec<TriplePattern>) -> Result<VarOrTerm, String> {
        self.nest(|p| {
            p.expect("<<(")?;
            let s = match p.peek().clone() {
                Tok::Punct("<<") | Tok::Punct("(") | Tok::Str(_) | Tok::Int(_) | Tok::Dec(_) | Tok::Dbl(_) => {
                    return p.err("the subject of a triple term must be a variable, IRI, blank node or triple term");
                }
                Tok::Punct("[") if !matches!(p.peek_at(1), Tok::Punct("]")) => return p.err("a blank node property list cannot be part of a triple term"),
                _ => p.node_inner(out)?,
            };
            let pr = p.tt_predicate()?;
            let o = match p.peek() {
                Tok::Punct("<<") | Tok::Punct("[") | Tok::Punct("(") if !matches!(p.peek_at(1), Tok::Punct("]")) => {
                    return p.err("a triple term object cannot be a reified triple, list or blank node property list");
                }
                _ => p.node_inner(out)?,
            };
            p.expect(")>>")?;
            Ok(Self::tt(s, pr, o))
        })
    }

    /// SPARQL 1.2 reified triple `<< s p o ~ r? >>`: the reifier, with `r rdf:reifies <<( s p o )>>`
    /// added to the pattern (a fresh blank node / variable when no reifier is given).
    fn quoted_pattern(&mut self, out: &mut Vec<TriplePattern>) -> Result<VarOrTerm, String> {
        self.expect("<<")?;
        let s = match self.peek() {
            Tok::Punct("(") | Tok::Str(_) | Tok::Int(_) | Tok::Dec(_) | Tok::Dbl(_) => {
                return self.err("bad subject in a reified triple");
            }
            Tok::Punct("[") if !matches!(self.peek_at(1), Tok::Punct("]")) => {
                return self.err("bad subject in a reified triple");
            }
            _ => self.node_inner(out)?,
        };
        let p = self.tt_predicate()?;
        let o = match self.peek() {
            Tok::Punct("(") => return self.err("a list cannot be part of a reified triple"),
            Tok::Punct("[") if !matches!(self.peek_at(1), Tok::Punct("]")) => {
                return self.err("a blank node property list cannot be part of a reified triple");
            }
            _ => self.node_inner(out)?,
        };
        let reifier = if self.punct("~") {
            self.reifier_id()?
        } else {
            self.blank_var(None)?
        };
        self.expect(">>")?;
        out.push(TriplePattern {
            s: reifier.clone(),
            p: Predicate::Path(Path::Iri(rdf::RDF_REIFIES.into())),
            o: Self::tt(s, p, o),
        });
        Ok(reifier)
    }

    /// After `~`: a variable, IRI or blank node naming a reifier, or a fresh one.
    fn reifier_id(&mut self) -> Result<VarOrTerm, String> {
        match self.peek().clone() {
            Tok::Var(v) => {
                self.i += 1;
                Ok(VarOrTerm::Var(self.var(&v)))
            }
            Tok::Blank(b) => {
                self.i += 1;
                self.blank_var(Some(&b))
            }
            Tok::Punct("[") if matches!(self.peek_at(1), Tok::Punct("]")) => {
                self.i += 2;
                self.blank_var(None)
            }
            Tok::Iri(_) | Tok::PName(..) => Ok(VarOrTerm::Term(Term::Iri(self.iri()?))),
            _ => self.blank_var(None),
        }
    }

    fn node(&mut self, out: &mut Vec<TriplePattern>) -> Result<VarOrTerm, String> {
        self.nest(|p| p.node_inner(out))
    }

    fn node_inner(&mut self, out: &mut Vec<TriplePattern>) -> Result<VarOrTerm, String> {
        if self.is_punct("<<(") {
            return self.triple_term_pattern(out);
        }
        if self.is_punct("<<") {
            return self.quoted_pattern(out);
        }
        match self.peek().clone() {
            Tok::Var(v) => {
                self.i += 1;
                Ok(VarOrTerm::Var(self.var(&v)))
            }
            Tok::Blank(b) => {
                self.i += 1;
                self.blank_var(Some(&b))
            }
            Tok::Punct("[") => {
                self.i += 1;
                let node = self.blank_var(None)?;
                if !self.punct("]") {
                    self.property_list_path(&node, out)?;
                    self.expect("]")?;
                }
                Ok(node)
            }
            Tok::Punct("(") => {
                self.i += 1;
                let mut items = Vec::new();
                while !self.punct(")") {
                    items.push(self.node(out)?);
                }
                if items.is_empty() {
                    return Ok(VarOrTerm::Term(Term::iri(rdf::RDF_NIL)));
                }
                let head = self.blank_var(None)?;
                let mut cur = head.clone();
                let n = items.len();
                for (k, item) in items.into_iter().enumerate() {
                    out.push(TriplePattern {
                        s: cur.clone(),
                        p: Predicate::Path(Path::Iri(rdf::RDF_FIRST.into())),
                        o: item,
                    });
                    let next = if k + 1 == n {
                        VarOrTerm::Term(Term::iri(rdf::RDF_NIL))
                    } else {
                        self.blank_var(None)?
                    };
                    out.push(TriplePattern {
                        s: cur.clone(),
                        p: Predicate::Path(Path::Iri(rdf::RDF_REST.into())),
                        o: next.clone(),
                    });
                    cur = next;
                }
                Ok(head)
            }
            _ => Ok(VarOrTerm::Term(self.graph_term()?)),
        }
    }

    fn triples_same_subject_path(&mut self, out: &mut Vec<TriplePattern>) -> Result<(), String> {
        let is_bnode_list = self.is_punct("[") && self.peek_at(1) != &Tok::Punct("]");
        let is_collection = self.is_punct("(") && self.peek_at(1) != &Tok::Punct(")");
        let is_reified = self.is_punct("<<");
        let subj = self.node(out)?;
        if (is_bnode_list || is_collection || is_reified)
            && (self.is_punct(".") || self.is_punct("}") || self.is_punct("|}"))
        {
            return Ok(());
        }
        self.property_list_path(&subj, out)
    }

    fn property_list_path(
        &mut self,
        subj: &VarOrTerm,
        out: &mut Vec<TriplePattern>,
    ) -> Result<(), String> {
        loop {
            let pred = if let Tok::Var(v) = self.peek().clone() {
                self.i += 1;
                Predicate::Var(self.var(&v))
            } else {
                Predicate::Path(self.path()?)
            };
            loop {
                let obj = self.node(out)?;
                out.push(TriplePattern {
                    s: subj.clone(),
                    p: pred.clone(),
                    o: obj.clone(),
                });
                // SPARQL 1.2 annotations: `~ r` names a reifier of the triple just written,
                // `{| ... |}` describes the preceding reifier or a fresh one
                if self.is_punct("~") || self.is_punct("{|") {
                    let p = match &pred {
                        Predicate::Var(v) => VarOrTerm::Var(*v),
                        Predicate::Path(Path::Iri(i)) => VarOrTerm::Term(Term::Iri(i.clone())),
                        Predicate::Path(_) => {
                            return self.err("an annotated triple needs a simple predicate");
                        }
                    };
                    let triple = Self::tt(subj.clone(), p, obj.clone());
                    let reifies = Predicate::Path(Path::Iri(rdf::RDF_REIFIES.into()));
                    let mut current: Option<VarOrTerm> = None;
                    loop {
                        if self.punct("~") {
                            let r = self.reifier_id()?;
                            out.push(TriplePattern {
                                s: r.clone(),
                                p: reifies.clone(),
                                o: triple.clone(),
                            });
                            current = Some(r);
                        } else if self.punct("{|") {
                            let r = match current.take() {
                                Some(r) => r,
                                None => {
                                    let r = self.blank_var(None)?;
                                    out.push(TriplePattern {
                                        s: r.clone(),
                                        p: reifies.clone(),
                                        o: triple.clone(),
                                    });
                                    r
                                }
                            };
                            self.property_list_path(&r, out)?;
                            self.expect("|}")?;
                        } else {
                            break;
                        }
                    }
                }
                if !self.punct(",") {
                    break;
                }
            }
            if !self.punct(";") {
                return Ok(());
            }
            while self.punct(";") {}
            if self.is_punct(".")
                || self.is_punct("]")
                || self.is_punct("}")
                || self.is_punct("{")
                || self.is_punct("|}")
                || self.starts_pattern_keyword()
                || self.peek() == &Tok::Eof
            {
                return Ok(());
            }
        }
    }

    fn path(&mut self) -> Result<Path, String> {
        self.nest(Self::path_inner)
    }

    fn path_inner(&mut self) -> Result<Path, String> {
        let mut alts = vec![self.path_seq()?];
        while self.punct("|") {
            alts.push(self.path_seq()?);
        }
        Ok(if alts.len() == 1 {
            alts.pop().unwrap()
        } else {
            Path::Alt(alts)
        })
    }

    fn path_seq(&mut self) -> Result<Path, String> {
        let mut seq = vec![self.path_elt_or_inverse()?];
        while self.punct("/") {
            seq.push(self.path_elt_or_inverse()?);
        }
        Ok(if seq.len() == 1 {
            seq.pop().unwrap()
        } else {
            Path::Seq(seq)
        })
    }

    fn path_elt_or_inverse(&mut self) -> Result<Path, String> {
        if self.punct("^") {
            Ok(Path::Inverse(Box::new(self.path_elt()?)))
        } else {
            self.path_elt()
        }
    }

    fn path_elt(&mut self) -> Result<Path, String> {
        let prim = self.path_primary()?;
        Ok(if self.punct("*") {
            Path::ZeroOrMore(Box::new(prim))
        } else if self.punct("+") {
            Path::OneOrMore(Box::new(prim))
        } else if self.punct("?") {
            Path::ZeroOrOne(Box::new(prim))
        } else {
            prim
        })
    }

    fn path_primary(&mut self) -> Result<Path, String> {
        if self.word("a") {
            return Ok(Path::Iri(rdf::RDF_TYPE.into()));
        }
        if self.punct("!") {
            let mut set = Vec::new();
            let one = |p: &mut Self| -> Result<(String, bool), String> {
                let inv = p.punct("^");
                if p.word("a") {
                    return Ok((rdf::RDF_TYPE.into(), inv));
                }
                Ok((p.iri()?, inv))
            };
            if self.punct("(") {
                if !self.punct(")") {
                    set.push(one(self)?);
                    while self.punct("|") {
                        set.push(one(self)?);
                    }
                    self.expect(")")?;
                }
            } else {
                set.push(one(self)?);
            }
            return Ok(Path::Negated(set));
        }
        if self.punct("(") {
            let p = self.path()?;
            self.expect(")")?;
            return Ok(p);
        }
        Ok(Path::Iri(self.iri()?))
    }

    fn template_block(&mut self) -> Result<Vec<TriplePattern>, String> {
        self.expect("{")?;
        let saved = self.template;
        self.template = true;
        let mut out = Vec::new();
        while !self.punct("}") {
            if self.punct(".") {
                continue;
            }
            self.triples_same_subject_path(&mut out)?;
        }
        self.template = saved;
        Ok(out)
    }

    // ------------------------------------------------------------ expressions

    fn constraint(&mut self) -> Result<Expr, String> {
        if self.is_punct("(") {
            return self.bracketted();
        }
        if !matches!(self.peek(), Tok::Word(_) | Tok::Iri(_) | Tok::PName(..)) {
            return self.err("FILTER needs a parenthesized expression or a function call");
        }
        self.primary()
    }

    fn expr(&mut self) -> Result<Expr, String> {
        self.nest(|p| {
            let mut ors = vec![p.and_expr()?];
            while p.punct("||") {
                ors.push(p.and_expr()?);
            }
            Ok(balanced(ors, &|a, b| Expr::Or(Box::new(a), Box::new(b))))
        })
    }

    fn and_expr(&mut self) -> Result<Expr, String> {
        let mut ands = vec![self.rel_expr()?];
        while self.punct("&&") {
            ands.push(self.rel_expr()?);
        }
        Ok(balanced(ands, &|a, b| Expr::And(Box::new(a), Box::new(b))))
    }

    fn rel_expr(&mut self) -> Result<Expr, String> {
        let e = self.add_expr()?;
        let op = match self.peek() {
            Tok::Punct("=") => CmpOp::Eq,
            Tok::Punct("!=") => CmpOp::Ne,
            Tok::Punct("<") => CmpOp::Lt,
            Tok::Punct(">") => CmpOp::Gt,
            Tok::Punct("<=") => CmpOp::Le,
            Tok::Punct(">=") => CmpOp::Ge,
            _ => {
                let negated = if self.is_word("NOT")
                    && matches!(self.peek_at(1), Tok::Word(w) if w.eq_ignore_ascii_case("IN"))
                {
                    self.i += 2;
                    true
                } else if self.word("IN") {
                    false
                } else {
                    return Ok(e);
                };
                let list = self.arg_list()?;
                return Ok(Expr::In(Box::new(e), list, negated));
            }
        };
        self.i += 1;
        let r = self.add_expr()?;
        Ok(Expr::Cmp(op, Box::new(e), Box::new(r)))
    }

    fn add_expr(&mut self) -> Result<Expr, String> {
        let mut e = self.mul_expr()?;
        loop {
            if matches!(self.peek(), Tok::Punct("+") | Tok::Punct("-")) {
                self.link()?;
            }
            if self.punct("+") {
                let r = self.mul_expr()?;
                e = Expr::Arith('+', Box::new(e), Box::new(r));
            } else if self.punct("-") {
                let r = self.mul_expr()?;
                e = Expr::Arith('-', Box::new(e), Box::new(r));
            } else if matches!(self.peek(), Tok::Int(n) | Tok::Dec(n) | Tok::Dbl(n) if n.starts_with('-') || n.starts_with('+'))
            {
                // signed numeric literal directly after an operand
                let r = self.mul_expr()?;
                e = Expr::Arith('+', Box::new(e), Box::new(r));
            } else {
                return Ok(e);
            }
        }
    }

    fn mul_expr(&mut self) -> Result<Expr, String> {
        let mut e = self.unary()?;
        loop {
            if matches!(self.peek(), Tok::Punct("*") | Tok::Punct("/")) {
                self.link()?;
            }
            if self.punct("*") {
                let r = self.unary()?;
                e = Expr::Arith('*', Box::new(e), Box::new(r));
            } else if self.punct("/") {
                let r = self.unary()?;
                e = Expr::Arith('/', Box::new(e), Box::new(r));
            } else {
                return Ok(e);
            }
        }
    }

    fn unary(&mut self) -> Result<Expr, String> {
        self.nest(Self::unary_inner)
    }

    fn unary_inner(&mut self) -> Result<Expr, String> {
        if self.punct("!") {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        if self.punct("-") {
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        if self.punct("+") {
            return self.unary();
        }
        self.primary()
    }

    fn arg_list(&mut self) -> Result<Vec<Expr>, String> {
        self.expect("(")?;
        let mut args = Vec::new();
        if self.punct(")") {
            return Ok(args);
        }
        loop {
            args.push(self.expr()?);
            if self.punct(")") {
                return Ok(args);
            }
            self.expect(",")?;
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            Tok::Punct("(") => self.bracketted(),
            Tok::Var(v) => {
                self.i += 1;
                Ok(Expr::Var(self.var(&v)))
            }
            Tok::Iri(_) | Tok::PName(..) => {
                let iri = self.iri()?;
                if self.is_punct("(") {
                    // function call (possibly with DISTINCT for custom aggregates)
                    self.expect("(")?;
                    self.word("DISTINCT");
                    let mut args = Vec::new();
                    if !self.punct(")") {
                        loop {
                            args.push(self.expr()?);
                            if self.punct(")") {
                                break;
                            }
                            self.expect(",")?;
                        }
                    }
                    Ok(Expr::Call(iri, args))
                } else {
                    Ok(Expr::Const(Term::Iri(iri)))
                }
            }
            Tok::Str(_) | Tok::Int(_) | Tok::Dec(_) | Tok::Dbl(_) => {
                Ok(Expr::Const(self.graph_term()?))
            }
            Tok::Punct("<<(") => {
                self.check_constant_triple_term()?;
                let mut scratch = Vec::new();
                let vt = self.triple_term_pattern(&mut scratch)?;
                if !scratch.is_empty() {
                    return self.err(
                        "a triple term in an expression cannot contain a blank node property list",
                    );
                }
                fn to_expr(vt: VarOrTerm) -> Expr {
                    match vt {
                        VarOrTerm::Var(v) => Expr::Var(v),
                        VarOrTerm::Term(t) => Expr::Const(t),
                        VarOrTerm::Triple(p) => {
                            let [s, p, o] = *p;
                            Expr::Call("TRIPLE".into(), vec![to_expr(s), to_expr(p), to_expr(o)])
                        }
                    }
                }
                Ok(to_expr(vt))
            }
            Tok::Word(w) => {
                let up = w.to_ascii_uppercase();
                match up.as_str() {
                    "TRUE" | "FALSE" => Ok(Expr::Const(self.graph_term()?)),
                    "NOT" => {
                        self.i += 1;
                        self.expect_word("EXISTS")?;
                        let p = self.group_graph_pattern()?;
                        Ok(Expr::Exists(Box::new(p), true))
                    }
                    "EXISTS" => {
                        self.i += 1;
                        let p = self.group_graph_pattern()?;
                        Ok(Expr::Exists(Box::new(p), false))
                    }
                    "BOUND" => {
                        self.i += 1;
                        self.expect("(")?;
                        let v = match self.next() {
                            Tok::Var(v) => self.var(&v),
                            _ => return self.err("BOUND expects a variable"),
                        };
                        self.expect(")")?;
                        Ok(Expr::Bound(v))
                    }
                    "COUNT" | "SUM" | "MIN" | "MAX" | "AVG" | "SAMPLE" | "GROUP_CONCAT" => {
                        self.i += 1;
                        if !self.allow_aggs {
                            return self.err("aggregate not allowed here");
                        }
                        self.expect("(")?;
                        let distinct = self.word("DISTINCT");
                        // no aggregate inside an aggregate
                        self.allow_aggs = false;
                        let expr = if up == "COUNT" && self.punct("*") {
                            None
                        } else {
                            Some(self.expr()?)
                        };
                        self.allow_aggs = true;
                        let mut separator = " ".to_string();
                        if self.punct(";") {
                            self.expect_word("SEPARATOR")?;
                            self.expect("=")?;
                            match self.next() {
                                Tok::Str(s) => separator = s,
                                _ => return self.err("expected separator string"),
                            }
                        }
                        self.expect(")")?;
                        self.aggs.push(Aggregate {
                            func: up,
                            distinct,
                            expr,
                            separator,
                        });
                        let hv = self.vars.hidden();
                        self.agg_vars.push(hv);
                        Ok(Expr::Var(hv))
                    }
                    _ => {
                        if self.peek_at(1) != &Tok::Punct("(") && up != "NOW" && up != "RAND" {
                            return self.err(&format!("unexpected '{w}'"));
                        }
                        self.i += 1;
                        let args = if self.is_punct("(") {
                            self.arg_list()?
                        } else {
                            Vec::new()
                        };
                        Ok(Expr::Call(up, args))
                    }
                }
            }
            Tok::Punct("+" | "-") => Ok(Expr::Const(self.graph_term()?)),
            _ => self.err("expected expression"),
        }
    }

    // ------------------------------------------------------------ updates

    fn quad_block(&mut self, template: bool) -> Result<Vec<QuadPattern>, String> {
        self.expect("{")?;
        let saved = self.template;
        self.template = template;
        let mut out = Vec::new();
        loop {
            if self.punct("}") {
                break;
            }
            if self.punct(".") {
                continue;
            }
            if self.word("GRAPH") {
                let g = self.var_or_iri()?;
                self.expect("{")?;
                let mut triples = Vec::new();
                while !self.punct("}") {
                    if self.punct(".") {
                        continue;
                    }
                    self.triples_same_subject_path(&mut triples)?;
                }
                for t in triples {
                    out.push(self.to_quad(t, Some(g.clone()))?);
                }
            } else {
                let mut triples = Vec::new();
                self.triples_same_subject_path(&mut triples)?;
                for t in triples {
                    out.push(self.to_quad(t, None)?);
                }
            }
        }
        self.template = saved;
        Ok(out)
    }

    fn to_quad(&self, t: TriplePattern, g: Option<VarOrTerm>) -> Result<QuadPattern, String> {
        if self.data_block
            && [&t.s, &t.o]
                .into_iter()
                .chain(g.as_ref())
                .any(|x| matches!(x, VarOrTerm::Var(_)))
            || self.data_block && matches!(t.p, Predicate::Var(_))
        {
            return self.err("variables are not allowed in INSERT DATA / DELETE DATA");
        }
        let p = match t.p {
            Predicate::Var(v) => VarOrTerm::Var(v),
            Predicate::Path(Path::Iri(i)) => VarOrTerm::Term(Term::Iri(i)),
            Predicate::Path(_) => {
                return self.err("property paths are not allowed in update templates");
            }
        };
        Ok(QuadPattern {
            s: t.s,
            p,
            o: t.o,
            g,
        })
    }

    fn graph_ref_all(&mut self) -> Result<GraphTarget, String> {
        if self.word("DEFAULT") {
            Ok(GraphTarget::Default)
        } else if self.word("NAMED") {
            Ok(GraphTarget::Named)
        } else if self.word("ALL") {
            Ok(GraphTarget::All)
        } else {
            self.expect_word("GRAPH")?;
            Ok(GraphTarget::Graph(self.iri()?))
        }
    }

    fn graph_or_default(&mut self) -> Result<Option<String>, String> {
        if self.word("DEFAULT") {
            return Ok(None);
        }
        self.word("GRAPH");
        Ok(Some(self.iri()?))
    }

    fn update_op(&mut self) -> Result<UpdateOp, String> {
        if self.word("LOAD") {
            let silent = self.word("SILENT");
            let source = self.iri()?;
            let into = if self.word("INTO") {
                self.expect_word("GRAPH")?;
                Some(self.iri()?)
            } else {
                None
            };
            return Ok(UpdateOp::Load {
                source,
                into,
                silent,
            });
        }
        if self.word("CLEAR") {
            let silent = self.word("SILENT");
            return Ok(UpdateOp::Clear(self.graph_ref_all()?, silent));
        }
        if self.word("DROP") {
            let silent = self.word("SILENT");
            return Ok(UpdateOp::Drop(self.graph_ref_all()?, silent));
        }
        if self.word("CREATE") {
            let silent = self.word("SILENT");
            self.expect_word("GRAPH")?;
            return Ok(UpdateOp::Create(self.iri()?, silent));
        }
        for op in ["ADD", "MOVE", "COPY"] {
            if self.word(op) {
                let silent = self.word("SILENT");
                let from = self.graph_or_default()?;
                self.expect_word("TO")?;
                let to = self.graph_or_default()?;
                return Ok(UpdateOp::Transfer(op.into(), from, to, silent));
            }
        }
        if self.is_word("INSERT")
            && matches!(self.peek_at(1), Tok::Word(w) if w.eq_ignore_ascii_case("DATA"))
        {
            self.i += 2;
            self.data_block = true;
            let b = self.quad_block(true);
            self.data_block = false;
            return Ok(UpdateOp::InsertData(b?));
        }
        if self.is_word("DELETE")
            && matches!(self.peek_at(1), Tok::Word(w) if w.eq_ignore_ascii_case("DATA"))
        {
            self.i += 2;
            self.data_block = true;
            self.no_bnodes = true;
            let b = self.quad_block(true);
            self.data_block = false;
            self.no_bnodes = false;
            return Ok(UpdateOp::DeleteData(b?));
        }
        if self.is_word("DELETE")
            && matches!(self.peek_at(1), Tok::Word(w) if w.eq_ignore_ascii_case("WHERE"))
        {
            self.i += 2;
            self.no_bnodes = true;
            let b = self.quad_block(false);
            self.no_bnodes = false;
            return Ok(UpdateOp::DeleteWhere(b?));
        }
        let with = if self.word("WITH") {
            Some(self.iri()?)
        } else {
            None
        };
        let mut delete = Vec::new();
        let mut insert = Vec::new();
        if self.word("DELETE") {
            self.no_bnodes = true;
            let b = self.quad_block(false);
            self.no_bnodes = false;
            delete = b?;
            if self.word("INSERT") {
                insert = self.quad_block(true)?;
            }
        } else if self.word("INSERT") {
            insert = self.quad_block(true)?;
        } else {
            return self.err("expected an update operation");
        }
        let mut using = Dataset::default();
        let mut any_using = false;
        while self.word("USING") {
            any_using = true;
            if self.word("NAMED") {
                using.named.push(self.iri()?);
            } else {
                using.default.push(self.iri()?);
            }
        }
        self.expect_word("WHERE")?;
        let pattern = self.group_graph_pattern()?;
        Ok(UpdateOp::Modify {
            with,
            delete,
            insert,
            using: any_using.then_some(using),
            pattern,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> ParsedQuery {
        parse_query(s, "", &HashMap::new()).unwrap_or_else(|e| panic!("{e}\n{s}"))
    }

    #[test]
    fn parses_common_queries() {
        q("SELECT * WHERE { ?s ?p ?o } LIMIT 10");
        q(
            "PREFIX ex: <http://ex.org/> SELECT DISTINCT ?s (COUNT(?o) AS ?n) WHERE { ?s ex:p/ex:q* ?o . OPTIONAL { ?o a ex:C FILTER(?o != ex:x) } } GROUP BY ?s HAVING (COUNT(?o) > 1) ORDER BY DESC(?n) LIMIT 5 OFFSET 2",
        );
        q(
            "SELECT ?x WHERE { { ?x <p> 1 } UNION { ?x <p> -2.5 } MINUS { ?x <q> [] } BIND(STR(?x) AS ?y) VALUES ?x { <a> <b> } FILTER(REGEX(?y, \"^a\", \"i\") && ?x IN (<a>, <b>)) }",
        );
        q("CONSTRUCT { ?s <p> [ <q> ?o ] } WHERE { GRAPH ?g { ?s ?p ?o } }");
        q("CONSTRUCT WHERE { ?s ?p ?o }");
        q("ASK { ?s ^<p> ?o FILTER NOT EXISTS { ?o !(<x>|^<y>) ?z } }");
        q("SELECT ?s { ?s ?p ?o { SELECT ?s WHERE { ?s ?p ?o } LIMIT 1 } }");
        q("DESCRIBE <http://ex.org/a>");
        q("select ?s where {?s ?p ?o. ?o ?p ?s.}");
    }

    #[test]
    fn parses_updates() {
        let u = parse_update(
            "PREFIX ex: <http://ex.org/> INSERT DATA { ex:a ex:p \"x\" . GRAPH ex:g { ex:a ex:p 1 } } ; \
             DELETE { ?s ex:p ?o } INSERT { ?s ex:q ?o } WHERE { ?s ex:p ?o } ; CLEAR ALL ; DROP SILENT GRAPH ex:g",
            "",
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(u.ops.len(), 4);
    }
}
