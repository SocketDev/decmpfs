//! Self-replacing executable packing (feature `exe`).
//!
//! [`pack_executable`] turns a real executable `E` into a stub `E'` carrying a
//! zstd-compressed copy of `E` in a signable `SMOL/__DECMPFS` section (macOS) or
//! an EOF footer (ELF/PE). On first run `E'` calls [`self_replace_and_exec`]:
//! it decompresses the payload, writes `E` back to disk **FS-compressed** via
//! [`crate::compress_bytes`], atomically renames over `argv[0]`, re-signs on
//! macOS, and `execve`s the materialized binary. Every later run is native — the
//! stub is gone, replaced by the real (smaller-on-disk) executable.
//!
//! The section/footer wire format is owned by [`section`]; the object surgery by
//! [`inject`]; the runtime swap by [`replace`]. All three are private; the crate
//! surface is the two functions below plus [`PackOutcome`].

use std::path::Path;

use crate::{Error, Gate};

mod inject;
mod replace;
mod section;

/// The result of packing an executable. Only `Err` is a hard failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackOutcome {
  /// Packed: `before` = original size, `after` = the stub's size on disk.
  Packed { before: u64, after: u64 },
  /// The gate excluded the input (by glob/size) — nothing written.
  SkippedGate,
}

/// Host-side packer: read `src`, compress it, inject the payload into a stub, and
/// write the self-replacing executable to `dest`. `gate` filters by glob/size the
/// same way [`crate::compress_bytes`] does; a gate miss returns
/// [`PackOutcome::SkippedGate`] without writing.
///
/// The stub bytes are the CURRENT executable's own image by default (a decmpfs
/// binary that links the `exe` feature IS the stub); pass an explicit stub via
/// [`pack_executable_with_stub`] to cross-pack.
pub fn pack_executable(src: &Path, dest: &Path, gate: &Gate) -> Result<PackOutcome, Error> {
  let stub = std::env::current_exe().map_err(|source| Error::Io {
    context: "resolve current_exe for the pack stub",
    source,
  })?;
  pack_executable_with_stub(&stub, src, dest, gate)
}

/// [`pack_executable`] with an explicit stub image — the self-replacing runtime
/// binary whose `SMOL/__DECMPFS` section/footer receives the payload.
///
/// `gate` is checked against `dest` (the caller's chosen name, normalized to
/// forward slashes) before anything is read or written — a miss returns
/// [`PackOutcome::SkippedGate`] and touches neither `stub` nor `dest`.
pub fn pack_executable_with_stub(
  stub: &Path,
  src: &Path,
  dest: &Path,
  gate: &Gate,
) -> Result<PackOutcome, Error> {
  let src_bytes = std::fs::read(src).map_err(|source| Error::Io {
    context: "read the source executable to pack",
    source,
  })?;
  let src_len = src_bytes.len() as u64;

  let dest_name = dest.to_string_lossy().replace('\\', "/");
  if !gate.matches(&dest_name, src_len) {
    return Ok(PackOutcome::SkippedGate);
  }

  let content_hash = section::fnv1a64(&src_bytes);
  let zstd_payload =
    zstd::stream::encode_all(src_bytes.as_slice(), 19).map_err(|source| Error::Io {
      context: "zstd-compress the source executable",
      source,
    })?;

  let stub_bytes = std::fs::read(stub).map_err(|source| Error::Io {
    context: "read the stub executable",
    source,
  })?;

  // Mach-O carries the payload in a signable `SMOL/__DECMPFS` section body;
  // ELF/PE carry it in an EOF footer. `inject::inject_payload`'s ELF/PE arm is
  // a verbatim append (no structural surgery), so the footer must already be
  // fully formed here — only the Mach-O arm gets its own wrapper.
  let section_body = if is_macho64(&stub_bytes) {
    section::build_section_payload(content_hash, &zstd_payload)
  } else {
    section::build_footer(content_hash, &zstd_payload)
  };

  let packed = inject::inject_payload(&stub_bytes, &section_body).map_err(|message| Error::Io {
    context: "inject the packed payload into the stub",
    source: std::io::Error::other(message),
  })?;

  std::fs::write(dest, &packed).map_err(|source| Error::Io {
    context: "write the packed executable",
    source,
  })?;

  #[cfg(unix)]
  mark_executable(dest)?;

  // Re-sign only a Mach-O output — gate on the STUB's format, not the host OS,
  // so cross-packing an ELF/PE stub on a macOS host never hands a non-Mach-O
  // file (whose trailing footer a future codesign could mutate) to codesign.
  // `resign` is itself a no-op off macOS.
  if is_macho64(&stub_bytes) {
    inject::resign(dest).map_err(|message| Error::Io {
      context: "re-sign the packed executable",
      source: std::io::Error::other(message),
    })?;
  }

  let after = std::fs::metadata(dest)
    .map_err(|source| Error::Io {
      context: "stat the packed executable's on-disk size",
      source,
    })?
    .len();

  Ok(PackOutcome::Packed {
    before: src_len,
    after,
  })
}

/// A 64-bit little-endian Mach-O magic at the head of `bytes` — the same
/// dispatch [`inject::inject_payload`] makes internally, mirrored here so the
/// right payload wrapper (section body vs. footer) is built before the call and
/// so the runtime swap only hands a Mach-O to `codesign`.
pub(crate) fn is_macho64(bytes: &[u8]) -> bool {
  bytes.get(0..4) == Some(&0xfeed_facfu32.to_le_bytes())
}

/// Set the owner/group/world execute bits on `path`, on top of whatever mode
/// it landed with.
#[cfg(unix)]
fn mark_executable(path: &Path) -> Result<(), Error> {
  use std::os::unix::fs::PermissionsExt;

  let mut perms = std::fs::metadata(path)
    .map_err(|source| Error::Io {
      context: "read the packed executable's permissions",
      source,
    })?
    .permissions();
  perms.set_mode(perms.mode() | 0o111);
  std::fs::set_permissions(path, perms).map_err(|source| Error::Io {
    context: "set the packed executable's execute bit",
    source,
  })
}

/// True when `path` is a packed stub — it carries a decmpfs payload (a
/// `SMOL/__DECMPFS` section on Mach-O, an EOF footer on ELF/PE). A plain or
/// already-materialized executable returns `false`.
pub fn contains_payload(path: &Path) -> bool {
  section::read_self_section_bytes(path).is_some()
}

/// Runtime entry the packed stub calls from its `main`: resolve self → read the
/// payload → decompress → FS-compress the bytes to disk → atomically replace
/// `argv[0]` → re-sign (macOS) → `execve`. On success it does NOT return (the
/// process image is replaced); `Ok(false)` means "this binary is not a packed
/// stub, run your normal main". `Err` is a genuine I/O / integrity failure.
pub fn self_replace_and_exec(argv: &[String]) -> Result<bool, Error> {
  replace::materialize_and_exec(argv)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
  use super::*;

  /// A scratch dir unique to one test, cleaned up by the caller via
  /// [`std::fs::remove_dir_all`] once the test finishes.
  fn scratch_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("decmpfs-pack-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
  }

  #[cfg(target_os = "macos")]
  fn synthetic_macho_stub_with_linkedit(
    first_section_offset: u32,
    linkedit_fileoff: u64,
    linkedit_body: &[u8],
  ) -> Vec<u8> {
    // Mirrors `inject`'s own test fixture: a minimal two-segment Mach-O
    // (`__TEXT` anchoring the header-slack boundary, `__LINKEDIT` as the
    // splice point) big enough for `inject_macho` to walk for real.
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_SEGMENT_64: u32 = 0x19;
    const MACH_HEADER_64_SIZE: usize = 32;
    const SEGMENT_COMMAND_64_SIZE: usize = 72;
    const NEW_LC_SIZE: usize = SEGMENT_COMMAND_64_SIZE + 80; // segment + one section_64

    let linkedit_lc_off = MACH_HEADER_64_SIZE + NEW_LC_SIZE;
    let linkedit_len = linkedit_body.len() as u64;
    let mut m = vec![0u8; linkedit_fileoff as usize + linkedit_body.len()];

    m[0..4].copy_from_slice(&MH_MAGIC_64.to_le_bytes());
    m[16..20].copy_from_slice(&2u32.to_le_bytes()); // ncmds: __TEXT, __LINKEDIT
    m[20..24].copy_from_slice(&((NEW_LC_SIZE + SEGMENT_COMMAND_64_SIZE) as u32).to_le_bytes());

    let text = MACH_HEADER_64_SIZE;
    m[text..text + 4].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
    m[text + 4..text + 8].copy_from_slice(&(NEW_LC_SIZE as u32).to_le_bytes());
    m[text + 8..text + 14].copy_from_slice(b"__TEXT");
    m[text + 64..text + 68].copy_from_slice(&1u32.to_le_bytes()); // nsects
    let text_sect = text + SEGMENT_COMMAND_64_SIZE;
    m[text_sect..text_sect + 6].copy_from_slice(b"__text");
    m[text_sect + 16..text_sect + 22].copy_from_slice(b"__TEXT");
    m[text_sect + 48..text_sect + 52].copy_from_slice(&first_section_offset.to_le_bytes());

    m[linkedit_lc_off..linkedit_lc_off + 4].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
    m[linkedit_lc_off + 4..linkedit_lc_off + 8]
      .copy_from_slice(&(SEGMENT_COMMAND_64_SIZE as u32).to_le_bytes());
    m[linkedit_lc_off + 8..linkedit_lc_off + 18].copy_from_slice(b"__LINKEDIT");
    m[linkedit_lc_off + 24..linkedit_lc_off + 32]
      .copy_from_slice(&(0x1_0000_0000u64 + linkedit_fileoff).to_le_bytes());
    m[linkedit_lc_off + 40..linkedit_lc_off + 48].copy_from_slice(&linkedit_fileoff.to_le_bytes());
    m[linkedit_lc_off + 48..linkedit_lc_off + 56].copy_from_slice(&linkedit_len.to_le_bytes());

    m[linkedit_fileoff as usize..linkedit_fileoff as usize + linkedit_body.len()]
      .copy_from_slice(linkedit_body);
    m
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn pack_into_a_macho_stub_round_trips_through_read_self_section_bytes() {
    let dir = scratch_dir("macho");
    let stub_path = dir.join("stub.bin");
    let src_path = dir.join("src.bin");
    let dest_path = dir.join("dest.bin");

    let stub = synthetic_macho_stub_with_linkedit(512, 600, b"LINKEDIT-CONTENT");
    std::fs::write(&stub_path, &stub).expect("write stub");
    let src_bytes = b"the original executable's bytes, repeated a bit to give zstd something to chew on. the original executable's bytes.".to_vec();
    std::fs::write(&src_path, &src_bytes).expect("write src");

    let outcome = pack_executable_with_stub(&stub_path, &src_path, &dest_path, &Gate::any())
      .expect("pack succeeds");
    let Some(after) = (match outcome {
      PackOutcome::Packed { before, after } => {
        assert_eq!(before, src_bytes.len() as u64);
        Some(after)
      }
      PackOutcome::SkippedGate => None,
    }) else {
      panic!("Gate::any() must never skip");
    };
    assert_eq!(
      after,
      std::fs::metadata(&dest_path).expect("stat dest").len()
    );

    let got = section::read_self_section_bytes(&dest_path).expect("section found");
    assert_eq!(got.content_hash, section::fnv1a64(&src_bytes));
    let decompressed = zstd::stream::decode_all(got.payload.as_slice()).expect("zstd decode");
    assert_eq!(decompressed, src_bytes);

    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&dest_path)
      .expect("stat dest")
      .permissions()
      .mode();
    assert_ne!(mode & 0o111, 0, "packed executable must be executable");

    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn pack_into_an_elf_stub_appends_a_footer_that_round_trips() {
    let dir = scratch_dir("elf");
    let stub_path = dir.join("stub.bin");
    let src_path = dir.join("src.bin");
    let dest_path = dir.join("dest.bin");

    // A minimal ELF-magic stub — `is_macho64` rejects it, so the packer routes
    // it through `section::build_footer` + the append-only ELF/PE arm of
    // `inject::inject_payload`, regardless of the host this test runs on.
    let mut stub = vec![0u8; 64];
    stub[0..4].copy_from_slice(b"\x7fELF");
    std::fs::write(&stub_path, &stub).expect("write stub");
    let src_bytes = b"another synthetic executable payload for the ELF/PE footer path".to_vec();
    std::fs::write(&src_path, &src_bytes).expect("write src");

    let outcome = pack_executable_with_stub(&stub_path, &src_path, &dest_path, &Gate::any())
      .expect("pack succeeds");
    assert_eq!(
      outcome,
      PackOutcome::Packed {
        before: src_bytes.len() as u64,
        after: (stub.len()
          + section::build_footer(0, &[]).len()
          + zstd::stream::encode_all(src_bytes.as_slice(), 19)
            .expect("zstd encode")
            .len()) as u64,
      }
    );

    let dest_bytes = std::fs::read(&dest_path).expect("read dest");
    let raw_footer = section::find_footer(&dest_bytes).expect("footer found");
    // Wire format: `[zstd payload][content_hash u64 LE][payload_len u64 LE][MAGIC]`.
    let payload_len = raw_footer
      .len()
      .checked_sub(24)
      .expect("footer long enough");
    let payload = &raw_footer[0..payload_len];
    let hash_bytes: [u8; 8] = raw_footer[payload_len..payload_len + 8]
      .try_into()
      .expect("hash slice is 8 bytes");
    let content_hash = u64::from_le_bytes(hash_bytes);

    assert_eq!(content_hash, section::fnv1a64(&src_bytes));
    let decompressed = zstd::stream::decode_all(payload).expect("zstd decode");
    assert_eq!(decompressed, src_bytes);

    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn a_gate_miss_skips_and_writes_nothing() {
    let dir = scratch_dir("gate-miss");
    let stub_path = dir.join("stub.bin"); // never read: the gate rejects first.
    let src_path = dir.join("src.bin");
    let dest_path = dir.join("reject.bin");

    std::fs::write(&src_path, b"some source bytes").expect("write src");
    let gate = Gate::new(Some("*.selected"), None).expect("gate parses");

    let outcome = pack_executable_with_stub(&stub_path, &src_path, &dest_path, &gate)
      .expect("gate miss is not an error");
    assert_eq!(outcome, PackOutcome::SkippedGate);
    assert!(!dest_path.exists(), "a gate miss must write nothing");

    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn is_macho64_recognizes_only_the_64_bit_le_magic() {
    assert!(is_macho64(&0xfeed_facfu32.to_le_bytes()));
    assert!(!is_macho64(b"\x7fELF"));
    assert!(!is_macho64(b"MZ"));
    assert!(!is_macho64(b"sho"));
  }
}
