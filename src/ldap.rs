//! LDAP sign-in: a user who is not a local user is checked by binding to an LDAP directory
//! (OpenLDAP, Active Directory, 389 DS...) with their own DN and password, and gets the
//! authorities of the groups that list them as a member.
//!
//! The protocol is spoken directly (LDAPv3 bind and search requests in BER), so no crates are
//! needed. Plain `ldap://` only: run it on a trusted network, or reach the directory through a
//! TLS tunnel.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// An LDAP directory users sign in against.
pub struct Directory {
    /// host:port
    pub address: String,
    /// The user's DN, with `{user}` for the name signed in with.
    pub user_dn: String,
    /// Where groups are searched (`member=<user DN>`); None: no groups.
    pub group_base: Option<String>,
    /// A service account that searches the groups (DN, password), for directories where users
    /// cannot read them; None: the user's own bind searches.
    pub search_as: Option<(String, String)>,
    /// Group name (its `cn`) -> GraphRD authorities.
    pub group_map: HashMap<String, Vec<String>>,
    /// Authorities every directory user gets.
    pub default_roles: Vec<String>,
}

// ------------------------------------------------------------------ BER

fn len_bytes(n: usize) -> Vec<u8> {
    if n < 0x80 {
        vec![n as u8]
    } else {
        let b: Vec<u8> = n
            .to_be_bytes()
            .into_iter()
            .skip_while(|&x| x == 0)
            .collect();
        let mut out = vec![0x80 | b.len() as u8];
        out.extend(b);
        out
    }
}

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend(len_bytes(content.len()));
    out.extend_from_slice(content);
    out
}

fn int(tag: u8, v: u32) -> Vec<u8> {
    let b: Vec<u8> = v
        .to_be_bytes()
        .into_iter()
        .skip_while(|&x| x == 0)
        .collect();
    let mut b = if b.is_empty() { vec![0] } else { b };
    if b[0] & 0x80 != 0 {
        b.insert(0, 0);
    }
    tlv(tag, &b)
}

/// One element: (tag, content, rest).
fn read_tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *b.first()?;
    let first = *b.get(1)?;
    let (len, start) = if first & 0x80 == 0 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *b.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let end = start.checked_add(len)?;
    Some((tag, b.get(start..end)?, &b[end..]))
}

/// Read one whole LDAP message from the connection.
fn read_message(s: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut head = [0u8; 2];
    s.read_exact(&mut head)?;
    let mut msg = head.to_vec();
    let len = if head[1] & 0x80 == 0 {
        head[1] as usize
    } else {
        let n = (head[1] & 0x7f) as usize;
        if n == 0 || n > 4 {
            return Err(std::io::Error::other("bad LDAP length"));
        }
        let mut lb = vec![0u8; n];
        s.read_exact(&mut lb)?;
        msg.extend(&lb);
        lb.iter().fold(0usize, |a, &x| (a << 8) | x as usize)
    };
    if len > 16 << 20 {
        return Err(std::io::Error::other("LDAP message too large"));
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body)?;
    msg.extend(body);
    Ok(msg)
}

/// (message id, protocol op tag, op content).
fn parse_message(m: &[u8]) -> Option<(u32, u8, &[u8])> {
    let (0x30, seq, _) = read_tlv(m)? else {
        return None;
    };
    let (0x02, id, rest) = read_tlv(seq)? else {
        return None;
    };
    let (tag, op, _) = read_tlv(rest)?;
    Some((id.iter().fold(0u32, |a, &x| (a << 8) | x as u32), tag, op))
}

/// The result code of an LDAPResult (bind or search done).
fn result_code(op: &[u8]) -> Option<u32> {
    let (0x0a, code, _) = read_tlv(op)? else {
        return None;
    };
    Some(code.iter().fold(0u32, |a, &x| (a << 8) | x as u32))
}

/// A DN value with RFC 4514's special characters escaped.
fn escape_dn(v: &str) -> String {
    let mut out = String::new();
    for (i, c) in v.chars().enumerate() {
        if ",+\"\\<>;=".contains(c)
            || (i == 0 && (c == '#' || c == ' '))
            || (i == v.chars().count() - 1 && c == ' ')
        {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl Directory {
    /// Authorities for a user whose password the directory accepts; Err with the reason
    /// otherwise.
    pub fn authenticate(&self, user: &str, password: &str) -> Result<Vec<String>, String> {
        if user.is_empty() || password.is_empty() {
            return Err("empty user or password".into()); // an empty password would be an anonymous bind
        }
        let dn = self.user_dn.replace("{user}", &escape_dn(user));
        let addr = std::net::ToSocketAddrs::to_socket_addrs(&self.address)
            .map_err(|e| e.to_string())?
            .next()
            .ok_or("no address")?;
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .map_err(|e| format!("{}: {e}", self.address))?;
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        // bind as the user
        let bind = tlv(
            0x60,
            &[
                int(0x02, 3),
                tlv(0x04, dn.as_bytes()),
                tlv(0x80, password.as_bytes()),
            ]
            .concat(),
        );
        s.write_all(&tlv(0x30, &[int(0x02, 1), bind].concat()))
            .map_err(|e| e.to_string())?;
        let reply = read_message(&mut s).map_err(|e| e.to_string())?;
        match parse_message(&reply) {
            Some((1, 0x61, op)) if result_code(op) == Some(0) => {}
            Some((1, 0x61, op)) => {
                return Err(format!(
                    "bind refused (result {})",
                    result_code(op).unwrap_or(u32::MAX)
                ));
            }
            _ => return Err("unexpected bind response".into()),
        }
        let mut authorities = self.default_roles.clone();
        if let Some(base) = &self.group_base {
            match self.groups(&mut s, base, &dn) {
                Ok(groups) => {
                    for cn in groups {
                        match self.group_map.get(&cn) {
                            Some(mapped) => authorities.extend(mapped.iter().cloned()),
                            None if cn.starts_with("ROLE_")
                                || cn.starts_with("READ_REPO_")
                                || cn.starts_with("WRITE_REPO_") =>
                            {
                                authorities.push(cn)
                            }
                            None => {}
                        }
                    }
                }
                // the password is right; without its groups the user gets the default roles
                Err(e) => eprintln!(
                    "LDAP: the groups of {user:?} could not be read ({e}); signed in with the default roles"
                ),
            }
        }
        let _ = s.write_all(&tlv(0x30, &[int(0x02, 9), vec![0x42, 0x00]].concat())); // unbind
        if !authorities.iter().any(|a| a == "ROLE_USER") {
            authorities.push("ROLE_USER".into());
        }
        Ok(authorities)
    }

    /// The `cn` of every group under `base` that lists `dn` as a member.
    fn groups(&self, s: &mut TcpStream, base: &str, dn: &str) -> Result<Vec<String>, String> {
        if let Some((bind_dn, password)) = &self.search_as {
            let bind = tlv(
                0x60,
                &[
                    int(0x02, 3),
                    tlv(0x04, bind_dn.as_bytes()),
                    tlv(0x80, password.as_bytes()),
                ]
                .concat(),
            );
            s.write_all(&tlv(0x30, &[int(0x02, 4), bind].concat()))
                .map_err(|e| e.to_string())?;
            let reply = read_message(s).map_err(|e| e.to_string())?;
            match parse_message(&reply) {
                Some((4, 0x61, op)) if result_code(op) == Some(0) => {}
                _ => return Err("the search account's bind was refused".into()),
            }
        }
        let mut out = Vec::new();
        {
            // groups that list the user: (member=<dn>), their cn
            let filter = tlv(
                0xa3,
                &[tlv(0x04, b"member"), tlv(0x04, dn.as_bytes())].concat(),
            );
            let search = tlv(
                0x63,
                &[
                    tlv(0x04, base.as_bytes()),
                    int(0x0a, 2),
                    int(0x0a, 0),
                    int(0x02, 0),
                    int(0x02, 10),
                    tlv(0x01, &[0]),
                    filter,
                    tlv(0x30, &tlv(0x04, b"cn")),
                ]
                .concat(),
            );
            s.write_all(&tlv(0x30, &[int(0x02, 2), search].concat()))
                .map_err(|e| e.to_string())?;
            loop {
                let m = read_message(s).map_err(|e| e.to_string())?;
                match parse_message(&m) {
                    Some((2, 0x64, entry)) => {
                        // objectName, then attributes: SEQUENCE OF { type, SET OF value }
                        let Some((_, _, rest)) = read_tlv(entry) else {
                            continue;
                        };
                        let Some((0x30, mut attrs, _)) = read_tlv(rest) else {
                            continue;
                        };
                        while let Some((_, attr, more)) = read_tlv(attrs) {
                            attrs = more;
                            let Some((_, ty, vals)) = read_tlv(attr) else {
                                continue;
                            };
                            if !ty.eq_ignore_ascii_case(b"cn") {
                                continue;
                            }
                            let Some((_, mut set, _)) = read_tlv(vals) else {
                                continue;
                            };
                            while let Some((_, v, more)) = read_tlv(set) {
                                set = more;
                                out.push(String::from_utf8_lossy(v).into_owned());
                            }
                        }
                    }
                    Some((2, 0x65, done)) => {
                        if result_code(done) != Some(0) {
                            return Err(format!(
                                "group search failed (result {})",
                                result_code(done).unwrap_or(u32::MAX)
                            ));
                        }
                        break;
                    }
                    Some((2, _, _)) => continue, // references
                    _ => return Err("unexpected search response".into()),
                }
            }
        }
        Ok(out)
    }

    /// A directory from the serve options.
    pub fn new(
        address: &str,
        user_dn: &str,
        group_base: Option<String>,
        search_as: Option<(String, String)>,
        group_map: &str,
        default_roles: &str,
    ) -> Directory {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for pair in group_map.split(',').filter(|p| !p.trim().is_empty()) {
            if let Some((g, a)) = pair.split_once('=') {
                map.entry(g.trim().to_string())
                    .or_default()
                    .extend(a.split('+').map(|x| x.trim().to_string()));
            }
        }
        Directory {
            address: address
                .trim_start_matches("ldap://")
                .trim_end_matches('/')
                .to_string(),
            user_dn: user_dn.to_string(),
            group_base,
            search_as,
            group_map: map,
            default_roles: default_roles
                .split(',')
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(String::from)
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ber_round_trip_and_dn_escaping() {
        let m = tlv(
            0x30,
            &[
                int(0x02, 7),
                tlv(
                    0x61,
                    &[int(0x0a, 49), tlv(0x04, b""), tlv(0x04, b"")].concat(),
                ),
            ]
            .concat(),
        );
        let (id, tag, op) = parse_message(&m).unwrap();
        assert_eq!((id, tag, result_code(op)), (7, 0x61, Some(49)));
        let long = vec![b'x'; 300];
        let encoded = tlv(0x04, &long);
        let (t, c, rest) = read_tlv(&encoded).unwrap();
        assert_eq!((t, c.len(), rest.len()), (0x04, 300, 0));
        assert_eq!(int(0x02, 200), vec![0x02, 0x02, 0x00, 0xc8]);
        assert_eq!(escape_dn("a,b=c"), "a\\,b\\=c");
        assert_eq!(escape_dn(" x "), "\\ x\\ ");
    }
}
