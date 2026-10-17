//! YAML-LD: a YAML-LD document is a JSON-LD document in YAML syntax, so this is a YAML
//! subset parser that produces the crate's `Json` (the document then goes through the
//! JSON-LD 1.1 algorithms) and a writer that emits JSON-compatible YAML.
//!
//! The subset follows YAML-LD's canonical profile: block and flow mappings and sequences,
//! plain, quoted and block scalars, comments and document markers. Anchors, aliases, tags,
//! merge keys, complex keys and tab indentation are refused with an error naming the line.

use crate::json::Json;

/// Parse a YAML-LD document into the JSON its JSON-LD algorithms read.
pub fn parse(text: &str) -> Result<Json, String> {
    let mut lines = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let mut indent = 0;
        let mut t = raw;
        while let Some(rest) = t.strip_prefix(' ') {
            indent += 1;
            t = rest;
        }
        let trimmed = t.trim_end();
        if trimmed.starts_with('\t') {
            return Err(format!("YAML error on line {}: tab indentation is not allowed", i + 1));
        }
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "---" || trimmed == "..." {
            continue;
        }
        lines.push(Line { no: i + 1, indent, text: trimmed.to_string() });
    }
    if lines.is_empty() {
        return Ok(Json::Null);
    }
    let mut p = P { lines, at: 0 };
    let v = p.value(0)?;
    if p.at < p.lines.len() {
        return Err(p.err("content after the document"));
    }
    Ok(v)
}

struct Line {
    no: usize,
    indent: usize,
    text: String,
}

struct P {
    lines: Vec<Line>,
    at: usize,
}

impl P {
    fn err(&self, msg: &str) -> String {
        let no = self.lines.get(self.at).map_or(0, |l| l.no);
        format!("YAML error on line {no}: {msg}")
    }

    /// The value starting at the current line, which is indented `want` columns or more.
    fn value(&mut self, want: usize) -> Result<Json, String> {
        let (indent, t) = {
            let line = &self.lines[self.at];
            if line.indent < want {
                return Err(self.err("bad indentation"));
            }
            (line.indent, line.text.clone())
        };
        if let Some(rest) = t.strip_prefix("- ") {
            return self.sequence(indent, rest);
        }
        if t == "-" {
            return self.sequence(indent, "");
        }
        if t.starts_with('[') || t.starts_with('{') {
            let mut chars = Chars { s: &t, at: 0 };
            let v = chars.value()?;
            self.at += 1;
            return Ok(v);
        }
        if t.starts_with('|') || t.starts_with('>') {
            return self.block_scalar(indent, &t);
        }
        match split_key(&t) {
            Some(_) => self.mapping(indent),
            None => {
                let v = scalar(&t).ok_or_else(|| self.err("cannot parse the value"))?;
                self.at += 1;
                Ok(v)
            }
        }
    }

    fn mapping(&mut self, indent: usize) -> Result<Json, String> {
        let mut obj = Json::obj();
        while let Some(line) = self.lines.get(self.at) {
            if line.indent < indent {
                break;
            }
            if line.indent > indent {
                return Err(self.err("bad indentation"));
            }
            let t = line.text.clone();
            let (k, rest) = split_key(&t).ok_or_else(|| self.err("expected key: value"))?;
            self.at += 1;
            let v = self.inline_or_block(indent, &rest)?;
            obj.set(&k, v);
        }
        Ok(obj)
    }

    /// The value after `key:`, which may be empty (the block below, a sequence at the
    /// same indentation, or null).
    fn inline_or_block(&mut self, indent: usize, rest: &str) -> Result<Json, String> {
        if rest.is_empty() {
            return match self.lines.get(self.at) {
                Some(next) if next.indent > indent => self.value(indent + 1),
                Some(next) if next.indent == indent && (next.text.starts_with("- ") || next.text == "-") => {
                    self.sequence(indent, "")
                }
                _ => Ok(Json::Null),
            };
        }
        if rest.starts_with('[') || rest.starts_with('{') {
            let mut chars = Chars { s: rest, at: 0 };
            return chars.value();
        }
        if rest.starts_with('|') || rest.starts_with('>') {
            return self.block_scalar(indent, rest);
        }
        scalar(rest).ok_or_else(|| self.err("cannot parse the value"))
    }

    fn sequence(&mut self, indent: usize, first: &str) -> Result<Json, String> {
        let mut items = Vec::new();
        let mut first = first.to_string();
        loop {
            self.at += 1; // the dash line
            if first.is_empty() {
                items.push(match self.lines.get(self.at) {
                    Some(next) if next.indent > indent => self.value(indent + 1)?,
                    _ => Json::Null,
                });
            } else if let Some((k, rest)) = split_key(&first) {
                // a mapping starting on the dash line; its further keys sit at indent + 2
                let key_col = indent + 2;
                let mut obj = Json::obj();
                obj.set(&k, self.inline_or_block(indent, &rest)?);
                while let Some(line) = self.lines.get(self.at) {
                    if line.indent != key_col || line.text.starts_with("- ") || line.text == "-" {
                        break;
                    }
                    let t = line.text.clone();
                    let (k, rest) = split_key(&t).ok_or_else(|| self.err("expected key: value"))?;
                    self.at += 1;
                    obj.set(&k, self.inline_or_block(key_col, &rest)?);
                }
                items.push(obj);
            } else {
                items.push(scalar(&first).ok_or_else(|| self.err("cannot parse the value"))?);
            }
            match self.lines.get(self.at) {
                Some(line) if line.indent == indent => {
                    let t = line.text.clone();
                    if let Some(rest) = t.strip_prefix("- ") {
                        first = rest.to_string();
                        continue;
                    }
                    if t == "-" {
                        first = String::new();
                        continue;
                    }
                    break;
                }
                _ => break,
            }
        }
        Ok(Json::Arr(items))
    }

    /// A `|` (literal) or `>` (folded) block scalar with the default (clip) chomping.
    fn block_scalar(&mut self, indent: usize, head: &str) -> Result<Json, String> {
        let folded = head.starts_with('>');
        let mut lines = Vec::new();
        let base = match self.lines.get(self.at) {
            Some(line) if line.indent > indent => line.indent,
            _ => return Ok(Json::Str(String::new())),
        };
        while let Some(line) = self.lines.get(self.at) {
            if line.indent <= indent {
                break;
            }
            lines.push((line.indent - base, line.text.clone()));
            self.at += 1;
        }
        let mut out = String::new();
        for (i, (extra, text)) in lines.iter().enumerate() {
            let text = format!("{}{}", " ".repeat(*extra), text);
            if folded && i > 0 && *extra == 0 && !text.trim().is_empty() {
                out.push(' ');
                out.push_str(&text);
            } else {
                if i > 0 {
                    out.push('\n');
                }
                out.push_str(&text);
            }
        }
        if !out.is_empty() {
            out.push('\n');
        }
        Ok(Json::Str(out))
    }
}

/// `key: value` on one line; the key may be quoted. Returns (key, rest).
fn split_key(t: &str) -> Option<(String, &str)> {
    if t.starts_with('[') || t.starts_with('{') || t.starts_with("- ") || t == "-" || t.starts_with('|') || t.starts_with('>') {
        return None;
    }
    if t.starts_with("? ") || t.starts_with('&') || t.starts_with('*') || t.starts_with('!') {
        return None; // complex keys, anchors, aliases and tags are refused
    }
    if t.starts_with('"') || t.starts_with('\'') {
        let quote = t.as_bytes()[0] as char;
        let bytes = t.as_bytes();
        let mut end = 1;
        while end < bytes.len() {
            if bytes[end] == b'\\' && quote == '"' {
                end += 2;
                continue;
            }
            if bytes[end] == quote as u8 {
                if quote == '\'' && end + 1 < bytes.len() && bytes[end + 1] == b'\'' {
                    end += 2;
                    continue;
                }
                break;
            }
            end += 1;
        }
        if end >= bytes.len() || bytes[end] != quote as u8 {
            return None;
        }
        let rest = t[end + 1..].strip_prefix(':')?;
        let key = if quote == '"' {
            unescape_double(&t[1..end]).ok()?
        } else {
            t[1..end].replace("''", "'")
        };
        return Some((key, rest.strip_prefix(' ').unwrap_or(rest)));
    }
    if let Some(pos) = t.find(": ") {
        if pos > 0 {
            return Some((t[..pos].to_string(), &t[pos + 2..]));
        }
    }
    if t.ends_with(':') && t.len() > 1 {
        let k = t[..t.len() - 1].to_string();
        if !k.contains(' ') || k.starts_with('\'') {
            return Some((k, ""));
        }
    }
    None
}

/// A flow-style (JSON-like) value.
struct Chars<'a> {
    s: &'a str,
    at: usize,
}

impl Chars<'_> {
    fn ws(&mut self) {
        while self.s[self.at..].starts_with(' ') {
            self.at += 1;
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.ws();
        let b = self.s.as_bytes();
        if self.at >= b.len() {
            return Err("YAML error: flow value ends early".into());
        }
        match b[self.at] {
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                loop {
                    self.ws();
                    if self.at < b.len() && b[self.at] == b']' {
                        self.at += 1;
                        break;
                    }
                    items.push(self.value()?);
                    self.ws();
                    if self.at < b.len() && b[self.at] == b',' {
                        self.at += 1;
                    }
                }
                Ok(Json::Arr(items))
            }
            b'{' => {
                self.at += 1;
                let mut obj = Json::obj();
                loop {
                    self.ws();
                    if self.at < b.len() && b[self.at] == b'}' {
                        self.at += 1;
                        break;
                    }
                    let k = match self.value()? {
                        Json::Str(s) => s,
                        Json::Num(n) => format!("{n}"),
                        _ => return Err("YAML error: expected a key in flow mapping".into()),
                    };
                    self.ws();
                    if self.at >= b.len() || b[self.at] != b':' {
                        return Err("YAML error: expected ':' in flow mapping".into());
                    }
                    self.at += 1;
                    obj.set(&k, self.value()?);
                    self.ws();
                    if self.at < b.len() && b[self.at] == b',' {
                        self.at += 1;
                    }
                }
                Ok(obj)
            }
            b'"' | b'\'' => {
                let quote = b[self.at] as char;
                self.at += 1;
                let start = self.at;
                while self.at < b.len() {
                    if b[self.at] == b'\\' && quote == '"' {
                        self.at += 2;
                        continue;
                    }
                    if b[self.at] == quote as u8 {
                        break;
                    }
                    self.at += 1;
                }
                if self.at >= b.len() {
                    return Err("YAML error: quoted string does not end".into());
                }
                let inner = &self.s[start..self.at];
                self.at += 1;
                Ok(Json::Str(if quote == '"' { unescape_double(inner)? } else { inner.replace("''", "'") }))
            }
            _ => {
                let start = self.at;
                while self.at < b.len() && !matches!(b[self.at], b',' | b']' | b'}' | b':') {
                    self.at += 1;
                }
                let t = self.s[start..self.at].trim_end();
                scalar(t).ok_or_else(|| "YAML error: cannot parse the scalar".to_string())
            }
        }
    }
}

/// A plain or quoted scalar, as a JSON value. Refuses YAML features outside the subset.
fn scalar(t: &str) -> Option<Json> {
    let t = t.trim();
    if t.starts_with('&') || t.starts_with('*') || t.starts_with('!') || t.starts_with('%') {
        return None; // anchors, aliases, tags and directives
    }
    if t.starts_with('"') {
        if !t.ends_with('"') || t.len() < 2 {
            return None;
        }
        return Some(Json::Str(unescape_double(&t[1..t.len() - 1]).ok()?));
    }
    if t.starts_with('\'') {
        if !t.ends_with('\'') || t.len() < 2 {
            return None;
        }
        return Some(Json::Str(t[1..t.len() - 1].replace("''", "'")));
    }
    if t.is_empty() || t == "null" || t == "~" {
        return Some(Json::Null);
    }
    if t == "true" {
        return Some(Json::Bool(true));
    }
    if t == "false" {
        return Some(Json::Bool(false));
    }
    let c0 = t.as_bytes()[0];
    if (c0.is_ascii_digit() || c0 == b'-') && t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) {
        if let Ok(n) = t.parse::<f64>() {
            if n.is_finite() {
                return Some(Json::Num(n));
            }
        }
    }
    Some(Json::Str(t.to_string()))
}

fn unescape_double(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                let cp = u32::from_str_radix(&hex, 16).map_err(|_| "YAML error: bad \\u escape".to_string())?;
                out.push(char::from_u32(cp).ok_or("YAML error: bad \\u escape")?);
            }
            _ => return Err("YAML error: bad escape".into()),
        }
    }
    Ok(out)
}

/// JSON-compatible YAML (flow style): what the writer emits. Quoted strings carry JSON
/// escapes, which are also valid YAML double-quoted escapes.
pub fn write(v: &Json) -> String {
    let mut out = String::new();
    write_at(v, &mut out);
    out.push('\n');
    out
}

fn write_at(v: &Json, out: &mut String) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Num(n) => out.push_str(&n.to_string()),
        Json::Str(s) => {
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\t' => out.push_str("\\t"),
                    '\r' => out.push_str("\\r"),
                    c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        Json::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_at(item, out);
            }
            out.push(']');
        }
        Json::Obj(fields) => {
            out.push('{');
            for (i, (k, val)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_at(&Json::Str(k.clone()), out);
                out.push_str(": ");
                write_at(val, out);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_block_and_flow() {
        let doc = "
# a YAML-LD document
---
\"@context\":
  ex: http://example.org/
\"@id\": ex:book/1
name: Safahat
authors:
  - ex:person/1
  - ex:person/2
meta: {pages: 200, tags: [roman, siir]}
";
        let v = parse(doc).unwrap();
        assert_eq!(v.str_at(&["@id"]), "ex:book/1");
        assert_eq!(v.str_at(&["name"]), "Safahat");
        assert_eq!(v.get("authors").unwrap().as_array().unwrap().len(), 2);
        let tags = v.get("meta").unwrap().get("tags").unwrap().as_array().unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].str_at(&[]), "roman");
    }

    #[test]
    fn numbers_bools_null_and_quotes() {
        let v = parse("a: 42\nb: 3.5\nc: true\nd: null\ne: 'it''s'\nf: \"x\\ny\"\n").unwrap();
        assert!(matches!(v.get("a"), Some(Json::Num(n)) if *n == 42.0));
        assert!(matches!(v.get("b"), Some(Json::Num(n)) if *n == 3.5));
        assert!(matches!(v.get("c"), Some(Json::Bool(true))));
        assert_eq!(v.str_at(&["e"]), "it's");
        assert_eq!(v.str_at(&["f"]), "x\ny");
    }

    #[test]
    fn block_scalars() {
        let v = parse("text: |\n  satir bir\n  satir iki\nfolded: >\n  bir\n  iki\n").unwrap();
        assert_eq!(v.str_at(&["text"]), "satir bir\nsatir iki\n");
        assert_eq!(v.str_at(&["folded"]), "bir iki\n");
    }

    #[test]
    fn refuses_anchors_and_tabs() {
        assert!(parse("a: &x 1\nb: *x").is_err());
        assert!(parse("a:\n\tb: 1").is_err());
    }

    #[test]
    fn sequence_of_mappings_on_dash_lines() {
        let v = parse("list:\n  - id: a\n    v: 1\n  - id: b\n    v: 2\n").unwrap();
        let arr = v.get("list").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[1].str_at(&["id"]), "b");
    }

    #[test]
    fn write_round_trips() {
        let doc = parse("a: 1\nb: [x, y]\nc: {d: \"e\"}\n").unwrap();
        let text = write(&doc);
        assert_eq!(parse(&text).unwrap(), doc);
    }
}
