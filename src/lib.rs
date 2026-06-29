//! `decmpfs` — apply the operating system's transparent per-file compression to a file
//! in place: macOS APFS (decmpfs), Linux btrfs, Windows NTFS. The kernel decompresses
//! on read, so the file keeps its logical size + exact contents and loads at near-native
//! speed while taking less space on disk.
//!
//! `compress_file(path)` detects the filesystem, applies compression, then verifies the
//! kernel reads the bytes back identically — rolling back on any failure. `probe(path)`
//! is the detect-only / capability-reporting half.
//!
//! Backends: btrfs (`FS_COMPR_FL` + the `btrfs.compression` property), NTFS
//! (`FSCTL_SET_COMPRESSION`), and macOS decmpfs (resource fork, kernel-roundtrip
//! verified); other targets report `Unsupported`.
//!
//! Contract: every `Outcome` is a SUCCESS; `Err` is reserved for genuine I/O failures
//! that leave the file's integrity unknown. An unsupported FS, a permission/lock issue,
//! an incompressible or too-large file are non-fatal `Outcome`s.
//!
//! Panic-free invariant: the deny below keeps non-test code free of the obvious panic
//! sources; all slice indexing is length-guarded.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::path::Path;

/// What happened to the file. Only `Err` is a hard failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
  /// Applied and on-disk allocation actually decreased.
  Compressed { before: u64, after: u64 },
  /// Applied (or already set) but on-disk size did not drop — incompressible
  /// or sub-cluster. Content is byte-identical and fully loadable.
  NoGain { before: u64, after: u64 },
  /// Already carried the compression flag/xattr before we touched it.
  AlreadyCompressed { before: u64 },
  /// This FS/OS has no per-file transparent compression (ext4, xfs, ZFS, ReFS,
  /// FAT, tmpfs, overlay/network mounts). Caller falls through to the cache.
  Unsupported { reason: UnsupportedReason },
  /// Detected support but could not apply (permissions, lock, immutable,
  /// rollback). Warn-and-continue; never a hard error.
  Skipped { reason: SkipReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedReason {
  /// Filesystem (by allowlist) has no transparent compression.
  Filesystem,
  /// Network/overlay/bind mount where the signal is unreliable.
  NetworkOrOverlay,
  /// Built for an OS with no backend (or skeleton: not yet implemented).
  PlatformBuild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
  /// EACCES / EPERM / EROFS — read-only or unowned (e.g. unprivileged container).
  PermissionDenied,
  /// A write handle is held / ETXTBSY / sharing violation; could not lock.
  Busy,
  /// UF_IMMUTABLE / SF_IMMUTABLE and we declined to toggle it.
  Immutable,
  /// EFS / FILE_ATTRIBUTE_ENCRYPTED.
  Encrypted,
  /// Applied, structural verification failed, rolled back to the original.
  IntegrityRevert,
  /// Post-apply loadability (magic-bytes) check failed, rolled back.
  NotLoadable,
  /// Exceeds a backend limit (e.g. decmpfs u32 offsets cap at 4 GiB).
  TooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
  Supported,
  AlreadyCompressed,
  Unsupported(UnsupportedReason),
}

/// Genuine failures only. A capability/permission gap is an `Outcome`, not an `Error`.
#[derive(Debug)]
pub enum Error {
  Io {
    context: &'static str,
    source: std::io::Error,
  },
  NotFound(std::path::PathBuf),
}

impl std::fmt::Display for Error {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Error::Io { context, source } => write!(f, "io error at {context}: {source}"),
      Error::NotFound(p) => write!(f, "file not found: {}", p.display()),
    }
  }
}

impl std::error::Error for Error {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Error::Io { source, .. } => Some(source),
      Error::NotFound(_) => None,
    }
  }
}

/// Wrap the last OS error with context — shared by every backend.
pub(crate) fn io(context: &'static str) -> Error {
  Error::Io {
    context,
    source: std::io::Error::last_os_error(),
  }
}

/// A NUL-checked C string from a path, for the unix backends that hand paths to
/// libc.
#[cfg(unix)]
pub(crate) fn cstring(path: &Path) -> Result<std::ffi::CString, Error> {
  use std::os::unix::ffi::OsStrExt;
  std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::Io {
    context: "path has interior NUL",
    source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
  })
}

/// Detect-only, no mutation — for dry-run / capability reporting.
pub fn probe(path: &Path) -> Result<Support, Error> {
  backend::detect(path)
}

/// THE entry point: detect → gate → apply → verify → rollback-on-failure.
/// Idempotent. Never panics. Never corrupts the file.
pub fn compress_file(path: &Path) -> Result<Outcome, Error> {
  if !path.exists() {
    return Err(Error::NotFound(path.to_path_buf()));
  }
  match backend::detect(path)? {
    Support::Unsupported(reason) => Ok(Outcome::Unsupported { reason }),
    Support::AlreadyCompressed => Ok(Outcome::AlreadyCompressed {
      before: verify::on_disk_bytes(path)?,
    }),
    Support::Supported => safety::apply_guarded(path),
  }
}

mod safety;
mod verify;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod backend;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod backend;
#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod backend;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[path = "unsupported.rs"]
mod backend;


#[cfg(test)]
mod tests {
  use super::*;

  fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("decmpfs-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
  }

  // A minimal native-magic payload (ELF header) so a backend will attempt to
  // compress it rather than skip a trivially-small file.
  fn fake_addon() -> Vec<u8> {
    let mut raw = vec![0x7f, 0x45, 0x4c, 0x46];
    raw.extend_from_slice(&[7u8; 9000]);
    raw
  }

  #[test]
  fn compress_file_errors_when_missing() {
    let p = std::path::Path::new("/no/such/addon.node");
    assert!(matches!(compress_file(p), Err(Error::NotFound(_))));
  }

  #[test]
  fn error_display_and_source() {
    let nf = Error::NotFound(std::path::PathBuf::from("/x"));
    assert!(nf.to_string().contains("not found"));
    assert!(std::error::Error::source(&nf).is_none());
    let io = Error::Io {
      context: "ctx",
      source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    };
    assert!(io.to_string().contains("ctx"));
    assert!(std::error::Error::source(&io).is_some());
  }

  #[cfg(unix)]
  #[test]
  fn probe_reports_a_support_variant_without_mutating() {
    // probe never errors on an existing path — it returns a Support.
    assert!(matches!(
      probe(std::path::Path::new("/dev/null")),
      Ok(Support::Supported | Support::AlreadyCompressed | Support::Unsupported(_))
    ));
  }

  #[cfg(unix)]
  #[test]
  fn compress_file_reports_unsupported_on_a_non_compressing_fs() {
    // /dev/null exists but devfs has no compression backend → Unsupported.
    let out = compress_file(std::path::Path::new("/dev/null"));
    assert!(
      matches!(out, Ok(Outcome::Unsupported { .. })),
      "devfs → Unsupported, got {out:?}"
    );
  }

  // APFS is always a compressing FS, so macOS exercises the full success path:
  // compress_file → apply_guarded → backend::apply_inplace → verify → classify.
  #[cfg(target_os = "macos")]
  #[test]
  fn compress_file_compresses_then_is_idempotent_and_transparent() {
    let dir = scratch("ok");
    let path = dir.join("addon.node");
    std::fs::write(&path, fake_addon()).unwrap();

    let out = compress_file(&path);
    assert!(
      matches!(
        out,
        Ok(Outcome::Compressed { .. } | Outcome::NoGain { .. } | Outcome::AlreadyCompressed { .. })
      ),
      "writable addon on APFS → applied, got {out:?}"
    );
    // Transparent: the kernel hands back the exact original bytes.
    assert_eq!(std::fs::read(&path).unwrap(), fake_addon());
    // Idempotent: a second pass detects it's already compressed.
    assert!(matches!(
      compress_file(&path),
      Ok(Outcome::AlreadyCompressed { .. })
    ));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn compress_file_skips_a_read_only_file() {
    // On a compressing FS a read-only file can't be opened rw → fail-soft turns the
    // EACCES into Skipped(PermissionDenied). Root bypasses mode bits, so skip there.
    if unsafe { libc::geteuid() } == 0 {
      return;
    }
    let dir = scratch("ro");
    let path = dir.join("addon.node");
    std::fs::write(&path, fake_addon()).unwrap();
    if !matches!(probe(&path), Ok(Support::Supported)) {
      std::fs::remove_dir_all(&dir).ok();
      return;
    }
    let mut perm = std::fs::metadata(&path).unwrap().permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(&path, perm).unwrap();
    let outcome = compress_file(&path);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
    assert!(
      matches!(
        outcome,
        Ok(Outcome::Skipped {
          reason: SkipReason::PermissionDenied
        })
      ),
      "read-only → Skipped(PermissionDenied), got {outcome:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
  }
}
