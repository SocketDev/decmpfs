//! Shared fixtures for the real-filesystem integration tests (btrfs today, more
//! later). Not every test uses every helper, so dead_code is expected.
#![allow(dead_code)]

/// ELF magic + a compressible (non-zero, so it isn't stored as a sparse hole)
/// 2 MiB body — large enough for a real on-disk compression measurement.
pub fn fake_addon() -> Vec<u8> {
  let mut raw = vec![0x7f, 0x45, 0x4c, 0x46];
  let pattern = b"decmpfs compressible addon body pattern ";
  while raw.len() < 2 * 1024 * 1024 {
    raw.extend_from_slice(pattern);
  }
  raw.truncate(2 * 1024 * 1024);
  raw
}
