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

/// Best-effort exclusive advisory lock over a stub's self-replace. Concurrent
/// first-loads of the SAME packed binary (a burst of parallel launches right
/// after install) would otherwise EACH run the expensive
/// decode+re-sign+FS-compress+rename; the lock lets one win while the rest wait,
/// then observe the completed swap and exec the real binary. Held for the
/// guard's lifetime: std's file-lock fd is `O_CLOEXEC`, so the lock releases at
/// `execve` (the re-exec'd real binary must not inherit it) and on any early
/// `return` (the guard drops → the `File` closes). A lock we can't create or
/// acquire (a read-only dir, an FS without advisory locks) yields an UNLOCKED
/// guard — the swap stays correct via the atomic rename, only the dedup is lost.
///
/// Unix-only: Windows defers the swap to a `.decmpfs-pending` sibling and spawns
/// it (never overwrites the running image), so its launches are independent and
/// the per-write atomicity already covers concurrency — a lock there would
/// wrongly serialize concurrent launches.
#[cfg(unix)]
struct SelfReplaceLock {
  // Kept alive to hold the OS lock; dropped (closed → unlocked) at scope end.
  _file: Option<std::fs::File>,
}

#[cfg(unix)]
impl SelfReplaceLock {
  fn acquire(dir: &std::path::Path, file_name: &str) -> Self {
    let path = dir.join(format!(".{file_name}.decmpfs.lock"));
    let file = std::fs::OpenOptions::new()
      .create(true)
      .truncate(false)
      .write(true)
      .open(&path)
      .ok();
    if let Some(f) = file.as_ref() {
      // Blocking exclusive lock; an Err leaves the guard effectively unlocked
      // (we fall through — the atomic rename still guarantees correctness).
      let _ = f.lock();
    }
    Self { _file: file }
  }
}

/// Materialize + swap + exec. Does not return on success (process image
/// replaced). `Ok(false)` means "this binary is not a packed stub, run your
/// normal main"; `Err` is a genuine I/O / integrity failure.
#[cfg(unix)]
pub(crate) fn materialize_and_exec(argv: &[String]) -> Result<bool, Error> {
  use std::os::unix::process::CommandExt;

  // Cheap pre-check: a plain executable (no payload) runs its normal main.
  if read_self_payload()?.is_none() {
    return Ok(false);
  }
  let (exe, dir) = resolve_exe_and_dir()?;
  let file_name = exe
    .file_name()
    .ok_or_else(|| Error::Io {
      context: "resolve current_exe's file name for the runtime swap",
      source: std::io::Error::other("current_exe has no file name"),
    })?
    .to_string_lossy()
    .into_owned();

  // Serialize concurrent first-loads of this stub (see SelfReplaceLock). Released
  // at execve (O_CLOEXEC) or when this guard drops on an early return.
  let _lock = SelfReplaceLock::acquire(&dir, &file_name);

  // Re-read UNDER the lock: a process that went before us may have already
  // swapped the on-disk exe, so its payload section is gone. If so, skip the
  // whole materialize and exec the already-real binary directly.
  let Some(section) = read_self_payload()? else {
    let source = std::process::Command::new(&exe)
      .args(argv.get(1..).unwrap_or(&[]))
      .exec();
    return Err(Error::Io {
      context: "exec the already-materialized binary",
      source,
    });
  };
  let raw = decode_verified(&section)?;
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
#[cfg_attr(coverage_nightly, coverage(off))]
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

  #[cfg(unix)]
  #[test]
  fn self_replace_lock_is_exclusive_and_releases_on_drop() {
    let dir = std::env::temp_dir().join(format!("decmpfs-srlock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let name = "toolstub";
    let lock_path = dir.join(format!(".{name}.decmpfs.lock"));
    {
      let _guard = SelfReplaceLock::acquire(&dir, name);
      // A distinct handle on the same lock file can't take it while the guard
      // holds the exclusive lock (flock conflicts across open descriptions).
      let other = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .unwrap();
      assert!(
        other.try_lock().is_err(),
        "lock is held exclusively while the guard is alive"
      );
    }
    // The guard dropped → the File closed → the lock is free again.
    let other = std::fs::OpenOptions::new()
      .create(true)
      .truncate(false)
      .write(true)
      .open(&lock_path)
      .unwrap();
    assert!(
      other.try_lock().is_ok(),
      "lock released when the guard drops"
    );
    other.unlock().ok();
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn self_replace_lock_falls_through_when_the_dir_is_unwritable() {
    // A lock file we can't create yields an unlocked guard, no panic — the swap
    // still works via the atomic rename, just without the dedup.
    let guard = SelfReplaceLock::acquire(std::path::Path::new("/no/such/decmpfs/dir"), "x");
    drop(guard);
  }
}
