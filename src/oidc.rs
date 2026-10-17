//! OpenID Connect: GraphRD as a resource server. Clients send an access token from an identity
//! provider (Keycloak, Entra ID, Okta, Auth0...) as `Authorization: Bearer <JWT>`; GraphRD checks
//! its RS256 signature against the provider's published keys (JWKS, found through discovery),
//! its issuer, audience and lifetime, and takes the user name and roles from its claims.
//!
//! RSA verification needs modular exponentiation of 2048 to 4096-bit numbers: done here with a
//! small Montgomery multiplication, so the default build keeps no third-party crates.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::json::Json;

// ------------------------------------------------------------------ big numbers

/// Little-endian 32-bit limbs.
type Limbs = Vec<u32>;

fn from_be(bytes: &[u8]) -> Limbs {
    let mut out = vec![0u32; bytes.len().div_ceil(4).max(1)];
    for (i, b) in bytes.iter().rev().enumerate() {
        out[i / 4] |= (*b as u32) << (8 * (i % 4));
    }
    out
}

fn to_be(x: &[u32], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    for i in 0..len {
        let limb = i / 4;
        if limb < x.len() {
            out[len - 1 - i] = (x[limb] >> (8 * (i % 4))) as u8;
        }
    }
    out
}

/// a >= b, both `n` limbs.
fn geq(a: &[u32], b: &[u32]) -> bool {
    for i in (0..a.len()).rev() {
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    true
}

/// a -= b in place (a >= b).
fn sub_in(a: &mut [u32], b: &[u32]) {
    let mut borrow = 0i64;
    for i in 0..a.len() {
        let d = a[i] as i64 - b[i] as i64 - borrow;
        borrow = (d < 0) as i64;
        a[i] = d.rem_euclid(1 << 32) as u32;
    }
}

/// Montgomery product a * b * R^-1 mod n (CIOS), all `n.len()` limbs.
fn mont_mul(a: &[u32], b: &[u32], n: &[u32], n0: u32) -> Limbs {
    let s = n.len();
    let mut t = vec![0u32; s + 2];
    for i in 0..s {
        let mut c = 0u64;
        for j in 0..s {
            let x = t[j] as u64 + a[j] as u64 * b[i] as u64 + c;
            t[j] = x as u32;
            c = x >> 32;
        }
        let x = t[s] as u64 + c;
        t[s] = x as u32;
        t[s + 1] = (x >> 32) as u32;
        let m = t[0].wrapping_mul(n0);
        let x = t[0] as u64 + m as u64 * n[0] as u64;
        let mut c = x >> 32;
        for j in 1..s {
            let x = t[j] as u64 + m as u64 * n[j] as u64 + c;
            t[j - 1] = x as u32;
            c = x >> 32;
        }
        let x = t[s] as u64 + c;
        t[s - 1] = x as u32;
        t[s] = t[s + 1] + (x >> 32) as u32;
        t[s + 1] = 0;
    }
    let mut r: Limbs = t[..s].to_vec();
    if t[s] != 0 || geq(&r, n) {
        sub_in(&mut r, n);
    }
    r
}

/// base^exp mod n for an odd modulus n.
fn mod_pow(base: &[u8], exp: &[u8], modulus: &[u8]) -> Option<Vec<u8>> {
    let n = from_be(modulus);
    let s = n.len();
    if n[0] & 1 == 0 {
        return None;
    }
    let mut b = from_be(base);
    b.resize(s, 0);
    if geq(&b, &n) {
        return None; // a signature is always below the modulus
    }
    // n0 = -n^-1 mod 2^32 (Newton iteration)
    let mut inv: u32 = 1;
    for _ in 0..5 {
        inv = inv.wrapping_mul(2u32.wrapping_sub(n[0].wrapping_mul(inv)));
    }
    let n0 = inv.wrapping_neg();
    // R^2 mod n, by doubling 1 2 * 32 * s times
    let mut r2 = vec![0u32; s];
    r2[0] = 1;
    for _ in 0..(64 * s) {
        let mut carry = 0u32;
        for limb in r2.iter_mut() {
            let next = *limb >> 31;
            *limb = (*limb << 1) | carry;
            carry = next;
        }
        if carry == 1 || geq(&r2, &n) {
            sub_in(&mut r2, &n);
        }
    }
    let mut one = vec![0u32; s];
    one[0] = 1;
    let bm = mont_mul(&b, &r2, &n, n0);
    let mut acc = mont_mul(&one, &r2, &n, n0); // 1 in Montgomery form
    for byte in exp {
        for bit in (0..8).rev() {
            acc = mont_mul(&acc, &acc, &n, n0);
            if byte >> bit & 1 == 1 {
                acc = mont_mul(&acc, &bm, &n, n0);
            }
        }
    }
    Some(to_be(&mont_mul(&acc, &one, &n, n0), modulus.len()))
}

/// RSASSA-PKCS1-v1_5 with SHA-256 (JWT `RS256`).
pub fn rsa_verify_sha256(n: &[u8], e: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let n = strip_zeros(n);
    if signature.len() != n.len() || n.len() < 256 {
        return false; // keys below 2048 bits are not accepted
    }
    let Some(em) = mod_pow(signature, e, n) else {
        return false;
    };
    const DIGEST_INFO: [u8; 19] = [
        0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01,
        0x05, 0x00, 0x04, 0x20,
    ];
    let hash = crate::sparql::hash::sha256(message);
    let pad = n.len() - 3 - DIGEST_INFO.len() - hash.len();
    let mut expected = vec![0x00, 0x01];
    expected.extend(std::iter::repeat_n(0xff, pad));
    expected.push(0x00);
    expected.extend_from_slice(&DIGEST_INFO);
    expected.extend_from_slice(&hash);
    em == expected
}

fn strip_zeros(b: &[u8]) -> &[u8] {
    let i = b.iter().position(|&x| x != 0).unwrap_or(b.len());
    &b[i..]
}

// ------------------------------------------------------------------ JWT

/// Base64url-decode (unpadded); `None` on a character outside the alphabet.
pub fn base64url(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        } as u32)
    };
    let bytes: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= val(c)? << (18 - 6 * i);
        }
        let n = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => return None,
        };
        for i in 0..n {
            out.push((acc >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

/// The identity provider and how its tokens map to GraphRD users.
pub struct Provider {
    /// Who signs the tokens.
    pub issuer: String,
    /// Accepted when it is in `aud` or is `azp`; None accepts any audience.
    pub audience: Option<String>,
    /// Claim with the user name (default `preferred_username`, then `sub`).
    pub username_claim: String,
    /// Dotted path of the claim listing roles (default `realm_access.roles`).
    pub roles_claim: String,
    /// Provider role -> GraphRD authorities; a role that is not mapped is taken as an authority
    /// itself when it looks like one (ROLE_..., READ_REPO_..., WRITE_REPO_...).
    pub role_map: HashMap<String, Vec<String>>,
    keys: Mutex<(HashMap<String, (Vec<u8>, Vec<u8>)>, Option<Instant>)>,
    verified: Mutex<HashMap<[u8; 32], (String, Vec<String>, i64)>>,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl Provider {
    /// A provider from the serve options: issuer, audience, roles claim and role map.
    pub fn new(
        issuer: &str,
        audience: Option<String>,
        roles_claim: Option<String>,
        role_map: &str,
    ) -> Provider {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for pair in role_map.split(',').filter(|p| !p.trim().is_empty()) {
            if let Some((role, auth)) = pair.split_once('=') {
                map.entry(role.trim().to_string())
                    .or_default()
                    .extend(auth.split('+').map(|a| a.trim().to_string()));
            }
        }
        Provider {
            issuer: issuer.trim_end_matches('/').to_string(),
            audience,
            username_claim: "preferred_username".into(),
            roles_claim: roles_claim.unwrap_or_else(|| "realm_access.roles".into()),
            role_map: map,
            keys: Mutex::new((HashMap::new(), None)),
            verified: Mutex::new(HashMap::new()),
        }
    }

    /// The provider's signing keys (kid -> (n, e)), fetched through discovery; fetched again at
    /// most every 30 seconds when a token names an unknown key (key rotation).
    fn key(&self, kid: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
        let mut keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = keys.0.get(kid) {
            return Ok(k.clone());
        }
        if keys
            .1
            .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        {
            return Err(format!("unknown signing key {kid}"));
        }
        keys.1 = Some(Instant::now());
        let get = |url: &str| -> Result<Json, String> {
            let (_, body) = crate::http::fetch_with(
                url,
                "GET",
                "application/json",
                None,
                Duration::from_secs(10),
            )?;
            Json::parse(&String::from_utf8_lossy(&body))
        };
        let discovery = get(&format!("{}/.well-known/openid-configuration", self.issuer))?;
        let jwks = get(discovery.str_at(&["jwks_uri"]))?;
        keys.0.clear();
        for k in jwks.get("keys").and_then(|k| k.as_array()).unwrap_or(&[]) {
            if k.str_at(&["kty"]) != "RSA"
                || (k.get("use").is_some() && k.str_at(&["use"]) != "sig")
            {
                continue;
            }
            if let (Some(n), Some(e)) = (base64url(k.str_at(&["n"])), base64url(k.str_at(&["e"]))) {
                keys.0.insert(k.str_at(&["kid"]).to_string(), (n, e));
            }
        }
        keys.0
            .get(kid)
            .cloned()
            .ok_or_else(|| format!("unknown signing key {kid}"))
    }

    /// The user name and authorities of a valid token.
    pub fn verify(&self, token: &str) -> Result<(String, Vec<String>), String> {
        let digest = crate::sparql::hash::sha256(token.as_bytes());
        if let Some((name, auth, exp)) = self
            .verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&digest)
            .cloned()
        {
            if exp > now_secs() {
                return Ok((name, auth));
            }
        }
        let mut parts = token.split('.');
        let (Some(h), Some(p), Some(sig), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("not a JWT".into());
        };
        let header = Json::parse(&String::from_utf8_lossy(&base64url(h).ok_or("bad header")?))?;
        if header.str_at(&["alg"]) != "RS256" {
            return Err(format!(
                "signature algorithm {:?} is not accepted (RS256)",
                header.str_at(&["alg"])
            ));
        }
        let (n, e) = self.key(header.str_at(&["kid"]))?;
        let signature = base64url(sig).ok_or("bad signature encoding")?;
        if !rsa_verify_sha256(&n, &e, format!("{h}.{p}").as_bytes(), &signature) {
            return Err("bad signature".into());
        }
        let claims = Json::parse(&String::from_utf8_lossy(
            &base64url(p).ok_or("bad payload")?,
        ))?;
        let num = |k: &str| claims.get(k).and_then(|v| v.as_f64()).map(|v| v as i64);
        let now = now_secs();
        let exp = num("exp").ok_or("the token has no expiry")?;
        if exp + 30 < now {
            return Err("the token has expired".into());
        }
        if num("nbf").is_some_and(|nbf| nbf > now + 30) {
            return Err("the token is not valid yet".into());
        }
        if claims.str_at(&["iss"]).trim_end_matches('/') != self.issuer {
            return Err(format!(
                "issuer {:?} is not {}",
                claims.str_at(&["iss"]),
                self.issuer
            ));
        }
        if let Some(aud) = &self.audience {
            let in_aud = match claims.get("aud") {
                Some(Json::Str(a)) => a == aud,
                Some(Json::Arr(a)) => a.iter().any(|x| x.as_str() == Some(aud)),
                _ => false,
            };
            if !in_aud && claims.str_at(&["azp"]) != aud {
                return Err(format!("the token is not for audience {aud}"));
            }
        }
        let name = [self.username_claim.as_str(), "sub"]
            .iter()
            .map(|c| claims.str_at(&[c]))
            .find(|v| !v.is_empty())
            .ok_or("the token names no user")?
            .to_string();
        let path: Vec<&str> = self.roles_claim.split('.').collect();
        let mut roles: Vec<String> = Vec::new();
        let mut node = Some(&claims);
        for k in &path {
            node = node.and_then(|n| n.get(k));
        }
        if let Some(Json::Arr(list)) = node {
            roles.extend(list.iter().filter_map(|r| r.as_str().map(String::from)));
        }
        let mut authorities = vec!["ROLE_USER".to_string()];
        for r in roles {
            match self.role_map.get(&r) {
                Some(mapped) => authorities.extend(mapped.iter().cloned()),
                None if r.starts_with("ROLE_")
                    || r.starts_with("READ_REPO_")
                    || r.starts_with("WRITE_REPO_") =>
                {
                    authorities.push(r)
                }
                None => {}
            }
        }
        let mut cache = self.verified.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() > 10_000 {
            cache.retain(|_, v| v.2 > now);
        }
        cache.insert(digest, (name.clone(), authorities.clone(), exp));
        Ok((name, authorities))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn montgomery_power_matches_small_numbers() {
        // 4^13 mod 497 = 445; 7^560 mod 561 = 1 (561 is a Carmichael number)
        assert_eq!(
            mod_pow(&[4], &[13], &[0x01, 0xf1]).unwrap(),
            vec![0x01, 0xbd]
        );
        assert_eq!(
            mod_pow(&[7], &[0x02, 0x30], &[0x02, 0x31]).unwrap(),
            vec![0x00, 0x01]
        );
        // a 2048-bit modulus: (2^2047 - 1)... use n = 2^2048 - 159 (odd) and check x^1 = x
        let mut n = vec![0xffu8; 256];
        n[255] = 0x61;
        let x = vec![0x12u8; 255];
        let mut xp = vec![0u8];
        xp.extend(&x);
        assert_eq!(mod_pow(&xp, &[1], &n).unwrap(), xp);
        assert_eq!(base64url("SGVsbG8").unwrap(), b"Hello");
    }

    #[test]
    fn verifies_an_openssl_rsa_signature() {
        // `openssl genrsa 2048`; `openssl dgst -sha256 -sign` of "header.payload"
        let hexb = |h: &str| {
            (0..h.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
                .collect::<Vec<u8>>()
        };
        let n = hexb(
            "e983c3a33c1c792001315aa8ac5421c83e6c2325819885593c61a3a54d0f3a51e764d6b6d0ae2140061cfd044e6cf4e3a79520a2113b2a68283ab59e6aff7bed03a40d302edb3b201b3e75203f67a17807fbc5e97e3d038df8e8dc763d3e326030c48d046e620901ac206f63c09899f152da51fee78cce0a2fc838284f739d68257a1b7903a02d835bec4beb53d14c2468344d56584a029d559fd551f94cbf62ad77f0ce5beb2a17f3baf6be2a548eb4c3fe310afdcba189819b89f0d9e8a9dbb2969df31130e9240ce3fef0768f4fe1daf20ff3db061b0d50f3fd8b7d13a13019a4b1be4cce245a5a9636dced68c42e26c1801e0f6520e1add6b4c63883610b",
        );
        let sig = hexb(
            "050bd8f07c5644e3b1449332278d76fa4e2c1f1a6b84c16d82e5fd0880413f8bec80f4ee077ac3330b8d85e01c0997f1b0b42bf48a0baef642a141d425a4c50498acb9922f6398f4e0abe19a2192090ebbd116fb2d21b4e27979d5ff261d771ab8f1dde882ef331ee2dbf9e0dbdcf0b9c875dc67260b7e437ae6bf5ca7de6abd515b2a8f79795e2367e132f9251ae081d11ecf07f7c362e9757e22180568e9ef11bcdf23561ff844306dc326573450dd42ed9143ab80caafcc89e3b668f4eddd0fd081b50f64c40eac7a42b4c3794a2ba458284845cdbe88fedd24d2c0f5d35329e7f0c47db0f25b096808aaacc2cd7315756280d2e07e28a75ec97bb526e49f",
        );
        let e = [1u8, 0, 1];
        assert!(rsa_verify_sha256(&n, &e, b"header.payload", &sig));
        assert!(
            !rsa_verify_sha256(&n, &e, b"header.payloaD", &sig),
            "another message"
        );
        let mut bad = sig.clone();
        bad[100] ^= 1;
        assert!(
            !rsa_verify_sha256(&n, &e, b"header.payload", &bad),
            "a changed signature"
        );
    }
}
