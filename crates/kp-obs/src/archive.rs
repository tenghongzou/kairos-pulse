//! 診斷包的 tar.gz（ustar ＋ gzip）。
//!
//! 不引入外部的壓縮套件：deflate 以固定 Huffman 表與 LZ77（32 KiB 視窗、雜湊鏈）實作，日誌與 JSON
//! 一般可壓到原本的 1/4～1/8；解壓縮用一般的 `tar -xzf` 或 Windows 的 `tar.exe`。

use std::io;

/// tar 中的一個檔案
#[derive(Debug, Clone)]
struct Entry {
    path: String,
    data: Vec<u8>,
    mode: u32,
}

/// 依序加入檔案，最後產生 tar.gz
#[derive(Debug, Default)]
pub struct TarGz {
    entries: Vec<Entry>,
    mtime: i64,
}

impl TarGz {
    /// `mtime`：所有檔案的修改時間（Unix 秒）
    pub fn new(mtime: i64) -> Self {
        Self {
            entries: Vec::new(),
            mtime,
        }
    }

    /// 加入一個檔案（`path` 以 `/` 分隔的相對路徑；中間的目錄由解壓縮程式自動建立）
    pub fn add(&mut self, path: &str, data: impl Into<Vec<u8>>, mode: u32) -> io::Result<()> {
        if path.is_empty()
            || path.starts_with('/')
            || path.split('/').any(|p| p.is_empty() || p == "..")
        {
            return Err(io::Error::other(format!("tar 路徑不合法：{path}")));
        }
        self.entries.push(Entry {
            path: path.to_string(),
            data: data.into(),
            mode,
        });
        Ok(())
    }

    /// 已加入的檔案路徑與大小
    pub fn files(&self) -> Vec<(&str, usize)> {
        self.entries
            .iter()
            .map(|e| (e.path.as_str(), e.data.len()))
            .collect()
    }

    /// 未壓縮的 tar
    pub fn tar(&self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        for e in &self.entries {
            out.extend_from_slice(&header(&e.path, e.data.len() as u64, e.mode, self.mtime)?);
            out.extend_from_slice(&e.data);
            let pad = (512 - e.data.len() % 512) % 512;
            out.resize(out.len() + pad, 0);
        }
        out.resize(out.len() + 1024, 0);
        Ok(out)
    }

    /// tar.gz
    pub fn finish(&self) -> io::Result<Vec<u8>> {
        Ok(gzip(&self.tar()?, self.mtime))
    }
}

fn octal(field: &mut [u8], value: u64) -> io::Result<()> {
    let digits = field.len() - 1;
    let s = format!("{value:0digits$o}");
    if s.len() > digits {
        return Err(io::Error::other("tar 欄位的數值太大"));
    }
    field[..digits].copy_from_slice(s.as_bytes());
    field[digits] = 0;
    Ok(())
}

/// ustar 標頭（512 bytes）。路徑超過 100 bytes 時拆到 prefix（最多 155 bytes）
fn header(path: &str, size: u64, mode: u32, mtime: i64) -> io::Result<[u8; 512]> {
    let mut h = [0u8; 512];
    let bytes = path.as_bytes();
    if bytes.len() <= 100 {
        h[..bytes.len()].copy_from_slice(bytes);
    } else {
        let split = path[..path.len().min(156)]
            .rfind('/')
            .filter(|i| path.len() - i - 1 <= 100 && *i <= 155)
            .ok_or_else(|| io::Error::other(format!("tar 路徑太長：{path}")))?;
        h[345..345 + split].copy_from_slice(&bytes[..split]);
        let name = &bytes[split + 1..];
        h[..name.len()].copy_from_slice(name);
    }
    octal(&mut h[100..108], u64::from(mode & 0o7777))?;
    octal(&mut h[108..116], 0)?;
    octal(&mut h[116..124], 0)?;
    octal(&mut h[124..136], size)?;
    octal(&mut h[136..148], mtime.max(0) as u64)?;
    h[148..156].fill(b' ');
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    let s = format!("{sum:06o}");
    h[148..154].copy_from_slice(s.as_bytes());
    h[154] = 0;
    h[155] = b' ';
    Ok(h)
}

// ---- CRC-32（gzip） ---------------------------------------------------------------------------

fn crc32_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    for (i, v) in t.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *v = c;
    }
    t
}

pub fn crc32(data: &[u8]) -> u32 {
    let t = crc32_table();
    let mut c = 0xFFFF_FFFFu32;
    for b in data {
        c = t[((c ^ u32::from(*b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// gzip（RFC 1952）
pub fn gzip(data: &[u8], mtime: i64) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0];
    out.extend_from_slice(&(mtime.clamp(0, i64::from(u32::MAX)) as u32).to_le_bytes());
    out.extend_from_slice(&[0, 3]); // XFL、OS = Unix
    out.extend_from_slice(&deflate(data));
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

// ---- deflate（RFC 1951，固定 Huffman） --------------------------------------------------------

struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, bits: u32, len: u32) {
        self.acc |= u64::from(bits) << self.n;
        self.n += len;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// Huffman 碼由高位元先送：反轉後放入
    fn put_code(&mut self, code: u32, len: u32) {
        let mut r = 0;
        for i in 0..len {
            r |= ((code >> i) & 1) << (len - 1 - i);
        }
        self.put(r, len);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

fn literal(w: &mut BitWriter, sym: u32) {
    match sym {
        0..=143 => w.put_code(0x30 + sym, 8),
        144..=255 => w.put_code(0x190 + sym - 144, 9),
        256..=279 => w.put_code(sym - 256, 7),
        _ => w.put_code(0xC0 + sym - 280, 8),
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
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

fn emit_match(w: &mut BitWriter, len: usize, dist: usize) {
    let li = LEN_BASE
        .iter()
        .rposition(|b| usize::from(*b) <= len)
        .unwrap_or(0);
    literal(w, 257 + li as u32);
    if LEN_EXTRA[li] > 0 {
        w.put(
            (len - usize::from(LEN_BASE[li])) as u32,
            u32::from(LEN_EXTRA[li]),
        );
    }
    let di = DIST_BASE
        .iter()
        .rposition(|b| usize::from(*b) <= dist)
        .unwrap_or(0);
    w.put_code(di as u32, 5);
    if DIST_EXTRA[di] > 0 {
        w.put(
            (dist - usize::from(DIST_BASE[di])) as u32,
            u32::from(DIST_EXTRA[di]),
        );
    }
}

const WINDOW: usize = 32 * 1024;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const HASH_BITS: u32 = 15;
const MAX_CHAIN: usize = 48;

fn hash3(d: &[u8], i: usize) -> usize {
    let v = (u32::from(d[i]) << 16) | (u32::from(d[i + 1]) << 8) | u32::from(d[i + 2]);
    (v.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
}

/// deflate 壓縮（單一固定 Huffman 區塊）
pub fn deflate(data: &[u8]) -> Vec<u8> {
    let mut w = BitWriter {
        out: Vec::with_capacity(data.len() / 3 + 16),
        acc: 0,
        n: 0,
    };
    w.put(1, 1); // BFINAL
    w.put(1, 2); // BTYPE = 01（固定 Huffman）
    let mut head = vec![usize::MAX; 1 << HASH_BITS];
    let mut prev = vec![usize::MAX; WINDOW];
    let insert = |head: &mut Vec<usize>, prev: &mut Vec<usize>, i: usize| {
        if i + MIN_MATCH <= data.len() {
            let h = hash3(data, i);
            prev[i % WINDOW] = head[h];
            head[h] = i;
        }
    };
    let mut i = 0;
    while i < data.len() {
        let mut best_len = 0;
        let mut best_dist = 0;
        if i + MIN_MATCH <= data.len() {
            let mut cand = head[hash3(data, i)];
            let max = (data.len() - i).min(MAX_MATCH);
            let mut chain = 0;
            while cand != usize::MAX && i - cand <= WINDOW && chain < MAX_CHAIN {
                if cand < i && data[cand + best_len.min(max - 1)] == data[i + best_len.min(max - 1)]
                {
                    let l = data[cand..]
                        .iter()
                        .zip(&data[i..i + max])
                        .take_while(|(a, b)| a == b)
                        .count();
                    if l > best_len {
                        best_len = l;
                        best_dist = i - cand;
                        if l == max {
                            break;
                        }
                    }
                }
                let next = prev[cand % WINDOW];
                if next == usize::MAX || next >= cand {
                    break;
                }
                cand = next;
                chain += 1;
            }
        }
        if best_len >= MIN_MATCH {
            emit_match(&mut w, best_len, best_dist);
            for k in i..i + best_len {
                insert(&mut head, &mut prev, k);
            }
            i += best_len;
        } else {
            literal(&mut w, u32::from(data[i]));
            insert(&mut head, &mut prev, i);
            i += 1;
        }
    }
    literal(&mut w, 256);
    w.finish()
}

/// 解開 [`gzip`] 的輸出（只支援固定 Huffman 與未壓縮的區塊，足以解開本模組產生的檔案；
/// 給測試與自我檢查使用）。格式不符或 CRC 不符時為 `None`
pub fn gunzip(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 18 || data[..3] != [0x1f, 0x8b, 8] || data[3] != 0 {
        return None;
    }
    let body = &data[10..data.len() - 8];
    let out = inflate(body)?;
    let crc = u32::from_le_bytes(data[data.len() - 8..data.len() - 4].try_into().ok()?);
    let size = u32::from_le_bytes(data[data.len() - 4..].try_into().ok()?);
    (crc == crc32(&out) && size == out.len() as u32).then_some(out)
}

struct BitReader<'a> {
    d: &'a [u8],
    pos: usize,
    bit: u32,
}

impl BitReader<'_> {
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for i in 0..n {
            let b = (self.d.get(self.pos)? >> self.bit) & 1;
            v |= u32::from(b) << i;
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.pos += 1;
            }
        }
        Some(v)
    }

    /// Huffman 碼（高位元先）
    fn code(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bits(1)?;
        }
        Some(v)
    }

    fn sym(&mut self) -> Option<u32> {
        let mut c = self.code(7)?;
        if c <= 0x17 {
            return Some(256 + c);
        }
        c = (c << 1) | self.code(1)?;
        if (0x30..=0xBF).contains(&c) {
            return Some(c - 0x30);
        }
        if (0xC0..=0xC7).contains(&c) {
            return Some(280 + c - 0xC0);
        }
        c = (c << 1) | self.code(1)?;
        (0x190..=0x1FF).contains(&c).then(|| 144 + c - 0x190)
    }
}

/// 只支援固定 Huffman（BTYPE 01）與未壓縮（00）的區塊
fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut r = BitReader {
        d: data,
        pos: 0,
        bit: 0,
    };
    let mut out: Vec<u8> = Vec::new();
    loop {
        let last = r.bits(1)?;
        match r.bits(2)? {
            0 => {
                if r.bit != 0 {
                    r.bit = 0;
                    r.pos += 1;
                }
                let len = u16::from_le_bytes([*data.get(r.pos)?, *data.get(r.pos + 1)?]) as usize;
                r.pos += 4;
                out.extend_from_slice(data.get(r.pos..r.pos + len)?);
                r.pos += len;
            }
            1 => loop {
                let s = r.sym()?;
                if s < 256 {
                    out.push(s as u8);
                } else if s == 256 {
                    break;
                } else {
                    let li = (s - 257) as usize;
                    let len = usize::from(*LEN_BASE.get(li)?)
                        + r.bits(u32::from(LEN_EXTRA[li]))? as usize;
                    let di = r.code(5)? as usize;
                    let dist = usize::from(*DIST_BASE.get(di)?)
                        + r.bits(u32::from(DIST_EXTRA[di]))? as usize;
                    let start = out.len().checked_sub(dist)?;
                    for k in 0..len {
                        out.push(out[start + k]);
                    }
                }
            },
            _ => return None,
        }
        if last == 1 {
            return Some(out);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn sample_data() -> Vec<u8> {
        let mut d = Vec::new();
        for i in 0..3000 {
            d.extend_from_slice(
                format!(
                    "2026-10-14T06:22:{:02}.123Z [INFO] 連線 {i} 已註冊 conn.registered\n",
                    i % 60
                )
                .as_bytes(),
            );
        }
        // 不可壓縮的部分與所有位元組值
        let mut x: u32 = 12345;
        for _ in 0..5000 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            d.push((x >> 16) as u8);
        }
        d.extend(0u8..=255);
        d.extend(std::iter::repeat_n(b'a', 1000));
        d
    }

    #[test]
    fn deflate_round_trips_and_compresses() {
        for data in [
            Vec::new(),
            b"a".to_vec(),
            b"abcabcabcabc".to_vec(),
            sample_data(),
        ] {
            let z = deflate(&data);
            assert_eq!(inflate(&z).unwrap(), data);
        }
        let data = sample_data();
        let z = deflate(&data);
        assert!(z.len() < data.len() / 3, "{} → {}", data.len(), z.len());
    }

    #[test]
    fn crc32_known_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn tar_headers_are_valid_ustar() {
        let mut t = TarGz::new(1_760_000_000);
        t.add("a/b.txt", "hello", 0o600).unwrap();
        let long = format!("{}/{}", "d".repeat(120), "f".repeat(90));
        t.add(&long, vec![1u8; 600], 0o600).unwrap();
        assert!(t.add("../x", "", 0o600).is_err());
        assert!(t.add("/abs", "", 0o600).is_err());
        assert!(t.add(&"x".repeat(300), "", 0o600).is_err() || t.tar().is_err());
        let mut t2 = TarGz::new(1_760_000_000);
        t2.add("a/b.txt", "hello", 0o600).unwrap();
        t2.add(&long, vec![1u8; 600], 0o600).unwrap();
        let tar = t2.tar().unwrap();
        assert_eq!(tar.len() % 512, 0);
        let h = &tar[..512];
        assert_eq!(&h[..7], b"a/b.txt");
        assert_eq!(&h[257..263], b"ustar\0");
        assert_eq!(&h[100..107], b"0000600");
        // 校驗和
        let mut sum: u32 = 0;
        for (i, b) in h.iter().enumerate() {
            sum += if (148..156).contains(&i) {
                32
            } else {
                u32::from(*b)
            };
        }
        let stored = u32::from_str_radix(std::str::from_utf8(&h[148..154]).unwrap(), 8).unwrap();
        assert_eq!(stored, sum);
        // 第二個檔案的標頭在 512 + 512 之後
        let h2 = &tar[1024..1536];
        assert_eq!(&h2[345..465], "d".repeat(120).as_bytes());
        assert_eq!(&h2[..90], "f".repeat(90).as_bytes());
        let gz = t2.finish().unwrap();
        assert_eq!(&gz[..3], &[0x1f, 0x8b, 8]);
        let body = &gz[10..gz.len() - 8];
        assert_eq!(inflate(body).unwrap(), tar);
        assert_eq!(gunzip(&gz).unwrap(), tar);
        let mut bad = gz.clone();
        let n = bad.len();
        bad[n - 6] ^= 1;
        assert!(gunzip(&bad).is_none());
        let crc = u32::from_le_bytes(gz[gz.len() - 8..gz.len() - 4].try_into().unwrap());
        assert_eq!(crc, crc32(&tar));
    }

    /// 以系統的 tar 驗證（建置機為 Linux；沒有 tar 時略過）
    #[cfg(unix)]
    #[test]
    fn system_tar_can_extract() {
        let dir = std::env::temp_dir().join(format!("kp-obs-tar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut t = TarGz::new(1_760_000_000);
        let data = sample_data();
        t.add("bundle/logs/kairos.log", data.clone(), 0o600)
            .unwrap();
        t.add("bundle/MANIFEST.json", "{}", 0o600).unwrap();
        let file = dir.join("x.tar.gz");
        std::fs::write(&file, t.finish().unwrap()).unwrap();
        let ok = std::process::Command::new("tar")
            .arg("-xzf")
            .arg(&file)
            .arg("-C")
            .arg(&dir)
            .status();
        let Ok(status) = ok else {
            eprintln!("沒有 tar，略過");
            return;
        };
        assert!(status.success());
        assert_eq!(
            std::fs::read(dir.join("bundle/logs/kairos.log")).unwrap(),
            data
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
