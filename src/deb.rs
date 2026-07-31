use std::fs;
use std::io::{self, Cursor, Read};
use std::path::Path;

use crate::store::{HashingReader, Sha256, Store, StorePath};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DebMeta {
    pub package: String,
    pub version: String,
    pub architecture: String,
    pub depends: Vec<Vec<Dep>>,
    pub description: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dep {
    pub package: String,
    pub version: Option<(String, String)>,
}

impl Dep {
    #[cfg(test)]
    pub fn package_only(name: &str) -> Dep {
        Dep {
            package: name.to_string(),
            version: None,
        }
    }
}

pub fn install(store: &Store, deb: &[u8]) -> io::Result<(StorePath, DebMeta)> {
    if let Some((_, v)) = member(deb, "debian-binary")? {
        if String::from_utf8_lossy(&v).trim() != "2.0" {
            return Err(invalid("unsupported debian-binary format"));
        }
    }

    let (control_name, control) = member(deb, "control.tar")?
        .ok_or_else(|| invalid("no control.tar member in archive"))?;
    let (data_name, data) = member(deb, "data.tar")?
        .ok_or_else(|| invalid("no data.tar member in archive"))?;

    let meta = read_control(&control_name, &control)?;
    if meta.package.is_empty() || meta.version.is_empty() {
        return Err(invalid("control file missing Package or Version"));
    }
    let name = {
        let package = sanitize(&meta.package);
        let version = sanitize(&meta.version);
        let architecture = sanitize(&meta.architecture);
        if architecture.is_empty() {
            format!("{package}-{version}")
        } else {
            format!("{package}-{version}-{architecture}")
        }
    };

    let sp = store.add_tree(&name, |dir, ctx| unpack_data(&data_name, &data, dir, ctx))?;
    Ok((sp, meta))
}

fn read_control(name: &str, bytes: &[u8]) -> io::Result<DebMeta> {
    let mut tar = tar::Archive::new(decompress(name, bytes)?);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if path.file_name().map(|n| n == "control").unwrap_or(false) {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            return Ok(parse_control(&text));
        }
    }
    Err(invalid("control.tar has no `control` file"))
}

fn unpack_data(
    name: &str,
    bytes: &[u8],
    dir: &Path,
    ctx: &mut Sha256,
) -> io::Result<()> {
    let mut reader = open_payload(name, bytes)?;
    let mut archive = tar::Archive::new(HashingReader::new(&mut reader, ctx));
    archive.unpack(dir)?;
    Ok(())
}

fn open_payload<'a>(name: &str, bytes: &'a [u8]) -> io::Result<Box<dyn Read + 'a>> {
    let ext = name.rsplit('.').next().unwrap_or("");
    match ext {
        "gz" => Ok(Box::new(flate2::read::GzDecoder::new(Cursor::new(bytes)))),
        "zst" => Ok(Box::new(
            ruzstd::decoding::StreamingDecoder::new(Cursor::new(bytes))
                .map_err(|e| invalid(format!("zstd: {e}")))?,
        )),
        "xz" => {
            let payload = std::env::temp_dir().join(format!(
                "unipkg-xz-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let out = fs::File::create(&payload)?;
            if let Err(e) = lzma_rs::xz_decompress(&mut Cursor::new(bytes), &mut &out)
                .map_err(|e| invalid(format!("xz: {e}")))
            {
                drop(out);
                let _ = fs::remove_file(&payload);
                return Err(e);
            }
            drop(out);
            let file = fs::File::open(&payload)?;
            Ok(Box::new(TempFileReader { file, path: payload }))
        }
        "bz2" => Err(invalid(
            "bzip2 data.tar members are not supported yet",
        )),
        _ => Ok(Box::new(Cursor::new(bytes))),
    }
}

struct TempFileReader {
    file: fs::File,
    path: std::path::PathBuf,
}

impl Read for TempFileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Drop for TempFileReader {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn decompress<'a>(name: &str, bytes: &'a [u8]) -> io::Result<Box<dyn Read + 'a>> {
    let ext = name.rsplit('.').next().unwrap_or("");
    match ext {
        "gz" => Ok(Box::new(flate2::read::GzDecoder::new(Cursor::new(bytes)))),
        "zst" => Ok(Box::new(
            ruzstd::decoding::StreamingDecoder::new(Cursor::new(bytes))
                .map_err(|e| invalid(format!("zstd: {e}")))?,
        )),
        "xz" => {
            let mut out = Vec::new();
            lzma_rs::xz_decompress(&mut Cursor::new(bytes), &mut out)
                .map_err(|e| invalid(format!("xz: {e}")))?;
            Ok(Box::new(Cursor::new(out)))
        }
        "bz2" => Err(invalid(
            "bzip2 control.tar members are not supported yet",
        )),
        _ => Ok(Box::new(Cursor::new(bytes.to_vec()))),
    }
}

fn member(deb: &[u8], prefix: &str) -> io::Result<Option<(String, Vec<u8>)>> {
    let mut archive = ar::Archive::new(deb);
    while let Some(entry) = archive.next_entry() {
        let mut entry = entry?;
        let name = String::from_utf8_lossy(entry.header().identifier())
            .trim_end_matches('/')
            .to_string();
        if name.starts_with(prefix) {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            return Ok(Some((name, buf)));
        }
    }
    Ok(None)
}

fn parse_control(text: &str) -> DebMeta {
    let mut meta = DebMeta::default();
    for (key, value) in fields(text) {
        match key.as_str() {
            "Package" => meta.package = value,
            "Version" => meta.version = value,
            "Architecture" => meta.architecture = value,
            "Depends" => meta.depends = parse_depends(&value),
            "Description" => meta.description = value,
            _ => {}
        }
    }
    meta
}

fn fields(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cur: Option<(String, String)> = None;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, v)) = cur.as_mut() {
                v.push('\n');
                v.push_str(line.trim_start());
            }
        } else if let Some((k, v)) = line.split_once(':') {
            cur = Some((k.trim().to_string(), v.trim().to_string()));
            out.push(cur.as_ref().unwrap().clone());
        }
    }
    out
}

pub(crate) fn parse_depends(s: &str) -> Vec<Vec<Dep>> {
    s.split(',')
        .map(|group| {
            group
                .split('|')
                .filter_map(|alt| {
                    let alt = alt.trim();
                    if alt.is_empty() {
                        return None;
                    }
                    let mut it = alt.splitn(2, '(');
                    let package = it.next().unwrap_or("").trim().to_string();
                    let version = it.next().and_then(|rest| {
                        let inner = rest.trim_end_matches(')').trim();
                        let digit = inner.find(|c: char| c.is_ascii_digit())?;
                        let op = inner[..digit].trim().to_string();
                        let version = inner[digit..].trim().to_string();
                        if version.is_empty() {
                            None
                        } else {
                            Some((op, version))
                        }
                    });
                    Some(Dep { package, version })
                })
                .collect()
        })
        .filter(|g: &Vec<Dep>| !g.is_empty())
        .collect()
}

pub fn render_depends(groups: &[Vec<Dep>]) -> String {
    groups
        .iter()
        .map(|group| {
            group
                .iter()
                .map(|d| {
                    let mut s = d.package.clone();
                    if let Some((op, version)) = &d.version {
                        s.push_str(&format!(" ({op} {version})"));
                    }
                    s
                })
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn write_meta(meta: &DebMeta, sp: &StorePath) -> io::Result<()> {
    let dir = Store::state_dir()?.join(sp.to_string());
    fs::create_dir_all(&dir)?;
    let mut text = String::new();
    text.push_str(&format!("Package: {}\n", meta.package));
    text.push_str(&format!("Version: {}\n", meta.version));
    text.push_str(&format!("Architecture: {}\n", meta.architecture));
    if !meta.depends.is_empty() {
        text.push_str(&format!("Depends: {}\n", render_depends(&meta.depends)));
    }
    if !meta.description.is_empty() {
        text.push_str(&format!("Description: {}\n", meta.description));
    }
    fs::write(dir.join("meta"), text)
}

pub fn read_meta(sp: &StorePath) -> io::Result<DebMeta> {
    let path = Store::state_dir()?.join(sp.to_string()).join("meta");
    let text = fs::read_to_string(&path)?;
    Ok(parse_control(&text))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-' | '_'))
        .collect()
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CONTROL: &str = "Package: hello\n\
         Version: 1.0\n\
         Architecture: amd64\n\
         Depends: libc6 (>= 2.34), foo | bar\n\
         Description: greets\n long description\n";

    fn tar_bytes(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut raw = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut raw);
            for (path, data, mode) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(*mode);
                header.set_mtime(0);
                header.set_cksum();
                builder.append_data(&mut header, *path, &data[..]).unwrap();
            }
        }
        raw
    }

    fn tar_gz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let raw = tar_bytes(entries);
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&raw).unwrap();
        enc.finish().unwrap()
    }

    fn tar_xz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let raw = tar_bytes(entries);
        let mut out = Vec::new();
        lzma_rs::xz_compress(&mut Cursor::new(&raw), &mut out).unwrap();
        out
    }

    fn tar_zst(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let raw = tar_bytes(entries);
        zstd::encode_all(&raw[..], 3).unwrap()
    }

    fn make_deb(data_name: &str, data_tar: &[u8]) -> Vec<u8> {
        let control = tar_gz(&[
            ("./control", CONTROL.as_bytes(), 0o644),
            ("./md5sums", b"", 0o644),
        ]);
        let mut out = Vec::new();
        {
            let mut b = ar::Builder::new(&mut out);
            let h = ar::Header::new(b"debian-binary".to_vec(), 4);
            b.append(&h, &b"2.0\n"[..]).unwrap();
            let h = ar::Header::new(b"control.tar.gz".to_vec(), control.len() as u64);
            b.append(&h, &control[..]).unwrap();
            let h = ar::Header::new(data_name.as_bytes().to_vec(), data_tar.len() as u64);
            b.append(&h, data_tar).unwrap();
        }
        out
    }

    fn test_store(label: &str) -> (std::path::PathBuf, crate::store::Store) {
        let dir = std::env::temp_dir().join(format!(
            "unipkg-deb-test-{}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = crate::store::test_store(&dir);
        (dir, store)
    }

    fn hello_data() -> Vec<u8> {
        tar_gz(&[
            ("./usr/bin/hello", b"#!/bin/sh\necho hi\n", 0o755),
            ("./usr/share/doc/hello/readme", b"read me\n", 0o644),
        ])
    }

    #[test]
    fn install_gzip_package() {
        let (dir, store) = test_store("gzip");
        let deb = make_deb("data.tar.gz", &hello_data());
        let (sp, meta) = install(&store, &deb).unwrap();
        assert_eq!(meta.package, "hello");
        assert_eq!(meta.version, "1.0");
        assert_eq!(meta.architecture, "amd64");
        assert_eq!(
            meta.depends,
            vec![
                vec![Dep {
                    package: "libc6".into(),
                    version: Some((">=".into(), "2.34".into())),
                }],
                vec![
                    Dep { package: "foo".into(), version: None },
                    Dep { package: "bar".into(), version: None },
                ],
            ]
        );

        let dest = store.base().join(sp.to_string());
        assert!(sp.to_string().ends_with("-hello-1.0-amd64"));
        let bin = dest.join("usr/bin/hello");
        assert!(bin.is_file());
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(fs::metadata(&bin).unwrap().permissions().mode() & 0o111, 0);

        let (sp2, _) = install(&store, &deb).unwrap();
        assert_eq!(sp, sp2, "identical .deb must deduplicate");

        let names: Vec<String> = fs::read_dir(store.base())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        assert_eq!(names.len(), 1, "no stray temp dirs left in store");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn install_xz_package() {
        let (dir, store) = test_store("xz");
        let deb = make_deb("data.tar.xz", &tar_xz(&[("./usr/bin/hello", b"x", 0o755)]));
        let (sp, meta) = install(&store, &deb).unwrap();
        assert_eq!(meta.package, "hello");
        assert!(store.base().join(sp.to_string()).join("usr/bin/hello").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn xz_payload_temp_file_is_removed() {
        let (dir, store) = test_store("xz-cleanup");
        let deb = make_deb("data.tar.xz", &tar_xz(&[("./usr/bin/hello", b"x", 0o755)]));
        install(&store, &deb).unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("unipkg-xz-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "xz temp payload leaked: {leftovers:?}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    fn patch_tar_name(raw: &mut [u8], header_offset: usize, new_name: &[u8]) {
        let h = &mut raw[header_offset..header_offset + 512];
        h[..100].fill(0);
        h[..new_name.len()].copy_from_slice(new_name);
        h[148..156].fill(b' ');
        let sum: u32 = h.iter().map(|&b| b as u32).sum();
        let checksum = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(checksum.as_bytes());
    }

    #[test]
    fn install_zstd_package() {
        let (dir, store) = test_store("zstd");
        let deb = make_deb("data.tar.zst", &tar_zst(&[("./usr/bin/hello", b"z", 0o755)]));
        let (sp, meta) = install(&store, &deb).unwrap();
        assert_eq!(meta.package, "hello");
        assert!(store.base().join(sp.to_string()).join("usr/bin/hello").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn path_traversal_is_rejected() {
        let (dir, store) = test_store("traversal");
        let mut raw = tar_bytes(&[
            ("./usr/bin/ok", b"fine", 0o755),
            ("zzz-padding", b"pwn", 0o644),
        ]);
        patch_tar_name(&mut raw, 1024, b"../evil.txt");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&raw).unwrap();
        let data = enc.finish().unwrap();

        let deb = make_deb("data.tar.gz", &data);
        let (sp, _) = install(&store, &deb).unwrap();
        let dest = store.base().join(sp.to_string());
        assert!(dest.join("usr/bin/ok").is_file());
        assert!(!dest.join("evil.txt").exists());
        assert!(!store.base().join("evil.txt").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
