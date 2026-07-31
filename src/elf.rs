use std::fs;
use std::io;
use std::path::Path;

pub struct ElfInfo {
    pub e_type: u16,
    pub class: u8,
    pub machine: u16,
    pub interpreter: Option<String>,
    pub needed: Vec<String>,
}

impl ElfInfo {
    pub fn is_executable(&self) -> bool {
        matches!(self.e_type, 2 | 3) // ET_EXEC | ET_DYN
    }
}

pub fn read_elf(bytes: &[u8]) -> Option<ElfInfo> {
    if bytes.len() < 64 || &bytes[0..4] != b"\x7fELF" {
        return None;
    }
    let class = bytes[4]; 
    let le = bytes[5] == 1; 
    if !(class == 1 || class == 2) || (bytes[5] != 1 && bytes[5] != 2) {
        return None;
    }

    let e_type = rd_u16(bytes, 16, le)?;
    let machine = rd_u16(bytes, 18, le)?;
    let (phoff, phentsize, phnum) = match class {
        1 => (
            rd_u32(bytes, 28, le)? as u64,
            rd_u16(bytes, 42, le)? as u64,
            rd_u16(bytes, 44, le)? as u64,
        ),
        2 => (
            rd_u64(bytes, 32, le)?,
            rd_u16(bytes, 54, le)? as u64,
            rd_u16(bytes, 56, le)? as u64,
        ),
        _ => return None,
    };

    let mut interpreter = None;
    let mut loads: Vec<(u64, u64, u64)> = Vec::new(); 
    let mut dynamic: Option<(u64, u64)> = None;

    for i in 0..phnum {
        let off = phoff.checked_add(i * phentsize)?;
        let Some(p_type) = rd_u32(bytes, off as usize, le) else { break };
        let (p_offset, p_vaddr, p_filesz, p_memsz) = match class {
            1 => (
                rd_u32(bytes, off as usize + 4, le)? as u64,
                rd_u32(bytes, off as usize + 8, le)? as u64,
                rd_u32(bytes, off as usize + 16, le)? as u64,
                rd_u32(bytes, off as usize + 20, le)? as u64,
            ),
            _ => (
                rd_u64(bytes, off as usize + 8, le)?,
                rd_u64(bytes, off as usize + 16, le)?,
                rd_u64(bytes, off as usize + 32, le)?,
                rd_u64(bytes, off as usize + 40, le)?,
            ),
        };
        match p_type {
            3 => interpreter = read_cstring(bytes, p_offset as usize),
            2 => dynamic = Some((p_offset, p_filesz)),
            1 => loads.push((p_vaddr, p_offset, p_memsz)),
            _ => {}
        }
    }

    let mut needed = Vec::new();
    if let Some((dyn_off, dyn_size)) = dynamic {
        let entry_size = if class == 2 { 16 } else { 8 };
        let mut strtab_vaddr = None;
        let mut needed_offsets = Vec::new();
        for i in 0..dyn_size / entry_size {
            let off = dyn_off + i * entry_size;
            let (tag, val) = match class {
                2 => (rd_u64(bytes, off as usize, le)? as i64, rd_u64(bytes, off as usize + 8, le)?),
                _ => (
                    rd_u32(bytes, off as usize, le)? as i32 as i64,
                    rd_u32(bytes, off as usize + 4, le)? as u64,
                ),
            };
            match tag {
                0 => break, 
                1 => needed_offsets.push(val), 
                5 => strtab_vaddr = Some(val), 
                _ => {}
            }
        }
        if let Some(strtab) = strtab_vaddr {
            if let Some(off) = vaddr_to_offset(&loads, strtab) {
                for v in needed_offsets {
                    if let Some(so) = off.checked_add(v) {
                        if let Some(s) = read_cstring(bytes, so as usize) {
                            needed.push(s);
                        }
                    }
                }
            }
        }
    }

    Some(ElfInfo {
        e_type,
        class,
        machine,
        interpreter,
        needed,
    })
}

/// Map a virtual address inside a PT_LOAD segment to a file offset.
fn vaddr_to_offset(loads: &[(u64, u64, u64)], vaddr: u64) -> Option<u64> {
    for &(base, offset, memsz) in loads {
        if vaddr >= base && vaddr < base + memsz {
            return Some(offset + (vaddr - base));
        }
    }
    None
}

fn read_cstring(bytes: &[u8], offset: usize) -> Option<String> {
    let mut end = offset;
    while end < bytes.len() && bytes[end] != 0 {
        end += 1;
    }
    if end == bytes.len() {
        return None;
    }
    let slice = &bytes[offset..end];
    if slice.is_ascii() {
        Some(String::from_utf8_lossy(slice).into_owned())
    } else {
        None
    }
}

fn rd_u16(b: &[u8], o: usize, le: bool) -> Option<u16> {
    let s = b.get(o..o + 2)?;
    Some(if le {
        u16::from_le_bytes([s[0], s[1]])
    } else {
        u16::from_be_bytes([s[0], s[1]])
    })
}

fn rd_u32(b: &[u8], o: usize, le: bool) -> Option<u32> {
    let s = b.get(o..o + 4)?;
    Some(if le {
        u32::from_le_bytes([s[0], s[1], s[2], s[3]])
    } else {
        u32::from_be_bytes([s[0], s[1], s[2], s[3]])
    })
}

fn rd_u64(b: &[u8], o: usize, le: bool) -> Option<u64> {
    let s = b.get(o..o + 8)?;
    let mut arr = [0u8; 8];
    arr.copy_from_slice(s);
    Some(if le { u64::from_le_bytes(arr) } else { u64::from_be_bytes(arr) })
}

/// Convenience: read ELF info from a file on disk.
pub fn read_elf_file(path: &Path) -> io::Result<Option<ElfInfo>> {
    let bytes = fs::read(path)?;
    Ok(read_elf(&bytes))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn build_dyn(strtab: &[u8], needed: &[u32], interp: Option<&str>) -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[0..4].copy_from_slice(b"\x7fELF");
        b[4] = 2; // ELF64
        b[5] = 1; // little-endian
        b[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        b[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
        b[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff

        let dyn_off = 512usize;
        let strtab_off = 1024usize;
        let data_off = 2048usize;

        let interp_off = 3072usize;
        let nph = if interp.is_some() { 3 } else { 2 };
        b[54..56].copy_from_slice(&(56u16).to_le_bytes()); // phentsize
        b[56..58].copy_from_slice(&(nph as u16).to_le_bytes()); // phnum
        let dyn_bytes = (2 + needed.len()) * 16;

        let mut off = 64usize;
        b[off..off + 4].copy_from_slice(&1u32.to_le_bytes());
        b[off + 8..off + 16].copy_from_slice(&0u64.to_le_bytes());
        b[off + 16..off + 24].copy_from_slice(&0u64.to_le_bytes());
        b[off + 32..off + 40].copy_from_slice(&4096u64.to_le_bytes());
        b[off + 40..off + 48].copy_from_slice(&4096u64.to_le_bytes());
        off += 56;
        b[off..off + 4].copy_from_slice(&2u32.to_le_bytes());
        b[off + 8..off + 16].copy_from_slice(&(dyn_off as u64).to_le_bytes());
        b[off + 32..off + 40].copy_from_slice(&(dyn_bytes as u64).to_le_bytes());
        off += 56;
        if interp.is_some() {
            b[off..off + 4].copy_from_slice(&3u32.to_le_bytes()); // PT_INTERP
            b[off + 8..off + 16].copy_from_slice(&(interp_off as u64).to_le_bytes());
            b[off + 32..off + 40].copy_from_slice(&64u64.to_le_bytes());
        }

        b[strtab_off..strtab_off + strtab.len()].copy_from_slice(strtab);
        b[data_off..data_off + 4].copy_from_slice(b"DATA");

        let mut e = dyn_off;
        b[e..e + 8].copy_from_slice(&5i64.to_le_bytes());
        b[e + 8..e + 16].copy_from_slice(&(strtab_off as u64).to_le_bytes());
        e += 16;
        for n in needed {
            b[e..e + 8].copy_from_slice(&1i64.to_le_bytes());
            b[e + 8..e + 16].copy_from_slice(&(*n as u64).to_le_bytes());
            e += 16;
        }
        b[e..e + 8].copy_from_slice(&0i64.to_le_bytes());
        b[e + 8..e + 16].copy_from_slice(&0u64.to_le_bytes());

        if let Some(i) = interp {
            b[interp_off..interp_off + i.len()].copy_from_slice(i.as_bytes());
        }
        b
    }

    pub(crate) fn build_dyn_i386(strtab: &[u8], needed: &[u32]) -> Vec<u8> {
        let mut b = build_dyn(strtab, needed, Some("/lib/ld-linux.so.2"));
        b[4] = 1; // ELF32
        b[18..20].copy_from_slice(&3u16.to_le_bytes()); // EM_386
        b
    }

    #[test]
    fn parses_dynamic_deps_and_interp() {
        let strtab = b"\0libfoo.so.1\0libc.so.6\0/lib64/ld-linux-x86-64.so.2\0";
        let needed = [1u32, 13];
        let bytes = build_dyn(strtab, &needed, Some("/lib64/ld-linux-x86-64.so.2"));
        let info = read_elf(&bytes).unwrap();
        assert!(info.is_executable());
        assert_eq!(info.needed, vec!["libfoo.so.1", "libc.so.6"]);
        assert_eq!(info.interpreter.as_deref(), Some("/lib64/ld-linux-x86-64.so.2"));
    }

    #[test]
    fn non_elf_returns_none() {
        assert!(read_elf(b"not an elf file").is_none());
        assert!(read_elf(&[0u8; 128]).is_none());
    }

    #[test]
    fn static_binary_has_no_deps() {
        let strtab = b"\0";
        let bytes = build_dyn(strtab, &[], None);
        let info = read_elf(&bytes).unwrap();
        assert!(info.needed.is_empty());
        assert!(info.interpreter.is_none());
    }
}
