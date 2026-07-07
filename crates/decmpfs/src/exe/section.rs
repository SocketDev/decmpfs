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

/// Head of the payload. Distinguishes our section/footer from stray bytes and
/// fails closed on a malformed image.
pub(crate) const SECTION_MAGIC: &[u8; 8] = b"DCMPFSX1";

/// The validated payload extracted from a packed stub.
pub(crate) struct SectionData {
  /// FNV-1a of the RAW executable — names the materialized file + verifies decode.
  pub content_hash: u64,
  /// The zstd payload (the compressed raw executable).
  pub payload: Vec<u8>,
}

/// Assemble the section body the packer injects (Mach-O path).
pub(crate) fn build_section_payload(content_hash: u64, zstd_payload: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(16 + zstd_payload.len());
  out.extend_from_slice(SECTION_MAGIC);
  out.extend_from_slice(&content_hash.to_le_bytes());
  out.extend_from_slice(zstd_payload);
  out
}

/// Parse a Mach-O section body `[MAGIC][hash][payload]`. Pure, unit-testable.
#[allow(dead_code)] // wired in the section stage
pub(crate) fn parse_section_payload(raw: &[u8]) -> Option<SectionData> {
  if raw.len() < 16 || &raw[0..8] != SECTION_MAGIC {
    return None;
  }
  Some(SectionData {
    content_hash: u64::from_le_bytes(raw[8..16].try_into().ok()?),
    payload: raw[16..].to_vec(),
  })
}

/// FNV-1a (64-bit) — names the cache/materialize target without decoding.
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
  let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
  for &b in bytes {
    hash ^= b as u64;
    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
  }
  hash
}
