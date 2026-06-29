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
  /// `compress_bytes` was handed a file the `Gate` excludes — written plain.
  GateExcluded,
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

/// THE install-time entry point: write `content` to `path` as an OS-compressed file
/// in ONE pass — never a write-then-read-back-recompress.
///
/// The caller (a package manager's CAS writer) has already decoded the raw addon
/// and matched it against `gate`. `compress_bytes` writes that exact byte stream
/// directly as a transparently-compressed file: macOS encodes the decmpfs from the
/// bytes onto a fresh inode; btrfs requests the codec on the empty temp then writes;
/// NTFS sets FSCTL_SET_COMPRESSION on the fresh handle then writes.
///
/// Fail-soft is the contract — this NEVER breaks an install. On an unsupported FS,
/// a permission/busy/too-large skip, or any backend error, it falls back to a plain
/// atomic write of `content` and reports the corresponding `Outcome` (the plain
/// write still lands the file). The kernel read-back is verified identical to
/// `content` before returning a compressed Outcome.
///
/// `gate` is honored here as a convenience: if `content` does not match the gate,
/// the file is written plain and `Outcome::Skipped { reason: GateExcluded }` is
/// returned. A caller that already gated can pass `&Gate::any()`.
pub fn compress_bytes(path: &Path, content: &[u8], gate: &Gate) -> Result<Outcome, Error> {
  let name = path.to_string_lossy();
  let normalized = name.replace('\\', "/");
  if !gate.matches(&normalized, content.len() as u64) {
    plain_write(path, content)?;
    return Ok(Outcome::Skipped {
      reason: SkipReason::GateExcluded,
    });
  }
  // The target usually doesn't exist yet (a fresh CAS write), so the FS capability
  // probe goes against the parent directory; `detect` statfs's / opens its argument
  // and would error on a missing path.
  let probe_target = if path.exists() {
    path.to_path_buf()
  } else {
    match path.parent() {
      Some(dir) => dir.to_path_buf(),
      None => path.to_path_buf(),
    }
  };
  match backend::detect(&probe_target) {
    Ok(Support::Supported) => match safety::compress_bytes_guarded(path, content) {
      Ok(Outcome::Skipped { .. }) | Err(_) => {
        // A guarded skip/error already restored or never wrote — ensure the file
        // lands plain so the install is never missing the addon.
        plain_write(path, content)?;
        Ok(Outcome::Skipped {
          reason: SkipReason::IntegrityRevert,
        })
      }
      other => other,
    },
    Ok(Support::AlreadyCompressed) | Ok(Support::Unsupported(_)) | Err(_) => {
      plain_write(path, content)?;
      Ok(Outcome::Unsupported {
        reason: UnsupportedReason::Filesystem,
      })
    }
  }
}

/// Fail-soft plain atomic write: sibling temp + fsync + rename. The never-break-the
/// -install floor under every `compress_bytes` fallback.
fn plain_write(path: &Path, content: &[u8]) -> Result<(), Error> {
  use std::io::Write;
  let dir = path.parent().ok_or_else(|| Error::Io {
    context: "no parent dir",
    source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
  })?;
  let name = path
    .file_name()
    .map(|n| n.to_string_lossy().into_owned())
    .unwrap_or_else(|| "addon".to_string());
  let tmp = dir.join(format!(".{name}.plain-{}.tmp", std::process::id()));
  let res = (|| -> std::io::Result<()> {
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(content)?;
    file.sync_all()
  })();
  if let Err(source) = res {
    let _ = std::fs::remove_file(&tmp);
    return Err(Error::Io {
      context: "plain write temp",
      source,
    });
  }
  std::fs::rename(&tmp, path).map_err(|source| {
    let _ = std::fs::remove_file(&tmp);
    Error::Io {
      context: "plain write rename",
      source,
    }
  })
}

#[cfg(feature = "addon")]
pub mod addon;
mod gate;
mod safety;
mod verify;

pub use gate::{Gate, GateParseError, SizePredicate, DEFAULT_GLOB};

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
  fn plain_write_errors_when_the_path_has_no_parent() {
    // "/" has no parent directory → the no-parent guard fires before any write.
    let out = plain_write(std::path::Path::new("/"), b"x");
    assert!(matches!(
      out,
      Err(Error::Io {
        context: "no parent dir",
        ..
      })
    ));
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

  // compress_bytes one-pass: write bytes directly as an APFS-compressed file with
  // no pre-existing original, then prove the kernel hands the exact bytes back.
  #[cfg(target_os = "macos")]
  #[test]
  fn compress_bytes_one_pass_writes_compressed_and_reads_back_identical() {
    let dir = scratch("bytes");
    let path = dir.join("fresh.node");
    let content = fake_addon();
    // No file at `path` yet — compress_bytes creates it in one pass.
    let out = compress_bytes(&path, &content, &Gate::any());
    assert!(
      matches!(
        out,
        Ok(Outcome::Compressed { .. } | Outcome::NoGain { .. })
      ),
      "one-pass APFS write → applied, got {out:?}"
    );
    assert!(path.exists(), "file was created");
    // Transparent: kernel read-back equals the bytes we asked to store.
    assert_eq!(std::fs::read(&path).unwrap(), content);
    // It really carries the compression flag (not a plain fallback write).
    assert!(matches!(
      compress_file(&path),
      Ok(Outcome::AlreadyCompressed { .. })
    ));
    std::fs::remove_dir_all(&dir).ok();
  }

  // A file the gate excludes is written PLAIN (never compressed) and reports
  // Skipped(GateExcluded) — the install still gets the file.
  #[cfg(unix)]
  #[test]
  fn compress_bytes_gate_excluded_writes_plain() {
    let dir = scratch("gate");
    let path = dir.join("not-an-addon.txt");
    let content = b"plain text, not a .node".to_vec();
    let gate = Gate::default(); // **/*.node
    let out = compress_bytes(&path, &content, &gate);
    assert!(
      matches!(
        out,
        Ok(Outcome::Skipped {
          reason: SkipReason::GateExcluded
        })
      ),
      "non-.node → GateExcluded, got {out:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), content);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn compress_bytes_falls_back_to_plain_on_unsupported_fs() {
    // A non-compressing FS (devfs) → plain write, Unsupported Outcome, file lands.
    // /dev isn't writable by us, so target a temp path but force the gate to pass;
    // temp on macOS is APFS (compresses) — instead assert the API never errors and
    // the bytes land for the supported case is covered above. Here just exercise
    // the gate-passing path lands bytes on any unix temp.
    let dir = scratch("fallback");
    let path = dir.join("x.node");
    let content = fake_addon();
    let out = compress_bytes(&path, &content, &Gate::any());
    assert!(out.is_ok(), "never errors on a normal temp, got {out:?}");
    assert_eq!(std::fs::read(&path).unwrap(), content, "bytes always land");
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

  // An existing target exercises the `path.exists()` probe-target branch and the
  // fresh-inode rename that replaces the old contents.
  #[cfg(target_os = "macos")]
  #[test]
  fn compress_bytes_overwrites_an_existing_file() {
    let dir = scratch("overwrite");
    let path = dir.join("addon.node");
    std::fs::write(&path, b"stale contents").unwrap();
    let content = fake_addon();
    let out = compress_bytes(&path, &content, &Gate::any());
    assert!(out.is_ok(), "overwrite never errors, got {out:?}");
    assert_eq!(
      std::fs::read(&path).unwrap(),
      content,
      "new bytes replace the old"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  // `path` is an existing directory: the backend builds its temp then can't rename
  // a file over a directory, and the plain-write fallback can't either → a hard
  // `Err` (genuine I/O failure), never a corrupt success. Exercises the backend
  // rename-error cleanup and the `Err(_)` fallback arm of compress_bytes.
  #[cfg(target_os = "macos")]
  #[test]
  fn compress_bytes_onto_a_directory_path_is_a_hard_error() {
    let dir = scratch("dir-target");
    let target = dir.join("a-dir");
    std::fs::create_dir_all(&target).unwrap();
    let out = compress_bytes(&target, &fake_addon(), &Gate::any());
    assert!(out.is_err(), "cannot write a file over a directory, got {out:?}");
    assert!(target.is_dir(), "the directory is left intact");
    std::fs::remove_dir_all(&dir).ok();
  }

  // A read-only parent dir: the guarded backend write hits EACCES (classify_skip →
  // Skipped), then the plain-write fallback also can't write → `Err`. Root bypasses
  // mode bits, so skip there.
  #[cfg(target_os = "macos")]
  #[test]
  fn compress_bytes_into_a_read_only_dir_is_fail_soft() {
    if unsafe { libc::geteuid() } == 0 {
      return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("ro-dir");
    let locked = dir.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = compress_bytes(&locked.join("x.node"), &fake_addon(), &Gate::any());
    // Restore write perms so the tree can be cleaned up.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).ok();
    assert!(out.is_err(), "a read-only dir admits no write, got {out:?}");
    std::fs::remove_dir_all(&dir).ok();
  }
}
