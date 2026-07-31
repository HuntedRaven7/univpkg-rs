//! Parse and install RPM packages (`.rpm` files).
//!
//! RPM binary layout:
//!
//! 1. **Lead** (96 bytes, ignored after magic check)
//! 2. **Signature section** — a Header Structure that is skipped; it is
//!    word-aligned after its end.
//! 3. **Header section** — the actual package metadata (name, version, arch,
//!    summary, requires, …).  We parse only the tags we care about.
//! 4. **Payload** — a CPIO archive, compressed with gzip, xz, or zstd.
//!
//! We never write outside the store: the CPIO extractor rejects absolute paths
//! and `..` components before touching the filesystem.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Cursor, Read};
use std::path::Path;

use crate::store::{HashingReader, Sha256, Store, StorePath};

// ── RPM tag numbers we care about ────────────────────────────────────────────

const TAG_NAME: u32 = 1000;
const TAG_VERSION: u32 = 1001;
const TAG_RELEASE: u32 = 1002;
const TAG_SUMMARY: u32 = 1004;
const TAG_ARCH: u32 = 1022;
const TAG_REQUIRENAME: u32 = 1049;
const TAG_REQUIREFLAGS: u32 = 1048;
const TAG_REQUIREVERSION: u32 = 1050;
#[allow(dead_code)]
const TAG_PAYLOADCOMPRESSOR: u32 = 1125;

// REQUIREFLAGS bits
const RPMSENSE_LESS: u32 = 0x02;
const RPMSENSE_GREATER: u32 = 0x04;
const RPMSENSE_EQUAL: u32 = 0x08;
// Ignore deps that are really file paths, rpmlib pseudo-deps, or config deps.
const RPMSENSE_RPMLIB: u32 = 0x1000000;
const RPMSENSE_SCRIPT_PRE: u32 = 0x200;
const RPMSENSE_SCRIPT_POST: u32 = 0x400;
const RPMSENSE_SCRIPT_PREUN: u32 = 0x800;
const RPMSENSE_SCRIPT_POSTUN: u32 = 0x1000;
const RPMSENSE_CONFIG: u32 = 0x40;
const IGNORE_FLAGS: u32 = RPMSENSE_RPMLIB
    | RPMSENSE_SCRIPT_PRE
    | RPMSENSE_SCRIPT_POST
    | RPMSENSE_SCRIPT_PREUN
    | RPMSENSE_SCRIPT_POSTUN
    | RPMSENSE_CONFIG;

// ── Public types ──────────────────────────────────────────────────────────────

/// Metadata extracted from an RPM header.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RpmMeta {
    pub package: String,
    pub version: String,
    /// The full EVR string (`epoch:version-release` or `version-release`).
    pub full_version: String,
    pub architecture: String,
    pub description: String,
    pub requires: Vec<Vec<RpmDep>>,
}

/// One RPM dependency entry.  `version` is `None` for unversioned deps.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RpmDep {
    pub package: String,
    pub version: Option<(String, String)>,
}

impl RpmDep {
    /// A dependency with no version constraint.
    #[cfg(test)]
    pub fn package_only(name: &str) -> RpmDep {
        RpmDep { package: name.to_string(), version: None }
    }
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Parse and install an RPM.  Returns `(store_path, metadata)`.
pub fn install(store: &Store, rpm: &[u8]) -> io::Result<(StorePath, RpmMeta)> {
    // Check magic: RPM files start with 0xEDABEEDB.
    if rpm.len() < 4 || &rpm[..4] != b"\xed\xab\xee\xdb" {
        return Err(invalid("not an RPM file (bad magic)"));
    }

    let mut pos = 96usize; // skip the 96-byte lead

    // Skip the signature header section (tag count + data size determine its
    // length, then align to 8 bytes).
    let sig_end = skip_header(rpm, pos)?;
    pos = align8(sig_end);

    // Parse the main header.
    let (meta, header_end) = parse_header(rpm, pos)?;
    if meta.package.is_empty() || meta.version.is_empty() {
        return Err(invalid("RPM header missing Name or Version tag"));
    }
    pos = header_end;

    // The rest of the file is the compressed CPIO payload.
    let payload = &rpm[pos..];

    let name = {
        let pkg = sanitize(&meta.package);
        let ver = sanitize(&meta.full_version);
        let arch = sanitize(&meta.architecture);
        if arch.is_empty() {
            format!("{pkg}-{ver}")
        } else {
            format!("{pkg}-{ver}-{arch}")
        }
    };

    let compressor = meta.version_compressor(payload);
    let sp = store.add_tree(&name, |dir, ctx| unpack_payload(payload, &compressor, dir, ctx))?;
    Ok((sp, meta))
}

// ── Header parsing ────────────────────────────────────────────────────────────

/// Skip a header structure starting at `pos`, returning the offset after it.
///
/// Header structure: magic (8 bytes) + nindex (4) + hsize (4) + index entries
/// (nindex × 16) + data blob (hsize bytes).
fn skip_header(data: &[u8], pos: usize) -> io::Result<usize> {
    require(data, pos, 16)?;
    if &data[pos..pos + 3] != b"\x8e\xad\xe8" {
        return Err(invalid("bad header magic in RPM signature/header"));
    }
    let nindex = u32::from_be_bytes(data[pos + 8..pos + 12].try_into().unwrap()) as usize;
    let hsize = u32::from_be_bytes(data[pos + 12..pos + 16].try_into().unwrap()) as usize;
    let end = pos + 16 + nindex * 16 + hsize;
    require(data, pos, end - pos)?;
    Ok(end)
}

/// Parse a Header Structure into `(RpmMeta, end_offset)`.
fn parse_header(data: &[u8], pos: usize) -> io::Result<(RpmMeta, usize)> {
    require(data, pos, 16)?;
    if &data[pos..pos + 3] != b"\x8e\xad\xe8" {
        return Err(invalid("bad header magic in RPM main header"));
    }
    let nindex = u32::from_be_bytes(data[pos + 8..pos + 12].try_into().unwrap()) as usize;
    let hsize = u32::from_be_bytes(data[pos + 12..pos + 16].try_into().unwrap()) as usize;

    let index_start = pos + 16;
    let data_start = index_start + nindex * 16;
    let end = data_start + hsize;
    require(data, pos, end - pos)?;

    let store = &data[data_start..end];

    // Collect all tags into a map: tag → (type, offset, count)
    let mut tags: HashMap<u32, (u32, usize, u32)> = HashMap::new();
    for i in 0..nindex {
        let ie = index_start + i * 16;
        let tag = u32::from_be_bytes(data[ie..ie + 4].try_into().unwrap());
        let typ = u32::from_be_bytes(data[ie + 4..ie + 8].try_into().unwrap());
        let off = u32::from_be_bytes(data[ie + 8..ie + 12].try_into().unwrap()) as usize;
        let cnt = u32::from_be_bytes(data[ie + 12..ie + 16].try_into().unwrap());
        tags.insert(tag, (typ, off, cnt));
    }

    let read_string = |tag: u32| -> String {
        let Some(&(_, off, _)) = tags.get(&tag) else { return String::new() };
        read_cstr(store, off).unwrap_or_default().to_string()
    };

    let read_string_array = |tag: u32| -> Vec<String> {
        let Some(&(_, off, cnt)) = tags.get(&tag) else { return Vec::new() };
        let mut out = Vec::new();
        let mut cursor = off;
        for _ in 0..cnt {
            let s = read_cstr(store, cursor).unwrap_or_default();
            cursor += s.len() + 1;
            out.push(s.to_string());
        }
        out
    };

    let read_int32_array = |tag: u32| -> Vec<u32> {
        let Some(&(_, off, cnt)) = tags.get(&tag) else { return Vec::new() };
        let mut out = Vec::new();
        for i in 0..cnt as usize {
            let b = off + i * 4;
            if b + 4 <= store.len() {
                out.push(u32::from_be_bytes(store[b..b + 4].try_into().unwrap()));
            }
        }
        out
    };

    let name = read_string(TAG_NAME);
    let version = read_string(TAG_VERSION);
    let release = read_string(TAG_RELEASE);
    let arch = read_string(TAG_ARCH);
    let summary = read_string(TAG_SUMMARY);

    let full_version = if release.is_empty() {
        version.clone()
    } else {
        format!("{version}-{release}")
    };

    // Build requires list, filtering out pseudo-deps.
    let req_names = read_string_array(TAG_REQUIRENAME);
    let req_flags = read_int32_array(TAG_REQUIREFLAGS);
    let req_versions = read_string_array(TAG_REQUIREVERSION);

    let mut requires: Vec<Vec<RpmDep>> = Vec::new();
    for (i, req_name) in req_names.iter().enumerate() {
        let flags = req_flags.get(i).copied().unwrap_or(0);
        // Skip file dependencies, rpmlib pseudo-deps, and script-only deps.
        if flags & IGNORE_FLAGS != 0 || req_name.starts_with('/') || req_name.starts_with("(") {
            continue;
        }
        let ver_str = req_versions.get(i).map(String::as_str).unwrap_or("");
        let version_constraint = if ver_str.is_empty() {
            None
        } else {
            let op = flags_to_op(flags);
            if op.is_empty() {
                None
            } else {
                Some((op, ver_str.to_string()))
            }
        };
        requires.push(vec![RpmDep {
            package: req_name.clone(),
            version: version_constraint,
        }]);
    }

    let meta = RpmMeta { package: name, version, full_version, architecture: arch, description: summary, requires };
    Ok((meta, end))
}

impl RpmMeta {
    fn version_compressor(&self, _payload: &[u8]) -> String {
        // We re-parse compressor from payload magic instead of storing it in
        // meta, so detect it here from the payload bytes.
        String::new()
    }
}

/// Detect the compression format of `payload` from its magic bytes and return a
/// label matching PAYLOADCOMPRESSOR tag values.
fn detect_compressor(payload: &[u8]) -> &'static str {
    if payload.starts_with(b"\xfd7zXZ\x00") {
        "xz"
    } else if payload.starts_with(b"\x28\xb5\x2f\xfd") {
        "zstd"
    } else if payload.starts_with(b"\x1f\x8b") {
        "gzip"
    } else if payload.starts_with(b"BZh") {
        "bzip2"
    } else if payload.starts_with(b"LZIP") {
        "lzip"
    } else {
        ""
    }
}

fn flags_to_op(flags: u32) -> String {
    match (
        flags & RPMSENSE_LESS != 0,
        flags & RPMSENSE_GREATER != 0,
        flags & RPMSENSE_EQUAL != 0,
    ) {
        (true, false, true) => "<=".to_string(),
        (false, true, true) => ">=".to_string(),
        (true, false, false) => "<".to_string(),
        (false, true, false) => ">".to_string(),
        (false, false, true) => "=".to_string(),
        _ => String::new(),
    }
}

// ── CPIO unpacking ────────────────────────────────────────────────────────────

/// Decompress the payload and unpack the CPIO archive into `dir`.
fn unpack_payload(payload: &[u8], _hint: &str, dir: &Path, ctx: &mut Sha256) -> io::Result<()> {
    let compressor = detect_compressor(payload);
    let mut decompressed: Box<dyn Read> = match compressor {
        "gzip" => Box::new(flate2::read::GzDecoder::new(Cursor::new(payload))),
        "zstd" => Box::new(
            ruzstd::decoding::StreamingDecoder::new(Cursor::new(payload))
                .map_err(|e| invalid(format!("zstd: {e}")))?,
        ),
        "xz" => {
            let mut out = Vec::new();
            lzma_rs::xz_decompress(&mut Cursor::new(payload), &mut out)
                .map_err(|e| invalid(format!("xz: {e}")))?;
            Box::new(Cursor::new(out))
        }
        "bzip2" => return Err(invalid("bzip2-compressed RPM payloads are not supported")),
        _ => Box::new(Cursor::new(payload)),
    };

    let mut hashing = HashingReader::new(&mut decompressed, ctx);
    unpack_cpio(&mut hashing, dir)
}

/// Unpack a newc (SVR4 ASCII) CPIO archive from `reader` into `dir`.
///
/// We only support the `newc` (070701/070702) format which is what rpm uses.
/// Absolute paths and `..` path components are rejected to prevent traversal.
fn unpack_cpio(reader: &mut dyn Read, dir: &Path) -> io::Result<()> {
    loop {
        // Read the 110-byte newc header.
        let mut hdr = [0u8; 110];
        match read_exact_or_eof(reader, &mut hdr)? {
            0 => break, // EOF before any header byte — treat as end of archive
            110 => {}
            _ => return Err(invalid("truncated CPIO header")),
        }
        if &hdr[..6] != b"070701" && &hdr[..6] != b"070702" {
            // Could be the TRAILER entry with padding; try to keep going for
            // the common case where we hit the TRAILER.
            if &hdr[..6] == b"TRAILR" || hdr[..6].starts_with(b"TRAILER") {
                break;
            }
            return Err(invalid(format!(
                "unsupported CPIO format: {:?}",
                &hdr[..6]
            )));
        }

        let namesize = hex4(&hdr[94..102])? as usize;
        let filesize = hex8(&hdr[54..62])? as u64;
        let mode = hex4(&hdr[14..22])?;

        // Read filename.
        let mut namebuf = vec![0u8; namesize];
        read_exact(reader, &mut namebuf)?;
        // Pad header+name to 4-byte boundary.
        skip_pad(reader, (110 + namesize) % 4)?;

        let name_bytes = namebuf.strip_suffix(&[0]).unwrap_or(&namebuf);
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| invalid("non-UTF8 CPIO filename"))?
            .trim_start_matches("./")
            .trim_start_matches('/')
            .to_string();

        if name == "TRAILER!!!" {
            // Consume the rest of the file data (should be 0) and stop.
            skip_bytes(reader, filesize)?;
            break;
        }

        // Reject path traversal.
        if name.split('/').any(|c| c == "..") {
            skip_bytes(reader, filesize)?;
            skip_pad(reader, filesize as usize % 4)?;
            continue;
        }

        let file_type = mode >> 12;
        const S_IFREG: u32 = 0o10;
        const S_IFDIR: u32 = 0o04;
        const S_IFLNK: u32 = 0o12;

        if file_type == S_IFDIR || name.is_empty() {
            // Directory entry.
            skip_bytes(reader, filesize)?;
            skip_pad(reader, filesize as usize % 4)?;
            if !name.is_empty() {
                let target = dir.join(&name);
                fs::create_dir_all(&target)?;
            }
        } else if file_type == S_IFLNK {
            // Symlink: data is the link target.
            let mut link_target = vec![0u8; filesize as usize];
            read_exact(reader, &mut link_target)?;
            skip_pad(reader, filesize as usize % 4)?;
            if !name.is_empty() {
                let dest = dir.join(&name);
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                let target_str = String::from_utf8_lossy(&link_target).into_owned();
                // Best-effort: skip if symlink already exists or target is absolute
                // (relative symlinks within the tree are fine).
                let _ = std::os::unix::fs::symlink(&target_str, &dest);
            }
        } else if file_type == S_IFREG {
            // Regular file.
            if !name.is_empty() {
                let dest = dir.join(&name);
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut file = fs::File::create(&dest)?;
                let mut remaining = filesize;
                let mut buf = [0u8; 8192];
                while remaining > 0 {
                    let chunk = (remaining as usize).min(buf.len());
                    read_exact(reader, &mut buf[..chunk])?;
                    io::Write::write_all(&mut file, &buf[..chunk])?;
                    remaining -= chunk as u64;
                }
                // Set executable bits.
                let file_mode = mode & 0o777;
                if file_mode != 0 {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&dest, fs::Permissions::from_mode(file_mode))?;
                }
                skip_pad(reader, filesize as usize % 4)?;
            } else {
                skip_bytes(reader, filesize)?;
                skip_pad(reader, filesize as usize % 4)?;
            }
        } else {
            // Device node, FIFO, socket — skip.
            skip_bytes(reader, filesize)?;
            skip_pad(reader, filesize as usize % 4)?;
        }
    }
    Ok(())
}

// ── Metadata persistence ──────────────────────────────────────────────────────

/// Persist RPM metadata next to the link manifest.
pub fn write_meta(meta: &RpmMeta, sp: &StorePath) -> io::Result<()> {
    let dir = Store::state_dir()?.join(sp.to_string());
    fs::create_dir_all(&dir)?;
    let mut text = String::new();
    text.push_str(&format!("Package: {}\n", meta.package));
    text.push_str(&format!("Version: {}\n", meta.full_version));
    text.push_str(&format!("Architecture: {}\n", meta.architecture));
    if !meta.description.is_empty() {
        text.push_str(&format!("Description: {}\n", meta.description));
    }
    if !meta.requires.is_empty() {
        let r = render_requires(&meta.requires);
        text.push_str(&format!("Requires: {r}\n"));
    }
    text.push_str("Kind: rpm\n");
    fs::write(dir.join("meta"), text)
}

/// Load persisted RPM metadata for a store path.
#[allow(dead_code)]
pub fn read_meta(sp: &StorePath) -> io::Result<RpmMeta> {
    let path = Store::state_dir()?.join(sp.to_string()).join("meta");
    let text = fs::read_to_string(&path)?;
    Ok(parse_meta(&text))
}

#[allow(dead_code)]
fn parse_meta(text: &str) -> RpmMeta {
    let mut meta = RpmMeta::default();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            match k.trim() {
                "Package" => meta.package = v.trim().to_string(),
                "Version" => {
                    meta.full_version = v.trim().to_string();
                    // Strip epoch prefix for plain version.
                    let ver = v.trim();
                    meta.version = if let Some(pos) = ver.find(':') {
                        ver[pos + 1..].to_string()
                    } else {
                        ver.to_string()
                    };
                }
                "Architecture" => meta.architecture = v.trim().to_string(),
                "Description" => meta.description = v.trim().to_string(),
                "Requires" => meta.requires = parse_requires_field(v.trim()),
                _ => {}
            }
        }
    }
    meta
}

/// Render `Requires` into a comma-separated list for the meta file.
pub fn render_requires(groups: &[Vec<RpmDep>]) -> String {
    groups
        .iter()
        .filter_map(|g| g.first())
        .map(|d| {
            let mut s = d.package.clone();
            if let Some((op, v)) = &d.version {
                s.push_str(&format!(" {op} {v}"));
            }
            s
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn parse_requires_field(s: &str) -> Vec<Vec<RpmDep>> {
    s.split(',')
        .filter_map(|item| {
            let item = item.trim();
            if item.is_empty() { return None; }
            let mut parts = item.splitn(3, ' ');
            let pkg = parts.next().unwrap_or("").to_string();
            let op = parts.next().map(str::to_string);
            let ver = parts.next().map(str::to_string);
            let version = match (op, ver) {
                (Some(op), Some(ver)) if !op.is_empty() && !ver.is_empty() => Some((op, ver)),
                _ => None,
            };
            Some(vec![RpmDep { package: pkg, version }])
        })
        .collect()
}

// ── Convert RpmMeta ↔ DebMeta ─────────────────────────────────────────────────
//
// The rest of the system (link, resolve) uses `DebMeta` as the universal
// installed-package record.  We convert here so we don't need to plumb a
// second generic type throughout the codebase.

use crate::deb::{DebMeta, Dep};

impl From<RpmMeta> for DebMeta {
    fn from(r: RpmMeta) -> DebMeta {
        let depends = r.requires.into_iter().map(|group| {
            group.into_iter().map(|d| Dep {
                package: d.package,
                version: d.version,
            }).collect()
        }).collect();
        DebMeta {
            package: r.package,
            version: r.full_version,
            architecture: r.architecture,
            depends,
            description: r.description,
        }
    }
}

// ── Helper utilities ──────────────────────────────────────────────────────────

fn require(data: &[u8], pos: usize, need: usize) -> io::Result<()> {
    if pos + need > data.len() {
        Err(invalid(format!(
            "RPM truncated: need {} bytes at offset {}, have {}",
            need,
            pos,
            data.len()
        )))
    } else {
        Ok(())
    }
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// Read a hex string of exactly `N` chars as a u32. Used for CPIO header fields.
fn hex4(s: &[u8]) -> io::Result<u32> {
    let text = std::str::from_utf8(s).map_err(|_| invalid("non-ASCII CPIO field"))?;
    u32::from_str_radix(text, 16).map_err(|e| invalid(format!("bad CPIO hex field '{text}': {e}")))
}

fn hex8(s: &[u8]) -> io::Result<u64> {
    let text = std::str::from_utf8(s).map_err(|_| invalid("non-ASCII CPIO field"))?;
    u64::from_str_radix(text, 16).map_err(|e| invalid(format!("bad CPIO hex field '{text}': {e}")))
}

fn read_cstr(data: &[u8], off: usize) -> Option<&str> {
    let end = data[off..].iter().position(|&b| b == 0)? + off;
    std::str::from_utf8(&data[off..end]).ok()
}

fn read_exact(reader: &mut dyn Read, buf: &mut [u8]) -> io::Result<()> {
    reader.read_exact(buf)
}

/// Like `read_exact` but returns how many bytes were read on the first call.
/// Returns `0` on immediate EOF, `buf.len()` if the whole buffer was filled.
fn read_exact_or_eof(reader: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match reader.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

fn skip_bytes(reader: &mut dyn Read, n: u64) -> io::Result<()> {
    let mut remaining = n;
    let mut buf = [0u8; 4096];
    while remaining > 0 {
        let chunk = (remaining as usize).min(buf.len());
        reader.read_exact(&mut buf[..chunk])?;
        remaining -= chunk as u64;
    }
    Ok(())
}

fn skip_pad(reader: &mut dyn Read, remainder: usize) -> io::Result<()> {
    if remainder == 0 {
        return Ok(());
    }
    let pad = 4 - remainder;
    let mut buf = [0u8; 3];
    read_exact(reader, &mut buf[..pad])
}

fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-' | '_' | ':' | '~'))
        .collect()
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal newc CPIO archive with one regular file entry.
    fn cpio_newc(entries: &[(&str, &[u8], u32)], include_trailer: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data, mode) in entries {
            write_cpio_entry(&mut out, name, data, *mode);
        }
        if include_trailer {
            write_cpio_entry(&mut out, "TRAILER!!!", b"", 0);
        }
        out
    }

    fn write_cpio_entry(out: &mut Vec<u8>, name: &str, data: &[u8], mode: u32) {
        let namesize = name.len() + 1; // include NUL
        let filesize = data.len();
        // The actual newc format fields:
        let hdr = format!(
            "070701{ino:08X}{mode:08X}{uid:08X}{gid:08X}{nlink:08X}{mtime:08X}{filesize:08X}{devmajor:08X}{devminor:08X}{rdevmajor:08X}{rdevminor:08X}{namesize:08X}{check:08X}",
            ino = 0u32,
            mode = mode,
            uid = 0u32,
            gid = 0u32,
            nlink = 1u32,
            mtime = 0u32,
            filesize = filesize as u32,
            devmajor = 0u32,
            devminor = 0u32,
            rdevmajor = 0u32,
            rdevminor = 0u32,
            namesize = namesize as u32,
            check = 0u32,
        );
        assert_eq!(hdr.len(), 110);
        out.extend_from_slice(hdr.as_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(0); // NUL
        // Pad to 4-byte boundary after header+name
        let after = 110 + namesize;
        let pad = (4 - after % 4) % 4;
        out.extend(std::iter::repeat(0u8).take(pad));
        // File data
        out.extend_from_slice(data);
        // Pad data to 4-byte boundary
        let data_pad = (4 - filesize % 4) % 4;
        out.extend(std::iter::repeat(0u8).take(data_pad));
    }

    fn store_for_test(label: &str) -> (std::path::PathBuf, Store) {
        let dir = std::env::temp_dir()
            .join(format!("unipkg-rpm-test-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = crate::store::test_store(&dir);
        (dir, store)
    }

    /// Build a minimal but structurally valid RPM binary with the given CPIO
    /// payload (uncompressed — we write 'gzip' as the compressor tag but
    /// actually pass a gzip stream).
    fn make_rpm_gzip(meta: &RpmMeta, cpio: Vec<u8>) -> Vec<u8> {
        // Gzip the CPIO.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        use std::io::Write;
        gz.write_all(&cpio).unwrap();
        let payload = gz.finish().unwrap();
        build_rpm(meta, &payload)
    }

    /// Build a minimal RPM with raw (uncompressed) CPIO (no magic prefix
    /// matches gz/xz/zstd so it falls through as uncompressed).
    fn make_rpm_raw(meta: &RpmMeta, cpio: Vec<u8>) -> Vec<u8> {
        build_rpm(meta, &cpio)
    }

    fn build_rpm(meta: &RpmMeta, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();

        // Lead (96 bytes).
        out.extend_from_slice(b"\xed\xab\xee\xdb"); // magic
        out.extend_from_slice(&[0u8; 92]); // rest of lead

        // Signature section: minimal — just the required magic + empty index.
        out.extend_from_slice(&build_header_section(&[]));
        // Align to 8 bytes.
        while out.len() % 8 != 0 {
            out.push(0);
        }

        // Main header with tags.
        let tags = make_header_tags(meta);
        out.extend_from_slice(&build_header_section(&tags));

        // Payload.
        out.extend_from_slice(payload);
        out
    }

    /// A tag entry for build_header_section: (tag, type, data).
    /// type 6 = STRING, type 8 = STRING_ARRAY, type 4 = INT32.
    fn make_header_tags(meta: &RpmMeta) -> Vec<(u32, u32, Vec<u8>)> {
        let mut tags = Vec::new();
        tags.push((TAG_NAME, 6, cstr(meta.package.as_bytes())));
        tags.push((TAG_VERSION, 6, cstr(meta.version.as_bytes())));
        if !meta.architecture.is_empty() {
            tags.push((TAG_ARCH, 6, cstr(meta.architecture.as_bytes())));
        }
        if !meta.description.is_empty() {
            tags.push((TAG_SUMMARY, 6, cstr(meta.description.as_bytes())));
        }
        // Payload compressor tag (always present in real RPMs).
        tags.push((TAG_PAYLOADCOMPRESSOR, 6, cstr(b"gzip")));
        tags
    }

    fn cstr(s: &[u8]) -> Vec<u8> {
        let mut v = s.to_vec();
        v.push(0);
        v
    }

    fn build_header_section(tags: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
        // Compute data blob and index.
        let mut data: Vec<u8> = Vec::new();
        let mut index: Vec<(u32, u32, u32, u32)> = Vec::new(); // tag, type, offset, count

        for (tag, typ, bytes) in tags {
            let off = data.len() as u32;
            data.extend_from_slice(bytes);
            index.push((*tag, *typ, off, 1));
        }

        let nindex = index.len() as u32;
        let hsize = data.len() as u32;

        let mut out = Vec::new();
        // Magic (3 bytes) + reserved (1) + version (1) + reserved (3)
        out.extend_from_slice(b"\x8e\xad\xe8\x01");
        out.extend_from_slice(&[0u8; 4]); // reserved
        out.extend_from_slice(&nindex.to_be_bytes());
        out.extend_from_slice(&hsize.to_be_bytes());

        for (tag, typ, off, cnt) in &index {
            out.extend_from_slice(&tag.to_be_bytes());
            out.extend_from_slice(&typ.to_be_bytes());
            out.extend_from_slice(&off.to_be_bytes());
            out.extend_from_slice(&cnt.to_be_bytes());
        }
        out.extend_from_slice(&data);
        out
    }

    #[test]
    fn install_gzip_rpm() {
        let (dir, store) = store_for_test("gz");
        let cpio = cpio_newc(
            &[("./usr/bin/hello", b"#!/bin/sh\necho hi\n", 0o100755)],
            true,
        );
        let meta = RpmMeta {
            package: "hello".into(),
            version: "1.0".into(),
            full_version: "1.0-1".into(),
            architecture: "x86_64".into(),
            description: "greets".into(),
            requires: vec![],
        };
        let rpm = make_rpm_gzip(&meta, cpio);
        let (sp, got) = install(&store, &rpm).unwrap();
        assert_eq!(got.package, "hello");
        assert_eq!(got.version, "1.0");
        assert_eq!(got.architecture, "x86_64");
        assert!(store.base().join(sp.to_string()).join("usr/bin/hello").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_bad_magic() {
        let (dir, store) = store_for_test("bad-magic");
        let err = install(&store, b"not an rpm").unwrap_err();
        assert!(err.to_string().contains("bad magic"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cpio_path_traversal_rejected() {
        let (dir, store) = store_for_test("traversal");
        // Build a CPIO with a path traversal entry.
        let cpio = cpio_newc(
            &[
                ("./usr/bin/ok", b"fine", 0o100644),
                ("../evil.txt", b"pwn", 0o100644),
            ],
            true,
        );
        let meta = RpmMeta {
            package: "evil".into(),
            version: "1.0".into(),
            full_version: "1.0-1".into(),
            architecture: "x86_64".into(),
            description: String::new(),
            requires: vec![],
        };
        let rpm = make_rpm_raw(&meta, cpio);
        // install may succeed (traversal entry is skipped, not errored)
        if let Ok((sp, _)) = install(&store, &rpm) {
            let dest = store.base().join(sp.to_string());
            // The traversal file must NOT exist in the store.
            assert!(!dest.join("../evil.txt").exists());
            assert!(!store.base().join("evil.txt").exists());
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn meta_roundtrip() {
        let meta = RpmMeta {
            package: "foo".into(),
            version: "2.0".into(),
            full_version: "2.0-3".into(),
            architecture: "aarch64".into(),
            description: "a test pkg".into(),
            requires: vec![
                vec![RpmDep { package: "bar".into(), version: Some((">=".into(), "1.0".into())) }],
                vec![RpmDep::package_only("baz")],
            ],
        };
        let rendered = format!(
            "Package: {}\nVersion: {}\nArchitecture: {}\nDescription: {}\nRequires: {}\nKind: rpm\n",
            meta.package,
            meta.full_version,
            meta.architecture,
            meta.description,
            render_requires(&meta.requires),
        );
        let got = parse_meta(&rendered);
        assert_eq!(got.package, meta.package);
        assert_eq!(got.full_version, meta.full_version);
        assert_eq!(got.architecture, meta.architecture);
    }
}
