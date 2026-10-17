//! gzip compression (RFC 1952 around RFC 1951 DEFLATE) for HTTP responses: LZ77 with hash
//! chains, coded with the fixed Huffman tables. No decompression is needed by the server.

const WINDOW: usize = 32 * 1024;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const MAX_CHAIN: usize = 48;
const HASH_BITS: u32 = 15;

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    /// Appends `len` bits of `v`, least significant first.
    fn put(&mut self, v: u32, len: u32) {
        self.acc |= (v as u64) << self.n;
        self.n += len;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// Appends a Huffman code (stored most significant bit first).
    fn code(&mut self, code: u32, len: u32) {
        let rev = code.reverse_bits() >> (32 - len);
        self.put(rev, len);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

fn literal(b: &mut Bits, sym: u32) {
    match sym {
        0..=143 => b.code(0x30 + sym, 8),
        144..=255 => b.code(0x190 + sym - 144, 9),
        256..=279 => b.code(sym - 256, 7),
        _ => b.code(0xc0 + sym - 280, 8),
    }
}

fn copy(b: &mut Bits, len: usize, dist: usize) {
    let li = LENGTH_BASE
        .iter()
        .rposition(|&base| base as usize <= len)
        .unwrap();
    literal(b, 257 + li as u32);
    b.put(
        (len - LENGTH_BASE[li] as usize) as u32,
        LENGTH_EXTRA[li] as u32,
    );
    let di = DIST_BASE
        .iter()
        .rposition(|&base| base as usize <= dist)
        .unwrap();
    b.code(di as u32, 5);
    b.put(
        (dist - DIST_BASE[di] as usize) as u32,
        DIST_EXTRA[di] as u32,
    );
}

fn hash(d: &[u8], i: usize) -> usize {
    let v = (d[i] as u32) << 16 | (d[i + 1] as u32) << 8 | d[i + 2] as u32;
    (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
}

/// Raw DEFLATE data (one block with the fixed Huffman codes).
pub fn deflate(data: &[u8]) -> Vec<u8> {
    let mut b = Bits {
        out: Vec::with_capacity(data.len() / 3 + 16),
        acc: 0,
        n: 0,
    };
    b.put(1, 1); // final block
    b.put(1, 2); // fixed Huffman codes
    let mut head = vec![usize::MAX; 1 << HASH_BITS];
    let mut prev = vec![usize::MAX; data.len()];
    fn insert(data: &[u8], i: usize, head: &mut [usize], prev: &mut [usize]) {
        if i + MIN_MATCH <= data.len() {
            let h = hash(data, i);
            prev[i] = head[h];
            head[h] = i;
        }
    }
    let mut i = 0;
    while i < data.len() {
        let mut best = (0usize, 0usize);
        if i + MIN_MATCH <= data.len() {
            let mut cand = head[hash(data, i)];
            let limit = (data.len() - i).min(MAX_MATCH);
            let mut chain = 0;
            while cand != usize::MAX && i - cand <= WINDOW && chain < MAX_CHAIN {
                if data[cand + best.0.min(limit - 1)] == data[i + best.0.min(limit - 1)] {
                    let mut l = 0;
                    while l < limit && data[cand + l] == data[i + l] {
                        l += 1;
                    }
                    if l > best.0 {
                        best = (l, i - cand);
                        if l == limit {
                            break;
                        }
                    }
                }
                cand = prev[cand];
                chain += 1;
            }
        }
        if best.0 >= MIN_MATCH {
            copy(&mut b, best.0, best.1);
            for k in i..i + best.0 {
                insert(data, k, &mut head, &mut prev);
            }
            i += best.0;
        } else {
            literal(&mut b, data[i] as u32);
            insert(data, i, &mut head, &mut prev);
            i += 1;
        }
    }
    literal(&mut b, 256);
    b.finish()
}

/// CRC-32 (the gzip checksum).
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, slot) in t.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        t
    });
    !data.iter().fold(!0u32, |c, &b| {
        table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8)
    })
}

/// A gzip member holding `data`.
pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    out.extend(deflate(data));
    out.extend(crc32(data).to_le_bytes());
    out.extend((data.len() as u32).to_le_bytes());
    out
}

// ------------------------------------------------------------------ decompression

struct In<'a> {
    d: &'a [u8],
    pos: usize,
    bit: u32,
}

impl In<'_> {
    fn bits(&mut self, n: u32) -> Result<u32, String> {
        let mut v = 0u32;
        for i in 0..n {
            let byte = *self.d.get(self.pos).ok_or("gzip: data ends early")?;
            v |= (((byte >> self.bit) & 1) as u32) << i;
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.pos += 1;
            }
        }
        Ok(v)
    }

    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
    }
}

/// A canonical Huffman decoding table: (counts per length, symbols in code order).
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Huffman {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let mut offs = [0u16; 16];
        for i in 1..16 {
            offs[i] = offs[i - 1] + counts[i - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Huffman { counts, symbols }
    }

    fn decode(&self, input: &mut In) -> Result<u16, String> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= input.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                return self
                    .symbols
                    .get((index + code - first) as usize)
                    .copied()
                    .ok_or_else(|| "gzip: bad code".to_string());
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("gzip: bad Huffman code".into())
    }
}

/// Decompresses raw DEFLATE data.
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut input = In {
        d: data,
        pos: 0,
        bit: 0,
    };
    let mut out: Vec<u8> = Vec::with_capacity(data.len() * 4);
    loop {
        let last = input.bits(1)? == 1;
        match input.bits(2)? {
            0 => {
                input.align();
                let p = input.pos;
                let len = u16::from_le_bytes([
                    *data.get(p).ok_or("gzip: short")?,
                    *data.get(p + 1).ok_or("gzip: short")?,
                ]) as usize;
                let start = p + 4;
                out.extend_from_slice(
                    data.get(start..start + len)
                        .ok_or("gzip: stored block ends early")?,
                );
                input.pos = start + len;
            }
            t @ (1 | 2) => {
                let (lit, dist) =
                    if t == 1 {
                        let mut l = [8u8; 288];
                        l[144..256].fill(9);
                        l[256..280].fill(7);
                        (Huffman::new(&l), Huffman::new(&[5u8; 30]))
                    } else {
                        let hlit = input.bits(5)? as usize + 257;
                        let hdist = input.bits(5)? as usize + 1;
                        let hclen = input.bits(4)? as usize + 4;
                        const ORDER: [usize; 19] = [
                            16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
                        ];
                        let mut cl = [0u8; 19];
                        for &i in ORDER.iter().take(hclen) {
                            cl[i] = input.bits(3)? as u8;
                        }
                        let clh = Huffman::new(&cl);
                        let mut lengths = Vec::with_capacity(hlit + hdist);
                        while lengths.len() < hlit + hdist {
                            match clh.decode(&mut input)? {
                                s @ 0..=15 => lengths.push(s as u8),
                                16 => {
                                    let prev =
                                        *lengths.last().ok_or("gzip: repeat without a length")?;
                                    for _ in 0..3 + input.bits(2)? {
                                        lengths.push(prev);
                                    }
                                }
                                17 => lengths
                                    .extend(std::iter::repeat_n(0, 3 + input.bits(3)? as usize)),
                                _ => lengths
                                    .extend(std::iter::repeat_n(0, 11 + input.bits(7)? as usize)),
                            }
                        }
                        (
                            Huffman::new(&lengths[..hlit]),
                            Huffman::new(&lengths[hlit..hlit + hdist]),
                        )
                    };
                loop {
                    let sym = lit.decode(&mut input)? as usize;
                    if sym < 256 {
                        out.push(sym as u8);
                    } else if sym == 256 {
                        break;
                    } else {
                        let i = sym - 257;
                        let len = *LENGTH_BASE.get(i).ok_or("gzip: bad length")? as usize
                            + input.bits(LENGTH_EXTRA[i] as u32)? as usize;
                        let di = dist.decode(&mut input)? as usize;
                        let d = *DIST_BASE.get(di).ok_or("gzip: bad distance")? as usize
                            + input.bits(DIST_EXTRA[di] as u32)? as usize;
                        if d > out.len() {
                            return Err("gzip: distance too far back".into());
                        }
                        let start = out.len() - d;
                        for k in 0..len {
                            out.push(out[start + k]);
                        }
                    }
                }
            }
            _ => return Err("gzip: bad block type".into()),
        }
        if last {
            return Ok(out);
        }
    }
}

/// Decompresses a gzip member (and checks its CRC).
pub fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        return Err("not gzip data".into());
    }
    let flags = data[3];
    let mut p = 10;
    if flags & 4 != 0 {
        let xlen = u16::from_le_bytes([data[p], data[p + 1]]) as usize;
        p += 2 + xlen;
    }
    for flag in [8u8, 16] {
        if flags & flag != 0 {
            while *data.get(p).ok_or("gzip: header ends early")? != 0 {
                p += 1;
            }
            p += 1;
        }
    }
    if flags & 2 != 0 {
        p += 2;
    }
    let body = data.get(p..data.len() - 8).ok_or("gzip: too short")?;
    let out = inflate(body)?;
    let crc = u32::from_le_bytes(data[data.len() - 8..data.len() - 4].try_into().unwrap());
    if crc != crc32(&out) {
        return Err("gzip: checksum mismatch".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn round_trip_and_system_gzip() {
        let text: Vec<u8> = (0..20_000)
            .flat_map(|i| format!("line {} of {}\n", i % 97, i / 7).into_bytes())
            .collect();
        assert_eq!(gunzip(&gzip(&text)).unwrap(), text);
        assert_eq!(gunzip(&gzip(b"")).unwrap(), b"");
        // dynamic Huffman blocks, as made by gzip -9
        let Ok(mut child) = std::process::Command::new("gzip")
            .arg("-9c")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
        else {
            return;
        };
        use std::io::Write;
        child.stdin.take().unwrap().write_all(&text).unwrap();
        let z = child.wait_with_output().unwrap().stdout;
        assert_eq!(gunzip(&z).unwrap(), text);
        let mut bad = z.clone();
        let n = bad.len();
        bad[n - 6] ^= 1;
        assert!(gunzip(&bad).is_err());
    }

    /// Round trip through the system's gzip, when there is one.
    #[test]
    fn decompresses_with_gzip() {
        let mut text = String::new();
        for i in 0..3000 {
            text.push_str(&format!("{{\"@id\":\"https://1r.example.com/logistics-objects/{i}\",\"cargo:goodsDescription\":\"Books {}\"}},", i % 17));
        }
        let mut samples: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"a".to_vec(),
            b"abcabcabcabcabc".to_vec(),
            text.into_bytes(),
        ];
        samples.push(
            (0..70_000u32)
                .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
                .collect(),
        );
        for data in samples {
            let z = gzip(&data);
            let Ok(mut child) = std::process::Command::new("gzip")
                .arg("-dc")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
            else {
                return;
            };
            use std::io::Write;
            child.stdin.take().unwrap().write_all(&z).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "gzip -d failed for {} bytes",
                data.len()
            );
            assert_eq!(out.stdout, data);
            if data.len() > 100_000 {
                assert!(z.len() * 4 < data.len(), "{} -> {}", data.len(), z.len());
            }
        }
    }
}
