//! Minimal POSIX ustar archives (regular files only) for backups.

fn octal(out: &mut [u8], v: u64) {
    let s = format!("{:0width$o}", v, width = out.len() - 1);
    out[..s.len()].copy_from_slice(s.as_bytes());
    out[out.len() - 1] = 0;
}

/// The entries as a tar archive (a ustar header each).
pub fn write(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mtime = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    for (name, data) in entries {
        let mut h = [0u8; 512];
        let (prefix, base) = if name.len() > 100 {
            match name[..name.len().min(155)].rfind('/') {
                Some(i) => (&name[..i], &name[i + 1..]),
                None => ("", name.as_str()),
            }
        } else {
            ("", name.as_str())
        };
        let base = &base.as_bytes()[..base.len().min(100)];
        h[..base.len()].copy_from_slice(base);
        octal(&mut h[100..108], 0o644);
        octal(&mut h[108..116], 0);
        octal(&mut h[116..124], 0);
        octal(&mut h[124..136], data.len() as u64);
        octal(&mut h[136..148], mtime);
        h[148..156].fill(b' ');
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        let p = prefix.as_bytes();
        h[345..345 + p.len().min(155)].copy_from_slice(&p[..p.len().min(155)]);
        let sum: u32 = h.iter().map(|&b| b as u32).sum();
        let chk = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(chk.as_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.resize(out.len() + 1024, 0);
    out
}

/// Regular files in an archive; names never escape the archive root.
pub fn read(data: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 512 <= data.len() {
        let h = &data[pos..pos + 512];
        if h.iter().all(|&b| b == 0) {
            break;
        }
        let field = |r: std::ops::Range<usize>| String::from_utf8_lossy(&h[r]).trim_end_matches('\0').trim().to_string();
        let size = u64::from_str_radix(&field(124..136), 8).map_err(|_| "corrupt tar header")? as usize;
        let mut name = field(0..100);
        let prefix = field(345..500);
        if !prefix.is_empty() {
            name = format!("{prefix}/{name}");
        }
        let kind = h[156];
        pos += 512;
        if pos + size > data.len() {
            return Err("truncated tar archive".into());
        }
        if (kind == b'0' || kind == 0) && !name.split('/').any(|c| c == ".." || c.is_empty() && !name.ends_with('/')) && !name.starts_with('/') {
            out.push((name, data[pos..pos + size].to_vec()));
        }
        pos += size.div_ceil(512) * 512;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let long = format!("{}/data.nq", "d".repeat(120));
        let entries = vec![("a/config.json".to_string(), b"{}".to_vec()), (long.clone(), vec![7u8; 1000]), ("../evil".to_string(), b"x".to_vec())];
        let back = super::read(&super::write(&entries)).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].0, long);
        assert_eq!(back[1].1.len(), 1000);
    }
}
