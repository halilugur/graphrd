//! Small HTTP/1.1 server on std::net: one thread per connection, keep-alive,
//! Content-Length and chunked request bodies.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Limits for the server: the largest request body and the most connections served at once
/// (a server facing the internet sets them lower; see `set_limits`).
static MAX_REQUEST_BODY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(MAX_BODY_BYTES);
static MAX_CONNECTIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);
static OPEN_CONNECTIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Sets the largest request body (bytes) and the most connections served at once; further
/// connections are answered 503 and closed.
pub fn set_limits(max_request_body: usize, max_connections: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    MAX_REQUEST_BODY.store(max_request_body, Relaxed);
    MAX_CONNECTIONS.store(max_connections, Relaxed);
}

/// Counts a connection while it is served.
struct Admitted;

impl Drop for Admitted {
    fn drop(&mut self) {
        OPEN_CONNECTIONS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

fn admit(stream: &mut TcpStream) -> Option<Admitted> {
    use std::sync::atomic::Ordering::Relaxed;
    if OPEN_CONNECTIONS.fetch_add(1, Relaxed) >= MAX_CONNECTIONS.load(Relaxed) {
        OPEN_CONNECTIONS.fetch_sub(1, Relaxed);
        let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\nRetry-After: 1\r\n\r\n");
        return None;
    }
    Some(Admitted)
}

/// A parsed HTTP request.
pub struct Request {
/// The method, upper-case.
    pub method: String,
    /// Path without the query string, percent-decoded.
    pub path: String,
/// The query string as (name, value) pairs, both percent-decoded.
    pub query: Vec<(String, String)>,
    /// Header names are lower-cased.
    pub headers: Vec<(String, String)>,
/// The body, undecoded.
    pub body: Vec<u8>,
}

impl Request {
/// A header's value, by lower-cased name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

/// The first query parameter with this name.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Media type of the body, lower-cased, without parameters.
    pub fn content_type(&self) -> String {
        self.header("content-type")
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    }

    /// Query parameters plus urlencoded form fields from the body.
    pub fn params(&self) -> Vec<(String, String)> {
        let mut all = self.query.clone();
        if self.content_type() == "application/x-www-form-urlencoded" {
            all.extend(parse_query(&String::from_utf8_lossy(&self.body)));
        }
        all
    }

/// The body as UTF-8, lossily.
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// An HTTP response to write back.
pub struct Response {
/// The status code.
    pub status: u16,
/// Headers to send, in order.
    pub headers: Vec<(String, String)>,
/// The body, undecoded.
    pub body: Vec<u8>,
}

impl Response {
/// A response with a content type and body.
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Response {
            status,
            headers: vec![("Content-Type".into(), content_type.into())],
            body: body.into(),
        }
    }
/// A JSON response.
    pub fn json(status: u16, body: impl ToString) -> Self {
        Self::new(status, "application/json;charset=UTF-8", body.to_string())
    }
/// A plain-text response.
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::new(status, "text/plain;charset=UTF-8", body.into())
    }
/// A response with a status and no body or content type.
    pub fn empty(status: u16) -> Self {
        Response { status, headers: Vec::new(), body: Vec::new() }
    }
/// The same response with one more header.
    pub fn with_header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        207 => "Multi-Status",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "",
    }
}

/// Replace `%XX` escapes; with `plus_as_space`, also `+` (a form body).
pub fn percent_decode(s: &str, plus_as_space: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode everything but the unreserved characters.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-encode a path, keeping '/' separators.
pub fn percent_encode_path(p: &str) -> String {
    p.split('/').map(percent_encode).collect::<Vec<_>>().join("/")
}

/// Split a query string into (name, value) pairs, both percent-decoded.
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k, true), percent_decode(v, true))
        })
        .collect()
}

/// What every request is answered by.
pub type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;

/// Connection threads get a generous stack: query parsing and evaluation recurse over the
/// query tree (bounded by the parser's nesting limits). The memory is reserved, not used.
const STACK: usize = 64 << 20;

/// Start a thread with the generous connection stack.
pub fn spawn(f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().stack_size(STACK).spawn(f) {
        eprintln!("cannot start a connection thread: {e}");
    }
}

type Job = Box<dyn FnOnce() + Send>;

/// Connection threads are kept for a while after their connection ends and reused, so a
/// client that opens a connection per request does not pay for a new thread each time.
struct Pool {
    jobs: Mutex<std::sync::mpsc::Receiver<Job>>,
    send: Mutex<std::sync::mpsc::Sender<Job>>,
    /// Idle threads not yet promised a job.
    idle: std::sync::atomic::AtomicUsize,
}

static POOL: std::sync::OnceLock<Arc<Pool>> = std::sync::OnceLock::new();

fn pool() -> &'static Arc<Pool> {
    POOL.get_or_init(|| {
        let (send, jobs) = std::sync::mpsc::channel();
        Arc::new(Pool { jobs: Mutex::new(jobs), send: Mutex::new(send), idle: Default::default() })
    })
}

/// Run a connection on an idle pooled thread, or a new one.
pub fn run_pooled(f: impl FnOnce() + Send + 'static) {
    use std::sync::atomic::Ordering::SeqCst;
    let pool = pool();
    // promise the job to one idle thread; it cannot leave the pool while promised
    if pool.idle.fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1)).is_ok() {
        // the receiver lives as long as the process, so sending cannot fail
        let _ = pool.send.lock().unwrap_or_else(|e| e.into_inner()).send(Box::new(f));
        return;
    }
    let pool = pool.clone();
    spawn(move || {
        f();
        pool_worker(&pool);
    });
}

/// Start `n` connection threads ahead of the first connections (each makes its first
/// allocation now, so the allocator's per-thread setup is not paid by a request).
pub fn prestart(n: usize) {
    for _ in 0..n {
        let pool = pool().clone();
        spawn(move || {
            drop(std::hint::black_box(vec![0u8; 4096]));
            pool_worker(&pool);
        });
    }
}

fn pool_worker(pool: &Pool) {
    use std::sync::atomic::Ordering::SeqCst;
    {
        loop {
            pool.idle.fetch_add(1, SeqCst);
            loop {
                let next = pool.jobs.lock().unwrap_or_else(|e| e.into_inner()).recv_timeout(Duration::from_secs(30));
                match next {
                    Ok(job) => {
                        job();
                        break;
                    }
                    // leave after a quiet while, unless a job was promised meanwhile
                    Err(_) if pool.idle.fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1)).is_ok() => return,
                    Err(_) => continue,
                }
            }
        }
    }
}

/// Answer every connection on `addr`; a pooled thread runs each one.
pub fn serve(addr: &str, handler: Handler) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let Some(admitted) = admit(&mut stream) else { continue };
        let handler = handler.clone();
        run_pooled(move || {
            let _admitted = admitted;
            if prepare(&stream).is_ok() {
                let _ = connection(stream, handler);
            }
        });
    }
    Ok(())
}

fn prepare(stream: &TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_nodelay(true).ok();
    Ok(())
}

/// Serve HTTPS with the given certificate chain and private key (PEM files).
#[cfg(feature = "tls")]
pub fn serve_tls(addr: &str, handler: Handler, cert_pem: &std::path::Path, key_pem: &std::path::Path) -> io::Result<()> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let bad = |e: String| io::Error::new(io::ErrorKind::InvalidInput, e);
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_pem)
        .map_err(|e| bad(format!("{}: {e}", cert_pem.display())))?
        .collect::<Result<_, _>>()
        .map_err(|e| bad(format!("{}: {e}", cert_pem.display())))?;
    let key = PrivateKeyDer::from_pem_file(key_pem).map_err(|e| bad(format!("{}: {e}", key_pem.display())))?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| bad(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| bad(e.to_string()))?;
    serve_tls_config(addr, handler, Arc::new(config))
}

/// Serve HTTPS with a prepared TLS configuration (for example one that picks certificates by
/// server name; see `acme::serve_tls_with`).
#[cfg(feature = "tls")]
pub fn serve_tls_config(addr: &str, handler: Handler, config: Arc<rustls::ServerConfig>) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let Some(admitted) = admit(&mut stream) else { continue };
        let (handler, config) = (handler.clone(), config.clone());
        spawn(move || {
            let _admitted = admitted;
            if prepare(&stream).is_err() {
                return;
            }
            let Ok(conn) = rustls::ServerConnection::new(config) else { return };
            let _ = connection(rustls::StreamOwned::new(conn, stream), handler);
        });
    }
    Ok(())
}

fn read_line<R: BufRead>(r: &mut R, budget: &mut usize) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    let n = r.by_ref().take(*budget as u64).read_until(b'\n', &mut line)?;
    if n == 0 {
        return Ok(None);
    }
    *budget = budget.saturating_sub(n);
    if !line.ends_with(b"\n") {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "header too long"));
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    Ok(Some(String::from_utf8_lossy(&line).into_owned()))
}

fn connection<S: Read + Write>(stream: S, handler: Handler) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    loop {
        let mut budget = MAX_HEADER_BYTES;
        let Some(line) = read_line(&mut reader, &mut budget)? else { return Ok(()) };
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(method), Some(target), version) = (parts.next(), parts.next(), parts.next())
        else {
            return write_response(reader.get_mut(), Response::text(400, "bad request line"), false);
        };
        let http10 = version == Some("HTTP/1.0");
        let mut headers = Vec::new();
        loop {
            let Some(h) = read_line(&mut reader, &mut budget)? else { return Ok(()) };
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let hdr = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
        if hdr("expect").is_some_and(|e| e.eq_ignore_ascii_case("100-continue")) {
            reader.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        }
        let body = if hdr("transfer-encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked")) {
            read_chunked(&mut reader, MAX_REQUEST_BODY.load(std::sync::atomic::Ordering::Relaxed))?
        } else {
            let len: usize = hdr("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
            if len > MAX_REQUEST_BODY.load(std::sync::atomic::Ordering::Relaxed) {
                return write_response(reader.get_mut(), Response::text(413, "body too large"), false);
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body)?;
            body
        };
        let keep_alive = match hdr("connection").map(|c| c.to_ascii_lowercase()) {
            Some(c) if c.contains("close") => false,
            Some(c) if c.contains("keep-alive") => true,
            _ => !http10,
        };
        let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
        let req = Request {
            method: method.to_ascii_uppercase(),
            path: percent_decode(raw_path, false),
            query: parse_query(raw_query),
            headers,
            body,
        };
        let head_only = req.method == "HEAD";
        let mut resp = handler(&req);
        if head_only {
            let len = resp.body.len();
            resp.body.clear();
            resp.headers.push(("Content-Length".into(), len.to_string()));
        }
        write_response(reader.get_mut(), resp, keep_alive)?;
        if !keep_alive {
            return Ok(());
        }
    }
}

fn read_chunked<R: BufRead>(r: &mut R, max: usize) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut budget = 1024;
        let line = read_line(r, &mut budget)?.unwrap_or_default();
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        if size == 0 {
            // trailers
            loop {
                let mut budget = MAX_HEADER_BYTES;
                match read_line(r, &mut budget)? {
                    Some(l) if !l.is_empty() => continue,
                    _ => break,
                }
            }
            return Ok(body);
        }
        if body.len().saturating_add(size) > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        r.read_exact(&mut body[start..])?;
        let mut crlf = [0u8; 2];
        r.read_exact(&mut crlf)?;
    }
}

fn write_response<W: Write>(w: &mut W, resp: Response, keep_alive: bool) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, reason(resp.status));
    let has_len = resp.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
    for (k, v) in &resp.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has_len {
        head.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    }
    head.push_str(if keep_alive { "Connection: keep-alive\r\n\r\n" } else { "Connection: close\r\n\r\n" });
    if resp.body.len() <= 64 * 1024 {
        // one write, so a small response leaves in one segment
        let mut all = head.into_bytes();
        all.extend_from_slice(&resp.body);
        w.write_all(&all)?;
    } else {
        w.write_all(head.as_bytes())?;
        w.write_all(&resp.body)?;
    }
    w.flush()
}

/// Gzip-compresses a text response when the client accepts it and it is worth it.
pub fn compress(req: &Request, mut resp: Response) -> Response {
    let accepts = req.header("accept-encoding").is_some_and(|a| {
        a.split(',').any(|e| {
            let mut parts = e.split(';');
            let name = parts.next().unwrap_or("").trim();
            let q0 = parts.any(|p| p.trim().replace(' ', "") == "q=0");
            (name == "gzip" || name == "*") && !q0
        })
    });
    let header = |r: &Response, k: &str| r.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.to_ascii_lowercase());
    let textual = header(&resp, "content-type").is_some_and(|ct| ct.starts_with("text/") || ct.contains("json") || ct.contains("javascript") || ct.contains("xml") || ct.contains("turtle") || ct.contains("svg") || ct.contains("n-quads") || ct.contains("n-triples") || ct.contains("trig"));
    // very large bodies (bulk exports) are sent as they are: compressing them costs more than it saves
    if accepts && textual && (1024..=32 << 20).contains(&resp.body.len()) && header(&resp, "content-encoding").is_none() {
        resp.body = crate::gzip::gzip(&resp.body);
        resp.headers.push(("Content-Encoding".into(), "gzip".into()));
        resp.headers.push(("Vary".into(), "Accept-Encoding".into()));
    }
    resp
}

/// One part of a multipart/form-data body.
pub struct Part {
/// The form field's name.
    pub name: String,
/// The client's file name, when the part is a file upload.
    pub filename: Option<String>,
/// The part's own content type.
    pub content_type: Option<String>,
/// The part's bytes.
    pub data: Vec<u8>,
}

/// The parts of a `multipart/form-data` body; an empty list when the request is
/// not multipart.
pub fn parse_multipart(req: &Request) -> Vec<Part> {
    let ct = req.header("content-type").unwrap_or("");
    let Some(boundary) = ct
        .split(';')
        .find_map(|p| p.trim().strip_prefix("boundary="))
        .map(|b| b.trim_matches('"').to_string())
    else {
        return Vec::new();
    };
    let delim = format!("--{boundary}").into_bytes();
    let body = &req.body;
    let mut parts = Vec::new();
    let mut pos = match find(body, &delim, 0) {
        Some(p) => p + delim.len(),
        None => return parts,
    };
    loop {
        if body[pos..].starts_with(b"--") {
            break;
        }
        // skip CRLF after delimiter
        if body[pos..].starts_with(b"\r\n") {
            pos += 2;
        }
        let Some(hdr_end) = find(body, b"\r\n\r\n", pos) else { break };
        let headers = String::from_utf8_lossy(&body[pos..hdr_end]).into_owned();
        let data_start = hdr_end + 4;
        let Some(next) = find(body, &delim, data_start) else { break };
        let data_end = if next >= 2 && &body[next - 2..next] == b"\r\n" { next - 2 } else { next };
        let mut part = Part { name: String::new(), filename: None, content_type: None, data: body[data_start..data_end].to_vec() };
        for line in headers.split("\r\n") {
            let Some((k, v)) = line.split_once(':') else { continue };
            match k.trim().to_ascii_lowercase().as_str() {
                "content-disposition" => {
                    for attr in v.split(';') {
                        let attr = attr.trim();
                        if let Some(n) = attr.strip_prefix("name=") {
                            part.name = n.trim_matches('"').to_string();
                        } else if let Some(f) = attr.strip_prefix("filename=") {
                            part.filename = Some(f.trim_matches('"').to_string());
                        }
                    }
                }
                "content-type" => part.content_type = Some(v.trim().to_string()),
                _ => {}
            }
        }
        parts.push(part);
        pos = next + delim.len();
        if pos >= body.len() {
            break;
        }
    }
    parts
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

/// One HTTP/1.1 request over a fresh connection (cluster RPC and request
/// forwarding). Returns (status, headers, body).
pub fn call(
    host: &str,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
) -> io::Result<(u16, Vec<(String, String)>, Vec<u8>)> {
    let stream = connect(host, timeout)?;
    exchange(stream, host, method, path, headers, body)
}

fn connect(host: &str, timeout: Duration) -> io::Result<TcpStream> {
    use std::net::ToSocketAddrs;
    // try every address: `localhost` may resolve to ::1 first while the server listens on 127.0.0.1
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("cannot resolve {host}"));
    let mut connected = None;
    for addr in host.to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => {
                connected = Some(s);
                break;
            }
            Err(e) => last = e,
        }
    }
    let stream = connected.ok_or(last)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_nodelay(true).ok();
    Ok(stream)
}

/// Send a request and read the whole response over any stream.
fn exchange<S: Read + Write>(
    stream: S,
    host: &str,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> io::Result<(u16, Vec<(String, String)>, Vec<u8>)> {
    let mut r = BufReader::new(stream);
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        if !["content-length", "connection", "host", "transfer-encoding"].contains(&k.to_ascii_lowercase().as_str()) {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    head.push_str("\r\n");
    let w = r.get_mut();
    w.write_all(head.as_bytes())?;
    w.write_all(body)?;
    w.flush()?;
    let mut budget = MAX_HEADER_BYTES;
    let status_line = read_line(&mut r, &mut budget)?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no response"))?;
    let status: u16 = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(502);
    let mut hdrs = Vec::new();
    let mut len: Option<usize> = None;
    let mut chunked = false;
    while let Some(h) = read_line(&mut r, &mut budget)? {
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim().to_string(), v.trim().to_string());
            if k.eq_ignore_ascii_case("content-length") {
                len = v.parse().ok();
            }
            if k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
            hdrs.push((k, v));
        }
    }
    // these never have a body, whatever the headers say
    let body = if method.eq_ignore_ascii_case("HEAD") || status == 204 || status == 304 || (100..200).contains(&status) {
        Vec::new()
    } else if chunked {
        read_chunked(&mut r, MAX_BODY_BYTES)?
    } else if let Some(n) = len {
        if n > MAX_BODY_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "response too large"));
        }
        let mut b = vec![0u8; n];
        r.read_exact(&mut b)?;
        b
    } else {
        let mut b = Vec::new();
        // a TLS peer may close without close_notify; keep what arrived
        match r.read_to_end(&mut b) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && !b.is_empty() => {}
            Err(e) => return Err(e),
        }
        b
    };
    Ok((status, hdrs, body))
}

/// GET a URL (http:// always; https:// when built with the `tls` feature),
/// following up to five redirects. Returns (content type, body).
pub fn fetch(url: &str, accept: &str, timeout: Duration) -> Result<(String, Vec<u8>), String> {
    fetch_with(url, "GET", accept, None, timeout)
}

/// An HTTP(S) request to an absolute URL, following redirects; `body` is (content type, bytes).
/// Returns the response's content type and body.
pub fn fetch_with(url: &str, method: &str, accept: &str, body: Option<(&str, &[u8])>, timeout: Duration) -> Result<(String, Vec<u8>), String> {
    let mut url = url.to_string();
    let mut method = method.to_string();
    let mut body = body;
    for _ in 0..6 {
        let mut headers = vec![("Accept".to_string(), accept.to_string()), ("User-Agent".to_string(), "GraphRD".to_string())];
        if let Some((ctype, _)) = body {
            headers.push(("Content-Type".to_string(), ctype.to_string()));
        }
        let payload = body.map_or(&b""[..], |(_, b)| b);
        let (status, hdrs, resp) = request(&url, &method, &headers, payload, timeout)?;
        let header = |n: &str| hdrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone());
        if (300..400).contains(&status) {
            let loc = header("location").ok_or_else(|| format!("{url}: redirect without Location"))?;
            url = crate::rdf::resolve_iri(&url, &loc);
            if !matches!(status, 307 | 308) {
                // other redirects turn the request into a GET
                method = "GET".into();
                body = None;
            }
            continue;
        }
        if status >= 400 {
            let detail = String::from_utf8_lossy(&resp[..resp.len().min(300)]).trim().to_string();
            return Err(format!("{url}: HTTP {status}{}", if detail.is_empty() { String::new() } else { format!(": {detail}") }));
        }
        return Ok((header("content-type").unwrap_or_default(), resp));
    }
    Err(format!("{url}: too many redirects"))
}

/// One HTTP(S) request to an absolute URL with the given headers. Does not follow redirects
/// and returns every status as is: (status, headers, body).
pub fn request(url: &str, method: &str, headers: &[(String, String)], body: &[u8], timeout: Duration) -> Result<(u16, Vec<(String, String)>, Vec<u8>), String> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| format!("not an absolute URL: {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let path = path.split('#').next().unwrap_or("/");
    match scheme.to_ascii_lowercase().as_str() {
        "http" => {
            let host = if authority.contains(':') { authority.to_string() } else { format!("{authority}:80") };
            let stream = connect(&host, timeout).map_err(|e| format!("{url}: {e}"))?;
            exchange(stream, authority, method, path, headers, body).map_err(|e| format!("{url}: {e}"))
        }
        "https" => https_request(authority, method, path, headers, body, timeout).map_err(|e| format!("{url}: {e}")),
        other => Err(format!("unsupported URL scheme {other}")),
    }
}

#[cfg(feature = "tls")]
fn https_request(authority: &str, method: &str, path: &str, headers: &[(String, String)], body: &[u8], timeout: Duration) -> io::Result<(u16, Vec<(String, String)>, Vec<u8>)> {
    let host_only = authority.rsplit_once(':').map_or(authority, |(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { authority });
    let addr = if authority.len() > host_only.len() { authority.to_string() } else { format!("{authority}:443") };
    let mut roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    // SSL_CERT_FILE adds certificate authorities (a company CA, or a test CA)
    if let Ok(file) = std::env::var("SSL_CERT_FILE") {
        use rustls::pki_types::pem::PemObject;
        let certs = rustls::pki_types::CertificateDer::pem_file_iter(&file).map_err(|e| io::Error::other(format!("SSL_CERT_FILE {file}: {e}")))?;
        for c in certs.flatten() {
            roots.add(c).map_err(|e| io::Error::other(format!("SSL_CERT_FILE {file}: {e}")))?;
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| io::Error::other(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host_only.to_string()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| io::Error::other(e.to_string()))?;
    let tcp = connect(&addr, timeout)?;
    exchange(rustls::StreamOwned::new(conn, tcp), authority, method, path, headers, body)
}

#[cfg(not(feature = "tls"))]
fn https_request(_: &str, _: &str, _: &str, _: &[(String, String)], _: &[u8], _: Duration) -> io::Result<(u16, Vec<(String, String)>, Vec<u8>)> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "https needs a build with `--features tls`"))
}

/// `application/x-www-form-urlencoded` body for the given pairs.
pub fn encode_form(pairs: &[(String, String)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v))).collect::<Vec<_>>().join("&")
}
