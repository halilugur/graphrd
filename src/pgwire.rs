//! SQL over the PostgreSQL wire protocol (v3), so psql, JDBC, psycopg and
//! BI tools can query a repository. The database name selects the
//! repository; tables are classes and columns are `id` plus predicates
//! (local names), as in the derived GraphQL schema.
//!
//! Full SQL engine supporting:
//! - CTEs (`WITH ... AS (...)`), Subqueries (`FROM (SELECT ...)`, `IN (SELECT ...)`, `EXISTS`, scalar subqueries)
//! - Joins (`INNER`, `LEFT`, `RIGHT`, `FULL OUTER`, `CROSS`, `ON <expr>`, `USING (...)`, comma joins)
//! - Aggregations (`GROUP BY`, `HAVING`, `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `STRING_AGG`, `ARRAY_AGG`)
//! - Window Functions (`ROW_NUMBER`, `RANK`, `DENSE_RANK`, `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, `OVER (...)`)
//! - Set Operations (`UNION [ALL]`, `INTERSECT [ALL]`, `EXCEPT [ALL]`)
//! - Expressions (`CASE WHEN`, `CAST` / `::`, `COALESCE`, `NULLIF`, `BETWEEN`, `LIKE`/`ILIKE`, arithmetic, string/math funcs)
//! - DML (`INSERT INTO ... VALUES / SELECT ... [RETURNING]`, `UPDATE ... [RETURNING]`, `DELETE ... [RETURNING]`)
//! - Views (`CREATE [OR REPLACE] VIEW`, `DROP VIEW [IF EXISTS]`), `EXPLAIN`, `DESCRIBE`, `SHOW`, `pg_catalog` & `information_schema`.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use crate::graphql::{Schema, Value as GqlValue};
use crate::rdf::{RDF_TYPE, Term};
use crate::sparql::{self, QueryOptions, eval::QueryResult, func};
use crate::store::{Id, Store};

/// A result table: column names, then a row of optional strings per tuple.
pub type Rows = (Vec<String>, Vec<Vec<Option<String>>>);

/// What the protocol layer needs from the server.
pub trait Backend: Send + Sync + 'static {
    /// Err(message) when the user may not read the database.
    fn authenticate(
        &self,
        user: &str,
        password: Option<&str>,
        database: &str,
    ) -> Result<(), String>;
    /// Whether the client must authenticate.
    fn auth_required(&self) -> bool;
    /// Run `f` against the repository's store.
    fn with_store(
        &self,
        database: &str,
        f: &mut dyn FnMut(&Store) -> Result<Rows, String>,
    ) -> Result<Rows, String>;
    /// Run `f` as one write transaction as `user`: Err when the user may not
    /// write, or when `f` fails (then nothing it changed stays).
    fn with_store_mut(
        &self,
        user: &str,
        database: &str,
        f: &mut dyn FnMut(&mut Store) -> Result<Rows, String>,
    ) -> Result<Rows, String>;
}

/// Answer PostgreSQL protocol connections on `addr`.
pub fn serve(addr: &str, backend: Arc<dyn Backend>) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let backend = backend.clone();
        crate::http::spawn(move || {
            let _ = session(stream, backend);
        });
    }
    Ok(())
}

// ------------------------------------------------------------------ protocol

fn msg(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    out.push(kind);
    out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(body);
}

fn cstr(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}

fn error(out: &mut Vec<u8>, code: &str, message: &str) {
    let mut b = Vec::new();
    for (k, v) in [
        (b'S', "ERROR"),
        (b'V', "ERROR"),
        (b'C', code),
        (b'M', message),
    ] {
        b.push(k);
        cstr(&mut b, v);
    }
    b.push(0);
    msg(out, b'E', &b);
}

fn row_description(out: &mut Vec<u8>, cols: &[String]) {
    let mut b = (cols.len() as i16).to_be_bytes().to_vec();
    for c in cols {
        cstr(&mut b, c);
        b.extend_from_slice(&0i32.to_be_bytes()); // table oid
        b.extend_from_slice(&0i16.to_be_bytes()); // column number
        b.extend_from_slice(&25i32.to_be_bytes()); // type: text
        b.extend_from_slice(&(-1i16).to_be_bytes());
        b.extend_from_slice(&(-1i32).to_be_bytes());
        b.extend_from_slice(&0i16.to_be_bytes()); // text format
    }
    msg(out, b'T', &b);
}

fn data_rows(out: &mut Vec<u8>, rows: &[Vec<Option<String>>]) {
    for row in rows {
        let mut b = (row.len() as i16).to_be_bytes().to_vec();
        for cell in row {
            match cell {
                None => b.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(v) => {
                    b.extend_from_slice(&(v.len() as i32).to_be_bytes());
                    b.extend_from_slice(v.as_bytes());
                }
            }
        }
        msg(out, b'D', &b);
    }
}

fn read_i32(r: &mut impl Read) -> io::Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_be_bytes(b))
}

fn read_body(r: &mut impl Read, len: i32) -> io::Result<Vec<u8>> {
    if !(4..=64 * 1024 * 1024).contains(&len) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad message length",
        ));
    }
    let mut b = vec![0u8; len as usize - 4];
    r.read_exact(&mut b)?;
    Ok(b)
}

fn split_cstrs(b: &[u8]) -> Vec<String> {
    b.split(|&c| c == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

struct Portal {
    result: Result<Rows, String>,
    tag: String,
}

fn session(stream: TcpStream, backend: Arc<dyn Backend>) -> io::Result<()> {
    stream.set_nodelay(true).ok();
    let mut w = stream.try_clone()?;
    let mut r = BufReader::new(stream);
    // startup (possibly preceded by an SSL or GSS request we decline)
    let params = loop {
        let len = read_i32(&mut r)?;
        let body = read_body(&mut r, len)?;
        let code = i32::from_be_bytes(body[..4].try_into().unwrap_or([0; 4]));
        match code {
            80877103 | 80877104 => w.write_all(b"N")?, // no SSL / GSSAPI
            80877102 => return Ok(()),                 // cancel request
            196608 => {
                let parts = split_cstrs(&body[4..]);
                let mut map = HashMap::new();
                for kv in parts.chunks(2) {
                    if kv.len() == 2 && !kv[0].is_empty() {
                        map.insert(kv[0].clone(), kv[1].clone());
                    }
                }
                break map;
            }
            _ => {
                let mut out = Vec::new();
                error(&mut out, "08P01", "unsupported protocol version");
                w.write_all(&out)?;
                return Ok(());
            }
        }
    };
    let user = params.get("user").cloned().unwrap_or_default();
    let database = params
        .get("database")
        .cloned()
        .unwrap_or_else(|| user.clone());
    let mut out = Vec::new();
    let password = if backend.auth_required() {
        msg(&mut out, b'R', &3i32.to_be_bytes()); // cleartext password
        w.write_all(&out)?;
        out.clear();
        let mut kind = [0u8; 1];
        r.read_exact(&mut kind)?;
        let len = read_i32(&mut r)?;
        let body = read_body(&mut r, len)?;
        if kind[0] != b'p' {
            return Ok(());
        }
        Some(split_cstrs(&body).into_iter().next().unwrap_or_default())
    } else {
        None
    };
    if let Err(e) = backend.authenticate(&user, password.as_deref(), &database) {
        error(&mut out, "28P01", &e);
        w.write_all(&out)?;
        return Ok(());
    }
    msg(&mut out, b'R', &0i32.to_be_bytes());
    for (k, v) in [
        ("server_version", "15.0 (GraphRD)"),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("DateStyle", "ISO, MDY"),
        ("integer_datetimes", "on"),
        ("standard_conforming_strings", "on"),
        ("TimeZone", "UTC"),
    ] {
        let mut b = Vec::new();
        cstr(&mut b, k);
        cstr(&mut b, v);
        msg(&mut out, b'S', &b);
    }
    let mut key = std::process::id().to_be_bytes().to_vec();
    key.extend_from_slice(&7i32.to_be_bytes());
    msg(&mut out, b'K', &key);
    msg(&mut out, b'Z', b"I");
    w.write_all(&out)?;

    // statement name -> (sql, parameter type oids)
    let mut statements: HashMap<String, (String, Vec<i32>)> = HashMap::new();
    let mut portals: HashMap<String, Portal> = HashMap::new();
    let mut failed = false; // skip to Sync after an extended-protocol error
    loop {
        let mut kind = [0u8; 1];
        if r.read_exact(&mut kind).is_err() {
            return Ok(());
        }
        let len = read_i32(&mut r)?;
        let body = read_body(&mut r, len)?;
        let mut out = Vec::new();
        match kind[0] {
            b'X' => return Ok(()),
            b'Q' => {
                let sql = split_cstrs(&body).into_iter().next().unwrap_or_default();
                let stmts: Vec<&str> = split_statements(&sql);
                if stmts.is_empty() {
                    msg(&mut out, b'I', &[]);
                }
                for stmt in stmts {
                    match run(&*backend, &user, &database, stmt) {
                        Ok((rows, tag)) => {
                            if !rows.0.is_empty() {
                                row_description(&mut out, &rows.0);
                                data_rows(&mut out, &rows.1);
                            }
                            let mut b = Vec::new();
                            cstr(&mut b, &tag);
                            msg(&mut out, b'C', &b);
                        }
                        Err(e) => {
                            error(&mut out, "42601", &e);
                            break;
                        }
                    }
                }
                msg(&mut out, b'Z', b"I");
            }
            b'P' if !failed => {
                let name_end = body.iter().position(|&c| c == 0).unwrap_or(0);
                let sql_end = body[name_end + 1..]
                    .iter()
                    .position(|&c| c == 0)
                    .map_or(body.len(), |p| name_end + 1 + p);
                let name = String::from_utf8_lossy(&body[..name_end]).into_owned();
                let sql = String::from_utf8_lossy(&body[name_end + 1..sql_end]).into_owned();
                let mut i = sql_end + 1;
                let n = i16::from_be_bytes([
                    *body.get(i).unwrap_or(&0),
                    *body.get(i + 1).unwrap_or(&0),
                ])
                .max(0) as usize;
                i += 2;
                let oids = (0..n)
                    .map(|k| {
                        i32::from_be_bytes(
                            body.get(i + k * 4..i + k * 4 + 4)
                                .and_then(|b| b.try_into().ok())
                                .unwrap_or([0; 4]),
                        )
                    })
                    .collect();
                statements.insert(name, (sql, oids));
                msg(&mut out, b'1', &[]);
            }
            b'B' if !failed => {
                let mut i = 0;
                let take_cstr = |i: &mut usize| {
                    let end = body[*i..]
                        .iter()
                        .position(|&c| c == 0)
                        .map_or(body.len(), |p| *i + p);
                    let s = String::from_utf8_lossy(&body[*i..end]).into_owned();
                    *i = end + 1;
                    s
                };
                let portal = take_cstr(&mut i);
                let stmt = take_cstr(&mut i);
                let rd16 = |i: &mut usize| {
                    let v = i16::from_be_bytes([
                        *body.get(*i).unwrap_or(&0),
                        *body.get(*i + 1).unwrap_or(&0),
                    ]);
                    *i += 2;
                    v
                };
                let nfmt = rd16(&mut i).max(0) as usize;
                let formats: Vec<i16> = (0..nfmt).map(|_| rd16(&mut i)).collect();
                let nparams = rd16(&mut i);
                let (sql_text, oids) = statements.get(&stmt).cloned().unwrap_or_default();
                let mut values = Vec::new();
                for k in 0..nparams.max(0) as usize {
                    let l = i32::from_be_bytes(
                        body.get(i..i + 4)
                            .and_then(|b| b.try_into().ok())
                            .unwrap_or([0xff; 4]),
                    );
                    i += 4;
                    if l < 0 {
                        values.push(None);
                        continue;
                    }
                    let raw = &body[i..(i + l as usize).min(body.len())];
                    i += l as usize;
                    let binary = match formats.len() {
                        0 => false,
                        1 => formats[0] == 1,
                        _ => formats.get(k) == Some(&1),
                    };
                    values.push(Some(if binary {
                        decode_binary(raw, oids.get(k).copied().unwrap_or(0))
                    } else {
                        String::from_utf8_lossy(raw).into_owned()
                    }));
                }
                let sql = substitute(&sql_text, &values);
                let result = if sql.trim().is_empty() {
                    Ok(((Vec::new(), Vec::new()), String::new()))
                } else {
                    run(&*backend, &user, &database, &sql)
                };
                let tag = result.as_ref().map(|(_, t)| t.clone()).unwrap_or_default();
                portals.insert(
                    portal,
                    Portal {
                        result: result.map(|(rows, _)| rows),
                        tag,
                    },
                );
                msg(&mut out, b'2', &[]);
            }
            b'D' if !failed => {
                let target =
                    String::from_utf8_lossy(&body[1..body.len().saturating_sub(1)]).into_owned();
                if body.first() == Some(&b'S') {
                    msg(&mut out, b't', &0i16.to_be_bytes());
                    let _ = target;
                    msg(&mut out, b'n', &[]);
                } else {
                    match portals.get(&target).map(|p| &p.result) {
                        Some(Ok(rows)) if !rows.0.is_empty() => row_description(&mut out, &rows.0),
                        Some(Err(e)) => {
                            error(&mut out, "42601", e);
                            failed = true;
                        }
                        _ => msg(&mut out, b'n', &[]),
                    }
                }
            }
            b'E' if !failed => {
                let name = split_cstrs(&body).into_iter().next().unwrap_or_default();
                match portals.remove(&name) {
                    Some(Portal {
                        result: Ok(rows),
                        tag,
                    }) => {
                        if tag.is_empty() && rows.0.is_empty() {
                            msg(&mut out, b'I', &[]);
                        } else {
                            data_rows(&mut out, &rows.1);
                            let mut b = Vec::new();
                            cstr(&mut b, &tag);
                            msg(&mut out, b'C', &b);
                        }
                    }
                    Some(Portal { result: Err(e), .. }) => {
                        error(&mut out, "42601", &e);
                        failed = true;
                    }
                    None => msg(&mut out, b'I', &[]),
                }
            }
            b'C' => msg(&mut out, b'3', &[]),
            b'H' => {}
            b'S' => {
                failed = false;
                msg(&mut out, b'Z', b"I");
            }
            b'P' | b'B' | b'D' | b'E' => {}
            other => {
                error(
                    &mut out,
                    "08P01",
                    &format!("unsupported message '{}'", other as char),
                );
                msg(&mut out, b'Z', b"I");
            }
        }
        w.write_all(&out)?;
    }
}

/// Split on semicolons outside quotes and comments.
fn split_statements(sql: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = sql.as_bytes();
    let mut start = 0;
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
        } else if in_double {
            if b == b'"' {
                if bytes.get(i + 1) == Some(&b'"') {
                    i += 2;
                    continue;
                }
                in_double = false;
            }
            i += 1;
        } else if b == b'\'' {
            in_single = true;
            i += 1;
        } else if b == b'"' {
            in_double = true;
            i += 1;
        } else if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else if b == b';' {
            let s = sql[start..i].trim();
            if !s.is_empty() && !is_only_comments(s) {
                out.push(s);
            }
            start = i + 1;
            i += 1;
        } else {
            i += 1;
        }
    }
    let tail = sql[start..].trim();
    if !tail.is_empty() && !is_only_comments(tail) {
        out.push(tail);
    }
    out
}

fn is_only_comments(sql: &str) -> bool {
    lex(sql).map(|t| t.is_empty()).unwrap_or(false)
}

/// Text form of a binary-format parameter, by type oid (or size).
fn decode_binary(raw: &[u8], oid: i32) -> String {
    match (oid, raw.len()) {
        (16, 1) => (raw[0] != 0).to_string(),
        (700, 4) => f32::from_be_bytes(raw.try_into().unwrap()).to_string(),
        (701, 8) => f64::from_be_bytes(raw.try_into().unwrap()).to_string(),
        (21, 2) | (0, 2) => i16::from_be_bytes(raw.try_into().unwrap()).to_string(),
        (23, 4) | (0, 4) => i32::from_be_bytes(raw.try_into().unwrap()).to_string(),
        (20, 8) | (0, 8) => i64::from_be_bytes(raw.try_into().unwrap()).to_string(),
        _ => String::from_utf8_lossy(raw).into_owned(),
    }
}

/// Replace $1, $2 ... with quoted literals (numbers stay bare), skipping string literals.
fn substitute(sql: &str, values: &[Option<String>]) -> String {
    let mut out = String::with_capacity(sql.len());
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < chars.len() {
        let ch = chars[i];
        if in_single {
            out.push(ch);
            if ch == '\'' {
                if chars.get(i + 1) == Some(&'\'') {
                    out.push('\'');
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
        } else if in_double {
            out.push(ch);
            if ch == '"' {
                in_double = false;
            }
            i += 1;
        } else if ch == '\'' {
            in_single = true;
            out.push(ch);
            i += 1;
        } else if ch == '"' {
            in_double = true;
            out.push(ch);
            i += 1;
        } else if ch == '$' && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && chars[end].is_ascii_digit() {
                end += 1;
            }
            let num_str: String = chars[start..end].iter().collect();
            if let Ok(idx) = num_str.parse::<usize>() {
                if idx >= 1 && idx <= values.len() {
                    let lit = match &values[idx - 1] {
                        None => "NULL".to_string(),
                        Some(s) if s.parse::<f64>().is_ok() && !s.trim().is_empty() => s.clone(),
                        Some(s) => format!("'{}'", s.replace('\'', "''")),
                    };
                    out.push_str(&lit);
                    i = end;
                    continue;
                }
            }
            out.push(ch);
            i += 1;
        } else {
            out.push(ch);
            i += 1;
        }
    }
    out
}

// ------------------------------------------------------------------ Lexer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Num(String),
    Sym(String),
}

fn lex(sql: &str) -> Result<Vec<Tok>, String> {
    let c: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '-' && c.get(i + 1) == Some(&'-') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if ch == '/' && c.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < c.len() && !(c[i] == '*' && c[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(c.len());
        } else if (ch == 'E' || ch == 'e') && c.get(i + 1) == Some(&'\'') {
            i += 2;
            let mut s = String::new();
            loop {
                match c.get(i) {
                    None => return Err("unterminated string".into()),
                    Some('\\') => {
                        i += 1;
                        match c.get(i) {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some('r') => s.push('\r'),
                            Some(&x) => s.push(x),
                            None => return Err("unterminated escape".into()),
                        }
                        i += 1;
                    }
                    Some('\'') if c.get(i + 1) == Some(&'\'') => {
                        s.push('\'');
                        i += 2;
                    }
                    Some('\'') => {
                        i += 1;
                        break;
                    }
                    Some(&x) => {
                        s.push(x);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Str(s));
        } else if ch == '$'
            && (c.get(i + 1) == Some(&'$')
                || c.get(i + 1).is_some_and(|x| x.is_alphabetic() || *x == '_'))
        {
            // Dollar-quoted string: $$...$$ or $tag$...$tag$
            let mut tag_end = i + 1;
            while tag_end < c.len() && (c[tag_end].is_alphanumeric() || c[tag_end] == '_') {
                tag_end += 1;
            }
            if c.get(tag_end) == Some(&'$') {
                let tag: String = c[i..=tag_end].iter().collect();
                let tag_chars: Vec<char> = tag.chars().collect();
                i = tag_end + 1;
                let content_start = i;
                let mut found = false;
                while i + tag_chars.len() <= c.len() {
                    if c[i..i + tag_chars.len()] == tag_chars[..] {
                        let s: String = c[content_start..i].iter().collect();
                        out.push(Tok::Str(s));
                        i += tag_chars.len();
                        found = true;
                        break;
                    }
                    i += 1;
                }
                if !found {
                    return Err("unterminated dollar-quoted string".into());
                }
            } else {
                out.push(Tok::Sym("$".into()));
                i += 1;
            }
        } else if ch == '\'' {
            let mut s = String::new();
            i += 1;
            loop {
                match c.get(i) {
                    None => return Err("unterminated string".into()),
                    Some('\'') if c.get(i + 1) == Some(&'\'') => {
                        s.push('\'');
                        i += 2;
                    }
                    Some('\'') => {
                        i += 1;
                        break;
                    }
                    Some(&x) => {
                        s.push(x);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Str(s));
        } else if ch == '"' {
            let mut s = String::new();
            i += 1;
            loop {
                match c.get(i) {
                    None => break,
                    Some('"') if c.get(i + 1) == Some(&'"') => {
                        s.push('"');
                        i += 2;
                    }
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some(&x) => {
                        s.push(x);
                        i += 1;
                    }
                }
            }
            if c.get(i) == Some(&'.')
                && c.get(i + 1)
                    .is_some_and(|x| x.is_alphanumeric() || *x == '_' || *x == '"' || *x == '*')
            {
                s.push('.');
                i += 1;
                if c.get(i) == Some(&'"') {
                    i += 1;
                    while i < c.len() && c[i] != '"' {
                        s.push(c[i]);
                        i += 1;
                    }
                    if i < c.len() {
                        i += 1;
                    }
                } else {
                    while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '*') {
                        s.push(c[i]);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Word(s));
        } else if ch == '`' {
            let start = i + 1;
            i += 1;
            while i < c.len() && c[i] != '`' {
                i += 1;
            }
            out.push(Tok::Word(c[start..i].iter().collect()));
            if i < c.len() {
                i += 1;
            }
        } else if ch.is_ascii_digit()
            || (ch == '-'
                && c.get(i + 1).is_some_and(|d| d.is_ascii_digit())
                && !matches!(out.last(), Some(Tok::Num(_) | Tok::Word(_) | Tok::Str(_)))
                && out.last() != Some(&Tok::Sym(")".into())))
        {
            let start = i;
            i += 1;
            while i < c.len() && (c[i].is_ascii_digit() || c[i] == '.') {
                i += 1;
            }
            if i < c.len() && (c[i] == 'e' || c[i] == 'E') {
                i += 1;
                if i < c.len() && (c[i] == '+' || c[i] == '-') {
                    i += 1;
                }
                while i < c.len() && c[i].is_ascii_digit() {
                    i += 1;
                }
            }
            out.push(Tok::Num(c[start..i].iter().collect()));
        } else if ch.is_alphanumeric() || ch == '_' {
            let start = i;
            let mut s = String::new();
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '.') {
                s.push(c[i]);
                i += 1;
                if c.get(i - 1) == Some(&'.') && c.get(i) == Some(&'*') {
                    s.push('*');
                    i += 1;
                    break;
                }
                if c.get(i - 1) == Some(&'.') && c.get(i) == Some(&'"') {
                    i += 1;
                    while i < c.len() && c[i] != '"' {
                        s.push(c[i]);
                        i += 1;
                    }
                    if i < c.len() {
                        i += 1;
                    }
                }
            }
            out.push(Tok::Word(if s.is_empty() {
                c[start..i].iter().collect()
            } else {
                s
            }));
        } else {
            let two: String = c[i..(i + 2).min(c.len())].iter().collect();
            if ["<>", "!=", "<=", ">=", "::", "||", "!~", "~*"].contains(&two.as_str()) {
                out.push(Tok::Sym(two));
                i += 2;
            } else {
                out.push(Tok::Sym(ch.to_string()));
                i += 1;
            }
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ AST & Value Types

#[derive(Debug, Clone)]
enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Multi(Vec<Val>),
}

impl Val {
    fn is_null(&self) -> bool {
        match self {
            Val::Null => true,
            Val::Multi(v) => v.is_empty() || v.iter().all(|x| x.is_null()),
            _ => false,
        }
    }

    fn as_text(&self) -> Option<String> {
        match self {
            Val::Null => None,
            Val::Bool(b) => Some(b.to_string()),
            Val::Int(i) => Some(i.to_string()),
            Val::Float(f) => {
                if f.fract() == 0.0 && f.abs() < 1e15 {
                    Some(format!("{f:.1}"))
                } else {
                    Some(f.to_string())
                }
            }
            Val::Str(s) => Some(s.clone()),
            Val::Multi(list) => {
                let parts: Vec<String> = list.iter().filter_map(|v| v.as_text()).collect();
                if parts.is_empty() {
                    None
                } else {
                    Some(parts.join(", "))
                }
            }
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Val::Int(i) => Some(*i as f64),
            Val::Float(f) => Some(*f),
            Val::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Val::Str(s) => s.trim().parse::<f64>().ok(),
            Val::Multi(v) => v.first().and_then(|x| x.as_f64()),
            Val::Null => None,
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            Val::Int(i) => Some(*i),
            Val::Float(f) => Some(*f as i64),
            Val::Bool(b) => Some(if *b { 1 } else { 0 }),
            Val::Str(s) => s
                .trim()
                .parse::<i64>()
                .ok()
                .or_else(|| s.trim().parse::<f64>().ok().map(|f| f as i64)),
            Val::Multi(v) => v.first().and_then(|x| x.as_i64()),
            Val::Null => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Val::Bool(b) => Some(*b),
            Val::Int(i) => Some(*i != 0),
            Val::Float(f) => Some(*f != 0.0),
            Val::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "1" | "yes" | "on" => Some(true),
                "false" | "f" | "0" | "no" | "off" => Some(false),
                _ => None,
            },
            Val::Multi(v) => v.first().and_then(|x| x.as_bool()),
            Val::Null => None,
        }
    }

    fn items(&self) -> Vec<Val> {
        match self {
            Val::Null => Vec::new(),
            Val::Multi(v) => v.iter().filter(|x| !x.is_null()).cloned().collect(),
            other => vec![other.clone()],
        }
    }

    fn from_str_inferred(s: &str) -> Val {
        Val::Str(s.to_string())
    }

    fn from_term(t: &Term) -> Val {
        if let Some(n) = func::num(t) {
            let f = n.f();
            if let Ok(i) = t.value().parse::<i64>() {
                return Val::Int(i);
            }
            return Val::Float(f);
        }
        Val::Str(cell(t))
    }
}

fn cmp_vals(a: &Val, b: &Val) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) {
        if let Some(o) = x.partial_cmp(&y) {
            return o;
        }
    }
    if let (Val::Bool(x), Val::Bool(y)) = (a, b) {
        return x.cmp(y);
    }
    a.as_text()
        .unwrap_or_default()
        .cmp(&b.as_text().unwrap_or_default())
}

#[derive(Debug, Clone)]
enum Expr {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Col(String),
    Wildcard(Option<String>),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        not: bool,
    },
    IsBool {
        expr: Box<Expr>,
        val: bool,
        not: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        not: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        not: bool,
    },
    InSubquery {
        expr: Box<Expr>,
        query: Box<Query>,
        not: bool,
    },
    Exists {
        query: Box<Query>,
        not: bool,
    },
    Subquery(Box<Query>),
    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        else_expr: Option<Box<Expr>>,
    },
    Cast {
        expr: Box<Expr>,
        ty: String,
    },
    Func {
        name: String,
        args: Vec<Expr>,
        distinct: bool,
        filter: Option<Box<Expr>>,
        over: Option<WindowSpec>,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum UnaryOp {
    Not,
    Neg,
    Pos,
}

#[derive(Debug, Clone, PartialEq)]
enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Like,
    NotLike,
    ILike,
    NotILike,
    Regex,
    IRegex,
    NotRegex,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Concat,
}

#[derive(Debug, Clone)]
struct WindowSpec {
    partition_by: Vec<Expr>,
    order_by: Vec<OrderByItem>,
}

#[derive(Debug, Clone)]
struct OrderByItem {
    expr: Expr,
    desc: bool,
    nulls_first: Option<bool>,
}

#[derive(Debug, Clone)]
struct SelectItem {
    expr: Expr,
    alias: Option<String>,
    raw_name: String,
}

#[derive(Debug, Clone)]
enum TableFactor {
    Table {
        name: String,
        alias: String,
    },
    Subquery {
        query: Box<Query>,
        alias: String,
    },
    Sparql {
        query: String,
        alias: String,
    },
    Func {
        name: String,
        args: Vec<Expr>,
        alias: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone)]
enum JoinCond {
    On(Expr),
    Using(Vec<String>),
    None,
}

#[derive(Debug, Clone)]
struct JoinAst {
    kind: JoinKind,
    factor: TableFactor,
    cond: JoinCond,
}

#[derive(Debug, Clone, PartialEq)]
enum SetOp {
    Union,
    Intersect,
    Except,
}

#[derive(Debug, Clone)]
struct Cte {
    name: String,
    cols: Vec<String>,
    query: Query,
}

#[derive(Debug, Clone)]
struct Query {
    ctes: Vec<Cte>,
    body: QueryBody,
    order_by: Vec<OrderByItem>,
    limit: Option<Expr>,
    offset: Option<Expr>,
}

#[derive(Debug, Clone)]
enum QueryBody {
    Select(Box<SelectAst>),
    Values(Vec<Vec<Expr>>),
    SetOp {
        left: Box<QueryBody>,
        op: SetOp,
        all: bool,
        right: Box<QueryBody>,
    },
}

#[derive(Debug, Clone)]
struct SelectAst {
    distinct: bool,
    distinct_on: Vec<Expr>,
    projection: Vec<SelectItem>,
    from: Option<TableFactor>,
    joins: Vec<JoinAst>,
    where_clause: Option<Expr>,
    group_by: Vec<Expr>,
    having: Option<Expr>,
}

// ------------------------------------------------------------------ Parser

fn kw(t: Option<&Tok>, w: &str) -> bool {
    matches!(t, Some(Tok::Word(x)) if x.eq_ignore_ascii_case(w))
}

fn sym(t: Option<&Tok>, s: &str) -> bool {
    matches!(t, Some(Tok::Sym(x)) if x == s)
}

/// The column part of a possibly qualified name (`p.name` → `name`).
fn unqualified(c: &str) -> &str {
    c.rsplit('.').next().unwrap_or(c)
}

const RESERVED_CLAUSES: &[&str] = &[
    "where",
    "order",
    "limit",
    "offset",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "natural",
    "on",
    "using",
    "group",
    "having",
    "set",
    "union",
    "intersect",
    "except",
    "window",
    "fetch",
    "for",
    "returning",
    "values",
    "into",
    "from",
];

fn is_reserved_clause(w: &str) -> bool {
    RESERVED_CLAUSES.iter().any(|c| w.eq_ignore_ascii_case(c))
}

struct Parser<'a> {
    toks: &'a [Tok],
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(toks: &'a [Tok]) -> Self {
        Self { toks, i: 0 }
    }

    fn peek(&self) -> Option<&'a Tok> {
        self.toks.get(self.i)
    }

    fn peek_n(&self, n: usize) -> Option<&'a Tok> {
        self.toks.get(self.i + n)
    }

    fn next(&mut self) -> Option<&'a Tok> {
        let t = self.toks.get(self.i);
        if t.is_some() {
            self.i += 1;
        }
        t
    }

    fn eat_kw(&mut self, w: &str) -> bool {
        if kw(self.peek(), w) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, w: &str) -> Result<(), String> {
        if self.eat_kw(w) {
            Ok(())
        } else {
            Err(format!("expected {w}"))
        }
    }

    fn eat_sym(&mut self, s: &str) -> bool {
        if sym(self.peek(), s) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect_sym(&mut self, s: &str) -> Result<(), String> {
        if self.eat_sym(s) {
            Ok(())
        } else {
            Err(format!("expected '{s}'"))
        }
    }

    fn parse_query_top(&mut self) -> Result<Query, String> {
        let q = self.parse_query()?;
        while self.eat_sym(";") {}
        if let Some(t) = self.peek() {
            return Err(format!("unsupported SQL near {t:?}"));
        }
        Ok(q)
    }

    fn parse_query(&mut self) -> Result<Query, String> {
        let mut ctes = Vec::new();
        if self.eat_kw("with") {
            let _recursive = self.eat_kw("recursive");
            loop {
                let name = match self.next() {
                    Some(Tok::Word(w)) => w.clone(),
                    _ => return Err("expected CTE name".into()),
                };
                let mut cols = Vec::new();
                if self.eat_sym("(") {
                    while !self.eat_sym(")") {
                        match self.next() {
                            Some(Tok::Word(w)) => cols.push(w.clone()),
                            _ => return Err("expected column name in CTE".into()),
                        }
                        let _ = self.eat_sym(",");
                    }
                }
                self.expect_kw("as")?;
                let _ = self.eat_kw("materialized")
                    || (self.eat_kw("not") && self.eat_kw("materialized"));
                self.expect_sym("(")?;
                let subq = self.parse_query()?;
                self.expect_sym(")")?;
                ctes.push(Cte {
                    name,
                    cols,
                    query: subq,
                });
                if !self.eat_sym(",") {
                    break;
                }
            }
        }

        let body = self.parse_set_ops()?;

        let mut order_by = Vec::new();
        if self.eat_kw("order") {
            self.expect_kw("by")?;
            order_by = self.parse_order_by_list()?;
        }

        let mut limit = None;
        let mut offset = None;
        loop {
            if self.eat_kw("limit") {
                if !self.eat_kw("all") && !self.eat_kw("null") {
                    limit = Some(self.parse_expr()?);
                }
            } else if self.eat_kw("offset") {
                offset = Some(self.parse_expr()?);
                let _ = self.eat_kw("row") || self.eat_kw("rows");
            } else if self.eat_kw("fetch") {
                let _ = self.eat_kw("first") || self.eat_kw("next");
                if !self.eat_kw("row") && !self.eat_kw("rows") {
                    limit = Some(self.parse_expr()?);
                    let _ = self.eat_kw("row") || self.eat_kw("rows");
                }
                let _ = self.eat_kw("only") || (self.eat_kw("with") && self.eat_kw("ties"));
            } else {
                break;
            }
        }

        Ok(Query {
            ctes,
            body,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_set_ops(&mut self) -> Result<QueryBody, String> {
        let mut left = self.parse_primary_query_body()?;
        loop {
            let op = if self.eat_kw("union") {
                Some(SetOp::Union)
            } else if self.eat_kw("intersect") {
                Some(SetOp::Intersect)
            } else if self.eat_kw("except") {
                Some(SetOp::Except)
            } else {
                None
            };
            let Some(op) = op else { break };
            let all = self.eat_kw("all");
            if !all {
                let _ = self.eat_kw("distinct");
            }
            let right = self.parse_primary_query_body()?;
            left = QueryBody::SetOp {
                left: Box::new(left),
                op,
                all,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_primary_query_body(&mut self) -> Result<QueryBody, String> {
        if self.eat_kw("values") {
            let mut rows = Vec::new();
            loop {
                self.expect_sym("(")?;
                let mut row = Vec::new();
                if !self.eat_sym(")") {
                    loop {
                        row.push(self.parse_expr()?);
                        if self.eat_sym(",") {
                            continue;
                        }
                        self.expect_sym(")")?;
                        break;
                    }
                }
                rows.push(row);
                if !self.eat_sym(",") {
                    break;
                }
            }
            return Ok(QueryBody::Values(rows));
        }
        if self.eat_kw("table") {
            let name = match self.next() {
                Some(Tok::Word(w)) => w.clone(),
                _ => return Err("expected table name after TABLE".into()),
            };
            return Ok(QueryBody::Select(Box::new(SelectAst {
                distinct: false,
                distinct_on: Vec::new(),
                projection: vec![SelectItem {
                    expr: Expr::Wildcard(None),
                    alias: None,
                    raw_name: "*".into(),
                }],
                from: Some(TableFactor::Table {
                    name: unqualified(&name).to_string(),
                    alias: unqualified(&name).to_string(),
                }),
                joins: Vec::new(),
                where_clause: None,
                group_by: Vec::new(),
                having: None,
            })));
        }
        Ok(QueryBody::Select(Box::new(self.parse_select_ast()?)))
    }

    fn parse_select_ast(&mut self) -> Result<SelectAst, String> {
        self.expect_kw("select")?;
        let mut distinct = false;
        let mut distinct_on = Vec::new();
        if self.eat_kw("distinct") {
            distinct = true;
            if self.eat_kw("on") {
                self.expect_sym("(")?;
                loop {
                    distinct_on.push(self.parse_expr()?);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
            }
        } else {
            let _ = self.eat_kw("all");
        }

        let mut projection = Vec::new();
        loop {
            if kw(self.peek(), "from") || self.peek().is_none() {
                break;
            }
            let start_idx = self.i;
            let expr = self.parse_expr()?;
            let end_idx = self.i;
            let raw_name = expr_default_name(&expr, &self.toks[start_idx..end_idx]);
            let mut alias = None;
            if self.eat_kw("as") {
                match self.next() {
                    Some(Tok::Word(w)) | Some(Tok::Str(w)) => alias = Some(w.clone()),
                    _ => return Err("expected a name after AS".into()),
                }
            } else if let Some(Tok::Word(w)) = self.peek() {
                if !is_reserved_clause(w) && !w.eq_ignore_ascii_case("from") && !w.contains('.') {
                    alias = Some(w.clone());
                    self.i += 1;
                }
            }
            projection.push(SelectItem {
                expr,
                alias,
                raw_name,
            });
            if !self.eat_sym(",") {
                break;
            }
        }

        let mut from = None;
        let mut joins = Vec::new();
        if self.eat_kw("from") {
            from = Some(self.parse_table_factor()?);
            loop {
                if self.eat_sym(",") {
                    let factor = self.parse_table_factor()?;
                    joins.push(JoinAst {
                        kind: JoinKind::Cross,
                        factor,
                        cond: JoinCond::None,
                    });
                    continue;
                }
                let _natural = self.eat_kw("natural");
                let kind = if self.eat_kw("left") {
                    let _ = self.eat_kw("outer");
                    if !self.eat_kw("join") {
                        return Err("expected JOIN".into());
                    }
                    Some(JoinKind::Left)
                } else if self.eat_kw("right") {
                    let _ = self.eat_kw("outer");
                    if !self.eat_kw("join") {
                        return Err("expected JOIN".into());
                    }
                    Some(JoinKind::Right)
                } else if self.eat_kw("full") {
                    let _ = self.eat_kw("outer");
                    if !self.eat_kw("join") {
                        return Err("expected JOIN".into());
                    }
                    Some(JoinKind::Full)
                } else if self.eat_kw("inner") {
                    if !self.eat_kw("join") {
                        return Err("expected JOIN".into());
                    }
                    Some(JoinKind::Inner)
                } else if self.eat_kw("cross") {
                    if !self.eat_kw("join") {
                        return Err("expected JOIN".into());
                    }
                    Some(JoinKind::Cross)
                } else if self.eat_kw("join") {
                    Some(JoinKind::Inner)
                } else {
                    None
                };

                let Some(kind) = kind else { break };
                let factor = self.parse_table_factor()?;
                let cond = if self.eat_kw("on") {
                    JoinCond::On(self.parse_expr()?)
                } else if self.eat_kw("using") {
                    self.expect_sym("(")?;
                    let mut cols = Vec::new();
                    loop {
                        match self.next() {
                            Some(Tok::Word(w)) => cols.push(w.clone()),
                            _ => return Err("expected column name in USING".into()),
                        }
                        if !self.eat_sym(",") {
                            break;
                        }
                    }
                    self.expect_sym(")")?;
                    JoinCond::Using(cols)
                } else if kind == JoinKind::Cross || _natural {
                    JoinCond::None
                } else {
                    return Err("expected ON or USING after JOIN".into());
                };
                joins.push(JoinAst { kind, factor, cond });
            }
        }

        let where_clause = if self.eat_kw("where") {
            Some(self.parse_expr()?)
        } else {
            None
        };

        let mut group_by = Vec::new();
        if self.eat_kw("group") {
            self.expect_kw("by")?;
            loop {
                group_by.push(self.parse_expr()?);
                if !self.eat_sym(",") {
                    break;
                }
            }
        }

        let having = if self.eat_kw("having") {
            Some(self.parse_expr()?)
        } else {
            None
        };

        Ok(SelectAst {
            distinct,
            distinct_on,
            projection,
            from,
            joins,
            where_clause,
            group_by,
            having,
        })
    }

    fn parse_table_factor(&mut self) -> Result<TableFactor, String> {
        let _ = self.eat_kw("lateral");
        if self.eat_sym("(") {
            let subq = self.parse_query()?;
            self.expect_sym(")")?;
            let _ = self.eat_kw("as");
            let alias = match self.peek() {
                Some(Tok::Word(w)) if !is_reserved_clause(w) => {
                    let a = w.clone();
                    self.i += 1;
                    a
                }
                _ => "subquery".to_string(),
            };
            // Optional column alias list: (SELECT ...) AS t(c1, c2)
            if self.eat_sym("(") {
                while !self.eat_sym(")") && self.peek().is_some() {
                    self.i += 1;
                }
            }
            return Ok(TableFactor::Subquery {
                query: Box::new(subq),
                alias,
            });
        }

        let raw_name = match self.next() {
            Some(Tok::Word(w)) => w.clone(),
            _ => return Err("expected a table name".into()),
        };

        if self.eat_sym("(") {
            if raw_name.eq_ignore_ascii_case("sparql") {
                let q = match self.next() {
                    Some(Tok::Str(s)) => s.clone(),
                    _ => return Err("use sparql('SELECT ...')".into()),
                };
                self.expect_sym(")")?;
                let alias = self.parse_optional_table_alias("sparql");
                return Ok(TableFactor::Sparql { query: q, alias });
            }
            let mut args = Vec::new();
            if !self.eat_sym(")") {
                loop {
                    args.push(self.parse_expr()?);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
            }
            let default_alias = unqualified(&raw_name).to_string();
            let alias = self.parse_optional_table_alias(&default_alias);
            return Ok(TableFactor::Func {
                name: raw_name,
                args,
                alias,
            });
        }

        let name = normalize_table_name(&raw_name);
        let default_alias = unqualified(&name).to_string();
        let alias = self.parse_optional_table_alias(&default_alias);
        Ok(TableFactor::Table { name, alias })
    }

    fn parse_optional_table_alias(&mut self, default: &str) -> String {
        let had_as = self.eat_kw("as");
        match self.peek() {
            Some(Tok::Word(w)) if had_as || !is_reserved_clause(w) => {
                let a = w.clone();
                self.i += 1;
                if self.eat_sym("(") {
                    while !self.eat_sym(")") && self.peek().is_some() {
                        self.i += 1;
                    }
                }
                a
            }
            _ => default.to_string(),
        }
    }

    fn parse_order_by_list(&mut self) -> Result<Vec<OrderByItem>, String> {
        let mut list = Vec::new();
        loop {
            let expr = self.parse_expr()?;
            let mut desc = false;
            if self.eat_kw("desc") {
                desc = true;
            } else {
                let _ = self.eat_kw("asc");
            }
            let mut nulls_first = None;
            if self.eat_kw("nulls") {
                if self.eat_kw("first") {
                    nulls_first = Some(true);
                } else if self.eat_kw("last") {
                    nulls_first = Some(false);
                }
            }
            list.push(OrderByItem {
                expr,
                desc,
                nulls_first,
            });
            if !self.eat_sym(",") {
                break;
            }
        }
        Ok(list)
    }

    // Precedence climbing for expressions
    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and()?;
        while self.eat_kw("or") {
            let right = self.parse_and()?;
            left = Expr::Binary {
                left: Box::new(left),
                op: BinOp::Or,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_not()?;
        while self.eat_kw("and") {
            let right = self.parse_not()?;
            left = Expr::Binary {
                left: Box::new(left),
                op: BinOp::And,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr, String> {
        if self.eat_kw("not") {
            if kw(self.peek(), "exists") {
                self.i += 1;
                self.expect_sym("(")?;
                let q = self.parse_query()?;
                self.expect_sym(")")?;
                return Ok(Expr::Exists {
                    query: Box::new(q),
                    not: true,
                });
            }
            let expr = self.parse_not()?;
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(expr),
            });
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_additive()?;
        loop {
            if self.eat_kw("is") {
                let not = self.eat_kw("not");
                if self.eat_kw("null") {
                    left = Expr::IsNull {
                        expr: Box::new(left),
                        not,
                    };
                } else if self.eat_kw("true") {
                    left = Expr::IsBool {
                        expr: Box::new(left),
                        val: true,
                        not,
                    };
                } else if self.eat_kw("false") {
                    left = Expr::IsBool {
                        expr: Box::new(left),
                        val: false,
                        not,
                    };
                } else if self.eat_kw("distinct") {
                    self.expect_kw("from")?;
                    let right = self.parse_additive()?;
                    let eq = Expr::Binary {
                        left: Box::new(left),
                        op: if not { BinOp::Eq } else { BinOp::Ne },
                        right: Box::new(right),
                    };
                    left = eq;
                } else {
                    return Err("expected NULL, TRUE, or FALSE after IS".into());
                }
                continue;
            }
            if self.eat_kw("notnull") {
                left = Expr::IsNull {
                    expr: Box::new(left),
                    not: true,
                };
                continue;
            }
            if self.eat_kw("isnull") {
                left = Expr::IsNull {
                    expr: Box::new(left),
                    not: false,
                };
                continue;
            }

            let not_prefix = if kw(self.peek(), "not")
                && matches!(self.peek_n(1), Some(Tok::Word(w)) if ["in", "between", "like", "ilike"].iter().any(|k| w.eq_ignore_ascii_case(k)))
            {
                self.i += 1;
                true
            } else {
                false
            };

            if self.eat_kw("between") {
                let low = self.parse_additive()?;
                self.expect_kw("and")?;
                let high = self.parse_additive()?;
                left = Expr::Between {
                    expr: Box::new(left),
                    low: Box::new(low),
                    high: Box::new(high),
                    not: not_prefix,
                };
                continue;
            }
            if self.eat_kw("in") {
                self.expect_sym("(")?;
                if kw(self.peek(), "select") || kw(self.peek(), "with") || kw(self.peek(), "values")
                {
                    let subq = self.parse_query()?;
                    self.expect_sym(")")?;
                    left = Expr::InSubquery {
                        expr: Box::new(left),
                        query: Box::new(subq),
                        not: not_prefix,
                    };
                } else {
                    let mut list = Vec::new();
                    if !self.eat_sym(")") {
                        loop {
                            list.push(self.parse_expr()?);
                            if !self.eat_sym(",") {
                                break;
                            }
                        }
                        self.expect_sym(")")?;
                    }
                    left = Expr::InList {
                        expr: Box::new(left),
                        list,
                        not: not_prefix,
                    };
                }
                continue;
            }
            if self.eat_kw("like") {
                let right = self.parse_additive()?;
                left = Expr::Binary {
                    left: Box::new(left),
                    op: if not_prefix {
                        BinOp::NotLike
                    } else {
                        BinOp::Like
                    },
                    right: Box::new(right),
                };
                continue;
            }
            if self.eat_kw("ilike") {
                let right = self.parse_additive()?;
                left = Expr::Binary {
                    left: Box::new(left),
                    op: if not_prefix {
                        BinOp::NotILike
                    } else {
                        BinOp::ILike
                    },
                    right: Box::new(right),
                };
                continue;
            }

            let op = match self.peek() {
                Some(Tok::Sym(s)) => match s.as_str() {
                    "=" => Some(BinOp::Eq),
                    "!=" | "<>" => Some(BinOp::Ne),
                    "<" => Some(BinOp::Lt),
                    "<=" => Some(BinOp::Le),
                    ">" => Some(BinOp::Gt),
                    ">=" => Some(BinOp::Ge),
                    "~" => Some(BinOp::Regex),
                    "~*" => Some(BinOp::IRegex),
                    "!~" => Some(BinOp::NotRegex),
                    _ => None,
                },
                _ => None,
            };
            if let Some(op) = op {
                self.i += 1;
                if (op == BinOp::Eq || op == BinOp::Ne)
                    && (self.eat_kw("any") || self.eat_kw("some"))
                {
                    self.expect_sym("(")?;
                    if kw(self.peek(), "select") || kw(self.peek(), "with") {
                        let subq = self.parse_query()?;
                        self.expect_sym(")")?;
                        left = Expr::InSubquery {
                            expr: Box::new(left),
                            query: Box::new(subq),
                            not: op == BinOp::Ne,
                        };
                        continue;
                    } else {
                        let inner = self.parse_expr()?;
                        self.expect_sym(")")?;
                        left = Expr::Binary {
                            left: Box::new(left),
                            op,
                            right: Box::new(inner),
                        };
                        continue;
                    }
                }
                let right = self.parse_additive()?;
                left = Expr::Binary {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                };
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Sym(s)) if s == "+" => Some(BinOp::Add),
                Some(Tok::Sym(s)) if s == "-" => Some(BinOp::Sub),
                Some(Tok::Sym(s)) if s == "||" => Some(BinOp::Concat),
                _ => None,
            };
            if let Some(op) = op {
                self.i += 1;
                let right = self.parse_multiplicative()?;
                left = Expr::Binary {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                };
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_postfix()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Sym(s)) if s == "*" => Some(BinOp::Mul),
                Some(Tok::Sym(s)) if s == "/" => Some(BinOp::Div),
                Some(Tok::Sym(s)) if s == "%" => Some(BinOp::Rem),
                _ => None,
            };
            if let Some(op) = op {
                self.i += 1;
                let right = self.parse_postfix()?;
                left = Expr::Binary {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                };
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_unary()?;
        while self.eat_sym("::") {
            let ty = self.parse_type_name()?;
            expr = Expr::Cast {
                expr: Box::new(expr),
                ty,
            };
        }
        Ok(expr)
    }

    fn parse_type_name(&mut self) -> Result<String, String> {
        let mut ty = match self.next() {
            Some(Tok::Word(w)) => w.clone(),
            _ => return Err("expected type name".into()),
        };
        if ty.eq_ignore_ascii_case("double") && self.eat_kw("precision") {
            ty = "double precision".into();
        } else if ty.eq_ignore_ascii_case("character") && self.eat_kw("varying") {
            ty = "varchar".into();
        } else if (ty.eq_ignore_ascii_case("timestamp") || ty.eq_ignore_ascii_case("time"))
            && (self.eat_kw("with") || self.eat_kw("without"))
        {
            let _ = self.eat_kw("time");
            let _ = self.eat_kw("zone");
        }
        if self.eat_sym("(") {
            while !self.eat_sym(")") && self.peek().is_some() {
                self.i += 1;
            }
        }
        if self.eat_sym("[") {
            let _ = self.eat_sym("]");
        }
        Ok(ty)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        if self.eat_sym("-") {
            let e = self.parse_unary()?;
            return Ok(Expr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(e),
            });
        }
        if self.eat_sym("+") {
            let e = self.parse_unary()?;
            return Ok(Expr::Unary {
                op: UnaryOp::Pos,
                expr: Box::new(e),
            });
        }
        self.parse_atom()
    }

    fn parse_atom(&mut self) -> Result<Expr, String> {
        if self.eat_sym("*") {
            return Ok(Expr::Wildcard(None));
        }
        if self.eat_kw("null") {
            return Ok(Expr::Null);
        }
        if self.eat_kw("true") {
            return Ok(Expr::Bool(true));
        }
        if self.eat_kw("false") {
            return Ok(Expr::Bool(false));
        }
        if self.eat_kw("exists") {
            self.expect_sym("(")?;
            let q = self.parse_query()?;
            self.expect_sym(")")?;
            return Ok(Expr::Exists {
                query: Box::new(q),
                not: false,
            });
        }
        if self.eat_kw("case") {
            let operand = if !kw(self.peek(), "when") {
                Some(Box::new(self.parse_expr()?))
            } else {
                None
            };
            let mut branches = Vec::new();
            while self.eat_kw("when") {
                let cond = self.parse_expr()?;
                self.expect_kw("then")?;
                let val = self.parse_expr()?;
                branches.push((cond, val));
            }
            let else_expr = if self.eat_kw("else") {
                Some(Box::new(self.parse_expr()?))
            } else {
                None
            };
            self.expect_kw("end")?;
            return Ok(Expr::Case {
                operand,
                branches,
                else_expr,
            });
        }
        if self.eat_kw("cast") {
            self.expect_sym("(")?;
            let expr = self.parse_expr()?;
            self.expect_kw("as")?;
            let ty = self.parse_type_name()?;
            self.expect_sym(")")?;
            return Ok(Expr::Cast {
                expr: Box::new(expr),
                ty,
            });
        }
        if self.eat_kw("extract") {
            self.expect_sym("(")?;
            let field = match self.next() {
                Some(Tok::Word(w)) => w.clone(),
                _ => return Err("expected field in EXTRACT".into()),
            };
            self.expect_kw("from")?;
            let src = self.parse_expr()?;
            self.expect_sym(")")?;
            return Ok(Expr::Func {
                name: "extract".into(),
                args: vec![Expr::Str(field), src],
                distinct: false,
                filter: None,
                over: None,
            });
        }
        if self.eat_kw("position") && sym(self.peek(), "(") {
            self.expect_sym("(")?;
            let sub = self.parse_additive()?;
            if self.eat_kw("in") {
                let s = self.parse_expr()?;
                self.expect_sym(")")?;
                return Ok(Expr::Func {
                    name: "position".into(),
                    args: vec![sub, s],
                    distinct: false,
                    filter: None,
                    over: None,
                });
            } else {
                let mut args = vec![sub];
                while self.eat_sym(",") {
                    args.push(self.parse_expr()?);
                }
                self.expect_sym(")")?;
                return Ok(Expr::Func {
                    name: "position".into(),
                    args,
                    distinct: false,
                    filter: None,
                    over: None,
                });
            }
        }
        if self.eat_kw("substring") && sym(self.peek(), "(") {
            self.expect_sym("(")?;
            let src = self.parse_expr()?;
            let mut args = vec![src];
            if self.eat_kw("from") || self.eat_sym(",") {
                args.push(self.parse_expr()?);
                if self.eat_kw("for") || self.eat_sym(",") {
                    args.push(self.parse_expr()?);
                }
            }
            self.expect_sym(")")?;
            return Ok(Expr::Func {
                name: "substring".into(),
                args,
                distinct: false,
                filter: None,
                over: None,
            });
        }
        if self.eat_kw("trim") && sym(self.peek(), "(") {
            self.expect_sym("(")?;
            let mut mode = "both";
            if self.eat_kw("leading") {
                mode = "leading";
            } else if self.eat_kw("trailing") {
                mode = "trailing";
            } else {
                let _ = self.eat_kw("both");
            }
            let first = if self.eat_kw("from") {
                None
            } else {
                Some(self.parse_expr()?)
            };
            let args = if self.eat_kw("from") {
                let target = self.parse_expr()?;
                match first {
                    Some(chars) => vec![target, chars],
                    None => vec![target],
                }
            } else {
                let mut a = Vec::new();
                if let Some(f) = first {
                    a.push(f);
                }
                while self.eat_sym(",") {
                    a.push(self.parse_expr()?);
                }
                a
            };
            self.expect_sym(")")?;
            let fname = match mode {
                "leading" => "ltrim",
                "trailing" => "rtrim",
                _ => "trim",
            };
            return Ok(Expr::Func {
                name: fname.into(),
                args,
                distinct: false,
                filter: None,
                over: None,
            });
        }
        if self.eat_kw("array") && self.eat_sym("[") {
            let mut items = Vec::new();
            if !self.eat_sym("]") {
                loop {
                    items.push(self.parse_expr()?);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym("]")?;
            }
            return Ok(Expr::Func {
                name: "array_construct".into(),
                args: items,
                distinct: false,
                filter: None,
                over: None,
            });
        }
        if self.eat_sym("(") {
            if kw(self.peek(), "select") || kw(self.peek(), "with") || kw(self.peek(), "values") {
                let subq = self.parse_query()?;
                self.expect_sym(")")?;
                return Ok(Expr::Subquery(Box::new(subq)));
            }
            let expr = self.parse_expr()?;
            if self.eat_sym(",") {
                // Row constructor (a, b)
                let mut items = vec![expr];
                loop {
                    items.push(self.parse_expr()?);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
                return Ok(Expr::Func {
                    name: "row_construct".into(),
                    args: items,
                    distinct: false,
                    filter: None,
                    over: None,
                });
            }
            self.expect_sym(")")?;
            return Ok(expr);
        }

        match self.next().cloned() {
            Some(Tok::Num(n)) => {
                if let Ok(i) = n.parse::<i64>() {
                    Ok(Expr::Int(i))
                } else if let Ok(f) = n.parse::<f64>() {
                    Ok(Expr::Float(f))
                } else {
                    Err(format!("bad number {n}"))
                }
            }
            Some(Tok::Str(s)) => Ok(Expr::Str(s)),
            Some(Tok::Word(w)) => {
                // Typed literal like DATE '2025-01-01', TIMESTAMP '...', INTERVAL '...'
                if ["date", "timestamp", "timestamptz", "time", "interval"]
                    .iter()
                    .any(|k| w.eq_ignore_ascii_case(k))
                {
                    if let Some(Tok::Str(s)) = self.peek().cloned() {
                        self.i += 1;
                        return Ok(Expr::Str(s));
                    }
                }
                if let Some(prefix) = w.strip_suffix(".*") {
                    return Ok(Expr::Wildcard(Some(prefix.to_string())));
                }
                // Special SQL niladic functions
                if [
                    "current_user",
                    "user",
                    "session_user",
                    "current_date",
                    "current_timestamp",
                    "current_time",
                    "current_schema",
                    "current_catalog",
                ]
                .iter()
                .any(|k| w.eq_ignore_ascii_case(k))
                    && !sym(self.peek(), "(")
                {
                    return Ok(Expr::Func {
                        name: w.to_ascii_lowercase(),
                        args: Vec::new(),
                        distinct: false,
                        filter: None,
                        over: None,
                    });
                }

                if self.eat_sym("(") {
                    let mut distinct = false;
                    if self.eat_kw("distinct") {
                        distinct = true;
                    } else if self.eat_kw("all") {
                        distinct = false;
                    }
                    let mut args = Vec::new();
                    if !self.eat_sym(")") {
                        loop {
                            if self.eat_sym("*") {
                                args.push(Expr::Wildcard(None));
                            } else {
                                args.push(self.parse_expr()?);
                            }
                            if kw(self.peek(), "order") && kw(self.peek_n(1), "by") {
                                self.i += 2;
                                let _ = self.parse_order_by_list()?;
                            }
                            if !self.eat_sym(",") {
                                break;
                            }
                        }
                        self.expect_sym(")")?;
                    }
                    let mut filter = None;
                    if self.eat_kw("filter") {
                        self.expect_sym("(")?;
                        self.expect_kw("where")?;
                        filter = Some(Box::new(self.parse_expr()?));
                        self.expect_sym(")")?;
                    }
                    let mut over = None;
                    if self.eat_kw("over") {
                        if let Some(Tok::Word(_win_name)) = self.peek() {
                            self.i += 1;
                            over = Some(WindowSpec {
                                partition_by: Vec::new(),
                                order_by: Vec::new(),
                            });
                        } else {
                            self.expect_sym("(")?;
                            let mut partition_by = Vec::new();
                            if self.eat_kw("partition") {
                                self.expect_kw("by")?;
                                loop {
                                    partition_by.push(self.parse_expr()?);
                                    if !self.eat_sym(",") {
                                        break;
                                    }
                                }
                            }
                            let mut order_by = Vec::new();
                            if self.eat_kw("order") {
                                self.expect_kw("by")?;
                                order_by = self.parse_order_by_list()?;
                            }
                            // Ignore optional frame clause (ROWS / RANGE / GROUPS BETWEEN ...)
                            while !self.eat_sym(")") && self.peek().is_some() {
                                self.i += 1;
                            }
                            over = Some(WindowSpec {
                                partition_by,
                                order_by,
                            });
                        }
                    }
                    let fname = unqualified(&w).to_ascii_lowercase();
                    return Ok(Expr::Func {
                        name: fname,
                        args,
                        distinct,
                        filter,
                        over,
                    });
                }
                Ok(Expr::Col(w))
            }
            other => Err(format!("unexpected {other:?} in expression")),
        }
    }
}

fn normalize_table_name(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("information_schema.") || lower.starts_with("pg_catalog.") {
        return lower;
    }
    if let Some(rest) = raw
        .strip_prefix("public.")
        .or_else(|| raw.strip_prefix("PUBLIC."))
    {
        return rest.to_string();
    }
    unqualified(raw).to_string()
}

fn expr_default_name(expr: &Expr, toks: &[Tok]) -> String {
    match expr {
        Expr::Col(c) => unqualified(c).to_string(),
        Expr::Func { name, .. } => name.to_ascii_lowercase(),
        Expr::Cast { ty, .. } => unqualified(ty).to_ascii_lowercase(),
        Expr::Case { .. } => "case".to_string(),
        _ => {
            if toks.len() == 1 {
                match &toks[0] {
                    Tok::Word(w) => unqualified(w).to_string(),
                    Tok::Str(s) => s.clone(),
                    Tok::Num(n) => n.clone(),
                    Tok::Sym(s) => s.clone(),
                }
            } else {
                "?column?".to_string()
            }
        }
    }
}

// ------------------------------------------------------------------ Execution Engine

#[derive(Clone)]
struct ExecCtx<'a> {
    store: &'a Store,
    schema: &'a Schema,
    user: &'a str,
    database: &'a str,
    ctes: HashMap<String, Rel>,
    outer: Option<Arc<(Vec<(String, String)>, Vec<Val>)>>,
    depth: usize,
}

#[derive(Clone, Debug)]
struct Rel {
    /// (qualifier, column_name)
    cols: Vec<(String, String)>,
    rows: Vec<Vec<Val>>,
}

impl Rel {
    fn resolve(&self, name: &str) -> Result<usize, String> {
        if let Some((q, c)) = name.rsplit_once('.') {
            let q_short = unqualified(q);
            let matches: Vec<usize> = self
                .cols
                .iter()
                .enumerate()
                .filter(|(_, (rq, rc))| {
                    (rq.eq_ignore_ascii_case(q) || rq.eq_ignore_ascii_case(q_short))
                        && rc.eq_ignore_ascii_case(c)
                })
                .map(|(i, _)| i)
                .collect();
            match matches.as_slice() {
                [i] => return Ok(*i),
                [] => {}
                _ => return Ok(matches[0]),
            }
        }
        let target = unqualified(name);
        let matches: Vec<usize> = self
            .cols
            .iter()
            .enumerate()
            .filter(|(_, (_, rc))| rc.eq_ignore_ascii_case(target))
            .map(|(i, _)| i)
            .collect();
        match matches.as_slice() {
            [i] => Ok(*i),
            [] => Err(format!("column \"{name}\" does not exist")),
            _ => {
                if !name.contains('.') {
                    let first_col = &self.cols[matches[0]].1;
                    // Check if all matching columns come from same synthetic projection or USING merge
                    if matches.iter().all(|&idx| {
                        self.cols[idx].0.is_empty()
                            && self.cols[idx].1.eq_ignore_ascii_case(first_col)
                    }) {
                        return Ok(matches[0]);
                    }
                }
                Err(format!("column reference \"{name}\" is ambiguous"))
            }
        }
    }

    fn to_rows(self) -> Rows {
        let headers = self.cols.into_iter().map(|(_, c)| c).collect();
        let data = self
            .rows
            .into_iter()
            .map(|r| r.into_iter().map(|v| v.as_text()).collect())
            .collect();
        (headers, data)
    }
}

fn cell(t: &Term) -> String {
    match t {
        Term::Blank(b) => format!("_:{b}"),
        Term::Triple(_) => t.to_string(),
        other => other.value().to_string(),
    }
}

fn like_match(text: &str, pat: &str, case_insensitive: bool) -> bool {
    // `.` must also match newlines, as SQL LIKE does; the regex engine takes
    // flags separately, it does not read inline `(?s)` groups
    let rx = format!(
        "^{}$",
        pat.chars()
            .map(|c| match c {
                '%' => ".*".to_string(),
                '_' => ".".to_string(),
                c if "\\^$.|?*+()[]{}".contains(c) => format!("\\{c}"),
                c => c.to_string(),
            })
            .collect::<String>()
    );
    let flags = if case_insensitive { "si" } else { "s" };
    crate::sparql::regex::Regex::new(&rx, flags).is_ok_and(|r| r.is_match(text))
}

fn no_table(schema: &Schema, views: &[(String, String)], name: &str) -> String {
    let mut names: Vec<String> = schema.tables().into_iter().map(|(t, _)| t).collect();
    names.extend(views.iter().map(|(v, _)| v.clone()));
    format!(
        "relation \"{name}\" does not exist (tables: {})",
        names.join(", ")
    )
}

fn table_columns(schema: &Schema, table: &str) -> Vec<String> {
    let mut c = vec!["id".to_string()];
    c.extend(
        schema
            .tables()
            .into_iter()
            .find(|(t, _)| t.eq_ignore_ascii_case(table))
            .map(|(_, f)| f)
            .unwrap_or_default(),
    );
    c
}

fn node_col_val(store: &Store, schema: &Schema, n: Id, col: &str) -> Val {
    if col.eq_ignore_ascii_case("id") {
        return Val::Str(cell(&store.dict.term(n)));
    }
    let mut vals: Vec<Val> = schema
        .predicates(col)
        .into_iter()
        .flat_map(|p| {
            store
                .quads_matching(Some(n), Some(p), None, None)
                .map(|q| Val::from_term(&store.dict.term(q[2])))
                .collect::<Vec<_>>()
        })
        .collect();
    match vals.len() {
        0 => Val::Null,
        1 => vals.pop().unwrap(),
        _ => Val::Multi(vals),
    }
}

fn load_base_table(ctx: &ExecCtx, name: &str, alias: &str) -> Result<Rel, String> {
    if ctx.depth > 16 {
        return Err(format!("view \"{name}\" refers to itself"));
    }
    // 1. Check CTEs
    if let Some(cte_rel) = ctx.ctes.get(&name.to_ascii_lowercase()) {
        let cols = cte_rel
            .cols
            .iter()
            .map(|(_, c)| (alias.to_string(), c.clone()))
            .collect();
        return Ok(Rel {
            cols,
            rows: cte_rel.rows.clone(),
        });
    }
    // 2. Check system catalogs (information_schema & pg_catalog)
    if let Some(sys_rel) = load_system_table(ctx, name, alias)? {
        return Ok(sys_rel);
    }
    // 3. Check Views
    if let Some(sql) = view(ctx.store, name) {
        let toks = lex(&sql)?;
        let mut p = Parser::new(&toks);
        let q = p.parse_query_top()?;
        let mut sub_ctx = ctx.clone();
        sub_ctx.depth += 1;
        let view_rel = eval_query(&sub_ctx, &q)?;
        let cols = view_rel
            .cols
            .into_iter()
            .map(|(_, c)| (alias.to_string(), c))
            .collect();
        return Ok(Rel {
            cols,
            rows: view_rel.rows,
        });
    }
    // 4. Load RDF Class table
    let class = ctx
        .schema
        .class(name)
        .ok_or_else(|| no_table(ctx.schema, &views(ctx.store), name))?;
    let cols = table_columns(ctx.schema, name);
    let qual_cols: Vec<(String, String)> = cols
        .iter()
        .map(|c| (alias.to_string(), c.clone()))
        .collect();
    let Some(ty) = ctx.store.dict.lookup(&Term::iri(RDF_TYPE)) else {
        return Ok(Rel {
            cols: qual_cols,
            rows: Vec::new(),
        });
    };
    let mut seen = HashSet::new();
    let nodes: Vec<Id> = ctx
        .store
        .quads_matching(None, Some(ty), Some(class), None)
        .map(|q| q[0])
        .filter(|n| seen.insert(*n))
        .collect();
    let rows = nodes
        .into_iter()
        .map(|n| {
            cols.iter()
                .map(|c| node_col_val(ctx.store, ctx.schema, n, c))
                .collect()
        })
        .collect();
    Ok(Rel {
        cols: qual_cols,
        rows,
    })
}

fn load_system_table(ctx: &ExecCtx, name: &str, alias: &str) -> Result<Option<Rel>, String> {
    let lower = name.to_ascii_lowercase();
    let short = lower
        .strip_prefix("information_schema.")
        .or_else(|| lower.strip_prefix("pg_catalog."))
        .unwrap_or(&lower);
    let is_info = lower.starts_with("information_schema.");
    let is_pg = lower.starts_with("pg_catalog.") || short.starts_with("pg_");

    if !is_info && !is_pg {
        return Ok(None);
    }

    let make_rel = |col_names: &[&str], str_rows: Vec<Vec<Option<String>>>| -> Rel {
        let cols = col_names
            .iter()
            .map(|c| (alias.to_string(), (*c).to_string()))
            .collect();
        let rows = str_rows
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|c| c.map(|v| Val::from_str_inferred(&v)).unwrap_or(Val::Null))
                    .collect()
            })
            .collect();
        Rel { cols, rows }
    };

    if is_info {
        let tables = ctx.schema.tables();
        let v_list = views(ctx.store);
        return match short {
            "tables" => {
                let mut rows: Vec<Vec<Option<String>>> = tables
                    .into_iter()
                    .map(|(t, _)| {
                        vec![
                            Some(t),
                            Some("public".into()),
                            Some("BASE TABLE".into()),
                            Some(ctx.database.into()),
                        ]
                    })
                    .collect();
                rows.extend(v_list.into_iter().map(|(v, _)| {
                    vec![
                        Some(v),
                        Some("public".into()),
                        Some("VIEW".into()),
                        Some(ctx.database.into()),
                    ]
                }));
                Ok(Some(make_rel(
                    &["table_name", "table_schema", "table_type", "table_catalog"],
                    rows,
                )))
            }
            "columns" => {
                let mut rows: Vec<Vec<Option<String>>> = Vec::new();
                for (t, fields) in tables {
                    for (idx, c) in std::iter::once("id".to_string()).chain(fields).enumerate() {
                        rows.push(vec![
                            Some(t.clone()),
                            Some(c),
                            Some("text".into()),
                            Some("public".into()),
                            Some((idx + 1).to_string()),
                            Some("YES".into()),
                        ]);
                    }
                }
                for (v, sql) in &v_list {
                    let vcols = if ctx.depth > 8 {
                        Vec::new()
                    } else {
                        lex(sql)
                            .and_then(|toks| Parser::new(&toks).parse_query_top())
                            .and_then(|q| {
                                let mut sub = ctx.clone();
                                sub.depth += 1;
                                eval_query(&sub, &q)
                            })
                            .map(|r| r.cols.into_iter().map(|(_, c)| c).collect::<Vec<_>>())
                            .unwrap_or_default()
                    };
                    for (idx, c) in vcols.into_iter().enumerate() {
                        rows.push(vec![
                            Some(v.clone()),
                            Some(c),
                            Some("text".into()),
                            Some("public".into()),
                            Some((idx + 1).to_string()),
                            Some("YES".into()),
                        ]);
                    }
                }
                Ok(Some(make_rel(
                    &[
                        "table_name",
                        "column_name",
                        "data_type",
                        "table_schema",
                        "ordinal_position",
                        "is_nullable",
                    ],
                    rows,
                )))
            }
            "views" => {
                let rows = v_list
                    .into_iter()
                    .map(|(v, sql)| vec![Some(v), Some("public".into()), Some(sql)])
                    .collect();
                Ok(Some(make_rel(
                    &["table_name", "table_schema", "view_definition"],
                    rows,
                )))
            }
            "schemata" => {
                let rows = vec![
                    vec![
                        Some(ctx.database.into()),
                        Some("public".into()),
                        Some(ctx.user.into()),
                    ],
                    vec![
                        Some(ctx.database.into()),
                        Some("information_schema".into()),
                        Some(ctx.user.into()),
                    ],
                    vec![
                        Some(ctx.database.into()),
                        Some("pg_catalog".into()),
                        Some(ctx.user.into()),
                    ],
                ];
                Ok(Some(make_rel(
                    &["catalog_name", "schema_name", "schema_owner"],
                    rows,
                )))
            }
            "table_constraints" | "key_column_usage" | "referential_constraints" => {
                Ok(Some(make_rel(
                    &[
                        "constraint_catalog",
                        "constraint_schema",
                        "constraint_name",
                        "table_schema",
                        "table_name",
                        "constraint_type",
                        "column_name",
                    ],
                    Vec::new(),
                )))
            }
            _ => Ok(None),
        };
    }

    // pg_catalog tables for psql / JDBC / psycopg / BI compatibility
    match short {
        "pg_namespace" => {
            let rows = vec![
                vec![
                    Some("11".into()),
                    Some("pg_catalog".into()),
                    Some("10".into()),
                ],
                vec![
                    Some("2200".into()),
                    Some("public".into()),
                    Some("10".into()),
                ],
                vec![
                    Some("13000".into()),
                    Some("information_schema".into()),
                    Some("10".into()),
                ],
            ];
            Ok(Some(make_rel(&["oid", "nspname", "nspowner"], rows)))
        }
        "pg_type" => {
            let rows = vec![
                vec![
                    Some("16".into()),
                    Some("bool".into()),
                    Some("11".into()),
                    Some("1".into()),
                    Some("b".into()),
                ],
                vec![
                    Some("20".into()),
                    Some("int8".into()),
                    Some("11".into()),
                    Some("8".into()),
                    Some("b".into()),
                ],
                vec![
                    Some("21".into()),
                    Some("int2".into()),
                    Some("11".into()),
                    Some("2".into()),
                    Some("b".into()),
                ],
                vec![
                    Some("23".into()),
                    Some("int4".into()),
                    Some("11".into()),
                    Some("4".into()),
                    Some("b".into()),
                ],
                vec![
                    Some("25".into()),
                    Some("text".into()),
                    Some("11".into()),
                    Some("-1".into()),
                    Some("b".into()),
                ],
                vec![
                    Some("700".into()),
                    Some("float4".into()),
                    Some("11".into()),
                    Some("4".into()),
                    Some("b".into()),
                ],
                vec![
                    Some("701".into()),
                    Some("float8".into()),
                    Some("11".into()),
                    Some("8".into()),
                    Some("b".into()),
                ],
                vec![
                    Some("1043".into()),
                    Some("varchar".into()),
                    Some("11".into()),
                    Some("-1".into()),
                    Some("b".into()),
                ],
            ];
            Ok(Some(make_rel(
                &["oid", "typname", "typnamespace", "typlen", "typtype"],
                rows,
            )))
        }
        "pg_class" => {
            let mut rows = Vec::new();
            for (idx, (t, _)) in ctx.schema.tables().into_iter().enumerate() {
                rows.push(vec![
                    Some((16384 + idx).to_string()),
                    Some(t),
                    Some("2200".into()),
                    Some("r".into()),
                    Some("10".into()),
                ]);
            }
            for (idx, (v, _)) in views(ctx.store).into_iter().enumerate() {
                rows.push(vec![
                    Some((32768 + idx).to_string()),
                    Some(v),
                    Some("2200".into()),
                    Some("v".into()),
                    Some("10".into()),
                ]);
            }
            Ok(Some(make_rel(
                &["oid", "relname", "relnamespace", "relkind", "relowner"],
                rows,
            )))
        }
        "pg_attribute" => {
            let mut rows = Vec::new();
            for (idx, (_, fields)) in ctx.schema.tables().into_iter().enumerate() {
                let relid = (16384 + idx).to_string();
                for (cidx, col) in std::iter::once("id".to_string()).chain(fields).enumerate() {
                    rows.push(vec![
                        Some(relid.clone()),
                        Some(col),
                        Some("25".into()),
                        Some((cidx + 1).to_string()),
                        Some("false".into()),
                        Some("false".into()),
                    ]);
                }
            }
            Ok(Some(make_rel(
                &[
                    "attrelid",
                    "attname",
                    "atttypid",
                    "attnum",
                    "attnotnull",
                    "attisdropped",
                ],
                rows,
            )))
        }
        "pg_database" => {
            let rows = vec![vec![
                Some("16380".into()),
                Some(ctx.database.into()),
                Some("10".into()),
                Some("6".into()),
            ]];
            Ok(Some(make_rel(
                &["oid", "datname", "datdba", "encoding"],
                rows,
            )))
        }
        "pg_roles" | "pg_user" | "pg_authid" => {
            let rows = vec![vec![
                Some("10".into()),
                Some(ctx.user.into()),
                Some("true".into()),
                Some("true".into()),
            ]];
            Ok(Some(make_rel(
                &["oid", "rolname", "rolsuper", "rolcanlogin"],
                rows,
            )))
        }
        "pg_settings" => {
            let rows = vec![
                vec![Some("server_version".into()), Some("15.0 (GraphRD)".into())],
                vec![Some("server_encoding".into()), Some("UTF8".into())],
                vec![Some("client_encoding".into()), Some("UTF8".into())],
                vec![Some("TimeZone".into()), Some("UTC".into())],
                vec![
                    Some("standard_conforming_strings".into()),
                    Some("on".into()),
                ],
            ];
            Ok(Some(make_rel(&["name", "setting"], rows)))
        }
        "pg_index" | "pg_constraint" | "pg_description" | "pg_attrdef" | "pg_am" => {
            Ok(Some(make_rel(
                &[
                    "oid",
                    "indrelid",
                    "indexrelid",
                    "conname",
                    "description",
                    "amname",
                ],
                Vec::new(),
            )))
        }
        _ => Ok(None),
    }
}

fn eval_table_factor(ctx: &ExecCtx, tf: &TableFactor) -> Result<Rel, String> {
    match tf {
        TableFactor::Table { name, alias } => load_base_table(ctx, name, alias),
        TableFactor::Subquery { query, alias } => {
            let rel = eval_query(ctx, query)?;
            let cols = rel
                .cols
                .into_iter()
                .map(|(_, c)| (alias.clone(), c))
                .collect();
            Ok(Rel {
                cols,
                rows: rel.rows,
            })
        }
        TableFactor::Sparql { query, alias } => {
            let opts = QueryOptions {
                base: "",
                prefixes: &HashMap::new(),
                dataset: None,
                timeout_secs: Some(60),
                infer: true,
                at_query: false,
            };
            let (vars, str_rows) = match sparql::query(ctx.store, query, &opts)? {
                QueryResult::Solutions { vars, rows } => {
                    let r: Vec<Vec<Val>> = rows
                        .into_iter()
                        .map(|row| {
                            row.into_iter()
                                .map(|t| t.as_ref().map(Val::from_term).unwrap_or(Val::Null))
                                .collect()
                        })
                        .collect();
                    (vars, r)
                }
                QueryResult::Boolean(b) => (vec!["result".into()], vec![vec![Val::Bool(b)]]),
                QueryResult::Graph(g) => (
                    vec!["subject".into(), "predicate".into(), "object".into()],
                    g.into_iter()
                        .map(|q| {
                            vec![
                                Val::from_term(&q.s),
                                Val::from_term(&q.p),
                                Val::from_term(&q.o),
                            ]
                        })
                        .collect(),
                ),
            };
            let cols = vars.into_iter().map(|v| (alias.clone(), v)).collect();
            Ok(Rel {
                cols,
                rows: str_rows,
            })
        }
        TableFactor::Func { name, args, alias } => {
            let empty_rel = Rel {
                cols: Vec::new(),
                rows: vec![Vec::new()],
            };
            let mut arg_vals = Vec::new();
            for a in args {
                arg_vals.push(eval_expr(ctx, &empty_rel, &[], a)?);
            }
            if name.eq_ignore_ascii_case("generate_series") {
                let start = arg_vals.first().and_then(|v| v.as_i64()).unwrap_or(1);
                let stop = arg_vals.get(1).and_then(|v| v.as_i64()).unwrap_or(start);
                let step = arg_vals
                    .get(2)
                    .and_then(|v| v.as_i64())
                    .unwrap_or(if start <= stop { 1 } else { -1 });
                if step == 0 {
                    return Err("step size cannot equal zero".into());
                }
                let mut rows = Vec::new();
                let mut cur = start;
                while (step > 0 && cur <= stop) || (step < 0 && cur >= stop) {
                    rows.push(vec![Val::Int(cur)]);
                    cur += step;
                    if rows.len() > 100_000 {
                        break;
                    }
                }
                return Ok(Rel {
                    cols: vec![(alias.clone(), alias.clone())],
                    rows,
                });
            }
            if name.eq_ignore_ascii_case("unnest") {
                let items = arg_vals.first().map(|v| v.items()).unwrap_or_default();
                let rows = items.into_iter().map(|v| vec![v]).collect();
                return Ok(Rel {
                    cols: vec![(alias.clone(), alias.clone())],
                    rows,
                });
            }
            Err(format!("unsupported table function \"{name}\""))
        }
    }
}

/// Flatten multi-valued cells joined on equality so RDF 1-to-N links match naturally.
fn join_values_match(left: &Val, right: &Val) -> bool {
    if left.is_null() || right.is_null() {
        return false;
    }
    let l_items = left.items();
    let r_items = right.items();
    for a in &l_items {
        for b in &r_items {
            if cmp_vals(a, b) == Ordering::Equal {
                return true;
            }
        }
    }
    false
}

/// Extract simple `a = b` equi-join column indices when possible, to support multi-valued RDF predicate unnesting.
fn extract_simple_equi(left_rel: &Rel, right_rel: &Rel, expr: &Expr) -> Option<(usize, usize)> {
    if let Expr::Binary {
        left,
        op: BinOp::Eq,
        right,
    } = expr
    {
        if let (Expr::Col(a), Expr::Col(b)) = (left.as_ref(), right.as_ref()) {
            if let (Ok(l), Ok(r)) = (left_rel.resolve(a), right_rel.resolve(b)) {
                return Some((l, r));
            }
            if let (Ok(l), Ok(r)) = (left_rel.resolve(b), right_rel.resolve(a)) {
                return Some((l, r));
            }
        }
    }
    None
}

fn execute_join(ctx: &ExecCtx, left_rel: Rel, j: &JoinAst) -> Result<Rel, String> {
    let right_rel = eval_table_factor(ctx, &j.factor)?;
    let mut combined_cols = left_rel.cols.clone();
    combined_cols.extend(right_rel.cols.clone());

    let dummy_rel = Rel {
        cols: combined_cols.clone(),
        rows: Vec::new(),
    };
    // Validate column references in ON or USING early
    match &j.cond {
        JoinCond::On(expr) => {
            validate_expr_cols(&dummy_rel, expr)?;
        }
        JoinCond::Using(ucols) => {
            for u in ucols {
                left_rel.resolve(u)?;
                right_rel.resolve(u)?;
            }
        }
        JoinCond::None => {}
    }

    let simple_equi = match &j.cond {
        JoinCond::On(expr) => extract_simple_equi(&left_rel, &right_rel, expr),
        _ => None,
    };

    let mut out_rows = Vec::new();
    let mut right_matched = vec![false; right_rel.rows.len()];

    for l_row in &left_rel.rows {
        let mut matched_any = false;
        for (r_idx, r_row) in right_rel.rows.iter().enumerate() {
            let mut combined_row = Vec::with_capacity(l_row.len() + r_row.len());
            combined_row.extend_from_slice(l_row);
            combined_row.extend_from_slice(r_row);

            let matches = match &j.cond {
                JoinCond::None => true,
                JoinCond::Using(ucols) => {
                    let mut ok = true;
                    for u in ucols {
                        let li = left_rel.resolve(u)?;
                        let ri = right_rel.resolve(u)?;
                        if !join_values_match(&l_row[li], &r_row[ri]) {
                            ok = false;
                            break;
                        }
                    }
                    ok
                }
                JoinCond::On(expr) => {
                    if let Some((li, ri)) = simple_equi {
                        join_values_match(&l_row[li], &r_row[ri])
                    } else {
                        eval_expr(ctx, &dummy_rel, &combined_row, expr)?
                            .as_bool()
                            .unwrap_or(false)
                    }
                }
            };

            if matches {
                matched_any = true;
                right_matched[r_idx] = true;
                out_rows.push(combined_row);
            }
        }
        if !matched_any && (j.kind == JoinKind::Left || j.kind == JoinKind::Full) {
            let mut combined_row = l_row.clone();
            combined_row.extend(std::iter::repeat_n(Val::Null, right_rel.cols.len()));
            out_rows.push(combined_row);
        }
    }

    if j.kind == JoinKind::Right || j.kind == JoinKind::Full {
        for (r_idx, r_row) in right_rel.rows.iter().enumerate() {
            if !right_matched[r_idx] {
                let mut combined_row: Vec<Val> =
                    std::iter::repeat_n(Val::Null, left_rel.cols.len()).collect();
                combined_row.extend(r_row.iter().cloned());
                out_rows.push(combined_row);
            }
        }
    }

    // If USING was specified, merge duplicate USING columns so unqualified reference isn't ambiguous
    if let JoinCond::Using(ucols) = &j.cond {
        for u in ucols {
            if let (Ok(li), Ok(ri_rel)) = (left_rel.resolve(u), right_rel.resolve(u)) {
                let ri = left_rel.cols.len() + ri_rel;
                for row in &mut out_rows {
                    if row[li].is_null() && !row[ri].is_null() {
                        row[li] = row[ri].clone();
                    }
                }
                combined_cols[li].0 = String::new();
                combined_cols[ri].0 = "__hidden__".to_string();
                combined_cols[ri].1 = format!("__using_dup_{u}");
            }
        }
    }

    Ok(Rel {
        cols: combined_cols,
        rows: out_rows,
    })
}

fn validate_expr_cols(rel: &Rel, expr: &Expr) -> Result<(), String> {
    match expr {
        Expr::Col(c) => {
            rel.resolve(c)?;
            Ok(())
        }
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::IsBool { expr, .. }
        | Expr::Cast { expr, .. } => validate_expr_cols(rel, expr),
        Expr::Binary { left, right, .. } => {
            validate_expr_cols(rel, left)?;
            validate_expr_cols(rel, right)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            validate_expr_cols(rel, expr)?;
            validate_expr_cols(rel, low)?;
            validate_expr_cols(rel, high)
        }
        Expr::InList { expr, list, .. } => {
            validate_expr_cols(rel, expr)?;
            for item in list {
                validate_expr_cols(rel, item)?;
            }
            Ok(())
        }
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => {
            if let Some(op) = operand {
                validate_expr_cols(rel, op)?;
            }
            for (w, t) in branches {
                validate_expr_cols(rel, w)?;
                validate_expr_cols(rel, t)?;
            }
            if let Some(el) = else_expr {
                validate_expr_cols(rel, el)?;
            }
            Ok(())
        }
        Expr::Func { args, filter, .. } => {
            for a in args {
                if !matches!(a, Expr::Wildcard(_)) {
                    validate_expr_cols(rel, a)?;
                }
            }
            if let Some(f) = filter {
                validate_expr_cols(rel, f)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn is_agg_func(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "string_agg"
            | "group_concat"
            | "array_agg"
            | "bool_and"
            | "bool_or"
            | "every"
    )
}

fn expr_contains_agg(expr: &Expr) -> bool {
    match expr {
        Expr::Func {
            name, args, over, ..
        } => {
            if over.is_some() {
                return false;
            }
            if is_agg_func(name) {
                return true;
            }
            args.iter().any(expr_contains_agg)
        }
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::IsBool { expr, .. }
        | Expr::Cast { expr, .. } => expr_contains_agg(expr),
        Expr::Binary { left, right, .. } => expr_contains_agg(left) || expr_contains_agg(right),
        Expr::Between {
            expr, low, high, ..
        } => expr_contains_agg(expr) || expr_contains_agg(low) || expr_contains_agg(high),
        Expr::InList { expr, list, .. } => {
            expr_contains_agg(expr) || list.iter().any(expr_contains_agg)
        }
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => {
            operand.as_ref().is_some_and(|e| expr_contains_agg(e))
                || branches
                    .iter()
                    .any(|(w, t)| expr_contains_agg(w) || expr_contains_agg(t))
                || else_expr.as_ref().is_some_and(|e| expr_contains_agg(e))
        }
        _ => false,
    }
}

fn expr_contains_window(expr: &Expr) -> bool {
    match expr {
        Expr::Func { over, args, .. } => over.is_some() || args.iter().any(expr_contains_window),
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::IsBool { expr, .. }
        | Expr::Cast { expr, .. } => expr_contains_window(expr),
        Expr::Binary { left, right, .. } => {
            expr_contains_window(left) || expr_contains_window(right)
        }
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => {
            operand.as_ref().is_some_and(|e| expr_contains_window(e))
                || branches
                    .iter()
                    .any(|(w, t)| expr_contains_window(w) || expr_contains_window(t))
                || else_expr.as_ref().is_some_and(|e| expr_contains_window(e))
        }
        _ => false,
    }
}

fn eval_expr(ctx: &ExecCtx, rel: &Rel, row: &[Val], expr: &Expr) -> Result<Val, String> {
    eval_expr_group(ctx, rel, row, None, expr)
}

fn eval_expr_group(
    ctx: &ExecCtx,
    rel: &Rel,
    repr_row: &[Val],
    group_rows: Option<&[Vec<Val>]>,
    expr: &Expr,
) -> Result<Val, String> {
    match expr {
        Expr::Null => Ok(Val::Null),
        Expr::Bool(b) => Ok(Val::Bool(*b)),
        Expr::Int(i) => Ok(Val::Int(*i)),
        Expr::Float(f) => Ok(Val::Float(*f)),
        Expr::Str(s) => Ok(Val::Str(s.clone())),
        Expr::Wildcard(_) => Ok(Val::Null),
        Expr::Col(name) => {
            if let Ok(idx) = rel.resolve(name) {
                return Ok(repr_row.get(idx).cloned().unwrap_or(Val::Null));
            }
            if let Some(outer) = &ctx.outer {
                let outer_rel = Rel {
                    cols: outer.0.clone(),
                    rows: Vec::new(),
                };
                if let Ok(idx) = outer_rel.resolve(name) {
                    return Ok(outer.1.get(idx).cloned().unwrap_or(Val::Null));
                }
            }
            Err(rel.resolve(name).unwrap_err())
        }
        Expr::Unary { op, expr: inner } => {
            let v = eval_expr_group(ctx, rel, repr_row, group_rows, inner)?;
            match op {
                UnaryOp::Not => Ok(match v.as_bool() {
                    Some(b) => Val::Bool(!b),
                    None => Val::Null,
                }),
                UnaryOp::Neg => {
                    if let Some(i) = v.as_i64() {
                        if matches!(v, Val::Int(_)) {
                            return Ok(Val::Int(-i));
                        }
                    }
                    Ok(v.as_f64().map(|f| Val::Float(-f)).unwrap_or(Val::Null))
                }
                UnaryOp::Pos => Ok(v),
            }
        }
        Expr::IsNull { expr: inner, not } => {
            let v = eval_expr_group(ctx, rel, repr_row, group_rows, inner)?;
            let is_n = v.is_null();
            Ok(Val::Bool(if *not { !is_n } else { is_n }))
        }
        Expr::IsBool {
            expr: inner,
            val,
            not,
        } => {
            let v = eval_expr_group(ctx, rel, repr_row, group_rows, inner)?;
            let matches = v.as_bool() == Some(*val);
            Ok(Val::Bool(if *not { !matches } else { matches }))
        }
        Expr::Between {
            expr: target,
            low,
            high,
            not,
        } => {
            let v = eval_expr_group(ctx, rel, repr_row, group_rows, target)?;
            let l = eval_expr_group(ctx, rel, repr_row, group_rows, low)?;
            let h = eval_expr_group(ctx, rel, repr_row, group_rows, high)?;
            if v.is_null() || l.is_null() || h.is_null() {
                return Ok(Val::Bool(false));
            }
            let items = v.items();
            let hit = items.iter().any(|item| {
                cmp_vals(item, &l) != Ordering::Less && cmp_vals(item, &h) != Ordering::Greater
            });
            Ok(Val::Bool(if *not { !hit } else { hit }))
        }
        Expr::InList {
            expr: target,
            list,
            not,
        } => {
            let v = eval_expr_group(ctx, rel, repr_row, group_rows, target)?;
            if v.is_null() {
                return Ok(Val::Bool(false));
            }
            let v_items = v.items();
            let mut hit = false;
            for item_expr in list {
                let lv = eval_expr_group(ctx, rel, repr_row, group_rows, item_expr)?;
                if !lv.is_null()
                    && v_items
                        .iter()
                        .any(|vi| cmp_vals(vi, &lv) == Ordering::Equal)
                {
                    hit = true;
                    break;
                }
            }
            Ok(Val::Bool(if *not { !hit } else { hit }))
        }
        Expr::InSubquery {
            expr: target,
            query,
            not,
        } => {
            let v = eval_expr_group(ctx, rel, repr_row, group_rows, target)?;
            if v.is_null() {
                return Ok(Val::Bool(false));
            }
            let mut sub_ctx = ctx.clone();
            sub_ctx.outer = Some(Arc::new((rel.cols.clone(), repr_row.to_vec())));
            let sub_rel = eval_query(&sub_ctx, query)?;
            let v_items = v.items();
            let hit = sub_rel
                .rows
                .iter()
                .filter_map(|r| r.first())
                .filter(|cell| !cell.is_null())
                .any(|cell| {
                    v_items
                        .iter()
                        .any(|vi| cmp_vals(vi, cell) == Ordering::Equal)
                });
            Ok(Val::Bool(if *not { !hit } else { hit }))
        }
        Expr::Exists { query, not } => {
            let mut sub_ctx = ctx.clone();
            sub_ctx.outer = Some(Arc::new((rel.cols.clone(), repr_row.to_vec())));
            let sub_rel = eval_query(&sub_ctx, query)?;
            let exists = !sub_rel.rows.is_empty();
            Ok(Val::Bool(if *not { !exists } else { exists }))
        }
        Expr::Subquery(query) => {
            let mut sub_ctx = ctx.clone();
            sub_ctx.outer = Some(Arc::new((rel.cols.clone(), repr_row.to_vec())));
            let sub_rel = eval_query(&sub_ctx, query)?;
            Ok(sub_rel
                .rows
                .into_iter()
                .next()
                .and_then(|r| r.into_iter().next())
                .unwrap_or(Val::Null))
        }
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => {
            let op_val = match operand {
                Some(op) => Some(eval_expr_group(ctx, rel, repr_row, group_rows, op)?),
                None => None,
            };
            for (when_e, then_e) in branches {
                let w_val = eval_expr_group(ctx, rel, repr_row, group_rows, when_e)?;
                let matched = match &op_val {
                    Some(ov) => {
                        !ov.is_null() && !w_val.is_null() && cmp_vals(ov, &w_val) == Ordering::Equal
                    }
                    None => w_val.as_bool().unwrap_or(false),
                };
                if matched {
                    return eval_expr_group(ctx, rel, repr_row, group_rows, then_e);
                }
            }
            match else_expr {
                Some(el) => eval_expr_group(ctx, rel, repr_row, group_rows, el),
                None => Ok(Val::Null),
            }
        }
        Expr::Cast { expr: inner, ty } => {
            let v = eval_expr_group(ctx, rel, repr_row, group_rows, inner)?;
            if v.is_null() {
                return Ok(Val::Null);
            }
            let t = ty.to_ascii_lowercase();
            if t.contains("int") || t == "serial" || t == "bigserial" || t == "oid" {
                v.as_i64()
                    .map(Val::Int)
                    .ok_or_else(|| format!("cannot cast {:?} to {ty}", v.as_text()))
            } else if t.contains("float")
                || t.contains("double")
                || t.contains("numeric")
                || t.contains("decimal")
                || t == "real"
            {
                v.as_f64()
                    .map(Val::Float)
                    .ok_or_else(|| format!("cannot cast {:?} to {ty}", v.as_text()))
            } else if t.starts_with("bool") {
                v.as_bool()
                    .map(Val::Bool)
                    .ok_or_else(|| format!("cannot cast {:?} to {ty}", v.as_text()))
            } else {
                Ok(Val::Str(v.as_text().unwrap_or_default()))
            }
        }
        Expr::Binary { left, op, right } => {
            if *op == BinOp::And {
                let l = eval_expr_group(ctx, rel, repr_row, group_rows, left)?.as_bool();
                if l == Some(false) {
                    return Ok(Val::Bool(false));
                }
                let r = eval_expr_group(ctx, rel, repr_row, group_rows, right)?.as_bool();
                return Ok(match (l, r) {
                    (Some(true), Some(true)) => Val::Bool(true),
                    (Some(false), _) | (_, Some(false)) => Val::Bool(false),
                    _ => Val::Null,
                });
            }
            if *op == BinOp::Or {
                let l = eval_expr_group(ctx, rel, repr_row, group_rows, left)?.as_bool();
                if l == Some(true) {
                    return Ok(Val::Bool(true));
                }
                let r = eval_expr_group(ctx, rel, repr_row, group_rows, right)?.as_bool();
                return Ok(match (l, r) {
                    (Some(true), _) | (_, Some(true)) => Val::Bool(true),
                    (Some(false), Some(false)) => Val::Bool(false),
                    _ => Val::Null,
                });
            }

            let lv = eval_expr_group(ctx, rel, repr_row, group_rows, left)?;
            let rv = eval_expr_group(ctx, rel, repr_row, group_rows, right)?;
            eval_binary_op(&lv, op, &rv)
        }
        Expr::Func {
            name,
            args,
            distinct,
            filter,
            over,
        } => {
            if over.is_some() {
                return Ok(Val::Null); // Evaluated in window pass
            }
            if is_agg_func(name) {
                let one_row = vec![repr_row.to_vec()]; // an aggregate without GROUP BY sees one row
                let rows_slice = group_rows.unwrap_or(&one_row);
                return eval_aggregate(
                    ctx,
                    rel,
                    rows_slice,
                    name,
                    args,
                    *distinct,
                    filter.as_deref(),
                );
            }
            let mut eval_args = Vec::with_capacity(args.len());
            for a in args {
                eval_args.push(eval_expr_group(ctx, rel, repr_row, group_rows, a)?);
            }
            eval_scalar_func(ctx, name, &eval_args)
        }
    }
}

fn eval_binary_op(lv: &Val, op: &BinOp, rv: &Val) -> Result<Val, String> {
    if lv.is_null() || rv.is_null() {
        return Ok(Val::Null);
    }
    match op {
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
            if let (Val::Int(a), Val::Int(b)) = (lv, rv) {
                match op {
                    BinOp::Add => return Ok(Val::Int(a.wrapping_add(*b))),
                    BinOp::Sub => return Ok(Val::Int(a.wrapping_sub(*b))),
                    BinOp::Mul => return Ok(Val::Int(a.wrapping_mul(*b))),
                    BinOp::Div => {
                        if *b == 0 {
                            return Err("division by zero".into());
                        }
                        return Ok(Val::Int(a / b));
                    }
                    BinOp::Rem => {
                        if *b == 0 {
                            return Err("division by zero".into());
                        }
                        return Ok(Val::Int(a % b));
                    }
                    _ => {}
                }
            }
            let (Some(a), Some(b)) = (lv.as_f64(), rv.as_f64()) else {
                return Ok(Val::Null);
            };
            let res = match op {
                BinOp::Add => a + b,
                BinOp::Sub => a - b,
                BinOp::Mul => a * b,
                BinOp::Div => {
                    if b == 0.0 {
                        return Err("division by zero".into());
                    }
                    a / b
                }
                BinOp::Rem => {
                    if b == 0.0 {
                        return Err("division by zero".into());
                    }
                    a % b
                }
                _ => unreachable!(),
            };
            // 58000 * 1.1 computes 63800.00000000001: a result that close to a
            // whole number is that whole number (real fractions stay)
            let res = if (res - res.round()).abs() < 1e-9 {
                res.round()
            } else {
                res
            };
            if res.fract() == 0.0 && matches!((lv, rv), (Val::Int(_), Val::Int(_))) {
                Ok(Val::Int(res as i64))
            } else {
                Ok(Val::Float(res))
            }
        }
        BinOp::Concat => Ok(Val::Str(format!(
            "{}{}",
            lv.as_text().unwrap_or_default(),
            rv.as_text().unwrap_or_default()
        ))),
        BinOp::Like | BinOp::NotLike | BinOp::ILike | BinOp::NotILike => {
            let pat = rv.as_text().unwrap_or_default();
            let ci = matches!(op, BinOp::ILike | BinOp::NotILike);
            let neg = matches!(op, BinOp::NotLike | BinOp::NotILike);
            let hit = lv
                .items()
                .iter()
                .any(|item| like_match(&item.as_text().unwrap_or_default(), &pat, ci));
            Ok(Val::Bool(if neg { !hit } else { hit }))
        }
        BinOp::Regex | BinOp::IRegex | BinOp::NotRegex => {
            let pat = rv.as_text().unwrap_or_default();
            let flags = if *op == BinOp::IRegex { "i" } else { "" };
            let neg = *op == BinOp::NotRegex;
            let hit = lv.items().iter().any(|item| {
                crate::sparql::regex::Regex::new(&pat, flags)
                    .is_ok_and(|r| r.is_match(&item.as_text().unwrap_or_default()))
            });
            Ok(Val::Bool(if neg { !hit } else { hit }))
        }
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let l_items = lv.items();
            let r_items = rv.items();
            let hit = l_items.iter().any(|a| {
                r_items.iter().any(|b| {
                    let ord = match (a.as_f64(), b.as_f64()) {
                        (Some(x), Some(y)) => x.partial_cmp(&y),
                        _ => Some(
                            a.as_text()
                                .unwrap_or_default()
                                .cmp(&b.as_text().unwrap_or_default()),
                        ),
                    };
                    let Some(o) = ord else { return false };
                    match op {
                        BinOp::Eq => o.is_eq(),
                        BinOp::Ne => o.is_ne(),
                        BinOp::Lt => o.is_lt(),
                        BinOp::Le => o.is_le(),
                        BinOp::Gt => o.is_gt(),
                        BinOp::Ge => o.is_ge(),
                        _ => false,
                    }
                })
            });
            Ok(Val::Bool(hit))
        }
        BinOp::And | BinOp::Or => unreachable!(),
    }
}

fn eval_aggregate(
    ctx: &ExecCtx,
    rel: &Rel,
    rows: &[Vec<Val>],
    name: &str,
    args: &[Expr],
    distinct: bool,
    filter: Option<&Expr>,
) -> Result<Val, String> {
    let fname = name.to_ascii_lowercase();
    let is_star = args.is_empty() || matches!(args.first(), Some(Expr::Wildcard(_)));

    let mut filtered_rows: Vec<&Vec<Val>> = Vec::with_capacity(rows.len());
    for r in rows {
        if let Some(f_expr) = filter {
            if !eval_expr(ctx, rel, r, f_expr)?.as_bool().unwrap_or(false) {
                continue;
            }
        }
        filtered_rows.push(r);
    }

    if fname == "count" && is_star {
        return Ok(Val::Int(filtered_rows.len() as i64));
    }

    let arg_expr = args
        .first()
        .ok_or_else(|| format!("{name}() requires an argument"))?;
    let mut vals = Vec::new();
    let mut seen_texts = HashSet::new();

    for r in &filtered_rows {
        let v = eval_expr(ctx, rel, r, arg_expr)?;
        if v.is_null() {
            continue;
        }
        if distinct {
            let key = v.as_text().unwrap_or_default();
            if !seen_texts.insert(key) {
                continue;
            }
        }
        vals.push(v);
    }

    match fname.as_str() {
        "count" => Ok(Val::Int(vals.len() as i64)),
        "sum" => {
            if vals.is_empty() {
                return Ok(Val::Null);
            }
            let all_int = vals.iter().all(|v| matches!(v, Val::Int(_)));
            if all_int {
                Ok(Val::Int(vals.iter().filter_map(|v| v.as_i64()).sum()))
            } else {
                let sum: f64 = vals.iter().filter_map(|v| v.as_f64()).sum();
                if sum.fract() == 0.0 {
                    Ok(Val::Int(sum as i64))
                } else {
                    Ok(Val::Float(sum))
                }
            }
        }
        "avg" => {
            let nums: Vec<f64> = vals.iter().filter_map(|v| v.as_f64()).collect();
            if nums.is_empty() {
                Ok(Val::Null)
            } else {
                let avg = nums.iter().sum::<f64>() / nums.len() as f64;
                if avg.fract() == 0.0 {
                    Ok(Val::Int(avg as i64))
                } else {
                    Ok(Val::Float(avg))
                }
            }
        }
        "min" => Ok(vals.into_iter().min_by(cmp_vals).unwrap_or(Val::Null)),
        "max" => Ok(vals.into_iter().max_by(cmp_vals).unwrap_or(Val::Null)),
        "string_agg" | "group_concat" => {
            if vals.is_empty() {
                return Ok(Val::Null);
            }
            let sep = if let Some(sep_expr) = args.get(1) {
                if let Some(first_row) = filtered_rows.first() {
                    eval_expr(ctx, rel, first_row, sep_expr)?
                        .as_text()
                        .unwrap_or_else(|| ",".into())
                } else {
                    ",".into()
                }
            } else {
                ",".into()
            };
            let joined = vals
                .into_iter()
                .filter_map(|v| v.as_text())
                .collect::<Vec<_>>()
                .join(&sep);
            Ok(Val::Str(joined))
        }
        "array_agg" => {
            if vals.is_empty() {
                return Ok(Val::Null);
            }
            let joined = vals
                .into_iter()
                .filter_map(|v| v.as_text())
                .collect::<Vec<_>>()
                .join(",");
            Ok(Val::Str(format!("{{{joined}}}")))
        }
        "bool_and" | "every" => {
            if vals.is_empty() {
                Ok(Val::Null)
            } else {
                Ok(Val::Bool(vals.iter().all(|v| v.as_bool().unwrap_or(false))))
            }
        }
        "bool_or" => {
            if vals.is_empty() {
                Ok(Val::Null)
            } else {
                Ok(Val::Bool(vals.iter().any(|v| v.as_bool().unwrap_or(false))))
            }
        }
        _ => Err(format!("unknown aggregate function \"{name}\"")),
    }
}

fn eval_scalar_func(ctx: &ExecCtx, name: &str, args: &[Val]) -> Result<Val, String> {
    let s0 = || args.first().and_then(|v| v.as_text());
    let n0 = || args.first().and_then(|v| v.as_f64());

    match name {
        "version" => Ok(Val::Str(format!(
            "PostgreSQL 15.0 (GraphRD {})",
            crate::server::VERSION
        ))),
        "current_database" | "current_catalog" => Ok(Val::Str(ctx.database.to_string())),
        "current_schema" => Ok(Val::Str("public".into())),
        "current_schemas" => Ok(Val::Str("{pg_catalog,public}".into())),
        "current_user" | "user" | "session_user" | "getpgusername" => {
            Ok(Val::Str(ctx.user.to_string()))
        }
        "pg_backend_pid" => Ok(Val::Int(std::process::id() as i64)),
        "pg_is_in_recovery" => Ok(Val::Bool(false)),
        "current_setting" => {
            let key = s0().unwrap_or_default().to_ascii_lowercase();
            let val = match key.as_str() {
                "server_version" => "15.0 (GraphRD)",
                "server_version_num" => "150000",
                "server_encoding" | "client_encoding" => "UTF8",
                "timezone" => "UTC",
                "standard_conforming_strings" => "on",
                "transaction_isolation" => "read committed",
                "search_path" => "\"$user\", public",
                _ => "",
            };
            Ok(Val::Str(val.into()))
        }
        "set_config" => Ok(args.get(1).cloned().unwrap_or(Val::Str(String::new()))),
        "format_type" => {
            let oid = args.first().and_then(|v| v.as_i64()).unwrap_or(25);
            let t = match oid {
                16 => "boolean",
                20 => "bigint",
                21 => "smallint",
                23 => "integer",
                25 => "text",
                700 => "real",
                701 => "double precision",
                1043 => "character varying",
                _ => "text",
            };
            Ok(Val::Str(t.into()))
        }
        "pg_get_expr"
        | "pg_get_indexdef"
        | "pg_get_constraintdef"
        | "obj_description"
        | "col_description"
        | "shobj_description" => Ok(Val::Null),
        "pg_table_is_visible"
        | "pg_type_is_visible"
        | "pg_function_is_visible"
        | "has_table_privilege"
        | "has_schema_privilege"
        | "has_database_privilege" => Ok(Val::Bool(true)),
        "pg_get_userbyid" => Ok(Val::Str(ctx.user.to_string())),
        "pg_typeof" => Ok(Val::Str(match args.first() {
            Some(Val::Int(_)) => "integer".into(),
            Some(Val::Float(_)) => "double precision".into(),
            Some(Val::Bool(_)) => "boolean".into(),
            Some(Val::Null) | None => "unknown".into(),
            _ => "text".into(),
        })),
        "coalesce" | "ifnull" | "nvl" => Ok(args
            .iter()
            .find(|v| !v.is_null())
            .cloned()
            .unwrap_or(Val::Null)),
        "nullif" => {
            let (Some(a), Some(b)) = (args.first(), args.get(1)) else {
                return Ok(Val::Null);
            };
            if !a.is_null() && !b.is_null() && cmp_vals(a, b) == Ordering::Equal {
                Ok(Val::Null)
            } else {
                Ok(a.clone())
            }
        }
        "greatest" => Ok(args
            .iter()
            .filter(|v| !v.is_null())
            .max_by(|a, b| cmp_vals(a, b))
            .cloned()
            .unwrap_or(Val::Null)),
        "least" => Ok(args
            .iter()
            .filter(|v| !v.is_null())
            .min_by(|a, b| cmp_vals(a, b))
            .cloned()
            .unwrap_or(Val::Null)),
        "iif" | "if" => {
            let cond = args.first().and_then(|v| v.as_bool()).unwrap_or(false);
            Ok(if cond {
                args.get(1).cloned().unwrap_or(Val::Null)
            } else {
                args.get(2).cloned().unwrap_or(Val::Null)
            })
        }
        // String functions
        "upper" | "ucase" => Ok(s0()
            .map(|s| Val::Str(s.to_uppercase()))
            .unwrap_or(Val::Null)),
        "lower" | "lcase" => Ok(s0()
            .map(|s| Val::Str(s.to_lowercase()))
            .unwrap_or(Val::Null)),
        "length" | "char_length" | "character_length" | "len" => Ok(s0()
            .map(|s| Val::Int(s.chars().count() as i64))
            .unwrap_or(Val::Null)),
        "octet_length" => Ok(s0().map(|s| Val::Int(s.len() as i64)).unwrap_or(Val::Null)),
        "trim" | "btrim" => {
            let Some(s) = s0() else { return Ok(Val::Null) };
            if let Some(chars) = args.get(1).and_then(|v| v.as_text()) {
                let pat: Vec<char> = chars.chars().collect();
                Ok(Val::Str(s.trim_matches(&pat[..]).to_string()))
            } else {
                Ok(Val::Str(s.trim().to_string()))
            }
        }
        "ltrim" => {
            let Some(s) = s0() else { return Ok(Val::Null) };
            if let Some(chars) = args.get(1).and_then(|v| v.as_text()) {
                let pat: Vec<char> = chars.chars().collect();
                Ok(Val::Str(s.trim_start_matches(&pat[..]).to_string()))
            } else {
                Ok(Val::Str(s.trim_start().to_string()))
            }
        }
        "rtrim" => {
            let Some(s) = s0() else { return Ok(Val::Null) };
            if let Some(chars) = args.get(1).and_then(|v| v.as_text()) {
                let pat: Vec<char> = chars.chars().collect();
                Ok(Val::Str(s.trim_end_matches(&pat[..]).to_string()))
            } else {
                Ok(Val::Str(s.trim_end().to_string()))
            }
        }
        "substr" | "substring" => {
            let Some(s) = s0() else { return Ok(Val::Null) };
            let chars: Vec<char> = s.chars().collect();
            let start_1 = args.get(1).and_then(|v| v.as_i64()).unwrap_or(1);
            let start = (start_1 - 1).max(0) as usize;
            if start >= chars.len() {
                return Ok(Val::Str(String::new()));
            }
            if let Some(len) = args.get(2).and_then(|v| v.as_i64()) {
                let take = len.max(0) as usize;
                Ok(Val::Str(
                    chars[start..(start + take).min(chars.len())]
                        .iter()
                        .collect(),
                ))
            } else {
                Ok(Val::Str(chars[start..].iter().collect()))
            }
        }
        "left" => {
            let Some(s) = s0() else { return Ok(Val::Null) };
            let n = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
            Ok(Val::Str(s.chars().take(n).collect()))
        }
        "right" => {
            let Some(s) = s0() else { return Ok(Val::Null) };
            let chars: Vec<char> = s.chars().collect();
            let n = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
            let skip = chars.len().saturating_sub(n);
            Ok(Val::Str(chars.into_iter().skip(skip).collect()))
        }
        "replace" => {
            let (Some(s), Some(from)) = (s0(), args.get(1).and_then(|v| v.as_text())) else {
                return Ok(Val::Null);
            };
            let to = args.get(2).and_then(|v| v.as_text()).unwrap_or_default();
            Ok(Val::Str(s.replace(&from, &to)))
        }
        "concat" => {
            let mut out = String::new();
            for a in args {
                if let Some(t) = a.as_text() {
                    out.push_str(&t);
                }
            }
            Ok(Val::Str(out))
        }
        "concat_ws" => {
            let Some(sep) = s0() else {
                return Ok(Val::Null);
            };
            let parts: Vec<String> = args.iter().skip(1).filter_map(|a| a.as_text()).collect();
            Ok(Val::Str(parts.join(&sep)))
        }
        "split_part" => {
            let (Some(s), Some(delim), Some(idx)) = (
                s0(),
                args.get(1).and_then(|v| v.as_text()),
                args.get(2).and_then(|v| v.as_i64()),
            ) else {
                return Ok(Val::Null);
            };
            if idx < 1 {
                return Err("field position must be greater than zero".into());
            }
            let part = s.split(&delim).nth((idx - 1) as usize).unwrap_or("");
            Ok(Val::Str(part.to_string()))
        }
        "position" | "strpos" => {
            let (Some(a), Some(b)) = (s0(), args.get(1).and_then(|v| v.as_text())) else {
                return Ok(Val::Null);
            };
            // position(sub in str) passes [sub, str]; strpos(str, sub) passes [str, sub]
            let (haystack, needle) = if name == "position" { (b, a) } else { (a, b) };
            let pos = haystack
                .find(&needle)
                .map(|byte_idx| haystack[..byte_idx].chars().count() as i64 + 1)
                .unwrap_or(0);
            Ok(Val::Int(pos))
        }
        "starts_with" => {
            let (Some(s), Some(prefix)) = (s0(), args.get(1).and_then(|v| v.as_text())) else {
                return Ok(Val::Null);
            };
            Ok(Val::Bool(s.starts_with(&prefix)))
        }
        "reverse" => Ok(s0()
            .map(|s| Val::Str(s.chars().rev().collect()))
            .unwrap_or(Val::Null)),
        "repeat" => {
            let (Some(s), Some(n)) = (s0(), args.get(1).and_then(|v| v.as_i64())) else {
                return Ok(Val::Null);
            };
            Ok(Val::Str(s.repeat(n.clamp(0, 10_000) as usize)))
        }
        "lpad" => {
            let (Some(s), Some(len)) = (s0(), args.get(1).and_then(|v| v.as_i64())) else {
                return Ok(Val::Null);
            };
            let fill = args
                .get(2)
                .and_then(|v| v.as_text())
                .unwrap_or_else(|| " ".into());
            let target = len.max(0) as usize;
            let cur_len = s.chars().count();
            if cur_len >= target {
                Ok(Val::Str(s.chars().take(target).collect()))
            } else {
                let pad: String = fill.chars().cycle().take(target - cur_len).collect();
                Ok(Val::Str(format!("{pad}{s}")))
            }
        }
        "rpad" => {
            let (Some(s), Some(len)) = (s0(), args.get(1).and_then(|v| v.as_i64())) else {
                return Ok(Val::Null);
            };
            let fill = args
                .get(2)
                .and_then(|v| v.as_text())
                .unwrap_or_else(|| " ".into());
            let target = len.max(0) as usize;
            let cur_len = s.chars().count();
            if cur_len >= target {
                Ok(Val::Str(s.chars().take(target).collect()))
            } else {
                let pad: String = fill.chars().cycle().take(target - cur_len).collect();
                Ok(Val::Str(format!("{s}{pad}")))
            }
        }
        "initcap" => {
            let Some(s) = s0() else { return Ok(Val::Null) };
            let mut out = String::with_capacity(s.len());
            let mut cap = true;
            for ch in s.chars() {
                if ch.is_alphanumeric() {
                    if cap {
                        out.extend(ch.to_uppercase());
                        cap = false;
                    } else {
                        out.extend(ch.to_lowercase());
                    }
                } else {
                    out.push(ch);
                    cap = true;
                }
            }
            Ok(Val::Str(out))
        }
        // Math functions
        "abs" => match args.first() {
            Some(Val::Int(i)) => Ok(Val::Int(i.abs())),
            _ => Ok(n0().map(|f| Val::Float(f.abs())).unwrap_or(Val::Null)),
        },
        "ceil" | "ceiling" => Ok(n0().map(|f| Val::Int(f.ceil() as i64)).unwrap_or(Val::Null)),
        "floor" => Ok(n0()
            .map(|f| Val::Int(f.floor() as i64))
            .unwrap_or(Val::Null)),
        "round" => {
            let Some(f) = n0() else { return Ok(Val::Null) };
            let digits = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
            if digits <= 0 {
                let factor = 10f64.powi((-digits) as i32);
                Ok(Val::Int(((f / factor).round() * factor) as i64))
            } else {
                let factor = 10f64.powi(digits as i32);
                Ok(Val::Float((f * factor).round() / factor))
            }
        }
        "trunc" | "truncate" => {
            let Some(f) = n0() else { return Ok(Val::Null) };
            let digits = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
            if digits == 0 {
                Ok(Val::Int(f.trunc() as i64))
            } else {
                let factor = 10f64.powi(digits as i32);
                Ok(Val::Float((f * factor).trunc() / factor))
            }
        }
        "power" | "pow" => {
            let (Some(a), Some(b)) = (n0(), args.get(1).and_then(|v| v.as_f64())) else {
                return Ok(Val::Null);
            };
            let r = a.powf(b);
            if r.fract() == 0.0 {
                Ok(Val::Int(r as i64))
            } else {
                Ok(Val::Float(r))
            }
        }
        "sqrt" => Ok(n0().map(|f| Val::Float(f.sqrt())).unwrap_or(Val::Null)),
        "mod" => {
            let (Some(a), Some(b)) = (args.first(), args.get(1)) else {
                return Ok(Val::Null);
            };
            eval_binary_op(a, &BinOp::Rem, b)
        }
        "sign" => Ok(n0()
            .map(|f| {
                if f > 0.0 {
                    Val::Int(1)
                } else if f < 0.0 {
                    Val::Int(-1)
                } else {
                    Val::Int(0)
                }
            })
            .unwrap_or(Val::Null)),
        "ln" => Ok(n0().map(|f| Val::Float(f.ln())).unwrap_or(Val::Null)),
        "log" | "log10" => Ok(n0().map(|f| Val::Float(f.log10())).unwrap_or(Val::Null)),
        "exp" => Ok(n0().map(|f| Val::Float(f.exp())).unwrap_or(Val::Null)),
        // Date/Time functions
        "now"
        | "current_timestamp"
        | "transaction_timestamp"
        | "statement_timestamp"
        | "clock_timestamp" => Ok(Val::Str("2026-01-01T00:00:00Z".into())),
        "current_date" => Ok(Val::Str("2026-01-01".into())),
        "current_time" => Ok(Val::Str("00:00:00".into())),
        "extract" | "date_part" => {
            let (Some(field), Some(src)) = (s0(), args.get(1).and_then(|v| v.as_text())) else {
                return Ok(Val::Null);
            };
            let f = field.to_ascii_lowercase();
            // Parse YYYY-MM-DD[THH:MM:SS]
            let parts: Vec<&str> = src
                .split(|c: char| !c.is_ascii_digit())
                .filter(|s| !s.is_empty())
                .collect();
            let val = match f.as_str() {
                "year" => parts.first().and_then(|s| s.parse::<i64>().ok()),
                "month" => parts.get(1).and_then(|s| s.parse::<i64>().ok()),
                "day" => parts.get(2).and_then(|s| s.parse::<i64>().ok()),
                "hour" => parts.get(3).and_then(|s| s.parse::<i64>().ok()).or(Some(0)),
                "minute" => parts.get(4).and_then(|s| s.parse::<i64>().ok()).or(Some(0)),
                "second" => parts.get(5).and_then(|s| s.parse::<i64>().ok()).or(Some(0)),
                _ => None,
            };
            Ok(val.map(Val::Int).unwrap_or(Val::Null))
        }
        "to_char" => Ok(s0().map(Val::Str).unwrap_or(Val::Null)),
        "array_construct" | "row_construct" => {
            let s = args
                .iter()
                .filter_map(|a| a.as_text())
                .collect::<Vec<_>>()
                .join(",");
            Ok(Val::Str(format!("{{{s}}}")))
        }
        "array_length" | "cardinality" => {
            let Some(s) = s0() else { return Ok(Val::Null) };
            let trimmed = s.trim_start_matches('{').trim_end_matches('}');
            if trimmed.is_empty() {
                Ok(Val::Int(0))
            } else {
                Ok(Val::Int(trimmed.split(',').count() as i64))
            }
        }
        _ => Err(format!("unknown function \"{name}\"")),
    }
}

fn eval_window_expr(
    ctx: &ExecCtx,
    rel: &Rel,
    rows: &[Vec<Val>],
    row_idx: usize,
    expr: &Expr,
) -> Result<Val, String> {
    match expr {
        Expr::Func {
            name,
            args,
            distinct,
            filter,
            over: Some(win),
        } => {
            let cur_row = &rows[row_idx];
            // Build partition matching current row
            let cur_part_key: Vec<Val> = win
                .partition_by
                .iter()
                .map(|pe| eval_expr(ctx, rel, cur_row, pe))
                .collect::<Result<_, _>>()?;

            let mut part_indices: Vec<usize> = Vec::new();
            for (idx, r) in rows.iter().enumerate() {
                let k: Vec<Val> = win
                    .partition_by
                    .iter()
                    .map(|pe| eval_expr(ctx, rel, r, pe))
                    .collect::<Result<_, _>>()?;
                let eq = cur_part_key.len() == k.len()
                    && cur_part_key
                        .iter()
                        .zip(&k)
                        .all(|(a, b)| cmp_vals(a, b) == Ordering::Equal);
                if eq {
                    part_indices.push(idx);
                }
            }

            // Sort partition if ORDER BY is present in OVER(...)
            if !win.order_by.is_empty() {
                let mut keyed: Vec<(Vec<Val>, usize)> = Vec::with_capacity(part_indices.len());
                for &idx in &part_indices {
                    let mut ob_vals = Vec::with_capacity(win.order_by.len());
                    for ob in &win.order_by {
                        ob_vals.push(eval_expr(ctx, rel, &rows[idx], &ob.expr)?);
                    }
                    keyed.push((ob_vals, idx));
                }
                keyed.sort_by(|(ka, _), (kb, _)| compare_order_keys(ka, kb, &win.order_by));
                part_indices = keyed.into_iter().map(|(_, idx)| idx).collect();
            }

            let pos_in_part = part_indices
                .iter()
                .position(|&idx| idx == row_idx)
                .unwrap_or(0);
            let fname = name.to_ascii_lowercase();
            match fname.as_str() {
                "row_number" => Ok(Val::Int((pos_in_part + 1) as i64)),
                "rank" | "dense_rank" => {
                    if win.order_by.is_empty() {
                        return Ok(Val::Int(1));
                    }
                    let mut rank = 1i64;
                    let mut dense_rank = 1i64;
                    for p in 1..=pos_in_part {
                        let prev_row = &rows[part_indices[p - 1]];
                        let this_row = &rows[part_indices[p]];
                        let mut same = true;
                        for ob in &win.order_by {
                            let va = eval_expr(ctx, rel, prev_row, &ob.expr)?;
                            let vb = eval_expr(ctx, rel, this_row, &ob.expr)?;
                            if cmp_vals(&va, &vb) != Ordering::Equal {
                                same = false;
                                break;
                            }
                        }
                        if !same {
                            dense_rank += 1;
                            rank = (p + 1) as i64;
                        }
                    }
                    Ok(Val::Int(if fname == "dense_rank" {
                        dense_rank
                    } else {
                        rank
                    }))
                }
                "lag" | "lead" => {
                    let arg = args.first().ok_or("LAG/LEAD requires an argument")?;
                    let offset = if let Some(off_e) = args.get(1) {
                        eval_expr(ctx, rel, cur_row, off_e)?.as_i64().unwrap_or(1)
                    } else {
                        1
                    };
                    let default_val = if let Some(def_e) = args.get(2) {
                        eval_expr(ctx, rel, cur_row, def_e)?
                    } else {
                        Val::Null
                    };
                    let target_pos = if fname == "lag" {
                        pos_in_part as isize - offset as isize
                    } else {
                        pos_in_part as isize + offset as isize
                    };
                    if target_pos < 0 || target_pos as usize >= part_indices.len() {
                        Ok(default_val)
                    } else {
                        let target_row = &rows[part_indices[target_pos as usize]];
                        eval_expr(ctx, rel, target_row, arg)
                    }
                }
                "first_value" => {
                    let arg = args.first().ok_or("FIRST_VALUE requires an argument")?;
                    let first_row = &rows[part_indices[0]];
                    eval_expr(ctx, rel, first_row, arg)
                }
                "last_value" => {
                    let arg = args.first().ok_or("LAST_VALUE requires an argument")?;
                    let last_idx = if win.order_by.is_empty() {
                        *part_indices.last().unwrap()
                    } else {
                        part_indices[pos_in_part]
                    };
                    eval_expr(ctx, rel, &rows[last_idx], arg)
                }
                "ntile" => {
                    let buckets = args
                        .first()
                        .and_then(|e| eval_expr(ctx, rel, cur_row, e).ok())
                        .and_then(|v| v.as_i64())
                        .unwrap_or(1)
                        .max(1) as usize;
                    let n = part_indices.len().max(1);
                    let bucket = (pos_in_part * buckets / n) + 1;
                    Ok(Val::Int(bucket as i64))
                }
                _ if is_agg_func(&fname) => {
                    // Running aggregate if ORDER BY is given, otherwise full partition aggregate
                    let slice_end = if win.order_by.is_empty() {
                        part_indices.len()
                    } else {
                        pos_in_part + 1
                    };
                    let frame_rows: Vec<Vec<Val>> = part_indices[..slice_end]
                        .iter()
                        .map(|&i| rows[i].clone())
                        .collect();
                    eval_aggregate(
                        ctx,
                        rel,
                        &frame_rows,
                        &fname,
                        args,
                        *distinct,
                        filter.as_deref(),
                    )
                }
                _ => Err(format!("unsupported window function \"{name}\"")),
            }
        }
        Expr::Unary { op, expr: inner } => {
            let v = eval_window_expr(ctx, rel, rows, row_idx, inner)?;
            eval_expr(
                ctx,
                rel,
                &rows[row_idx],
                &Expr::Unary {
                    op: op.clone(),
                    expr: Box::new(val_to_lit_expr(v)),
                },
            )
        }
        Expr::Binary { left, op, right } => {
            let lv = eval_window_expr(ctx, rel, rows, row_idx, left)?;
            let rv = eval_window_expr(ctx, rel, rows, row_idx, right)?;
            eval_binary_op(&lv, op, &rv)
        }
        Expr::Cast { expr: inner, ty } => {
            let v = eval_window_expr(ctx, rel, rows, row_idx, inner)?;
            eval_expr(
                ctx,
                rel,
                &rows[row_idx],
                &Expr::Cast {
                    expr: Box::new(val_to_lit_expr(v)),
                    ty: ty.clone(),
                },
            )
        }
        _ => eval_expr(ctx, rel, &rows[row_idx], expr),
    }
}

fn val_to_lit_expr(v: Val) -> Expr {
    match v {
        Val::Null => Expr::Null,
        Val::Bool(b) => Expr::Bool(b),
        Val::Int(i) => Expr::Int(i),
        Val::Float(f) => Expr::Float(f),
        Val::Str(s) => Expr::Str(s),
        Val::Multi(m) => Val::Multi(m).as_text().map(Expr::Str).unwrap_or(Expr::Null),
    }
}

fn compare_order_keys(a: &[Val], b: &[Val], specs: &[OrderByItem]) -> Ordering {
    for (idx, spec) in specs.iter().enumerate() {
        let va = &a[idx];
        let vb = &b[idx];
        match (va.is_null(), vb.is_null()) {
            (true, true) => continue,
            (true, false) => {
                let nulls_first = spec.nulls_first.unwrap_or(spec.desc);
                return if nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (false, true) => {
                let nulls_first = spec.nulls_first.unwrap_or(spec.desc);
                return if nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            (false, false) => {
                let mut o = cmp_vals(va, vb);
                if spec.desc {
                    o = o.reverse();
                }
                if o != Ordering::Equal {
                    return o;
                }
            }
        }
    }
    Ordering::Equal
}

fn eval_query(ctx: &ExecCtx, q: &Query) -> Result<Rel, String> {
    let mut local_ctx = ctx.clone();
    for cte in &q.ctes {
        let mut cte_rel = eval_query(&local_ctx, &cte.query)?;
        for (idx, col_alias) in cte.cols.iter().enumerate() {
            if let Some(c) = cte_rel.cols.get_mut(idx) {
                c.1 = col_alias.clone();
            }
        }
        local_ctx
            .ctes
            .insert(cte.name.to_ascii_lowercase(), cte_rel);
    }

    let mut rel = eval_query_body(&local_ctx, &q.body)?;

    // Top-level ORDER BY (for UNIONs or queries where ORDER BY wasn't already applied or needs re-sorting)
    if !q.order_by.is_empty() && !matches!(&q.body, QueryBody::Select(_)) {
        sort_rel_by_order(&local_ctx, &mut rel, &q.order_by, &[])?;
    }

    let dummy = Rel {
        cols: Vec::new(),
        rows: vec![Vec::new()],
    };
    let offset = match &q.offset {
        Some(e) => eval_expr(&local_ctx, &dummy, &[], e)?
            .as_i64()
            .unwrap_or(0)
            .max(0) as usize,
        None => 0,
    };
    let limit = match &q.limit {
        Some(e) => eval_expr(&local_ctx, &dummy, &[], e)?
            .as_i64()
            .map(|n| n.max(0) as usize),
        None => None,
    };

    if offset > 0 || limit.is_some() {
        rel.rows = rel
            .rows
            .into_iter()
            .skip(offset)
            .take(limit.unwrap_or(usize::MAX))
            .collect();
    }

    Ok(rel)
}

fn eval_query_body(ctx: &ExecCtx, body: &QueryBody) -> Result<Rel, String> {
    match body {
        QueryBody::Select(sel) => eval_select_ast(ctx, sel, &[]),
        QueryBody::Values(expr_rows) => {
            let empty_rel = Rel {
                cols: Vec::new(),
                rows: vec![Vec::new()],
            };
            let ncols = expr_rows.first().map(|r| r.len()).unwrap_or(0);
            let cols = (1..=ncols)
                .map(|i| (String::new(), format!("column{i}")))
                .collect();
            let mut rows = Vec::with_capacity(expr_rows.len());
            for er in expr_rows {
                let mut r = Vec::with_capacity(er.len());
                for e in er {
                    r.push(eval_expr(ctx, &empty_rel, &[], e)?);
                }
                rows.push(r);
            }
            Ok(Rel { cols, rows })
        }
        QueryBody::SetOp {
            left,
            op,
            all,
            right,
        } => {
            let l_rel = eval_query_body(ctx, left)?;
            let r_rel = eval_query_body(ctx, right)?;
            if l_rel.cols.len() != r_rel.cols.len() {
                return Err(format!(
                    "each {:?} query must have the same number of columns",
                    op
                ));
            }
            let cols = l_rel.cols;
            let row_key =
                |r: &[Val]| -> Vec<Option<String>> { r.iter().map(|v| v.as_text()).collect() };
            let mut out_rows = Vec::new();
            match op {
                SetOp::Union => {
                    if *all {
                        out_rows.extend(l_rel.rows);
                        out_rows.extend(r_rel.rows);
                    } else {
                        let mut seen = HashSet::new();
                        for r in l_rel.rows.into_iter().chain(r_rel.rows) {
                            if seen.insert(row_key(&r)) {
                                out_rows.push(r);
                            }
                        }
                    }
                }
                SetOp::Intersect => {
                    let r_keys: HashSet<Vec<Option<String>>> =
                        r_rel.rows.iter().map(|r| row_key(r)).collect();
                    let mut seen = HashSet::new();
                    for r in l_rel.rows {
                        let k = row_key(&r);
                        if r_keys.contains(&k) && (*all || seen.insert(k)) {
                            out_rows.push(r);
                        }
                    }
                }
                SetOp::Except => {
                    let r_keys: HashSet<Vec<Option<String>>> =
                        r_rel.rows.iter().map(|r| row_key(r)).collect();
                    let mut seen = HashSet::new();
                    for r in l_rel.rows {
                        let k = row_key(&r);
                        if !r_keys.contains(&k) && (*all || seen.insert(k)) {
                            out_rows.push(r);
                        }
                    }
                }
            }
            Ok(Rel {
                cols,
                rows: out_rows,
            })
        }
    }
}

/// Evaluate a SELECT query. Note: `Query::order_by` is passed down when body is `QueryBody::Select` so `ORDER BY` can reference both underlying table columns and `SELECT` aliases.
fn eval_select_ast(
    ctx: &ExecCtx,
    sel: &SelectAst,
    order_by: &[OrderByItem],
) -> Result<Rel, String> {
    // 1. FROM & JOINs
    let mut base_rel = match &sel.from {
        Some(tf) => eval_table_factor(ctx, tf)?,
        None => Rel {
            cols: Vec::new(),
            rows: vec![Vec::new()],
        },
    };
    for j in &sel.joins {
        base_rel = execute_join(ctx, base_rel, j)?;
    }

    // 2. WHERE
    if let Some(w_expr) = &sel.where_clause {
        // an unknown column reads as NULL here, not an error: the schema is the
        // data, so a predicate nobody uses anymore is simply absent
        if let Err(e) = validate_expr_cols(&base_rel, w_expr) {
            if !e.contains("does not exist") {
                return Err(e);
            }
        }
        let mut kept = Vec::with_capacity(base_rel.rows.len());
        for row in std::mem::take(&mut base_rel.rows) {
            let keep = eval_expr(ctx, &base_rel, &row, w_expr)
                .or_else(|e| {
                    if e.contains("does not exist") {
                        Ok(Val::Bool(false))
                    } else {
                        Err(e)
                    }
                })?
                .as_bool()
                .unwrap_or(false);
            if keep {
                kept.push(row);
            }
        }
        base_rel.rows = kept;
    }

    // 3. Determine if aggregation is active
    let has_agg = !sel.group_by.is_empty()
        || sel.having.is_some()
        || sel
            .projection
            .iter()
            .any(|item| expr_contains_agg(&item.expr));

    // Resolve alias/positional helper for GROUP BY / ORDER BY
    // (a free function, so it does not borrow base_rel while rows move)

    if has_agg {
        // Group rows
        let group_exprs: Vec<Expr> = sel
            .group_by
            .iter()
            .map(|e| resolve_proj_expr(e, sel, &base_rel))
            .collect();
        let mut groups: Vec<(Vec<Val>, Vec<Vec<Val>>)> = Vec::new();
        let mut key_map: HashMap<Vec<Option<String>>, usize> = HashMap::new();

        if group_exprs.is_empty() {
            // Implicit single group across all rows
            groups.push((Vec::new(), base_rel.rows.clone()));
        } else {
            for row in &base_rel.rows {
                let mut key_vals = Vec::with_capacity(group_exprs.len());
                let mut key_strs = Vec::with_capacity(group_exprs.len());
                for ge in &group_exprs {
                    let v = eval_expr(ctx, &base_rel, row, ge)?;
                    key_strs.push(v.as_text());
                    key_vals.push(v);
                }
                if let Some(&g_idx) = key_map.get(&key_strs) {
                    groups[g_idx].1.push(row.clone());
                } else {
                    let idx = groups.len();
                    key_map.insert(key_strs, idx);
                    groups.push((key_vals, vec![row.clone()]));
                }
            }
        }

        // HAVING filter
        if let Some(h_expr) = &sel.having {
            let mut filtered_groups = Vec::with_capacity(groups.len());
            for (k, g_rows) in groups {
                let repr = g_rows
                    .first()
                    .cloned()
                    .unwrap_or_else(|| vec![Val::Null; base_rel.cols.len()]);
                if eval_expr_group(ctx, &base_rel, &repr, Some(&g_rows), h_expr)?
                    .as_bool()
                    .unwrap_or(false)
                {
                    filtered_groups.push((k, g_rows));
                }
            }
            groups = filtered_groups;
        }

        // Sort groups if ORDER BY is present
        if !order_by.is_empty() {
            let mut keyed_groups: Vec<(Vec<Val>, (Vec<Val>, Vec<Vec<Val>>))> =
                Vec::with_capacity(groups.len());
            for (k, g_rows) in groups {
                let repr = g_rows
                    .first()
                    .cloned()
                    .unwrap_or_else(|| vec![Val::Null; base_rel.cols.len()]);
                let mut ob_vals = Vec::with_capacity(order_by.len());
                for ob in order_by {
                    let ob_expr = resolve_group_order_expr(&ob.expr, &sel.projection);
                    ob_vals.push(eval_expr_group(
                        ctx,
                        &base_rel,
                        &repr,
                        Some(&g_rows),
                        &ob_expr,
                    )?);
                }
                keyed_groups.push((ob_vals, (k, g_rows)));
            }
            keyed_groups.sort_by(|(ka, _), (kb, _)| compare_order_keys(ka, kb, order_by));
            groups = keyed_groups.into_iter().map(|(_, g)| g).collect();
        }

        // Project each group
        let mut out_cols = Vec::new();
        for item in &sel.projection {
            let col_name = item.alias.clone().unwrap_or_else(|| item.raw_name.clone());
            out_cols.push((String::new(), col_name));
        }
        let mut out_rows = Vec::with_capacity(groups.len());
        for (_, g_rows) in &groups {
            let repr = g_rows
                .first()
                .cloned()
                .unwrap_or_else(|| vec![Val::Null; base_rel.cols.len()]);
            let mut proj_row = Vec::with_capacity(sel.projection.len());
            for item in &sel.projection {
                proj_row.push(eval_expr_group(
                    ctx,
                    &base_rel,
                    &repr,
                    Some(g_rows),
                    &item.expr,
                )?);
            }
            out_rows.push(proj_row);
        }

        let mut res_rel = Rel {
            cols: out_cols,
            rows: out_rows,
        };
        if sel.distinct {
            dedup_rel(&mut res_rel, &sel.distinct_on, ctx)?;
        }
        return Ok(res_rel);
    }

    // 4. Non-aggregate path: Expand projection items (including `*` and `t.*`)
    let mut expanded_proj: Vec<(Expr, String)> = Vec::new();
    for item in &sel.projection {
        match &item.expr {
            Expr::Wildcard(None) => {
                for (i, (q, c)) in base_rel.cols.iter().enumerate() {
                    if q == "__hidden__" {
                        continue;
                    }
                    expanded_proj.push((
                        Expr::Col(if q.is_empty() {
                            c.clone()
                        } else {
                            format!("{q}.{c}")
                        }),
                        c.clone(),
                    ));
                    let _ = i;
                }
            }
            Expr::Wildcard(Some(prefix)) => {
                let p_short = unqualified(prefix);
                let mut found = false;
                for (q, c) in &base_rel.cols {
                    if q.eq_ignore_ascii_case(prefix) || q.eq_ignore_ascii_case(p_short) {
                        expanded_proj.push((Expr::Col(format!("{q}.{c}")), c.clone()));
                        found = true;
                    }
                }
                if !found {
                    return Err(format!("missing FROM-clause entry for table \"{prefix}\""));
                }
            }
            other => {
                validate_expr_cols(&base_rel, other)?;
                let name = item.alias.clone().unwrap_or_else(|| item.raw_name.clone());
                expanded_proj.push((other.clone(), name));
            }
        }
    }

    let has_window = expanded_proj.iter().any(|(e, _)| expr_contains_window(e));

    // Evaluate projection before or after sorting depending on window functions
    let mut projected_rows: Vec<Vec<Val>> = Vec::with_capacity(base_rel.rows.len());
    for (r_idx, row) in base_rel.rows.iter().enumerate() {
        let mut p_row = Vec::with_capacity(expanded_proj.len());
        for (expr, _) in &expanded_proj {
            if has_window && expr_contains_window(expr) {
                p_row.push(eval_window_expr(
                    ctx,
                    &base_rel,
                    &base_rel.rows,
                    r_idx,
                    expr,
                )?);
            } else {
                p_row.push(eval_expr(ctx, &base_rel, row, expr)?);
            }
        }
        projected_rows.push(p_row);
    }

    // 5. DISTINCT / DISTINCT ON
    if sel.distinct {
        if !sel.distinct_on.is_empty() {
            // Sort by ORDER BY first if present so DISTINCT ON picks the first ordered row
            if !order_by.is_empty() {
                let mut base_rows = std::mem::take(&mut base_rel.rows);
                sort_paired_rows(
                    ctx,
                    &base_rel,
                    &mut base_rows,
                    &mut projected_rows,
                    order_by,
                    &expanded_proj,
                )?;
                base_rel.rows = base_rows;
            }
            let mut seen = HashSet::new();
            let mut kept_base = Vec::new();
            let mut kept_proj = Vec::new();
            for (b_row, p_row) in base_rel.rows.iter().zip(projected_rows.iter()) {
                let mut key = Vec::with_capacity(sel.distinct_on.len());
                for de in &sel.distinct_on {
                    let resolved = resolve_proj_expr(de, sel, &base_rel);
                    key.push(eval_expr(ctx, &base_rel, b_row, &resolved)?.as_text());
                }
                if seen.insert(key) {
                    kept_base.push(b_row.clone());
                    kept_proj.push(p_row.clone());
                }
            }
            base_rel.rows = kept_base;
            projected_rows = kept_proj;
        } else {
            let mut seen = HashSet::new();
            let mut kept_base = Vec::new();
            let mut kept_proj = Vec::new();
            for (b_row, p_row) in base_rel.rows.into_iter().zip(projected_rows.into_iter()) {
                let key: Vec<Option<String>> = p_row.iter().map(|v| v.as_text()).collect();
                if seen.insert(key) {
                    kept_base.push(b_row);
                    kept_proj.push(p_row);
                }
            }
            base_rel.rows = kept_base;
            projected_rows = kept_proj;
        }
    }

    // 6. ORDER BY
    if !order_by.is_empty() {
        let mut base_rows = std::mem::take(&mut base_rel.rows);
        sort_paired_rows(
            ctx,
            &base_rel,
            &mut base_rows,
            &mut projected_rows,
            order_by,
            &expanded_proj,
        )?;
        base_rel.rows = base_rows;
    }

    let out_cols = expanded_proj
        .into_iter()
        .map(|(_, name)| (String::new(), name))
        .collect();
    Ok(Rel {
        cols: out_cols,
        rows: projected_rows,
    })
}

/// Resolve an ORDER BY / GROUP BY reference: a positional `2` or a bare name
/// that is a select-list alias stands for the aliased expression.
fn resolve_proj_expr(e: &Expr, sel: &SelectAst, base_rel: &Rel) -> Expr {
    match e {
        Expr::Int(pos) if *pos >= 1 && (*pos as usize) <= sel.projection.len() => {
            sel.projection[(*pos as usize) - 1].expr.clone()
        }
        Expr::Col(name) if !name.contains('.') => {
            if let Some(item) = sel.projection.iter().find(|p| {
                p.alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(name))
            }) {
                if base_rel.resolve(name).is_err() {
                    return item.expr.clone();
                }
            }
            e.clone()
        }
        _ => e.clone(),
    }
}

fn resolve_group_order_expr(ob_expr: &Expr, projection: &[SelectItem]) -> Expr {
    match ob_expr {
        Expr::Int(pos) if *pos >= 1 && (*pos as usize) <= projection.len() => {
            projection[(*pos as usize) - 1].expr.clone()
        }
        Expr::Col(name) => {
            if let Some(item) = projection.iter().find(|p| {
                p.alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(name))
                    || p.raw_name.eq_ignore_ascii_case(name)
            }) {
                return item.expr.clone();
            }
            ob_expr.clone()
        }
        _ => ob_expr.clone(),
    }
}

fn sort_paired_rows(
    ctx: &ExecCtx,
    base_rel: &Rel,
    base_rows: &mut Vec<Vec<Val>>,
    proj_rows: &mut Vec<Vec<Val>>,
    order_by: &[OrderByItem],
    expanded_proj: &[(Expr, String)],
) -> Result<(), String> {
    if base_rows.is_empty() {
        return Ok(());
    }
    let mut keyed: Vec<(Vec<Val>, Vec<Val>, Vec<Val>)> = Vec::with_capacity(base_rows.len());
    for (b_row, p_row) in base_rows.drain(..).zip(proj_rows.drain(..)) {
        let mut ob_vals = Vec::with_capacity(order_by.len());
        for ob in order_by {
            let v = match &ob.expr {
                Expr::Int(pos) if *pos >= 1 && (*pos as usize) <= p_row.len() => {
                    p_row[(*pos as usize) - 1].clone()
                }
                Expr::Col(name) => {
                    if let Some((idx, _)) = expanded_proj
                        .iter()
                        .enumerate()
                        .find(|(_, (_, alias))| alias.eq_ignore_ascii_case(name))
                    {
                        p_row[idx].clone()
                    } else {
                        eval_expr(ctx, base_rel, &b_row, &ob.expr)?
                    }
                }
                other => eval_expr(ctx, base_rel, &b_row, other)?,
            };
            ob_vals.push(v);
        }
        keyed.push((ob_vals, b_row, p_row));
    }
    keyed.sort_by(|(ka, _, _), (kb, _, _)| compare_order_keys(ka, kb, order_by));
    for (_, b, p) in keyed {
        base_rows.push(b);
        proj_rows.push(p);
    }
    Ok(())
}

fn sort_rel_by_order(
    ctx: &ExecCtx,
    rel: &mut Rel,
    order_by: &[OrderByItem],
    _proj: &[SelectItem],
) -> Result<(), String> {
    if rel.rows.is_empty() {
        return Ok(());
    }
    let snap_rel = Rel {
        cols: rel.cols.clone(),
        rows: Vec::new(),
    };
    let mut keyed: Vec<(Vec<Val>, Vec<Val>)> = Vec::with_capacity(rel.rows.len());
    for row in rel.rows.drain(..) {
        let mut ob_vals = Vec::with_capacity(order_by.len());
        for ob in order_by {
            let v = match &ob.expr {
                Expr::Int(pos) if *pos >= 1 && (*pos as usize) <= row.len() => {
                    row[(*pos as usize) - 1].clone()
                }
                other => eval_expr(ctx, &snap_rel, &row, other)?,
            };
            ob_vals.push(v);
        }
        keyed.push((ob_vals, row));
    }
    keyed.sort_by(|(ka, _), (kb, _)| compare_order_keys(ka, kb, order_by));
    rel.rows = keyed.into_iter().map(|(_, r)| r).collect();
    Ok(())
}

fn dedup_rel(rel: &mut Rel, _distinct_on: &[Expr], _ctx: &ExecCtx) -> Result<(), String> {
    let mut seen = HashSet::new();
    rel.rows.retain(|r| {
        let key: Vec<Option<String>> = r.iter().map(|v| v.as_text()).collect();
        seen.insert(key)
    });
    Ok(())
}

// Inject Query::order_by into SelectAst when QueryBody is a simple Select so ORDER BY can see unprojected columns.
fn eval_top_query(ctx: &ExecCtx, q: &Query) -> Result<Rel, String> {
    if let QueryBody::Select(sel) = &q.body {
        let mut local_ctx = ctx.clone();
        for cte in &q.ctes {
            let mut cte_rel = eval_top_query(&local_ctx, &cte.query)?;
            for (idx, col_alias) in cte.cols.iter().enumerate() {
                if let Some(c) = cte_rel.cols.get_mut(idx) {
                    c.1 = col_alias.clone();
                }
            }
            local_ctx
                .ctes
                .insert(cte.name.to_ascii_lowercase(), cte_rel);
        }
        let mut rel = eval_select_ast(&local_ctx, sel, &q.order_by)?;
        let dummy = Rel {
            cols: Vec::new(),
            rows: vec![Vec::new()],
        };
        let offset = match &q.offset {
            Some(e) => eval_expr(&local_ctx, &dummy, &[], e)?
                .as_i64()
                .unwrap_or(0)
                .max(0) as usize,
            None => 0,
        };
        let limit = match &q.limit {
            Some(e) => eval_expr(&local_ctx, &dummy, &[], e)?
                .as_i64()
                .map(|n| n.max(0) as usize),
            None => None,
        };
        if offset > 0 || limit.is_some() {
            rel.rows = rel
                .rows
                .into_iter()
                .skip(offset)
                .take(limit.unwrap_or(usize::MAX))
                .collect();
        }
        return Ok(rel);
    }
    eval_query(ctx, q)
}

// ------------------------------------------------------------------ views

/// Views are kept in the repository itself (so backups, time travel and a
/// cluster carry them): `<urn:graphrd:sql-view:NAME> <urn:graphrd:sql> "SELECT …"`
/// in this graph.
pub const VIEWS_GRAPH: &str = "urn:graphrd:sql-views";

const VIEW_SQL: &str = "urn:graphrd:sql";
const VIEW_PREFIX: &str = "urn:graphrd:sql-view:";

/// (name, SELECT text) of every view.
fn views(store: &Store) -> Vec<(String, String)> {
    let (Some(p), Some(g)) = (
        store.dict.lookup(&Term::iri(VIEW_SQL)),
        store.dict.lookup(&Term::iri(VIEWS_GRAPH)),
    ) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = store
        .quads_matching(None, Some(p), None, Some(g))
        .filter_map(|q| {
            Some((
                store
                    .dict
                    .term(q[0])
                    .value()
                    .strip_prefix(VIEW_PREFIX)?
                    .to_string(),
                store.dict.term(q[2]).value().to_string(),
            ))
        })
        .collect();
    out.sort();
    out
}

fn view(store: &Store, name: &str) -> Option<String> {
    views(store)
        .into_iter()
        .find(|(v, _)| v.eq_ignore_ascii_case(name))
        .map(|(_, sql)| sql)
}

/// CREATE [OR REPLACE] VIEW name AS SELECT … / DROP VIEW [IF EXISTS] name.
fn define_view(store: &mut Store, toks: &[Tok], sql: &str) -> Result<String, String> {
    let quad = |name: &str, text: &str| crate::rdf::Quad {
        s: Term::iri(format!("{VIEW_PREFIX}{name}")),
        p: Term::iri(VIEW_SQL),
        o: Term::string(text),
        g: Some(Term::iri(VIEWS_GRAPH)),
    };
    let existing = |store: &Store, name: &str| {
        views(store)
            .into_iter()
            .find(|(v, _)| v.eq_ignore_ascii_case(name))
    };
    if kw(toks.first(), "drop") {
        let if_exists = kw(toks.get(2), "if") && kw(toks.get(3), "exists");
        let Some(Tok::Word(name)) = toks.get(if if_exists { 4 } else { 2 }) else {
            return Err("expected a view name".into());
        };
        let name = unqualified(name);
        match existing(store, name) {
            Some((n, text)) => {
                store
                    .apply(&[quad(&n, &text)], &[])
                    .map_err(|e| e.to_string())?;
            }
            None if if_exists => {}
            None => return Err(format!("view \"{name}\" does not exist")),
        }
        return Ok("DROP VIEW".into());
    }
    let replace = kw(toks.get(1), "or") && kw(toks.get(2), "replace");
    let mut i = if replace { 3 } else { 1 };
    let _ =
        kw(toks.get(i), "temp") || kw(toks.get(i), "temporary") || kw(toks.get(i), "materialized");
    if !kw(toks.get(i), "view") {
        i += 1;
    }
    i += 1; // past VIEW
    if kw(toks.get(i), "if") && kw(toks.get(i + 1), "not") && kw(toks.get(i + 2), "exists") {
        i += 3;
    }
    let name = match toks.get(i) {
        Some(Tok::Word(w)) => unqualified(w).to_string(),
        _ => return Err("expected a view name".into()),
    };
    if !kw(toks.get(i + 1), "as") {
        return Err("expected CREATE VIEW name AS SELECT …".into());
    }
    let b = sql.as_bytes();
    let at = (1..b.len().saturating_sub(2))
        .find(|&p| {
            b[p - 1].is_ascii_whitespace()
                && b[p..p + 2].eq_ignore_ascii_case(b"as")
                && b[p + 2].is_ascii_whitespace()
        })
        .ok_or("expected AS")?;
    let body = sql[at + 2..]
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_string();
    let schema = Schema::build(store, 200);
    if schema.class(&name).is_some() {
        return Err(format!("relation \"{name}\" already exists (a table)"));
    }
    let old = existing(store, &name);
    if old.is_some() && !replace {
        return Err(format!("relation \"{name}\" already exists"));
    }
    let mut remove = Vec::new();
    if let Some((n, text)) = old {
        remove.push(quad(&n, &text));
    }
    store
        .apply(&remove, &[quad(&name, &body)])
        .map_err(|e| e.to_string())?;
    eval_select(store, &lex(&body)?, 0).map_err(|e| format!("the view's query fails: {e}"))?;
    Ok("CREATE VIEW".into())
}

// ------------------------------------------------------------------ writes

fn val_to_gql(v: &Val) -> GqlValue {
    match v {
        Val::Null => GqlValue::Null,
        Val::Bool(b) => GqlValue::Bool(*b),
        Val::Int(i) => GqlValue::Int(*i),
        Val::Float(f) => GqlValue::Float(*f),
        Val::Str(s) => GqlValue::Str(s.clone()),
        Val::Multi(m) => m.first().map(val_to_gql).unwrap_or(GqlValue::Null),
    }
}

fn writable_table(store: &Store, name: &str) -> Result<(), String> {
    if view(store, name).is_some() {
        return Err(format!("cannot change view \"{name}\""));
    }
    Ok(())
}

/// INSERT INTO t [(cols)] VALUES (…) | SELECT … [RETURNING …]
/// UPDATE t SET c = expr, … [WHERE …] [RETURNING …]
/// DELETE FROM t [WHERE …] [RETURNING …]
fn change(
    store: &mut Store,
    user: &str,
    database: &str,
    toks: &[Tok],
) -> Result<(Rows, String), String> {
    let mut p = Parser::new(toks);

    // Optional leading WITH ctes
    let mut ctes = HashMap::new();
    if p.eat_kw("with") {
        let _ = p.eat_kw("recursive");
        let schema_snap = Schema::build(store, 200);
        loop {
            let name = match p.next() {
                Some(Tok::Word(w)) => w.clone(),
                _ => return Err("expected CTE name".into()),
            };
            p.expect_kw("as")?;
            p.expect_sym("(")?;
            let subq = p.parse_query()?;
            p.expect_sym(")")?;
            let ctx = ExecCtx {
                store,
                schema: &schema_snap,
                user,
                database,
                ctes: ctes.clone(),
                outer: None,
                depth: 0,
            };
            let rel = eval_top_query(&ctx, &subq)?;
            ctes.insert(name.to_ascii_lowercase(), rel);
            if !p.eat_sym(",") {
                break;
            }
        }
    }

    let mut schema = Schema::build(store, 200);
    let write = |store: &mut Store,
                 schema: &mut Schema,
                 op: &str,
                 table: &str,
                 id: Option<String>,
                 input: &[(String, GqlValue)]|
     -> Result<String, String> {
        crate::graphql::write(
            store,
            schema,
            &crate::graphql::Write {
                op,
                type_name: table,
                class: None,
                id,
                input,
                graph: None,
            },
        )
    };

    if p.eat_kw("insert") {
        p.expect_kw("into")?;
        let table = match p.next() {
            Some(Tok::Word(w)) => unqualified(w).to_string(),
            _ => return Err("expected a table name".into()),
        };
        let _alias = if p.eat_kw("as") {
            match p.next() {
                Some(Tok::Word(w)) => Some(w.clone()),
                _ => None,
            }
        } else {
            None
        };
        writable_table(store, &table)?;
        if !p.eat_sym("(") {
            return Err("name the columns: INSERT INTO t (id, col, …) VALUES (…)".into());
        }
        let mut cols = Vec::new();
        while !p.eat_sym(")") {
            match p.next() {
                Some(Tok::Word(w)) => cols.push(unqualified(w).to_string()),
                Some(Tok::Sym(s)) if s == "," => {}
                _ => return Err("expected a column name".into()),
            }
        }

        let mut source_rows: Vec<Vec<Val>> = Vec::new();
        if p.eat_kw("values") {
            let empty_rel = Rel {
                cols: Vec::new(),
                rows: vec![Vec::new()],
            };
            let ctx = ExecCtx {
                store,
                schema: &schema,
                user,
                database,
                ctes: ctes.clone(),
                outer: None,
                depth: 0,
            };
            loop {
                p.expect_sym("(")?;
                let mut vals = Vec::new();
                while !p.eat_sym(")") {
                    if p.peek().is_none() {
                        return Err("unterminated VALUES row".into());
                    }
                    let expr = p.parse_expr()?;
                    vals.push(eval_expr(&ctx, &empty_rel, &[], &expr)?);
                    let _ = p.eat_sym(",");
                }
                if vals.len() != cols.len() {
                    return Err(format!(
                        "INSERT has {} columns but {} values",
                        cols.len(),
                        vals.len()
                    ));
                }
                source_rows.push(vals);
                if !p.eat_sym(",") {
                    break;
                }
            }
        } else if kw(p.peek(), "select") || kw(p.peek(), "with") {
            let subq = p.parse_query()?;
            let ctx = ExecCtx {
                store,
                schema: &schema,
                user,
                database,
                ctes: ctes.clone(),
                outer: None,
                depth: 0,
            };
            let rel = eval_top_query(&ctx, &subq)?;
            for r in rel.rows {
                if r.len() != cols.len() {
                    return Err(format!(
                        "INSERT has {} columns but SELECT returns {}",
                        cols.len(),
                        r.len()
                    ));
                }
                source_rows.push(r);
            }
        } else {
            return Err("expected VALUES or SELECT".into());
        }

        // Optional ON CONFLICT DO NOTHING
        let mut ignore_conflict = false;
        if p.eat_kw("on") && p.eat_kw("conflict") {
            while !p.eat_kw("do") && p.peek().is_some() {
                p.i += 1;
            }
            if p.eat_kw("nothing") {
                ignore_conflict = true;
            }
        }

        let returning = parse_returning(&mut p)?;
        while p.eat_sym(";") {}
        if let Some(t) = p.peek() {
            return Err(format!("unsupported SQL near {t:?}"));
        }

        let mut inserted_ids = Vec::new();
        for vals in source_rows {
            let mut id = None;
            let mut input = Vec::new();
            for (c, v) in cols.iter().zip(vals) {
                let gv = val_to_gql(&v);
                if c.eq_ignore_ascii_case("id") {
                    id = crate::graphql::text(&gv);
                } else {
                    input.push((c.clone(), gv));
                }
            }
            match write(store, &mut schema, "create", &table, id, &input) {
                Ok(created_id) => inserted_ids.push(created_id),
                Err(_) if ignore_conflict => {}
                Err(e) => return Err(e),
            }
        }
        let n = inserted_ids.len();
        let ret_rows = eval_returning(
            store,
            user,
            database,
            &table,
            &inserted_ids,
            &returning,
            None,
        )?;
        return Ok((ret_rows, format!("INSERT 0 {n}")));
    }

    let update = p.eat_kw("update");
    if !update {
        p.expect_kw("delete")?;
        p.expect_kw("from")?;
    }
    let table = match p.next() {
        Some(Tok::Word(w)) => unqualified(w).to_string(),
        _ => return Err("expected a table name".into()),
    };
    writable_table(store, &table)?;
    let alias = p.parse_optional_table_alias(&table);

    let mut assignments: Vec<(String, Expr)> = Vec::new();
    if update {
        p.expect_kw("set")?;
        loop {
            let col = match p.next() {
                Some(Tok::Word(w)) => unqualified(w).to_string(),
                _ => return Err("expected col = value".into()),
            };
            if col.eq_ignore_ascii_case("id") {
                return Err("the id of a row cannot be changed".into());
            }
            if !p.eat_sym("=") {
                return Err("expected col = value".into());
            }
            let val_expr = p.parse_expr()?;
            assignments.push((col, val_expr));
            if !p.eat_sym(",") {
                break;
            }
        }
    }

    let where_expr = if p.eat_kw("where") {
        Some(p.parse_expr()?)
    } else {
        None
    };
    let returning = parse_returning(&mut p)?;
    while p.eat_sym(";") {}
    if let Some(t) = p.peek() {
        return Err(format!("unsupported SQL near {t:?}"));
    }

    // Load table rows to evaluate WHERE and SET expressions
    let base_rel = {
        let ctx = ExecCtx {
            store,
            schema: &schema,
            user,
            database,
            ctes: ctes.clone(),
            outer: None,
            depth: 0,
        };
        load_base_table(&ctx, &table, &alias)?
    };
    let mut matching_rows: Vec<Vec<Val>> = Vec::new();
    {
        let ctx = ExecCtx {
            store,
            schema: &schema,
            user,
            database,
            ctes: ctes.clone(),
            outer: None,
            depth: 0,
        };
        for row in &base_rel.rows {
            let keep = match &where_expr {
                Some(we) => eval_expr(&ctx, &base_rel, row, we)?
                    .as_bool()
                    .unwrap_or(false),
                None => true,
            };
            if keep {
                matching_rows.push(row.clone());
            }
        }
    }

    let mut affected_ids = Vec::with_capacity(matching_rows.len());
    for row in &matching_rows {
        let id_str = row.first().and_then(|v| v.as_text()).unwrap_or_default();
        let mut input = Vec::with_capacity(assignments.len());
        if update {
            let ctx = ExecCtx {
                store,
                schema: &schema,
                user,
                database,
                ctes: ctes.clone(),
                outer: None,
                depth: 0,
            };
            for (col, expr) in &assignments {
                let v = eval_expr(&ctx, &base_rel, row, expr)?;
                input.push((col.clone(), val_to_gql(&v)));
            }
        }
        write(
            store,
            &mut schema,
            if update { "update" } else { "delete" },
            &table,
            Some(id_str.clone()),
            &input,
        )?;
        affected_ids.push(id_str);
    }

    let pre_snap = if !update {
        Some((base_rel.cols, matching_rows))
    } else {
        None
    };
    let ret_rows = eval_returning(
        store,
        user,
        database,
        &table,
        &affected_ids,
        &returning,
        pre_snap,
    )?;
    Ok((
        ret_rows,
        format!(
            "{} {}",
            if update { "UPDATE" } else { "DELETE" },
            affected_ids.len()
        ),
    ))
}

fn parse_returning(p: &mut Parser) -> Result<Vec<SelectItem>, String> {
    if !p.eat_kw("returning") {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    loop {
        let start = p.i;
        let expr = p.parse_expr()?;
        let raw_name = expr_default_name(&expr, &p.toks[start..p.i]);
        let alias = if p.eat_kw("as") {
            match p.next() {
                Some(Tok::Word(w)) | Some(Tok::Str(w)) => Some(w.clone()),
                _ => return Err("expected alias after AS".into()),
            }
        } else {
            None
        };
        items.push(SelectItem {
            expr,
            alias,
            raw_name,
        });
        if !p.eat_sym(",") {
            break;
        }
    }
    Ok(items)
}

fn eval_returning(
    store: &Store,
    user: &str,
    database: &str,
    table: &str,
    ids: &[String],
    returning: &[SelectItem],
    deleted_snap: Option<(Vec<(String, String)>, Vec<Vec<Val>>)>,
) -> Result<Rows, String> {
    if returning.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let schema = Schema::build(store, 200);
    let ctx = ExecCtx {
        store,
        schema: &schema,
        user,
        database,
        ctes: HashMap::new(),
        outer: None,
        depth: 0,
    };
    let rel = if let Some((cols, rows)) = deleted_snap {
        Rel { cols, rows }
    } else {
        let full = load_base_table(&ctx, table, table)?;
        let id_set: HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
        let rows = full
            .rows
            .into_iter()
            .filter(|r| {
                r.first()
                    .and_then(|v| v.as_text())
                    .is_some_and(|id| id_set.contains(id.as_str()))
            })
            .collect();
        Rel {
            cols: full.cols,
            rows,
        }
    };

    let mut out_cols = Vec::new();
    let mut out_exprs = Vec::new();
    for item in returning {
        if matches!(item.expr, Expr::Wildcard(_)) {
            for (_, c) in &rel.cols {
                out_cols.push(c.clone());
                out_exprs.push(Expr::Col(c.clone()));
            }
        } else {
            out_cols.push(item.alias.clone().unwrap_or_else(|| item.raw_name.clone()));
            out_exprs.push(item.expr.clone());
        }
    }
    let mut out_rows = Vec::with_capacity(rel.rows.len());
    for r in &rel.rows {
        let mut pr = Vec::with_capacity(out_exprs.len());
        for e in &out_exprs {
            pr.push(eval_expr(&ctx, &rel, r, e)?.as_text());
        }
        out_rows.push(pr);
    }
    Ok((out_cols, out_rows))
}

// ------------------------------------------------------------------ statements

/// A SELECT statement's rows (tables, joins, views, `sparql('…')`, the catalog).
fn eval_select(store: &Store, toks: &[Tok], depth: usize) -> Result<Rows, String> {
    let schema = Schema::build(store, 200);
    let ctx = ExecCtx {
        store,
        schema: &schema,
        user: "postgres",
        database: "postgres",
        ctes: HashMap::new(),
        outer: None,
        depth,
    };
    let mut parser = Parser::new(toks);
    let q = parser.parse_query_top()?;
    Ok(eval_top_query(&ctx, &q)?.to_rows())
}

/// Execute one SQL statement for `database` as `user`.
pub fn run(
    backend: &dyn Backend,
    user: &str,
    database: &str,
    sql: &str,
) -> Result<(Rows, String), String> {
    let toks = lex(sql)?;
    let first = match toks.first() {
        Some(Tok::Word(w)) => w.to_ascii_uppercase(),
        _ => return Err("empty or unsupported statement".into()),
    };
    let select_tag = |rows: Rows| {
        let tag = format!("SELECT {}", rows.1.len());
        (rows, tag)
    };
    match first.as_str() {
        "SET" | "BEGIN" | "COMMIT" | "END" | "ROLLBACK" | "START" | "DISCARD" | "RESET"
        | "DEALLOCATE" | "CLOSE" | "LISTEN" | "UNLISTEN" | "NOTIFY" | "SAVEPOINT" | "RELEASE" => {
            Ok(((Vec::new(), Vec::new()), first))
        }
        "SHOW" if kw(toks.get(1), "tables") => {
            let rows = backend.with_store(database, &mut |st| {
                let mut names: Vec<String> = Schema::build(st, 200)
                    .tables()
                    .into_iter()
                    .map(|(t, _)| t)
                    .collect();
                names.extend(views(st).into_iter().map(|(v, _)| v));
                Ok((
                    vec!["table_name".into()],
                    names.into_iter().map(|t| vec![Some(t)]).collect(),
                ))
            })?;
            Ok(select_tag(rows))
        }
        "SHOW" if kw(toks.get(1), "databases") || kw(toks.get(1), "schemas") => Ok(select_tag((
            vec!["name".into()],
            vec![vec![Some(database.to_string())]],
        ))),
        "SHOW" if kw(toks.get(1), "columns") && kw(toks.get(2), "from") => {
            let Some(Tok::Word(tname)) = toks.get(3) else {
                return Err("expected table name".into());
            };
            let rows = backend.with_store(database, &mut |st| {
                let schema = Schema::build(st, 200);
                let cols = table_columns(&schema, unqualified(tname));
                Ok((
                    vec!["column_name".into(), "data_type".into()],
                    cols.into_iter()
                        .map(|c| vec![Some(c), Some("text".into())])
                        .collect(),
                ))
            })?;
            Ok(select_tag(rows))
        }
        "SHOW" => {
            let setting = toks
                .get(1)
                .and_then(|t| {
                    if let Tok::Word(w) = t {
                        Some(w.to_ascii_lowercase())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let val = match setting.as_str() {
                "transaction_isolation" => "read committed",
                "server_version" => "15.0 (GraphRD)",
                "timezone" => "UTC",
                "search_path" => "\"$user\", public",
                _ => "",
            };
            Ok((
                (
                    vec![setting.max("setting".into())],
                    vec![vec![Some(val.into())]],
                ),
                "SHOW".into(),
            ))
        }
        "DESCRIBE" | "DESC" => {
            let Some(Tok::Word(tname)) = toks.get(1) else {
                return Err("expected table name".into());
            };
            let rows = backend.with_store(database, &mut |st| {
                let schema = Schema::build(st, 200);
                let cols = table_columns(&schema, unqualified(tname));
                Ok((
                    vec!["column_name".into(), "data_type".into()],
                    cols.into_iter()
                        .map(|c| vec![Some(c), Some("text".into())])
                        .collect(),
                ))
            })?;
            Ok(select_tag(rows))
        }
        "EXPLAIN" => {
            let mut idx = 1;
            let analyze = kw(toks.get(idx), "analyze");
            if analyze {
                idx += 1;
            }
            let inner_toks = &toks[idx..];
            let plan = format!(
                "GraphRD Query Plan: {:?}",
                inner_toks.iter().take(8).collect::<Vec<_>>()
            );
            if analyze {
                let _ = backend.with_store(database, &mut |st| eval_select(st, inner_toks, 0))?;
            }
            Ok((
                (vec!["QUERY PLAN".into()], vec![vec![Some(plan)]]),
                "EXPLAIN".into(),
            ))
        }
        "SELECT" | "VALUES" | "TABLE" => {
            Ok(select_tag(backend.with_store(database, &mut |st| {
                let schema = Schema::build(st, 200);
                let ctx = ExecCtx {
                    store: st,
                    schema: &schema,
                    user,
                    database,
                    ctes: HashMap::new(),
                    outer: None,
                    depth: 0,
                };
                let mut parser = Parser::new(&toks);
                let q = parser.parse_query_top()?;
                Ok(eval_top_query(&ctx, &q)?.to_rows())
            })?))
        }
        "WITH" => {
            // Check whether main statement after CTEs is SELECT or DML (INSERT/UPDATE/DELETE)
            let is_dml = is_with_dml(&toks);
            if is_dml {
                let mut tag = String::new();
                let rows = backend.with_store_mut(user, database, &mut |st| {
                    let (r, t) = change(st, user, database, &toks)?;
                    tag = t;
                    Ok(r)
                })?;
                Ok((rows, tag))
            } else {
                Ok(select_tag(backend.with_store(database, &mut |st| {
                    let schema = Schema::build(st, 200);
                    let ctx = ExecCtx {
                        store: st,
                        schema: &schema,
                        user,
                        database,
                        ctes: HashMap::new(),
                        outer: None,
                        depth: 0,
                    };
                    let mut parser = Parser::new(&toks);
                    let q = parser.parse_query_top()?;
                    Ok(eval_top_query(&ctx, &q)?.to_rows())
                })?))
            }
        }
        "CREATE" | "DROP" if toks.iter().take(6).any(|t| kw(Some(t), "view")) => {
            let mut tag = String::new();
            backend.with_store_mut(user, database, &mut |st| {
                tag = define_view(st, &toks, sql)?;
                Ok((Vec::new(), Vec::new()))
            })?;
            Ok(((Vec::new(), Vec::new()), tag))
        }
        "INSERT" | "UPDATE" | "DELETE" => {
            let mut tag = String::new();
            let rows = backend.with_store_mut(user, database, &mut |st| {
                let (r, t) = change(st, user, database, &toks)?;
                tag = t;
                Ok(r)
            })?;
            Ok((rows, tag))
        }
        "TRUNCATE" => {
            let idx = if kw(toks.get(1), "table") { 2 } else { 1 };
            let Some(Tok::Word(w)) = toks.get(idx) else {
                return Err("expected table name".into());
            };
            let del_toks = vec![
                Tok::Word("DELETE".into()),
                Tok::Word("FROM".into()),
                Tok::Word(w.clone()),
            ];
            let mut tag = String::new();
            backend.with_store_mut(user, database, &mut |st| {
                let (_, t) = change(st, user, database, &del_toks)?;
                tag = t;
                Ok((Vec::new(), Vec::new()))
            })?;
            Ok(((Vec::new(), Vec::new()), "TRUNCATE TABLE".into()))
        }
        other => Err(format!(
            "{other} is not supported (SELECT, INSERT, UPDATE, DELETE and CREATE/DROP VIEW are)"
        )),
    }
}

fn is_with_dml(toks: &[Tok]) -> bool {
    let mut depth = 0i32;
    for t in toks {
        match t {
            Tok::Sym(s) if s == "(" => depth += 1,
            Tok::Sym(s) if s == ")" => depth -= 1,
            Tok::Word(w)
                if depth == 0
                    && ["insert", "update", "delete"]
                        .iter()
                        .any(|k| w.eq_ignore_ascii_case(k)) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    struct One(std::sync::Mutex<Store>);
    impl Backend for One {
        fn authenticate(&self, _: &str, _: Option<&str>, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn auth_required(&self) -> bool {
            false
        }
        fn with_store(
            &self,
            _: &str,
            f: &mut dyn FnMut(&Store) -> Result<Rows, String>,
        ) -> Result<Rows, String> {
            f(&self.0.lock().unwrap())
        }
        fn with_store_mut(
            &self,
            _: &str,
            _: &str,
            f: &mut dyn FnMut(&mut Store) -> Result<Rows, String>,
        ) -> Result<Rows, String> {
            // all or nothing, as the server's transaction
            let mut st = self.0.lock().unwrap();
            st.start_capture();
            let r = f(&mut st);
            let (removed, inserted) = st.take_capture();
            if r.is_err() {
                st.apply(&inserted, &removed).unwrap();
            }
            r
        }
    }

    fn backend() -> One {
        let mut st = Store::memory();
        let ttl = "@prefix ex: <http://ex.org/> . ex:ann a ex:Person ; ex:name \"Ann\" ; ex:age 31 ; ex:worksFor ex:acme . ex:bob a ex:Person ; ex:name \"Bob\" ; ex:age 25 . ex:cy a ex:Person ; ex:name \"Cy O'Neil\" ; ex:age 40 ; ex:worksFor ex:acme, ex:init . ex:acme a ex:Company ; ex:name \"Acme\" . ex:init a ex:Company ; ex:name \"Initech\" .";
        st.insert(&crate::rdf::turtle::parse(ttl, "", crate::rdf::turtle::Mode::Turtle).unwrap())
            .unwrap();
        One(std::sync::Mutex::new(st))
    }

    #[test]
    fn sql_queries() {
        let b = backend();
        let ((cols, rows), tag) = run(
            &b,
            "u",
            "db",
            "SELECT name, age FROM Person WHERE age > 26 ORDER BY age DESC",
        )
        .unwrap();
        assert_eq!(cols, vec!["name", "age"]);
        assert_eq!(
            rows,
            vec![
                vec![Some("Cy O'Neil".into()), Some("40".into())],
                vec![Some("Ann".into()), Some("31".into())]
            ]
        );
        assert_eq!(tag, "SELECT 2");
        let ((_, rows), _) = run(
            &b,
            "u",
            "db",
            "SELECT COUNT(*) FROM person WHERE name LIKE 'C%'",
        )
        .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("1"));
        let ((_, rows), _) = run(
            &b,
            "u",
            "db",
            &substitute(
                "SELECT id FROM Person WHERE name = $1",
                &[Some("Cy O'Neil".into())],
            ),
        )
        .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("http://ex.org/cy"));
        let ((cols, rows), _) = run(&b, "u", "db", "SELECT * FROM sparql('SELECT ?n WHERE { ?p <http://ex.org/name> ?n } ORDER BY ?n LIMIT 1')").unwrap();
        assert_eq!(
            (cols[0].as_str(), rows[0][0].as_deref()),
            ("n", Some("Acme"))
        );
        assert!(run(&b, "u", "db", "SELECT * FROM Nope").is_err());
        assert!(run(&b, "u", "db", "DROP TABLE Person").is_err());
        assert_eq!(split_statements("SELECT 'a;b'; SELECT 1;").len(), 2);
    }

    fn rows(b: &One, sql: &str) -> Vec<Vec<Option<String>>> {
        run(b, "u", "db", sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
            .0
            .1
    }

    fn texts(r: &[Vec<Option<String>>]) -> Vec<String> {
        r.iter()
            .map(|row| {
                row.iter()
                    .map(|c| c.clone().unwrap_or("NULL".into()))
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect()
    }

    #[test]
    fn sql_joins_and_views() {
        let b = backend();
        // a multi-valued link joins each of its values; LEFT JOIN keeps unmatched rows
        let r = rows(
            &b,
            "SELECT p.name AS person, c.name AS company FROM Person p JOIN Company c ON p.worksFor = c.id ORDER BY person",
        );
        assert_eq!(
            texts(&r),
            ["Ann|Acme", "Cy O'Neil|Acme", "Cy O'Neil|Initech"]
        );
        let r = rows(
            &b,
            "SELECT p.name, c.name FROM Person AS p LEFT OUTER JOIN Company c ON c.id = p.worksFor WHERE p.age < 35 ORDER BY p.age",
        );
        assert_eq!(texts(&r), ["Bob|NULL", "Ann|Acme"]);
        assert!(
            run(
                &b,
                "u",
                "db",
                "SELECT name FROM Person p JOIN Company c ON p.worksFor = c.id"
            )
            .unwrap_err()
            .contains("ambiguous")
        );
        // views: stored, listed, queried with their own WHERE / ORDER / LIMIT, refused when broken
        assert_eq!(
            run(
                &b,
                "u",
                "db",
                "CREATE VIEW staff AS\n  -- who works where\n  SELECT p.name AS person, p.age, c.name AS employer FROM Person p JOIN Company c ON p.worksFor = c.id"
            )
            .unwrap()
            .1,
            "CREATE VIEW"
        );
        assert_eq!(
            texts(&rows(
                &b,
                "SELECT person FROM staff WHERE employer = 'Acme' ORDER BY age DESC"
            )),
            ["Cy O'Neil", "Ann"]
        );
        assert_eq!(texts(&rows(&b, "SELECT COUNT(*) FROM staff")), ["3"]);
        assert!(
            run(&b, "u", "db", "CREATE VIEW staff AS SELECT id FROM Person")
                .unwrap_err()
                .contains("already exists")
        );
        assert!(
            run(
                &b,
                "u",
                "db",
                "CREATE VIEW Person AS SELECT id FROM Company"
            )
            .unwrap_err()
            .contains("a table")
        );
        assert!(
            run(&b, "u", "db", "CREATE VIEW bad AS SELECT x FROM Nope")
                .unwrap_err()
                .contains("fails")
        );
        run(
            &b,
            "u",
            "db",
            "CREATE OR REPLACE VIEW elders AS SELECT name FROM Person WHERE age > 30",
        )
        .unwrap();
        // a view over a view, and one over SPARQL
        run(
            &b,
            "u",
            "db",
            "CREATE VIEW acme AS SELECT person FROM staff WHERE employer = 'Acme'",
        )
        .unwrap();
        assert_eq!(rows(&b, "SELECT * FROM acme").len(), 2);
        run(&b, "u", "db", "CREATE VIEW names AS SELECT * FROM sparql('SELECT ?n WHERE { ?s <http://ex.org/name> ?n }')").unwrap();
        assert_eq!(
            texts(&rows(&b, "SELECT n FROM names WHERE n LIKE 'I%'")),
            ["Initech"]
        );
        let tables = texts(&rows(&b, "SHOW TABLES"));
        assert!(
            tables.contains(&"staff".to_string()) && tables.contains(&"Person".to_string()),
            "{tables:?}"
        );
        assert!(
            texts(&rows(&b, "SELECT * FROM information_schema.tables"))
                .iter()
                .any(|s| s.starts_with("elders|public|VIEW"))
        );
        assert!(
            texts(&rows(&b, "SELECT * FROM information_schema.columns"))
                .iter()
                .any(|s| s.starts_with("staff|employer|text"))
        );
        assert_eq!(rows(&b, "SELECT * FROM information_schema.views").len(), 4);
        {
            // the views live in the graph the public constant names
            let st = b.0.lock().unwrap();
            let g = st
                .dict
                .lookup(&crate::rdf::Term::iri(VIEWS_GRAPH))
                .expect("VIEWS_GRAPH names the graph the views are in");
            assert!(st.count_graph(Some(g)) >= 4);
        }
        assert_eq!(
            run(&b, "u", "db", "DROP VIEW elders").unwrap().1,
            "DROP VIEW"
        );
        assert!(run(&b, "u", "db", "DROP VIEW elders").is_err());
        run(&b, "u", "db", "DROP VIEW IF EXISTS elders").unwrap();
        assert!(
            run(&b, "u", "db", "DELETE FROM staff")
                .unwrap_err()
                .contains("cannot change view")
        );
    }

    #[test]
    fn sql_writes() {
        let b = backend();
        let tag = |sql: &str| {
            run(&b, "u", "db", sql)
                .unwrap_or_else(|e| panic!("{sql}: {e}"))
                .1
        };
        assert_eq!(
            tag(
                "INSERT INTO Person (id, name, age, worksFor) VALUES ('http://ex.org/dan', 'Dan', 22, 'http://ex.org/init'), ('http://ex.org/eve', 'Eve', 29, NULL)"
            ),
            "INSERT 0 2"
        );
        assert_eq!(
            texts(&rows(
                &b,
                "SELECT name, age FROM Person WHERE age < 30 ORDER BY age"
            )),
            ["Dan|22", "Bob|25", "Eve|29"]
        );
        assert_eq!(
            texts(&rows(
                &b,
                "SELECT c.name FROM Person p JOIN Company c ON p.worksFor = c.id WHERE p.name = 'Dan'"
            )),
            ["Initech"]
        );
        // without an id, one is made up in the class's namespace
        assert_eq!(
            tag("INSERT INTO Person (name) VALUES ('Fay')"),
            "INSERT 0 1"
        );
        assert!(
            texts(&rows(&b, "SELECT id FROM Person WHERE name = 'Fay'"))[0]
                .starts_with("http://ex.org/person/")
        );
        assert_eq!(
            tag("UPDATE Person SET age = 26, email = 'x@ex.org' WHERE name = 'Bob'"),
            "UPDATE 1"
        );
        assert_eq!(
            texts(&rows(
                &b,
                "SELECT age, email FROM Person WHERE name = 'Bob'"
            )),
            ["26|x@ex.org"]
        );
        assert_eq!(
            tag("UPDATE Person SET worksFor = NULL WHERE worksFor IS NOT NULL"),
            "UPDATE 3"
        );
        assert_eq!(
            texts(&rows(
                &b,
                "SELECT COUNT(*) FROM Person WHERE worksFor IS NOT NULL"
            )),
            ["0"]
        );
        assert_eq!(tag("DELETE FROM Person WHERE age >= 29"), "DELETE 3");
        assert_eq!(
            texts(&rows(&b, "SELECT name FROM Person ORDER BY name")),
            ["Bob", "Dan", "Fay"]
        );
        // a failing statement changes nothing: the second row exists already
        assert!(run(&b, "u", "db", "INSERT INTO Person (id, name) VALUES ('http://ex.org/gus', 'Gus'), ('http://ex.org/bob', 'Bob')")
            .unwrap_err()
            .contains("already exists"));
        assert_eq!(
            rows(&b, "SELECT id FROM Person WHERE name = 'Gus'").len(),
            0
        );
        assert!(
            run(
                &b,
                "u",
                "db",
                "UPDATE Person SET age = 'old' WHERE name = 'Bob'"
            )
            .unwrap_err()
            .contains("not a valid")
        );
        assert!(run(&b, "u", "db", "UPDATE Person SET id = 'x'").is_err());
        assert!(
            run(&b, "u", "db", "INSERT INTO Person VALUES ('x')")
                .unwrap_err()
                .contains("name the columns")
        );
        assert!(
            run(&b, "u", "db", "INSERT INTO Robot (name) VALUES ('R2')")
                .unwrap_err()
                .contains("Unknown type")
        );
        assert_eq!(tag("DELETE FROM Company"), "DELETE 2");
    }

    #[test]
    fn sql_full_features() {
        let b = backend();
        // Aggregations, GROUP BY, HAVING
        let r = rows(
            &b,
            "SELECT c.name AS comp, COUNT(p.id) AS cnt, MIN(p.age), MAX(p.age), SUM(p.age)
             FROM Company c LEFT JOIN Person p ON p.worksFor = c.id
             GROUP BY c.name HAVING COUNT(p.id) >= 1 ORDER BY cnt DESC, comp ASC",
        );
        assert_eq!(texts(&r), ["Acme|2|31|40|71", "Initech|1|40|40|40"]);

        // CTEs, Subqueries, IN, EXISTS, CASE, Expressions, Functions, Casts
        let r = rows(
            &b,
            "WITH seniors AS (SELECT id, name, age FROM Person WHERE age >= 30)
             SELECT UPPER(name) || ' (' || (age + 1)::text || ')' AS label,
                    CASE WHEN age >= 40 THEN 'lead' ELSE 'senior' END AS tier
             FROM seniors
             WHERE id IN (SELECT id FROM Person WHERE worksFor IS NOT NULL)
               AND age BETWEEN 30 AND 45
             ORDER BY age DESC",
        );
        assert_eq!(texts(&r), ["CY O'NEIL (41)|lead", "ANN (32)|senior"]);

        // Window functions
        let r = rows(
            &b,
            "SELECT name, age, ROW_NUMBER() OVER (ORDER BY age DESC) AS rn,
                    LAG(name, 1, 'none') OVER (ORDER BY age DESC) AS prev_person
             FROM Person ORDER BY rn",
        );
        assert_eq!(
            texts(&r),
            ["Cy O'Neil|40|1|none", "Ann|31|2|Cy O'Neil", "Bob|25|3|Ann"]
        );

        // Set operations (UNION / INTERSECT / EXCEPT)
        let r = rows(
            &b,
            "SELECT name FROM Person WHERE age > 30
             UNION
             SELECT name FROM Person WHERE name = 'Bob'
             EXCEPT
             SELECT name FROM Person WHERE name = 'Ann'
             ORDER BY name",
        );
        assert_eq!(texts(&r), ["Bob", "Cy O'Neil"]);

        // RETURNING on INSERT/UPDATE/DELETE
        let r = rows(
            &b,
            "INSERT INTO Company (id, name) VALUES ('http://ex.org/globex', 'Globex') RETURNING name",
        );
        assert_eq!(texts(&r), ["Globex"]);
        let r = rows(
            &b,
            "UPDATE Person SET age = age + 5 WHERE name = 'Bob' RETURNING name, age",
        );
        assert_eq!(texts(&r), ["Bob|30"]);
    }

    #[test]
    fn wire_protocol_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let backend: Arc<dyn Backend> = Arc::new(backend());
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let _ = session(s, backend);
        });
        let mut c = TcpStream::connect(addr).unwrap();
        let mut startup = 196608i32.to_be_bytes().to_vec();
        for s in ["user", "app", "database", "db", ""] {
            startup.extend_from_slice(s.as_bytes());
            startup.push(0);
        }
        let mut m = ((startup.len() + 4) as i32).to_be_bytes().to_vec();
        m.extend(startup);
        c.write_all(&m).unwrap();
        let mut q = b"SELECT name FROM Person ORDER BY name\0".to_vec();
        let mut msgq = vec![b'Q'];
        msgq.extend(((q.len() + 4) as i32).to_be_bytes());
        msgq.append(&mut q);
        c.write_all(&msgq).unwrap();
        // read until the second ReadyForQuery
        let mut seen_rows = Vec::new();
        let mut ready = 0;
        while ready < 2 {
            let mut k = [0u8; 1];
            c.read_exact(&mut k).unwrap();
            let len = read_i32(&mut c).unwrap();
            let body = read_body(&mut c, len).unwrap();
            match k[0] {
                b'Z' => ready += 1,
                b'D' => {
                    let l = i32::from_be_bytes(body[2..6].try_into().unwrap()) as usize;
                    seen_rows.push(String::from_utf8_lossy(&body[6..6 + l]).into_owned());
                }
                b'E' => panic!("server error: {}", String::from_utf8_lossy(&body)),
                _ => {}
            }
        }
        assert_eq!(seen_rows, vec!["Ann", "Bob", "Cy O'Neil"]);
    }
}
