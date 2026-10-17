//! Certificates from an ACME certificate authority (RFC 8555, as Let's Encrypt runs it) with
//! HTTP-01 challenges, and an HTTPS server that picks its certificate by server name.
//!
//! The flow: [`Acme::new`] reads the directory, [`Acme::account`] registers (or finds) the
//! account for a P-256 key, and [`Acme::issue`] orders a certificate for some names, answers
//! the challenges through a shared [`Challenges`] (which the plain-HTTP server on port 80
//! must serve), sends a CSR for a fresh key and downloads the chain. [`Certificates`] holds
//! the certificates by name for [`serve_tls_with`] and can be updated while serving.

use crate::http::{Handler, Response};
use crate::json::Json;
use ring::rand::SystemRandom;
use ring::signature::{
    ECDSA_P256_SHA256_ASN1_SIGNING, ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub const LETS_ENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";
pub const LETS_ENCRYPT_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

fn err(m: impl Into<String>) -> io::Error {
    io::Error::other(m.into())
}

// ------------------------------------------------------------------ encodings

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(data: &[u8], url: bool) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..=chunk.len() {
            let c = B64[(n >> (18 - 6 * i) & 63) as usize];
            out.push(match (url, c) {
                (true, b'+') => '-',
                (true, b'/') => '_',
                _ => c as char,
            });
        }
        if !url {
            for _ in chunk.len()..3 {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64url(data: &[u8]) -> String {
    base64(data, true)
}

#[cfg(test)]
fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => continue,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn pem(label: &str, der: &[u8]) -> String {
    let b = base64(der, false);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// One DER element.
fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let n = content.len();
    if n < 128 {
        out.push(n as u8);
    } else {
        let bytes: Vec<u8> = n
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
    out.extend_from_slice(content);
    out
}

fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    der(0x30, &parts.concat())
}

const OID_CN: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03];
const OID_EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
const OID_P256: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
const OID_EXTENSION_REQUEST: &[u8] = &[
    0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x0E,
];
const OID_SAN: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x11];
const OID_ECDSA_SHA256: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];

/// A PKCS#10 certificate request for `names` (the first is the common name), signed by
/// `key` (PKCS#8, P-256).
pub fn csr(names: &[String], key_pkcs8: &[u8]) -> io::Result<Vec<u8>> {
    let rng = SystemRandom::new();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, key_pkcs8, &rng)
        .map_err(|e| err(format!("certificate key: {e}")))?;
    let first = names
        .first()
        .ok_or_else(|| err("a certificate needs a name"))?;
    let subject = seq(&[der(
        0x31,
        &seq(&[OID_CN.to_vec(), der(0x0C, first.as_bytes())]),
    )]);
    let mut point = vec![0u8];
    point.extend_from_slice(key.public_key().as_ref());
    let spki = seq(&[
        seq(&[OID_EC_PUBLIC_KEY.to_vec(), OID_P256.to_vec()]),
        der(0x03, &point),
    ]);
    let san: Vec<u8> = names.iter().flat_map(|n| der(0x82, n.as_bytes())).collect();
    let extensions = seq(&[seq(&[OID_SAN.to_vec(), der(0x04, &der(0x30, &san))])]);
    let attributes = der(
        0xA0,
        &seq(&[OID_EXTENSION_REQUEST.to_vec(), der(0x31, &extensions)]),
    );
    let info = seq(&[der(0x02, &[0]), subject, spki, attributes]);
    let sig = key
        .sign(&rng, &info)
        .map_err(|_| err("signing the certificate request failed"))?;
    let mut bits = vec![0u8];
    bits.extend_from_slice(sig.as_ref());
    Ok(seq(&[
        info,
        seq(&[OID_ECDSA_SHA256.to_vec()]),
        der(0x03, &bits),
    ]))
}

/// Reads one DER element at `pos`: (tag, content start, content end).
fn der_read(b: &[u8], pos: usize) -> Option<(u8, usize, usize)> {
    let tag = *b.get(pos)?;
    let first = *b.get(pos + 1)? as usize;
    let (len, start) = if first < 0x80 {
        (first, pos + 2)
    } else {
        let n = first & 0x7F;
        if n == 0 || n > 4 {
            return None;
        }
        let len = b
            .get(pos + 2..pos + 2 + n)?
            .iter()
            .fold(0usize, |a, x| a << 8 | *x as usize);
        (len, pos + 2 + n)
    };
    let end = start.checked_add(len)?;
    (end <= b.len()).then_some((tag, start, end))
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// When the first certificate in `chain_pem` expires (seconds since 1970).
pub fn not_after(chain_pem: &str) -> Option<i64> {
    let cert = CertificateDer::pem_slice_iter(chain_pem.as_bytes())
        .next()?
        .ok()?;
    let b = cert.as_ref();
    let (_, c0, _) = der_read(b, 0)?; // Certificate
    let (_, mut p, tbs_end) = der_read(b, c0)?; // TBSCertificate
    let mut fields = Vec::new();
    while p < tbs_end && fields.len() < 6 {
        let (tag, s, e) = der_read(b, p)?;
        fields.push((tag, s, e));
        p = e;
    }
    // [0] version is optional
    let skip = usize::from(fields.first()?.0 == 0xA0);
    let (_, vs, _) = *fields.get(skip + 3)?; // serial, signature, issuer, validity
    let (_, ns, ne) = der_read(b, der_read(b, vs)?.2)?; // notBefore, then notAfter
    let t = std::str::from_utf8(&b[ns..ne]).ok()?;
    let (year, rest) = if t.len() == 13 {
        let yy: i64 = t[..2].parse().ok()?;
        (if yy < 50 { 2000 + yy } else { 1900 + yy }, &t[2..])
    } else {
        (t[..4].parse().ok()?, &t[4..])
    };
    let n = |r: std::ops::Range<usize>| rest.get(r).and_then(|s| s.parse::<i64>().ok());
    Some(
        days_from_civil(year, n(0..2)?, n(2..4)?) * 86_400
            + n(4..6)? * 3600
            + n(6..8)? * 60
            + n(8..10)?,
    )
}

// ------------------------------------------------------------------ challenges and certificates

/// HTTP-01 answers: token → key authorization, served at `/.well-known/acme-challenge/{token}`.
#[derive(Default, Debug)]
pub struct Challenges(RwLock<HashMap<String, String>>);

impl Challenges {
    pub fn set(&self, token: &str, key_authorization: &str) {
        self.0
            .write()
            .unwrap()
            .insert(token.to_string(), key_authorization.to_string());
    }
    pub fn remove(&self, token: &str) {
        self.0.write().unwrap().remove(token);
    }
    /// The answer for a request path, if it is a challenge this process is waiting for.
    pub fn respond(&self, path: &str) -> Option<Response> {
        let token = path.strip_prefix("/.well-known/acme-challenge/")?;
        let answer = self.0.read().unwrap().get(token).cloned()?;
        Some(Response::new(200, "application/octet-stream", answer))
    }
}

/// Certificates by server name, changeable while serving.
#[derive(Default, Debug)]
pub struct Certificates {
    by_name: RwLock<HashMap<String, Arc<CertifiedKey>>>,
    fallback: RwLock<Option<Arc<CertifiedKey>>>,
}

fn certified(chain_pem: &str, key_pem: &str) -> io::Result<Arc<CertifiedKey>> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(chain_pem.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|e| err(format!("certificate: {e}")))?;
    if chain.is_empty() {
        return Err(err("no certificate in the PEM text"));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|e| err(format!("private key: {e}")))?;
    let signer = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| err(format!("private key: {e}")))?;
    Ok(Arc::new(CertifiedKey::new(chain, signer)))
}

impl Certificates {
    pub fn new() -> Certificates {
        Certificates::default()
    }

    /// Uses this certificate for `names`; with no names, for every name without its own.
    pub fn set(&self, names: &[String], chain_pem: &str, key_pem: &str) -> io::Result<()> {
        let ck = certified(chain_pem, key_pem)?;
        if names.is_empty() {
            *self.fallback.write().unwrap() = Some(ck);
        } else {
            let mut m = self.by_name.write().unwrap();
            for n in names {
                m.insert(n.to_ascii_lowercase(), ck.clone());
            }
        }
        Ok(())
    }

    pub fn has(&self, name: &str) -> bool {
        self.by_name
            .read()
            .unwrap()
            .contains_key(&name.to_ascii_lowercase())
    }
}

impl rustls::server::ResolvesServerCert for Certificates {
    fn resolve(&self, hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        if let Some(name) = hello.server_name() {
            let name = name.to_ascii_lowercase();
            let m = self.by_name.read().unwrap();
            if let Some(ck) = m.get(&name) {
                return Some(ck.clone());
            }
            if let Some((_, parent)) = name.split_once('.') {
                if let Some(ck) = m.get(&format!("*.{parent}")) {
                    return Some(ck.clone());
                }
            }
        }
        self.fallback.read().unwrap().clone()
    }
}

/// Serves HTTPS, choosing the certificate for each connection from `certs`.
pub fn serve_tls_with(addr: &str, handler: Handler, certs: Arc<Certificates>) -> io::Result<()> {
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| err(e.to_string()))?
    .with_no_client_auth()
    .with_cert_resolver(certs);
    crate::http::serve_tls_config(addr, handler, Arc::new(config))
}

// ------------------------------------------------------------------ the ACME client

pub struct Acme {
    directory: Json,
    key: EcdsaKeyPair,
    rng: SystemRandom,
    kid: Option<String>,
    nonce: Option<String>,
    challenges: Arc<Challenges>,
    /// How long to wait between status checks.
    pub poll: Duration,
    /// How many status checks before giving up.
    pub attempts: usize,
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// A new account or certificate key (PKCS#8, P-256).
pub fn new_key() -> io::Result<Vec<u8>> {
    EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
        .map(|d| d.as_ref().to_vec())
        .map_err(|_| err("generating a key failed"))
}

impl Acme {
    /// Reads the directory at `directory_url`; `account_key` is a PKCS#8 P-256 key (from
    /// [`new_key`], kept by the caller so the account stays the same).
    pub fn new(
        directory_url: &str,
        account_key: &[u8],
        challenges: Arc<Challenges>,
    ) -> io::Result<Acme> {
        let rng = SystemRandom::new();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, account_key, &rng)
            .map_err(|e| err(format!("account key: {e}")))?;
        let (status, _, body) =
            crate::http::request(directory_url, "GET", &[], b"", Duration::from_secs(30))
                .map_err(err)?;
        if status != 200 {
            return Err(err(format!(
                "ACME directory {directory_url}: HTTP {status}"
            )));
        }
        let directory = Json::parse(&String::from_utf8_lossy(&body))
            .map_err(|e| err(format!("ACME directory: {e}")))?;
        Ok(Acme {
            directory,
            key,
            rng,
            kid: None,
            nonce: None,
            challenges,
            poll: Duration::from_secs(2),
            attempts: 60,
        })
    }

    fn url(&self, name: &str) -> io::Result<String> {
        self.directory
            .get(name)
            .and_then(Json::as_str)
            .map(str::to_string)
            .ok_or_else(|| err(format!("the ACME directory has no {name}")))
    }

    fn jwk(&self) -> Json {
        let p = self.key.public_key().as_ref();
        Json::obj()
            .with("crv", "P-256")
            .with("kty", "EC")
            .with("x", base64url(&p[1..33]))
            .with("y", base64url(&p[33..65]))
    }

    /// RFC 7638 thumbprint of the account key, part of every key authorization.
    pub fn thumbprint(&self) -> String {
        let j = self.jwk();
        let canonical = format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
            j.str_at(&["x"]),
            j.str_at(&["y"])
        );
        base64url(ring::digest::digest(&ring::digest::SHA256, canonical.as_bytes()).as_ref())
    }

    fn fresh_nonce(&mut self) -> io::Result<String> {
        if let Some(n) = self.nonce.take() {
            return Ok(n);
        }
        let (_, h, _) = crate::http::request(
            &self.url("newNonce")?,
            "HEAD",
            &[],
            b"",
            Duration::from_secs(30),
        )
        .map_err(err)?;
        header(&h, "replay-nonce")
            .map(str::to_string)
            .ok_or_else(|| err("the ACME server sent no nonce"))
    }

    /// A signed POST; `payload` None is a POST-as-GET.
    fn post(
        &mut self,
        url: &str,
        payload: Option<&Json>,
    ) -> io::Result<(u16, Vec<(String, String)>, Vec<u8>)> {
        let mut retries = 0;
        loop {
            let mut protected = Json::obj()
                .with("alg", "ES256")
                .with("nonce", self.fresh_nonce()?)
                .with("url", url);
            protected = match &self.kid {
                Some(kid) => protected.with("kid", kid.as_str()),
                None => protected.with("jwk", self.jwk()),
            };
            let p64 = base64url(protected.to_string().as_bytes());
            let b64 = payload
                .map(|p| base64url(p.to_string().as_bytes()))
                .unwrap_or_default();
            let sig = self
                .key
                .sign(&self.rng, format!("{p64}.{b64}").as_bytes())
                .map_err(|_| err("signing failed"))?;
            let body = Json::obj()
                .with("protected", p64)
                .with("payload", b64)
                .with("signature", base64url(sig.as_ref()))
                .to_string();
            let headers = [(
                "Content-Type".to_string(),
                "application/jose+json".to_string(),
            )];
            let (status, h, b) = crate::http::request(
                url,
                "POST",
                &headers,
                body.as_bytes(),
                Duration::from_secs(60),
            )
            .map_err(err)?;
            self.nonce = header(&h, "replay-nonce").map(str::to_string);
            // a server may refuse a nonce at any time; the answer carries a fresh one
            if status == 400 && retries < 3 && String::from_utf8_lossy(&b).contains("badNonce") {
                retries += 1;
                continue;
            }
            if status >= 400 {
                let problem = Json::parse(&String::from_utf8_lossy(&b)).ok();
                let detail = problem
                    .as_ref()
                    .map(|p| p.str_at(&["detail"]).to_string())
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| String::from_utf8_lossy(&b).chars().take(300).collect());
                return Err(err(format!("ACME {url}: HTTP {status}: {detail}")));
            }
            return Ok((status, h, b));
        }
    }

    fn post_json(
        &mut self,
        url: &str,
        payload: Option<&Json>,
    ) -> io::Result<(Json, Vec<(String, String)>)> {
        let (_, h, b) = self.post(url, payload)?;
        let j = if b.is_empty() {
            Json::obj()
        } else {
            Json::parse(&String::from_utf8_lossy(&b))
                .map_err(|e| err(format!("ACME {url}: {e}")))?
        };
        Ok((j, h))
    }

    /// Registers the account key (or finds its existing account) and agrees to the terms.
    pub fn account(&mut self, contact_email: Option<&str>) -> io::Result<()> {
        let mut req = Json::obj().with("termsOfServiceAgreed", true);
        if let Some(e) = contact_email {
            req = req.with(
                "contact",
                Json::Arr(vec![Json::from(format!("mailto:{e}"))]),
            );
        }
        let url = self.url("newAccount")?;
        let (_, h) = self.post_json(&url, Some(&req))?;
        self.kid = Some(
            header(&h, "location")
                .ok_or_else(|| err("the ACME server sent no account URL"))?
                .to_string(),
        );
        Ok(())
    }

    /// Polls `url` until its status leaves `pending`/`processing`.
    fn wait(&mut self, url: &str) -> io::Result<Json> {
        for _ in 0..self.attempts {
            let (j, _) = self.post_json(url, None)?;
            match j.str_at(&["status"]) {
                "pending" | "processing" => std::thread::sleep(self.poll),
                _ => return Ok(j),
            }
        }
        Err(err(format!(
            "ACME {url}: still not done after {} checks",
            self.attempts
        )))
    }

    /// Orders and downloads a certificate for `names`: (chain PEM, private key PEM).
    pub fn issue(&mut self, names: &[String]) -> io::Result<(String, String)> {
        if self.kid.is_none() {
            self.account(None)?;
        }
        let ids: Vec<Json> = names
            .iter()
            .map(|n| Json::obj().with("type", "dns").with("value", n.as_str()))
            .collect();
        let url = self.url("newOrder")?;
        let (order, h) =
            self.post_json(&url, Some(&Json::obj().with("identifiers", Json::Arr(ids))))?;
        let order_url = header(&h, "location")
            .ok_or_else(|| err("the ACME server sent no order URL"))?
            .to_string();
        let thumb = self.thumbprint();
        let authorizations: Vec<String> = order
            .get("authorizations")
            .and_then(Json::as_array)
            .unwrap_or(&[])
            .iter()
            .filter_map(|a| a.as_str().map(str::to_string))
            .collect();
        for authz_url in authorizations {
            let (authz, _) = self.post_json(&authz_url, None)?;
            if authz.str_at(&["status"]) == "valid" {
                continue;
            }
            let challenge = authz
                .get("challenges")
                .and_then(Json::as_array)
                .unwrap_or(&[])
                .iter()
                .find(|c| c.str_at(&["type"]) == "http-01")
                .cloned()
                .ok_or_else(|| {
                    err(format!(
                        "{} offers no http-01 challenge",
                        authz.str_at(&["identifier", "value"])
                    ))
                })?;
            let token = challenge.str_at(&["token"]).to_string();
            self.challenges.set(&token, &format!("{token}.{thumb}"));
            let result = (|| {
                self.post_json(challenge.str_at(&["url"]), Some(&Json::obj()))?;
                let done = self.wait(&authz_url)?;
                if done.str_at(&["status"]) != "valid" {
                    let why = done
                        .get("challenges")
                        .and_then(Json::as_array)
                        .unwrap_or(&[])
                        .iter()
                        .map(|c| c.str_at(&["error", "detail"]).to_string())
                        .find(|d| !d.is_empty())
                        .unwrap_or_default();
                    return Err(err(format!(
                        "{} was not validated: {why}",
                        authz.str_at(&["identifier", "value"])
                    )));
                }
                Ok(())
            })();
            self.challenges.remove(&token);
            result?;
        }
        let key = new_key()?;
        let csr = csr(names, &key)?;
        let finalize = order.str_at(&["finalize"]).to_string();
        self.post_json(&finalize, Some(&Json::obj().with("csr", base64url(&csr))))?;
        let done = self.wait(&order_url)?;
        if done.str_at(&["status"]) != "valid" {
            return Err(err(format!(
                "the order ended as {}",
                done.str_at(&["status"])
            )));
        }
        let (_, _, chain) = self.post(done.str_at(&["certificate"]), None)?;
        let chain = String::from_utf8(chain).map_err(|_| err("the certificate is not PEM text"))?;
        if not_after(&chain).is_none() {
            return Err(err("the certificate chain cannot be read"));
        }
        Ok((chain, pem("PRIVATE KEY", &key)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ECDSA_P256_SHA256_ASN1, ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};
    use std::sync::Mutex;

    const CERT: &str = include_str!("../tests/fixtures/acme-test-cert.pem");

    #[test]
    fn encodings_and_expiry() {
        assert_eq!(
            base64(b"any carnal pleas", false),
            "YW55IGNhcm5hbCBwbGVhcw=="
        );
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64url_decode("-_8").unwrap(), vec![0xfb, 0xff]);
        assert_eq!(not_after(CERT), Some(1_799_792_415)); // Jan 12 22:20:15 2027 GMT
        assert_eq!(der(0x04, &[0; 200])[..3], [0x04, 0x81, 200]);
    }

    #[test]
    fn csr_is_signed_by_its_key() {
        let key = new_key().unwrap();
        let names = vec!["cargo.example".to_string(), "www.cargo.example".to_string()];
        let c = csr(&names, &key).unwrap();
        let (_, s, _) = der_read(&c, 0).unwrap();
        let (_, _, info_end) = der_read(&c, s).unwrap();
        let info = &c[s..info_end];
        let (_, _, alg_end) = der_read(&c, info_end).unwrap();
        let (_, bs, be) = der_read(&c, alg_end).unwrap();
        let pk =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &key, &SystemRandom::new())
                .unwrap();
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, pk.public_key().as_ref())
            .verify(info, &c[bs + 1..be])
            .unwrap();
        assert!(
            c.windows(names[1].len()).any(|w| w == names[1].as_bytes()),
            "the names are in the request"
        );
    }

    /// A small ACME server that checks every signature and nonce and validates challenges
    /// by asking the client's `Challenges`, as a CA would over port 80.
    struct Ca {
        base: String,
        challenges: Arc<Challenges>,
        nonces: Mutex<Vec<String>>,
        key: Mutex<Option<Vec<u8>>>,
        thumb: Mutex<String>,
        validated: Mutex<bool>,
        csr: Mutex<Vec<u8>>,
        counter: Mutex<u32>,
    }

    impl Ca {
        fn nonce(&self) -> String {
            let mut c = self.counter.lock().unwrap();
            *c += 1;
            let n = format!("nonce-{c}");
            self.nonces.lock().unwrap().push(n.clone());
            n
        }

        fn handle(&self, req: &crate::http::Request) -> Response {
            let ok = |status: u16, j: Json| {
                Response::json(status, j).with_header("Replay-Nonce", &self.nonce())
            };
            match (req.method.as_str(), req.path.as_str()) {
                ("GET", "/dir") => Response::json(
                    200,
                    Json::obj()
                        .with("newNonce", format!("{}/nonce", self.base))
                        .with("newAccount", format!("{}/account", self.base))
                        .with("newOrder", format!("{}/order", self.base)),
                ),
                ("HEAD", "/nonce") => {
                    Response::empty(200).with_header("Replay-Nonce", &self.nonce())
                }
                ("POST", path) => {
                    let jws = Json::parse(&String::from_utf8_lossy(&req.body)).unwrap();
                    let protected = Json::parse(&String::from_utf8_lossy(
                        &base64url_decode(jws.str_at(&["protected"])).unwrap(),
                    ))
                    .unwrap();
                    // every nonce works once
                    let nonce = protected.str_at(&["nonce"]).to_string();
                    let mut ns = self.nonces.lock().unwrap();
                    let Some(i) = ns.iter().position(|n| *n == nonce) else {
                        drop(ns);
                        return ok(
                            400,
                            Json::obj().with("type", "urn:ietf:params:acme:error:badNonce"),
                        );
                    };
                    ns.remove(i);
                    drop(ns);
                    assert_eq!(protected.str_at(&["url"]), format!("{}{path}", self.base));
                    if path == "/account" {
                        let jwk = protected.get("jwk").unwrap();
                        let mut point = vec![4u8];
                        point.extend(base64url_decode(jwk.str_at(&["x"])).unwrap());
                        point.extend(base64url_decode(jwk.str_at(&["y"])).unwrap());
                        *self.key.lock().unwrap() = Some(point);
                        let canonical = format!(
                            r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
                            jwk.str_at(&["x"]),
                            jwk.str_at(&["y"])
                        );
                        *self.thumb.lock().unwrap() = base64url(
                            ring::digest::digest(&ring::digest::SHA256, canonical.as_bytes())
                                .as_ref(),
                        );
                    } else {
                        assert_eq!(protected.str_at(&["kid"]), format!("{}/acct/1", self.base));
                    }
                    let key = self.key.lock().unwrap().clone().unwrap();
                    let signed = format!(
                        "{}.{}",
                        jws.str_at(&["protected"]),
                        jws.str_at(&["payload"])
                    );
                    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &key)
                        .verify(
                            signed.as_bytes(),
                            &base64url_decode(jws.str_at(&["signature"])).unwrap(),
                        )
                        .expect("a valid JWS signature");
                    let payload = base64url_decode(jws.str_at(&["payload"])).unwrap();
                    let b = &self.base;
                    match path {
                        "/account" => ok(201, Json::obj().with("status", "valid"))
                            .with_header("Location", &format!("{b}/acct/1")),
                        "/order" => ok(
                            201,
                            Json::obj()
                                .with("status", "pending")
                                .with(
                                    "authorizations",
                                    Json::Arr(vec![Json::from(format!("{b}/authz/1"))]),
                                )
                                .with("finalize", format!("{b}/finalize/1")),
                        )
                        .with_header("Location", &format!("{b}/order/1")),
                        "/authz/1" => {
                            let status = if *self.validated.lock().unwrap() {
                                "valid"
                            } else {
                                "pending"
                            };
                            ok(
                                200,
                                Json::obj()
                                    .with("status", status)
                                    .with(
                                        "identifier",
                                        Json::obj()
                                            .with("type", "dns")
                                            .with("value", "cargo.example"),
                                    )
                                    .with(
                                        "challenges",
                                        Json::Arr(vec![
                                            Json::obj()
                                                .with("type", "http-01")
                                                .with("url", format!("{b}/chall/1"))
                                                .with("token", "tok123"),
                                        ]),
                                    ),
                            )
                        }
                        "/chall/1" => {
                            // as the CA fetches http://cargo.example/.well-known/acme-challenge/tok123
                            let answer = self
                                .challenges
                                .respond("/.well-known/acme-challenge/tok123")
                                .expect("the challenge is served");
                            assert_eq!(
                                String::from_utf8_lossy(&answer.body),
                                format!("tok123.{}", self.thumb.lock().unwrap())
                            );
                            *self.validated.lock().unwrap() = true;
                            ok(200, Json::obj().with("status", "valid"))
                        }
                        "/finalize/1" => {
                            let j = Json::parse(&String::from_utf8_lossy(&payload)).unwrap();
                            *self.csr.lock().unwrap() =
                                base64url_decode(j.str_at(&["csr"])).unwrap();
                            ok(200, Json::obj().with("status", "processing"))
                        }
                        "/order/1" => {
                            let status = if self.csr.lock().unwrap().is_empty() {
                                "pending"
                            } else {
                                "valid"
                            };
                            ok(
                                200,
                                Json::obj()
                                    .with("status", status)
                                    .with("certificate", format!("{b}/cert/1")),
                            )
                        }
                        "/cert/1" => Response::new(200, "application/pem-certificate-chain", CERT)
                            .with_header("Replay-Nonce", &self.nonce()),
                        _ => ok(404, Json::obj()),
                    }
                }
                _ => Response::empty(404),
            }
        }
    }

    #[test]
    fn issues_a_certificate() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let challenges = Arc::new(Challenges::default());
        let ca = Arc::new(Ca {
            base: format!("http://127.0.0.1:{port}"),
            challenges: challenges.clone(),
            nonces: Mutex::default(),
            key: Mutex::default(),
            thumb: Mutex::default(),
            validated: Mutex::default(),
            csr: Mutex::default(),
            counter: Mutex::default(),
        });
        let server = ca.clone();
        std::thread::spawn(move || {
            crate::http::serve(
                &format!("127.0.0.1:{port}"),
                Arc::new(move |r| server.handle(r)),
            )
        });
        std::thread::sleep(Duration::from_millis(100));

        let mut acme = Acme::new(
            &format!("{}/dir", ca.base),
            &new_key().unwrap(),
            challenges.clone(),
        )
        .unwrap();
        acme.poll = Duration::from_millis(5);
        acme.account(Some("ops@cargo.example")).unwrap();
        let (chain, key) = acme.issue(&["cargo.example".to_string()]).unwrap();
        assert_eq!(not_after(&chain), Some(1_799_792_415));
        assert!(key.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert!(!ca.csr.lock().unwrap().is_empty(), "a CSR was sent");
        assert!(
            challenges
                .respond("/.well-known/acme-challenge/tok123")
                .is_none(),
            "the answer is withdrawn"
        );

        let certs = Certificates::new();
        certs
            .set(&["cargo.example".to_string()], &chain, &key)
            .unwrap();
        assert!(certs.has("Cargo.Example"));
        assert!(certs.set(&[], "not a certificate", &key).is_err());
    }
}
