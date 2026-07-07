//! The runtime swap: read the packed stub's own payload, decompress, write the
//! real executable back to disk FS-compressed, atomically replace `argv[0]`,
//! re-sign (macOS), and `execve`.
//!
//! A running image can't be overwritten on Windows, so there the swap is
//! deferred: write the materialized binary alongside and schedule a
//! rename-on-next-start (MoveFileEx with MOVEFILE_DELAY_UNTIL_REBOOT is too
//! coarse; the runtime writes a `.pending` sibling and a tiny relauncher).
//!
//! Unix path: `std::os::unix::process::CommandExt::exec` replaces the process
//! image so control never returns on success.

#[cfg(windows)]
use std::path::Path;

use crate::Error;
#[cfg(windows)]
use crate::Gate;

use super::section::SectionData;

/// Read the current executable's own packed payload, if any. `Ok(None)` means
/// this binary is a plain executable (not a packed stub) — the caller runs its
/// normal `main`.
pub(crate) fn read_self_payload() -> Result<Option<SectionData>, Error> {
  let exe = std::env::current_exe().map_err(|source| Error::Io {
    context: "resolve current_exe to read the self payload",
    source,
  })?;
  Ok(super::section::read_self_section_bytes(&exe))
}

/// Decompress `section`'s zstd payload and verify it against `content_hash`.
/// Pure and unit-testable independently of `current_exe`/the filesystem.
fn decode_verified(section: &SectionData) -> Result<Vec<u8>, Error> {
  let raw = zstd::stream::decode_all(section.payload.as_slice()).map_err(|source| Error::Io {
    context: "zstd-decompress the self payload",
    source,
  })?;
  if super::section::fnv1a64(&raw) != section.content_hash {
    return Err(Error::Io {
      context: "verify self payload integrity",
      source: std::io::Error::other("content hash mismatch: the packed payload is corrupt"),
    });
  }
  Ok(raw)
}

/// Resolve `current_exe()` and the directory it lives in (the directory the
/// materialized binary must land in too, so the swap stays on one filesystem —
/// a same-filesystem rename is atomic and a same-filesystem `clonefile` is
/// available to the FS-compression writer).
fn resolve_exe_and_dir() -> Result<(std::path::PathBuf, std::path::PathBuf), Error> {
  let exe = std::env::current_exe().map_err(|source| Error::Io {
    context: "resolve current_exe for the runtime swap",
    source,
  })?;
  let dir = exe
    .parent()
    .ok_or_else(|| Error::Io {
      context: "resolve current_exe's parent directory for the runtime swap",
      source: std::io::Error::other("current_exe has no parent directory"),
    })?
    .to_path_buf();
  Ok((exe, dir))
}

/// Materialize + swap + exec. Does not return on success (process image
/// replaced). `Ok(false)` means "this binary is not a packed stub, run your
/// normal main"; `Err` is a genuine I/O / integrity failure.
#[cfg(unix)]
pub(crate) fn materialize_and_exec(argv: &[String]) -> Result<bool, Error> {
  use std::os::unix::process::CommandExt;

  let Some(section) = read_self_payload()? else {
    return Ok(false);
  };
  let raw = decode_verified(&section)?;
  let (exe, dir) = resolve_exe_and_dir()?;
  let file_name = exe
    .file_name()
    .ok_or_else(|| Error::Io {
      context: "resolve current_exe's file name for the runtime swap",
      source: std::io::Error::other("current_exe has no file name"),
    })?
    .to_string_lossy()
    .into_owned();
  let temp = dir.join(format!(
    ".{file_name}.decmpfs-materializing-{}",
    std::process::id()
  ));

  // Order matters: write the raw bytes, sign, THEN FS-compress. `codesign`
  // rewrites the whole file (embedding the signature), which strips any decmpfs
  // compression — so signing must run on the plain bytes and compress_file
  // applies decmpfs last. decmpfs is transparent on read, so the signature the
  // kernel hands the loader stays valid.
  std::fs::write(&temp, &raw).map_err(|source| Error::Io {
    context: "write the materialized binary before compressing",
    source,
  })?;

  let mode = std::fs::metadata(&exe)
    .map_err(|source| Error::Io {
      context: "read current_exe's mode to preserve it on the materialized binary",
      source,
    })?
    .permissions();
  std::fs::set_permissions(&temp, mode).map_err(|source| Error::Io {
    context: "copy current_exe's mode onto the materialized binary",
    source,
  })?;

  // Re-sign only a Mach-O payload — codesign can't sign a script or other
  // non-Mach-O executable, and handing it one is a no-op at best.
  #[cfg(target_os = "macos")]
  if super::is_macho64(&raw) {
    super::inject::resign(&temp).map_err(|message| Error::Io {
      context: "re-sign the materialized binary",
      source: std::io::Error::other(message),
    })?;
  }

  // FS-compress in place, AFTER signing — the on-disk win the packer promised.
  crate::compress_file(&temp)?;

  std::fs::rename(&temp, &exe).map_err(|source| Error::Io {
    context: "atomically rename the materialized binary over argv[0]",
    source,
  })?;

  let source = std::process::Command::new(&exe)
    .args(argv.get(1..).unwrap_or(&[]))
    .exec();
  // `exec` only returns on failure — success replaces this process image.
  Err(Error::Io {
    context: "exec the materialized binary",
    source,
  })
}

/// Windows can't overwrite its own running image, so the swap is deferred: the
/// materialized binary is written to a `.decmpfs-pending` sibling and launched
/// directly. The original stub on disk is untouched — the swap-over-original
/// completes on a later run (a follow-up can add a rename-on-reboot / relauncher
/// so the pending binary takes the original's name and the stub disappears; for
/// now every launch re-materializes and re-execs the pending copy).
#[cfg(windows)]
pub(crate) fn materialize_and_exec(argv: &[String]) -> Result<bool, Error> {
  let Some(section) = read_self_payload()? else {
    return Ok(false);
  };
  let raw = decode_verified(&section)?;
  let (exe, _) = resolve_exe_and_dir()?;

  let mut pending = exe.clone().into_os_string();
  pending.push(".decmpfs-pending");
  let pending = Path::new(&pending).to_path_buf();

  crate::compress_bytes(&pending, &raw, &Gate::any())?;

  std::process::Command::new(&pending)
    .args(argv.get(1..).unwrap_or(&[]))
    .spawn()
    .map_err(|source| Error::Io {
      context: "spawn the pending materialized binary",
      source,
    })?;
  Ok(true)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn decode_verified_round_trips_a_valid_payload() {
    let raw = b"hello from the materialized executable".to_vec();
    let compressed = zstd::stream::encode_all(raw.as_slice(), 3).expect("zstd encode");
    let section = SectionData {
      content_hash: super::super::section::fnv1a64(&raw),
      payload: compressed,
    };
    let got = decode_verified(&section).expect("decodes and verifies");
    assert_eq!(got, raw);
  }

  #[test]
  fn decode_verified_rejects_a_corrupted_hash() {
    let raw = b"hello from the materialized executable".to_vec();
    let compressed = zstd::stream::encode_all(raw.as_slice(), 3).expect("zstd encode");
    let section = SectionData {
      // Flip a bit — the decompressed bytes no longer match this hash.
      content_hash: super::super::section::fnv1a64(&raw) ^ 1,
      payload: compressed,
    };
    assert!(decode_verified(&section).is_err());
  }

  #[test]
  fn decode_verified_rejects_a_non_zstd_payload() {
    let section = SectionData {
      content_hash: 0,
      payload: b"not zstd at all".to_vec(),
    };
    assert!(decode_verified(&section).is_err());
  }

  #[test]
  fn read_self_payload_returns_none_for_a_plain_test_binary() {
    // The cargo test binary is a plain executable, not a packed stub, so it
    // carries no `SMOL/__DECMPFS` section or EOF footer.
    assert!(read_self_payload().expect("reads current_exe").is_none());
  }
}
