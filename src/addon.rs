//! `addon` feature — unwrap a napi `--compress` hybrid back to the raw `.node`.
//!
//! A binpress hybrid carries the original addon, zstd-compressed, inside a
//! `PRESSED_DATA` SIGNABLE SECTION (Mach-O `__PRESSED_DATA` in segment `SMOL`, ELF
//! `.PRESSED_DATA`, PE `.PRESSED` — read from the binary's SECTION HEADERS, NOT an
//! EOF footer). The section content is the bin-infra pressed-data blob:
//!
//! ```text
//! [magic marker  32B]  "__SMOL_PRESSED_DATA_MAGIC_MARKER"
//! [compressed    u64 LE]  zstd payload length
//! [uncompressed  u64 LE]  raw addon length
//! [cache key     16B]  hex, ignored here
//! [platform      3B ]  platform/arch/libc, ignored here
//! [integrity     64B]  SHA-512 of the zstd payload
//! [has_config    1B ]  0/1
//! [config        1192B] only if has_config == 1
//! [payload       compressed bytes]  zstd frame
//! ```
//!
//! `unwrap_if_hybrid(&content)` returns `Some(raw addon)` for a hybrid whose payload
//! zstd-decodes to `uncompressed` bytes and matches the SHA-512, else `None`. The
//! PM helper then feeds the raw addon to `compress_bytes`. This is the
//! mirror-image ABI of `socket-btm/packages/bin-infra` (`smol_segment_reader.c` /
//! `compression_constants.h`).

use sha2::{Digest, Sha512};

/// "__SMOL_PRESSED_DATA_MAGIC_MARKER" (SMOL_PRESSED_DATA_MAGIC_MARKER).
const MAGIC_MARKER: &[u8; 32] = b"__SMOL_PRESSED_DATA_MAGIC_MARKER";
const SIZE_HEADER_LEN: usize = 16; // compressed u64 + uncompressed u64
const CACHE_KEY_LEN: usize = 16;
const PLATFORM_METADATA_LEN: usize = 3;
const INTEGRITY_HASH_LEN: usize = 64; // SHA-512
const SMOL_CONFIG_FLAG_LEN: usize = 1;
const SMOL_CONFIG_BINARY_LEN: usize = 1192;
/// Fixed header length up to and including the has-config flag.
const HEADER_LEN: usize = MAGIC_MARKER.len()
  + SIZE_HEADER_LEN
  + CACHE_KEY_LEN
  + PLATFORM_METADATA_LEN
  + INTEGRITY_HASH_LEN
  + SMOL_CONFIG_FLAG_LEN;
/// Refuse a decompressed-size claim past this (DoS guard, matches bin-infra's 512 MB).
const MAX_DECOMPRESSED: u64 = 512 * 1024 * 1024;

/// If `content` is a napi `--compress` hybrid, decode its embedded addon and return
/// the raw `.node` bytes; otherwise `None`. Integrity-checked (SHA-512 over the zstd
/// payload) — a hybrid that fails verification returns `None`, never partial bytes.
pub fn unwrap_if_hybrid(content: &[u8]) -> Option<Vec<u8>> {
  let section = find_pressed_data_section(content)?;
  decode_pressed_data(section)
}

/// Parse the bin-infra pressed-data blob (magic + header + zstd payload) into the
/// raw addon. Split from section-finding so the format round-trips in a unit test
/// without synthesizing a whole Mach-O/ELF/PE.
pub fn decode_pressed_data(section: &[u8]) -> Option<Vec<u8>> {
  if section.len() < HEADER_LEN {
    return None;
  }
  if &section[..MAGIC_MARKER.len()] != MAGIC_MARKER.as_slice() {
    return None;
  }
  let mut at = MAGIC_MARKER.len();
  let compressed_size = read_u64_le(section, at)?;
  at += 8;
  let uncompressed_size = read_u64_le(section, at)?;
  at += 8;
  // Skip cache key + platform metadata.
  at += CACHE_KEY_LEN + PLATFORM_METADATA_LEN;
  let integrity = section.get(at..at + INTEGRITY_HASH_LEN)?;
  let mut hash = [0u8; INTEGRITY_HASH_LEN];
  hash.copy_from_slice(integrity);
  at += INTEGRITY_HASH_LEN;
  let has_config = *section.get(at)?;
  at += SMOL_CONFIG_FLAG_LEN;
  if has_config != 0 {
    at = at.checked_add(SMOL_CONFIG_BINARY_LEN)?;
  }

  if compressed_size == 0
    || uncompressed_size == 0
    || uncompressed_size > MAX_DECOMPRESSED
    || compressed_size > MAX_DECOMPRESSED
  {
    return None;
  }
  let payload = section.get(at..at.checked_add(compressed_size as usize)?)?;

  // Integrity: SHA-512 of the zstd payload, BEFORE decompressing (reject a
  // tampered frame up front).
  let mut hasher = Sha512::new();
  hasher.update(payload);
  if hasher.finalize().as_slice() != hash {
    return None;
  }

  let raw = zstd::stream::decode_all(payload).ok()?;
  if raw.len() as u64 != uncompressed_size {
    return None;
  }
  Some(raw)
}

fn read_u64_le(buf: &[u8], at: usize) -> Option<u64> {
  let bytes = buf.get(at..at + 8)?;
  let mut arr = [0u8; 8];
  arr.copy_from_slice(bytes);
  Some(u64::from_le_bytes(arr))
}

fn read_u32_le(buf: &[u8], at: usize) -> Option<u32> {
  let bytes = buf.get(at..at + 4)?;
  let mut arr = [0u8; 4];
  arr.copy_from_slice(bytes);
  Some(u32::from_le_bytes(arr))
}

fn read_u16_le(buf: &[u8], at: usize) -> Option<u16> {
  let bytes = buf.get(at..at + 2)?;
  Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// Locate the PRESSED_DATA section's raw bytes by walking the binary's section /
/// load-command table — never an EOF footer. Dispatches on the leading magic.
fn find_pressed_data_section(content: &[u8]) -> Option<&[u8]> {
  match content.get(..4)? {
    // Mach-O 64-bit, both endiannesses.
    [0xcf, 0xfa, 0xed, 0xfe] | [0xfe, 0xed, 0xfa, 0xcf] => find_macho(content),
    [0x7f, b'E', b'L', b'F'] => find_elf(content),
    [b'M', b'Z', ..] => find_pe(content),
    _ => None,
  }
}

/// Mach-O 64-bit (little-endian host): find segment `SMOL` → section `__PRESSED_DATA`
/// → its (offset,size) and return the slice. Mirrors smol_segment_reader.c.
fn find_macho(content: &[u8]) -> Option<&[u8]> {
  const LC_SEGMENT_64: u32 = 0x19;
  // mach_header_64: magic(4) cputype(4) cpusubtype(4) filetype(4) ncmds(4) ...
  let ncmds = read_u32_le(content, 16)?;
  let mut cmd_off = 32usize; // sizeof(mach_header_64)
  for _ in 0..ncmds.min(10_000) {
    let cmd = read_u32_le(content, cmd_off)?;
    let cmdsize = read_u32_le(content, cmd_off + 4)? as usize;
    if cmdsize == 0 {
      return None;
    }
    if cmd == LC_SEGMENT_64 {
      // segment_command_64: cmd(4) cmdsize(4) segname(16) vmaddr(8) vmsize(8)
      //   fileoff(8) filesize(8) maxprot(4) initprot(4) nsects(4) flags(4)
      let segname = content.get(cmd_off + 8..cmd_off + 24)?;
      if name_eq(segname, b"SMOL") {
        let nsects = read_u32_le(content, cmd_off + 64)?;
        let mut sect_off = cmd_off + 72; // start of section_64 array
        for _ in 0..nsects.min(1000) {
          // section_64: sectname(16) segname(16) addr(8) size(8) offset(4) ...
          let sectname = content.get(sect_off..sect_off + 16)?;
          if name_eq(sectname, b"__PRESSED_DATA") {
            let size = read_u64_le(content, sect_off + 40)? as usize;
            let offset = read_u32_le(content, sect_off + 48)? as usize;
            return content.get(offset..offset.checked_add(size)?);
          }
          sect_off += 80; // sizeof(section_64)
        }
      }
    }
    cmd_off = cmd_off.checked_add(cmdsize)?;
  }
  None
}

/// ELF 64-bit: walk the section header table, match `.PRESSED_DATA` against the
/// section-header string table, return its (sh_offset, sh_size) slice.
fn find_elf(content: &[u8]) -> Option<&[u8]> {
  // EI_CLASS at offset 4: 2 == 64-bit. Only 64-bit addons ship.
  if *content.get(4)? != 2 {
    return None;
  }
  let e_shoff = read_u64_le(content, 40)? as usize;
  let e_shentsize = read_u16_le(content, 58)? as usize;
  let e_shnum = read_u16_le(content, 60)? as usize;
  let e_shstrndx = read_u16_le(content, 62)? as usize;
  if e_shentsize < 64 || e_shnum == 0 || e_shstrndx >= e_shnum {
    return None;
  }
  // String table section header → its (offset,size).
  let strtab_hdr = e_shoff.checked_add(e_shstrndx.checked_mul(e_shentsize)?)?;
  let strtab_off = read_u64_le(content, strtab_hdr + 24)? as usize;
  let strtab_size = read_u64_le(content, strtab_hdr + 32)? as usize;
  let strtab = content.get(strtab_off..strtab_off.checked_add(strtab_size)?)?;

  for i in 0..e_shnum {
    let shdr = e_shoff.checked_add(i.checked_mul(e_shentsize)?)?;
    let sh_name = read_u32_le(content, shdr)? as usize;
    if cstr_at(strtab, sh_name) == Some(b".PRESSED_DATA".as_slice()) {
      let sh_offset = read_u64_le(content, shdr + 24)? as usize;
      let sh_size = read_u64_le(content, shdr + 32)? as usize;
      return content.get(sh_offset..sh_offset.checked_add(sh_size)?);
    }
  }
  None
}

/// PE: parse the section table for `.PRESSED` (the 8-byte-name truncation of
/// `.PRESSED_DATA`) and return (PointerToRawData, SizeOfRawData).
fn find_pe(content: &[u8]) -> Option<&[u8]> {
  let pe_off = read_u32_le(content, 0x3c)? as usize;
  if content.get(pe_off..pe_off + 4)? != b"PE\0\0" {
    return None;
  }
  let coff = pe_off + 4;
  let number_of_sections = read_u16_le(content, coff + 2)? as usize;
  let size_of_optional = read_u16_le(content, coff + 16)? as usize;
  if number_of_sections > 200 {
    return None;
  }
  let mut sect = coff + 20 + size_of_optional; // section table start
  for _ in 0..number_of_sections {
    let name = content.get(sect..sect + 8)?;
    if name == b".PRESSED" {
      let size_of_raw = read_u32_le(content, sect + 16)? as usize;
      let ptr_raw = read_u32_le(content, sect + 20)? as usize;
      return content.get(ptr_raw..ptr_raw.checked_add(size_of_raw)?);
    }
    sect += 40; // sizeof(IMAGE_SECTION_HEADER)
  }
  None
}

/// Compare a fixed-width, NUL-padded name field against a logical name.
fn name_eq(field: &[u8], want: &[u8]) -> bool {
  if want.len() > field.len() {
    return false;
  }
  field[..want.len()] == *want && field[want.len()..].iter().all(|&b| b == 0)
}

/// The NUL-terminated string at `off` within a string table.
fn cstr_at(strtab: &[u8], off: usize) -> Option<&[u8]> {
  let rest = strtab.get(off..)?;
  let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
  Some(&rest[..end])
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Build a valid pressed-data section blob from `raw` (the addon) so the header
  /// parse + zstd decode + SHA-512 check round-trip without a real binary.
  fn synth_section(raw: &[u8], has_config: bool) -> Vec<u8> {
    let payload = zstd::stream::encode_all(raw, 3).unwrap();
    let mut hasher = Sha512::new();
    hasher.update(&payload);
    let hash = hasher.finalize();

    let mut s = Vec::new();
    s.extend_from_slice(MAGIC_MARKER);
    s.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    s.extend_from_slice(&(raw.len() as u64).to_le_bytes());
    s.extend_from_slice(&[b'a'; CACHE_KEY_LEN]); // cache key
    s.extend_from_slice(&[1u8, 1u8, 255u8]); // platform/arch/libc
    s.extend_from_slice(&hash);
    s.push(if has_config { 1 } else { 0 });
    if has_config {
      s.extend_from_slice(&[0u8; SMOL_CONFIG_BINARY_LEN]);
    }
    s.extend_from_slice(&payload);
    s
  }

  #[test]
  fn pressed_data_round_trips() {
    let raw = b"\x7fELF this is the original addon payload, repeated.".repeat(40);
    let section = synth_section(&raw, false);
    assert_eq!(decode_pressed_data(&section).as_deref(), Some(raw.as_slice()));
  }

  #[test]
  fn pressed_data_round_trips_with_config() {
    let raw = vec![0xABu8; 5000];
    let section = synth_section(&raw, true);
    assert_eq!(decode_pressed_data(&section).as_deref(), Some(raw.as_slice()));
  }

  #[test]
  fn rejects_a_non_hybrid() {
    assert!(unwrap_if_hybrid(b"not a binary at all").is_none());
    // Right magic, truncated header.
    assert!(decode_pressed_data(MAGIC_MARKER.as_slice()).is_none());
    // No marker.
    assert!(decode_pressed_data(&[0u8; HEADER_LEN + 10]).is_none());
  }

  #[test]
  fn rejects_a_tampered_payload() {
    let raw = vec![0x11u8; 2000];
    let mut section = synth_section(&raw, false);
    // Flip a byte inside the zstd payload → SHA-512 mismatch → None.
    let last = section.len() - 1;
    section[last] ^= 0xff;
    assert!(decode_pressed_data(&section).is_none());
  }

  #[test]
  fn rejects_a_wrong_uncompressed_size() {
    let raw = vec![0x22u8; 2000];
    let mut section = synth_section(&raw, false);
    // Corrupt the uncompressed-size field (offset 32 + 8).
    section[40] = section[40].wrapping_add(1);
    assert!(decode_pressed_data(&section).is_none());
  }

  #[test]
  fn finds_pressed_data_in_a_synthetic_macho() {
    // Minimal Mach-O 64: header + one LC_SEGMENT_64("SMOL") with one section
    // __PRESSED_DATA pointing at an appended pressed-data blob.
    let raw = vec![0x42u8; 3000];
    let blob = synth_section(&raw, false);

    const LC_SEGMENT_64: u32 = 0x19;
    let header_len = 32usize;
    let seg_cmd_len = 72 + 80; // segment_command_64 + one section_64
    let blob_off = header_len + seg_cmd_len;

    let mut bin = vec![0u8; blob_off];
    // mach_header_64
    bin[0..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]); // MH_MAGIC_64 LE bytes
    bin[16..20].copy_from_slice(&1u32.to_le_bytes()); // ncmds = 1
    // segment_command_64 at offset 32
    let seg = 32;
    bin[seg..seg + 4].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
    bin[seg + 4..seg + 8].copy_from_slice(&(seg_cmd_len as u32).to_le_bytes());
    bin[seg + 8..seg + 12].copy_from_slice(b"SMOL");
    bin[seg + 64..seg + 68].copy_from_slice(&1u32.to_le_bytes()); // nsects = 1
    // section_64 at offset seg + 72
    let sect = seg + 72;
    bin[sect..sect + 14].copy_from_slice(b"__PRESSED_DATA");
    bin[sect + 40..sect + 48].copy_from_slice(&(blob.len() as u64).to_le_bytes()); // size
    bin[sect + 48..sect + 52].copy_from_slice(&(blob_off as u32).to_le_bytes()); // offset
    bin.extend_from_slice(&blob);

    assert_eq!(find_macho(&bin).map(<[u8]>::to_vec), Some(blob.clone()));
    assert_eq!(unwrap_if_hybrid(&bin).as_deref(), Some(raw.as_slice()));
  }

  #[test]
  fn finds_pressed_data_in_a_synthetic_pe() {
    let raw = vec![0x55u8; 1500];
    let blob = synth_section(&raw, false);

    // DOS stub (64B) + PE sig/COFF (24B) + one 40B section header, no optional hdr.
    let pe_off = 64usize;
    let sect_table = pe_off + 24;
    let blob_off = sect_table + 40;
    let mut bin = vec![0u8; blob_off];
    bin[0] = b'M';
    bin[1] = b'Z';
    bin[0x3c..0x40].copy_from_slice(&(pe_off as u32).to_le_bytes());
    bin[pe_off..pe_off + 4].copy_from_slice(b"PE\0\0");
    // COFF: NumberOfSections at +2, SizeOfOptionalHeader at +16.
    bin[pe_off + 4 + 2..pe_off + 4 + 4].copy_from_slice(&1u16.to_le_bytes());
    bin[pe_off + 4 + 16..pe_off + 4 + 18].copy_from_slice(&0u16.to_le_bytes());
    // Section header.
    bin[sect_table..sect_table + 8].copy_from_slice(b".PRESSED");
    bin[sect_table + 16..sect_table + 20].copy_from_slice(&(blob.len() as u32).to_le_bytes());
    bin[sect_table + 20..sect_table + 24].copy_from_slice(&(blob_off as u32).to_le_bytes());
    bin.extend_from_slice(&blob);

    assert_eq!(unwrap_if_hybrid(&bin).as_deref(), Some(raw.as_slice()));
  }

  #[test]
  fn finds_pressed_data_in_a_synthetic_elf() {
    // Minimal ELF64: e_ident + just enough header to point at a 2-entry section
    // header table (a `.shstrtab` string table + a `.PRESSED_DATA` section) and an
    // appended pressed-data blob.
    let raw = vec![0x66u8; 2200];
    let blob = synth_section(&raw, false);

    let shentsize = 64usize;
    // String table content: "\0.shstrtab\0.PRESSED_DATA\0".
    let mut strtab = vec![0u8];
    let shstrtab_name = strtab.len() as u32;
    strtab.extend_from_slice(b".shstrtab\0");
    let pressed_name = strtab.len() as u32;
    strtab.extend_from_slice(b".PRESSED_DATA\0");

    // Layout: [ehdr 64] [strtab] [shdr table: 2 * 64] [blob].
    let ehdr_len = 64usize;
    let strtab_off = ehdr_len;
    let shoff = strtab_off + strtab.len();
    let blob_off = shoff + 2 * shentsize;

    let mut bin = vec![0u8; blob_off];
    bin[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    bin[4] = 2; // EI_CLASS = 64-bit
    bin[40..48].copy_from_slice(&(shoff as u64).to_le_bytes()); // e_shoff
    bin[58..60].copy_from_slice(&(shentsize as u16).to_le_bytes()); // e_shentsize
    bin[60..62].copy_from_slice(&2u16.to_le_bytes()); // e_shnum
    bin[62..64].copy_from_slice(&0u16.to_le_bytes()); // e_shstrndx = section 0
    bin[strtab_off..strtab_off + strtab.len()].copy_from_slice(&strtab);

    // Section header 0: the string table.
    let sh0 = shoff;
    bin[sh0..sh0 + 4].copy_from_slice(&shstrtab_name.to_le_bytes()); // sh_name
    bin[sh0 + 24..sh0 + 32].copy_from_slice(&(strtab_off as u64).to_le_bytes()); // sh_offset
    bin[sh0 + 32..sh0 + 40].copy_from_slice(&(strtab.len() as u64).to_le_bytes()); // sh_size

    // Section header 1: .PRESSED_DATA.
    let sh1 = shoff + shentsize;
    bin[sh1..sh1 + 4].copy_from_slice(&pressed_name.to_le_bytes());
    bin[sh1 + 24..sh1 + 32].copy_from_slice(&(blob_off as u64).to_le_bytes());
    bin[sh1 + 32..sh1 + 40].copy_from_slice(&(blob.len() as u64).to_le_bytes());
    bin.extend_from_slice(&blob);

    assert_eq!(find_elf(&bin).map(<[u8]>::to_vec), Some(blob.clone()));
    assert_eq!(unwrap_if_hybrid(&bin).as_deref(), Some(raw.as_slice()));
  }

  #[test]
  fn name_eq_is_exact_with_nul_padding() {
    assert!(name_eq(b"SMOL\0\0\0\0\0\0\0\0\0\0\0\0", b"SMOL"));
    assert!(!name_eq(b"SMOLX\0\0\0\0\0\0\0\0\0\0\0", b"SMOL"));
    assert!(!name_eq(b"SMO\0", b"SMOL"));
  }
}
