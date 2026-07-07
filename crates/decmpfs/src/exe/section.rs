//! The `SMOL/__DECMPFS` section / EOF-footer wire format, shared by the packer
//! (write) and the runtime (read-self). One source of truth for the ABI.
//!
//! Section body (Mach-O, signable): `[MAGIC][content_hash u64 LE][zstd payload]`.
//! Footer (ELF/PE, appended): `[zstd payload][content_hash u64 LE][payload_len
//! u64 LE][MAGIC]` at EOF, so the runtime seeks the tail, reads `payload_len`,
//! and validates the trailing magic.
//!
//! Ported from napi-rs `crates/decmpfs/src/section.rs`; parse is hand-rolled,
//! length-guarded, no `object` crate (the crate stays dep-lean + panic=abort).

use std::path::Path;

/// Head of the payload. Distinguishes our section/footer from stray bytes and
/// fails closed on a malformed image.
pub(crate) const SECTION_MAGIC: &[u8; 8] = b"DCMPFSX1";

/// The validated payload extracted from a packed stub.
#[allow(dead_code)] // fields read by the replace stage's consumer
pub(crate) struct SectionData {
  /// FNV-1a of the RAW executable — names the materialized file + verifies decode.
  pub content_hash: u64,
  /// The zstd payload (the compressed raw executable).
  pub payload: Vec<u8>,
}

/// Assemble the section body the packer injects (Mach-O path).
#[allow(dead_code)] // wired in the inject stage
pub(crate) fn build_section_payload(content_hash: u64, zstd_payload: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(16 + zstd_payload.len());
  out.extend_from_slice(SECTION_MAGIC);
  out.extend_from_slice(&content_hash.to_le_bytes());
  out.extend_from_slice(zstd_payload);
  out
}

/// Parse a Mach-O section body `[MAGIC][hash][payload]`. Pure, unit-testable.
pub(crate) fn parse_section_payload(raw: &[u8]) -> Option<SectionData> {
  if raw.len() < 16 || &raw[0..8] != SECTION_MAGIC {
    return None;
  }
  Some(SectionData {
    content_hash: u64::from_le_bytes(raw[8..16].try_into().ok()?),
    payload: raw[16..].to_vec(),
  })
}

/// Assemble the EOF footer the packer appends (ELF/PE path): the payload, then
/// its content hash, then its length (so the runtime can find the payload's
/// start by walking back from EOF), then the trailing magic.
#[allow(dead_code)] // wired in the inject stage
pub(crate) fn build_footer(content_hash: u64, zstd_payload: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(zstd_payload.len() + 24);
  out.extend_from_slice(zstd_payload);
  out.extend_from_slice(&content_hash.to_le_bytes());
  out.extend_from_slice(&(zstd_payload.len() as u64).to_le_bytes());
  out.extend_from_slice(SECTION_MAGIC);
  out
}

/// Parse the footer region located by [`find_footer`]: trailing MAGIC, then
/// `payload_len`, then `content_hash`, then the payload fills everything before
/// it. Pure, unit-testable against [`build_footer`] without a real ELF/PE.
#[allow(dead_code)] // used on non-macOS hosts in production; exercised by tests everywhere
fn parse_footer_payload(raw: &[u8]) -> Option<SectionData> {
  if raw.len() < 24 {
    return None;
  }
  let magic_off = raw.len().checked_sub(8)?;
  if &raw[magic_off..] != SECTION_MAGIC {
    return None;
  }
  let len_off = magic_off.checked_sub(8)?;
  let payload_len = u64::from_le_bytes(raw.get(len_off..len_off + 8)?.try_into().ok()?) as usize;
  let hash_off = len_off.checked_sub(8)?;
  if hash_off != payload_len {
    return None;
  }
  Some(SectionData {
    content_hash: u64::from_le_bytes(raw.get(hash_off..hash_off + 8)?.try_into().ok()?),
    payload: raw.get(0..hash_off)?.to_vec(),
  })
}

/// FNV-1a (64-bit) — names the cache/materialize target without decoding.
#[allow(dead_code)] // wired in the replace stage
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
  let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
  for &b in bytes {
    hash ^= b as u64;
    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
  }
  hash
}

/// Read `self_path` from disk and extract its packed payload: the Mach-O
/// `SMOL/__DECMPFS` section on macOS, the EOF footer everywhere else. `None`
/// if the file can't be read, has no such section/footer, or it's malformed —
/// the caller treats that as "not a packed stub".
#[allow(dead_code)] // wired in the replace stage
pub(crate) fn read_self_section_bytes(self_path: &Path) -> Option<SectionData> {
  let bytes = std::fs::read(self_path).ok()?;
  #[cfg(target_os = "macos")]
  {
    parse_section_payload(find_section(&bytes)?)
  }
  #[cfg(not(target_os = "macos"))]
  {
    parse_footer_payload(find_footer(&bytes)?)
  }
}

/// A null-padded fixed-width name slot equals `want` (Mach-O's 16-byte
/// `sectname`/`segname`): the leading bytes match and the remainder is NUL.
#[cfg(target_os = "macos")]
fn name_eq(slot: &[u8], want: &[u8]) -> bool {
  slot.len() >= want.len()
    && &slot[..want.len()] == want
    && slot[want.len()..].iter().all(|&b| b == 0)
}

// ---------------------------------------------------------------------------
// macOS — 64-bit little-endian Mach-O (every darwin target is x86_64/arm64,
// both LE). Walk load commands → SMOL segment → __DECMPFS section.
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
pub(crate) fn find_section(bytes: &[u8]) -> Option<&[u8]> {
  const MH_MAGIC_64: u32 = 0xfeed_facf;
  const LC_SEGMENT_64: u32 = 0x19;

  if u32::from_le_bytes(bytes.get(0..4)?.try_into().ok()?) != MH_MAGIC_64 {
    return None;
  }
  let ncmds = u32::from_le_bytes(bytes.get(16..20)?.try_into().ok()?);
  // mach_header_64 is 32 bytes; load commands follow.
  let mut off = 32usize;
  for _ in 0..ncmds {
    let cmd = u32::from_le_bytes(bytes.get(off..off + 4)?.try_into().ok()?);
    let cmdsize = u32::from_le_bytes(bytes.get(off + 4..off + 8)?.try_into().ok()?) as usize;
    if cmdsize < 8 {
      return None;
    }
    if cmd == LC_SEGMENT_64 && name_eq(bytes.get(off + 8..off + 24)?, b"SMOL") {
      // segment_command_64: cmd,cmdsize,segname[16],vmaddr,vmsize,fileoff,
      // filesize,maxprot,initprot,nsects,flags — nsects at off+64, sections at off+72.
      let nsects = u32::from_le_bytes(bytes.get(off + 64..off + 68)?.try_into().ok()?);
      let mut soff = off + 72;
      for _ in 0..nsects {
        // section_64: sectname[16],segname[16],addr(8),size(8),offset(4),...(80 total).
        if name_eq(bytes.get(soff..soff + 16)?, b"__DECMPFS") {
          let size = u64::from_le_bytes(bytes.get(soff + 40..soff + 48)?.try_into().ok()?) as usize;
          let offset =
            u32::from_le_bytes(bytes.get(soff + 48..soff + 52)?.try_into().ok()?) as usize;
          return bytes.get(offset..offset.checked_add(size)?);
        }
        soff = soff.checked_add(80)?;
      }
    }
    off = off.checked_add(cmdsize)?;
  }
  None
}

// ---------------------------------------------------------------------------
// ELF/PE — no per-format object surgery needed: the payload rides an appended
// EOF footer `[payload][content_hash][payload_len][MAGIC]`. Read the fixed
// trailer back-to-front and slice the payload out by its declared length.
// ---------------------------------------------------------------------------
// Not target-gated: the trailer bytes carry no OS-specific structure, so the
// parser is portable. [`read_self_section_bytes`] is what restricts it to
// non-macOS hosts at the call site.
#[allow(dead_code)] // used on non-macOS hosts in production; exercised by tests everywhere
pub(crate) fn find_footer(bytes: &[u8]) -> Option<&[u8]> {
  const TRAILER: usize = 24; // content_hash(8) + payload_len(8) + MAGIC(8)
  if bytes.len() < TRAILER {
    return None;
  }
  let magic_off = bytes.len().checked_sub(8)?;
  if &bytes[magic_off..] != SECTION_MAGIC {
    return None;
  }
  let len_off = magic_off.checked_sub(8)?;
  let payload_len = u64::from_le_bytes(bytes.get(len_off..len_off + 8)?.try_into().ok()?) as usize;
  let hash_off = len_off.checked_sub(8)?;
  let payload_start = hash_off.checked_sub(payload_len)?;
  bytes.get(payload_start..bytes.len())
}

/// Build a minimal Mach-O image carrying one `SMOL/__DECMPFS` section whose
/// bytes are exactly `body`, so [`find_section`]'s load-command/section-header
/// arithmetic is validated against a known layout without a real binary. The
/// real-binary round-trip is covered by the packer-phase integration test.
#[cfg(all(test, target_os = "macos"))]
pub(crate) fn synthetic_object_with_section(body: &[u8]) -> Vec<u8> {
  // header(32) + one LC_SEGMENT_64 (72 segment + 80 section = 152) = 184, then
  // the section body appended at offset 184.
  let mut m = vec![0u8; 184];
  m[0..4].copy_from_slice(&0xfeed_facfu32.to_le_bytes()); // MH_MAGIC_64
  m[16..20].copy_from_slice(&1u32.to_le_bytes()); // ncmds
  m[32..36].copy_from_slice(&0x19u32.to_le_bytes()); // LC_SEGMENT_64
  m[36..40].copy_from_slice(&152u32.to_le_bytes()); // cmdsize
  m[40..44].copy_from_slice(b"SMOL"); // segname
  m[96..100].copy_from_slice(&1u32.to_le_bytes()); // nsects (off 32 + 64)
  let s = 104; // sections start at off 32 + 72
  m[s..s + 9].copy_from_slice(b"__DECMPFS"); // sectname
  m[s + 16..s + 20].copy_from_slice(b"SMOL"); // section's segname
  m[s + 40..s + 48].copy_from_slice(&(body.len() as u64).to_le_bytes()); // size
  m[s + 48..s + 52].copy_from_slice(&184u32.to_le_bytes()); // offset
  m.extend_from_slice(body);
  m
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn build_and_parse_section_payload_round_trip() {
    let body = build_section_payload(0xfeed_face_dead_beef, b"the-zstd-bytes");
    let got = parse_section_payload(&body).expect("parses");
    assert_eq!(got.content_hash, 0xfeed_face_dead_beef);
    assert_eq!(got.payload, b"the-zstd-bytes");
    // Empty payload is still a valid (header-only) body.
    assert!(parse_section_payload(&build_section_payload(0, b"")).is_some());
  }

  #[test]
  fn parse_section_payload_rejects_bad_input() {
    assert!(parse_section_payload(b"short").is_none());
    assert!(parse_section_payload(b"WRONGMAG\0\0\0\0\0\0\0\0").is_none());
  }

  #[test]
  fn build_and_find_footer_round_trip() {
    let footer = build_footer(0xfeed_face_dead_beef, b"the-zstd-bytes");
    // The footer trails arbitrary leading bytes, exactly as it does at the end
    // of a real ELF/PE executable.
    let mut bytes = b"leading executable bytes before the footer".to_vec();
    bytes.extend_from_slice(&footer);

    let raw = find_footer(&bytes).expect("footer found");
    let got = parse_footer_payload(raw).expect("parses");
    assert_eq!(got.content_hash, 0xfeed_face_dead_beef);
    assert_eq!(got.payload, b"the-zstd-bytes");

    // Empty payload is still a valid (header-only) footer.
    let empty = build_footer(0, b"");
    assert!(parse_footer_payload(find_footer(&empty).expect("footer found")).is_some());
  }

  #[test]
  fn find_footer_rejects_malformed_or_short_input() {
    assert!(find_footer(b"short").is_none());
    assert!(find_footer(&[0u8; 23]).is_none());

    // Trailing magic corrupted.
    let mut bad_magic = build_footer(1, b"x");
    let last = bad_magic.len() - 1;
    bad_magic[last] = b'!';
    assert!(find_footer(&bad_magic).is_none());

    // payload_len lies about the payload's true length.
    let mut bad_len = build_footer(1, b"xyz");
    let len_off = bad_len.len() - 16;
    bad_len[len_off..len_off + 8].copy_from_slice(&999u64.to_le_bytes());
    assert!(find_footer(&bad_len).is_none());
  }

  #[test]
  fn fnv1a64_known_vector() {
    // FNV-1a 64-bit offset basis is the hash of the empty string.
    assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    // Published FNV-1a 64-bit test vector for the single byte "a".
    assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn find_section_reads_from_a_synthetic_macho() {
    let body = build_section_payload(0x0123_4567_89ab_cdef, b"the-zstd-payload-bytes");
    let obj = synthetic_object_with_section(&body);
    let raw = find_section(&obj).expect("section found");
    let got = parse_section_payload(raw).expect("parses");
    assert_eq!(got.content_hash, 0x0123_4567_89ab_cdef);
    assert_eq!(got.payload, b"the-zstd-payload-bytes");
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn find_section_rejects_non_macho() {
    assert!(find_section(b"not a binary at all").is_none());
    assert!(find_section(&[]).is_none());
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn read_self_section_bytes_reads_a_real_file() {
    let body = build_section_payload(0x0102_0304_0506_0708, b"stub-payload");
    let obj = synthetic_object_with_section(&body);

    let dir = std::env::temp_dir().join(format!("decmpfs-section-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("stub.bin");
    std::fs::write(&path, &obj).expect("write synthetic object");

    let got = read_self_section_bytes(&path).expect("section found");
    assert_eq!(got.content_hash, 0x0102_0304_0506_0708);
    assert_eq!(got.payload, b"stub-payload");

    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(not(target_os = "macos"))]
  #[test]
  fn read_self_section_bytes_reads_a_footer_from_a_real_file() {
    let mut bytes = b"leading executable bytes".to_vec();
    bytes.extend_from_slice(&build_footer(0x0102_0304_0506_0708, b"stub-payload"));

    let dir = std::env::temp_dir().join(format!("decmpfs-footer-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("stub.bin");
    std::fs::write(&path, &bytes).expect("write synthetic footer file");

    let got = read_self_section_bytes(&path).expect("footer found");
    assert_eq!(got.content_hash, 0x0102_0304_0506_0708);
    assert_eq!(got.payload, b"stub-payload");

    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn read_self_section_bytes_rejects_missing_file() {
    let missing = std::env::temp_dir().join("decmpfs-section-does-not-exist-at-all.bin");
    assert!(read_self_section_bytes(&missing).is_none());
  }
}
