use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const STORE_DIR: &str = "univ";
const STORE_NAME: &str = "store";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StorePath {
    hash: String,
    name: String,
}

impl StorePath {
    pub fn parse(component: &str) -> Option<StorePath> {
        let (hash, name) = component.split_once('-')?;
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(StorePath {
            hash: hash.to_string(),
            name: name.to_string(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.hash, self.name)
    }
}

#[derive(Clone, Debug)]
pub struct Store {
    base: PathBuf,
}

impl Store {
    pub fn init() -> io::Result<Store> {
        let base = Self::base_dir()?;
        fs::create_dir_all(&base)?;
        Ok(Store { base })
    }

    pub fn open() -> io::Result<Store> {
        let base = Self::base_dir()?;
        if !base.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no store at {} (run `univ init` first)",
                    base.display()
                ),
            ));
        }
        Ok(Store { base })
    }

    pub fn home_dir() -> io::Result<PathBuf> {
        std::env::var("HOME")
            .map(PathBuf::from)
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "$HOME is not set"))
    }

    pub fn root() -> io::Result<PathBuf> {
        Ok(Self::home_dir()?.join(".local").join(STORE_DIR))
    }

    fn base_dir() -> io::Result<PathBuf> {
        Ok(Self::root()?.join(STORE_NAME))
    }

    pub fn state_dir() -> io::Result<PathBuf> {
        Ok(Self::root()?.join("state"))
    }

    pub fn tmp_dir(&self) -> io::Result<PathBuf> {
        let d = std::env::temp_dir().join(format!(
            "univ-xz-{}-{}",
            std::process::id(),
            &sha256_hex(self.base.to_string_lossy().as_bytes())[..12]
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d)?;
        Ok(d)
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn add(&self, bytes: &[u8], name: &str) -> io::Result<StorePath> {
        let hash = sha256_hex(bytes);
        let sp = StorePath {
            hash,
            name: name.to_string(),
        };
        let dest = self.base.join(sp.to_string());
        if dest.exists() {
            return Ok(sp);
        }
        let tmp = self.base.join(format!("{}.tmp{}", sp, std::process::id()));
        fs::write(&tmp, bytes)?;
        if let Err(e) = fs::rename(&tmp, &dest) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(sp)
    }

    pub fn add_tree(
        &self,
        name: &str,
        populate: impl FnOnce(&Path, &mut Sha256) -> io::Result<()>,
    ) -> io::Result<StorePath> {
        let tmp = self.base.join(format!(".{name}.tmp{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp)?;

        let mut ctx = Sha256::new();
        let result = populate(&tmp, &mut ctx);
        if let Err(e) = result {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }

        let sp = StorePath {
            hash: hex(&ctx.finalize()),
            name: name.to_string(),
        };
        let dest = self.base.join(sp.to_string());
        if dest.exists() {
            let _ = fs::remove_dir_all(&tmp);
            return Ok(sp);
        }
        if let Err(e) = fs::rename(&tmp, &dest) {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
        Ok(sp)
    }

    pub fn paths(&self) -> io::Result<Vec<StorePath>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.base)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(sp) = StorePath::parse(&name) {
                out.push(sp);
            }
        }
        out.sort();
        Ok(out)
    }
}

pub fn mark_auto(sp: &StorePath) -> io::Result<()> {
    let dir = Store::state_dir()?.join(sp.to_string());
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("auto"), "")
}

pub fn mark_manual(sp: &StorePath) -> io::Result<()> {
    let marker = Store::state_dir()?.join(sp.to_string()).join("auto");
    let _ = fs::remove_file(&marker);
    Ok(())
}

pub fn is_auto(sp: &StorePath) -> bool {
    Store::state_dir()
        .ok()
        .map(|d| d.join(sp.to_string()).join("auto").is_file())
        .unwrap_or(false)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut ctx = Sha256::new();
    ctx.update(data);
    hex(&ctx.finalize())
}

pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_len: u64,
}

impl Sha256 {
    pub fn new() -> Sha256 {
        Sha256 {
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0; 64],
            buf_len: 0,
            total_len: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);

        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }

        while data.len() >= 64 {
            let (block, rest) = data.split_at(64);
            self.compress(block);
            data = rest;
        }

        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finalize(&mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);

        let mut tail = [0u8; 128];
        let mut tail_len = 0;
        tail[tail_len] = 0x80;
        tail_len += 1;
        let pad = if self.buf_len < 56 { 55 - self.buf_len } else { 119 - self.buf_len };
        tail_len += pad;
        tail[tail_len..tail_len + 8].copy_from_slice(&bit_len.to_be_bytes());
        tail_len += 8;

        self.update(&tail[..tail_len]);
        debug_assert_eq!(self.buf_len, 0);

        let mut out = [0u8; 32];
        for (i, v) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) =
            (self.h[0], self.h[1], self.h[2], self.h[3], self.h[4], self.h[5], self.h[6], self.h[7]);

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(b);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
        self.h[4] = self.h[4].wrapping_add(e);
        self.h[5] = self.h[5].wrapping_add(f);
        self.h[6] = self.h[6].wrapping_add(g);
        self.h[7] = self.h[7].wrapping_add(h);
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Sha256::new()
    }
}

pub struct HashingReader<'a, R> {
    inner: R,
    ctx: &'a mut Sha256,
}

impl<'a, R: std::io::Read> HashingReader<'a, R> {
    pub fn new(inner: R, ctx: &'a mut Sha256) -> HashingReader<'a, R> {
        HashingReader { inner, ctx }
    }
}

impl<R: std::io::Read> std::io::Read for HashingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.ctx.update(&buf[..n]);
        Ok(n)
    }
}

#[cfg(test)]
pub(crate) fn test_store(base: &Path) -> Store {
    Store {
        base: base.to_path_buf(),
    }
}

#[cfg(test)]
pub(crate) static TEST_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn store_add_is_content_addressed() {
        let tmp = std::env::temp_dir().join(format!("univ-test-{}", std::process::id()));
        let store = Store { base: tmp.clone() };
        fs::create_dir_all(&tmp).unwrap();

        let a = store.add(b"hello world", "greeting").unwrap();
        let b = store.add(b"hello world", "greeting").unwrap();
        assert_eq!(a, b, "identical contents must map to the same store path");
        assert!(store.base.join(a.to_string()).is_file());

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn parse_roundtrip() {
        let sp = StorePath {
            hash: "a".repeat(64),
            name: "foo-1.0".into(),
        };
        assert_eq!(StorePath::parse(&sp.to_string()), Some(sp));
        assert!(StorePath::parse("not-a-store-path").is_none());
    }
}
