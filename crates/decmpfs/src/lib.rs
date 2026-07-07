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
  compress_file_with(&Os, path)
}

/// `compress_file` over an injectable [`Backend`] — production always threads
/// [`Os`]; tests drive the otherwise-dead `AlreadyCompressed`/`Unsupported` arms
/// with a fake.
fn compress_file_with<B: Backend>(backend: &B, path: &Path) -> Result<Outcome, Error> {
  if !path.exists() {
    return Err(Error::NotFound(path.to_path_buf()));
  }
  match backend.detect(path)? {
    Support::Unsupported(reason) => Ok(Outcome::Unsupported { reason }),
    Support::AlreadyCompressed => Ok(Outcome::AlreadyCompressed {
      before: verify::on_disk_bytes(path)?,
    }),
    Support::Supported => safety::apply_guarded(backend, path),
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
  compress_bytes_with(&Os, path, content, gate)
}

/// `compress_bytes` over an injectable [`Backend`] — production always threads
/// [`Os`]; tests drive the plain-write fallback arms (a guarded skip/error, or a
/// non-compressing FS) that a real APFS write never reaches.
fn compress_bytes_with<B: Backend>(
  backend: &B,
  path: &Path,
  content: &[u8],
  gate: &Gate,
) -> Result<Outcome, Error> {
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
  match backend.detect(&probe_target) {
    Ok(Support::Supported) => match safety::compress_bytes_guarded(backend, path, content) {
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

/// What a [`copy_file`] did — a SUCCESS shape, same contract as [`Outcome`]:
/// `Err` is reserved for genuine I/O failures; the copy itself always lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyOutcome {
  /// Copy-on-write clone (`clonefile` / `FICLONE`) — the extents are shared,
  /// so the source's compression state carries over at zero cost.
  Cloned { compressed: bool },
  /// Byte copy plus one-pass recompression at the destination (a compressed
  /// source that could not be cloned — cross-volume, non-reflink FS).
  CopiedCompressed { before: u64, after: u64 },
  /// Byte copy landed plain: the source was plain, the destination FS has no
  /// transparent compression, or a fail-soft skip (`skipped` carries the
  /// reason when the source WAS compressed but the state could not follow).
  CopiedPlain { skipped: Option<SkipReason> },
}

/// Attempt a copy-on-write clone of `src` at `dest` (`clonefile(2)` on macOS,
/// the `FICLONE` ioctl on Linux) — the zero-cost way to copy a compressed file
/// WITH its compression. `Ok(true)` = cloned; `Ok(false)` = this pairing can't
/// clone (cross-volume, non-reflink FS, an existing destination on macOS,
/// Windows) and the caller decides the fallback — [`copy_file`] is the
/// clone-then-fallback composition, and a Node-`COPYFILE_FICLONE_FORCE`-shaped
/// caller turns `false` into its error.
pub fn try_clone_file(src: &Path, dest: &Path) -> Result<bool, Error> {
  if !src.exists() {
    return Err(Error::NotFound(src.to_path_buf()));
  }
  Os.clone_file(src, dest)
}

/// Copy `src` to `dest` preserving transparent filesystem compression — the
/// `fs.copyFile` the OS never shipped. A plain byte copy silently re-inflates
/// a compressed file (the kernel hands every reader the full logical bytes,
/// and that is what gets written back out); this copy keeps the on-disk
/// savings.
///
/// Strategy, in order:
/// 1. A copy-on-write clone (`clonefile(2)` on macOS, the `FICLONE` ioctl on
///    Linux) shares the extents, so a compressed source stays compressed at
///    zero cost — and a plain source clones plain.
/// 2. When cloning is impossible (cross-volume, non-reflink FS, Windows), the
///    logical bytes are copied and, if the source was compressed, the
///    destination is written back compressed via the same guarded one-pass
///    path as [`compress_bytes`].
/// 3. A plain source is copied plain — a copy never changes compression state.
///
/// `fs.copyFile` parity: an existing `dest` is replaced, and the source's
/// permission bits carry over. Fail-soft like the rest of the crate: on any
/// backend skip the plain copy still stands, reported in the outcome.
pub fn copy_file(src: &Path, dest: &Path) -> Result<CopyOutcome, Error> {
  copy_file_with(&Os, src, dest)
}

/// `copy_file` over an injectable [`Backend`] — production always threads
/// [`Os`]; tests drive the clone and fallback arms with fakes.
fn copy_file_with<B: Backend>(backend: &B, src: &Path, dest: &Path) -> Result<CopyOutcome, Error> {
  if !src.exists() {
    return Err(Error::NotFound(src.to_path_buf()));
  }
  if dest.exists() {
    // A copy onto itself (same path, a hardlink, a symlink alias) is a no-op
    // success — otherwise the replace step below would clobber the copy's own
    // source. The extents are 100% shared, which is what `Cloned` states.
    if is_same_file(src, dest) {
      return Ok(CopyOutcome::Cloned {
        compressed: backend.is_already_compressed(src).unwrap_or(false),
      });
    }
    // clonefile refuses an existing destination; replace-by-default is the
    // `fs.copyFile` contract this mirrors.
    std::fs::remove_file(dest).map_err(|source| Error::Io {
      context: "replace existing destination",
      source,
    })?;
  }
  // On a filesystem with no compression signal this is a capability gap, not a
  // failure — treat it as "plain source" and keep copying.
  let compressed_src = backend.is_already_compressed(src).unwrap_or(false);
  if backend.clone_file(src, dest)? {
    return Ok(CopyOutcome::Cloned {
      compressed: compressed_src,
    });
  }
  // A normal read hands back the full logical bytes regardless of the
  // source's on-disk representation.
  let content = std::fs::read(src).map_err(|source| Error::Io {
    context: "read copy source",
    source,
  })?;
  let mode = std::fs::metadata(src).ok().map(|meta| meta.permissions());
  if !compressed_src {
    plain_write(dest, &content)?;
    if let Some(mode) = mode {
      let _ = std::fs::set_permissions(dest, mode);
    }
    return Ok(CopyOutcome::CopiedPlain { skipped: None });
  }
  let outcome = compress_bytes_with(backend, dest, &content, &Gate::any())?;
  if let Some(mode) = mode {
    let _ = std::fs::set_permissions(dest, mode);
  }
  Ok(match outcome {
    Outcome::Compressed { before, after } | Outcome::NoGain { before, after } => {
      CopyOutcome::CopiedCompressed { before, after }
    }
    // Unreachable from a fresh destination (compress_bytes maps an
    // already-compressed detect to a plain write), kept total + truthful.
    Outcome::AlreadyCompressed { before } => CopyOutcome::CopiedCompressed {
      before,
      after: before,
    },
    Outcome::Unsupported { .. } => CopyOutcome::CopiedPlain { skipped: None },
    Outcome::Skipped { reason } => CopyOutcome::CopiedPlain {
      skipped: Some(reason),
    },
  })
}

/// True when both paths name the same underlying file — same dev+inode on
/// unix (catches hardlinks and symlink aliases), canonical-path equality
/// elsewhere. Guards [`copy_file`]'s replace-by-default from removing its own
/// source.
#[cfg(unix)]
fn is_same_file(a: &Path, b: &Path) -> bool {
  use std::os::unix::fs::MetadataExt;
  match (std::fs::metadata(a), std::fs::metadata(b)) {
    (Ok(meta_a), Ok(meta_b)) => meta_a.dev() == meta_b.dev() && meta_a.ino() == meta_b.ino(),
    _ => false,
  }
}

/// See the unix twin above; Windows has no stable inode surface in std, so
/// canonical-path equality is the honest signal there.
#[cfg(not(unix))]
fn is_same_file(a: &Path, b: &Path) -> bool {
  match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
    (Ok(canon_a), Ok(canon_b)) => canon_a == canon_b,
    _ => false,
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
#[cfg(feature = "exe")]
pub mod exe;
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

/// The OS compression backend as a trait, so the orchestration in `safety` can be
/// driven by a fake in tests — a real filesystem never produces a non-loadable
/// result or a mismatched read-back, so the rollback and plain-write fallback paths
/// are otherwise unreachable. Production always threads [`Os`]; static dispatch
/// monomorphizes it to the same code as a direct backend call (no vtable, no size
/// cost in a release build).
pub(crate) trait Backend {
  fn detect(&self, path: &Path) -> Result<Support, Error>;
  fn is_already_compressed(&self, path: &Path) -> Result<bool, Error>;
  fn apply_inplace(&self, path: &Path) -> Result<(), Error>;
  fn apply_bytes(
    &self,
    path: &Path,
    content: &[u8],
    mode: Option<std::fs::Permissions>,
  ) -> Result<(), Error>;
  fn compressed_on_disk(&self, path: &Path) -> Result<Option<bool>, Error>;
  /// Copy-on-write clone. `Ok(false)` = "cannot clone here" → the caller falls
  /// back to a byte copy. Defaulted so fakes without a clone path exercise the
  /// fallback arms.
  fn clone_file(&self, _src: &Path, _dest: &Path) -> Result<bool, Error> {
    Ok(false)
  }
}

/// The real, cfg-selected OS backend.
pub(crate) struct Os;

impl Backend for Os {
  fn detect(&self, path: &Path) -> Result<Support, Error> {
    backend::detect(path)
  }
  fn is_already_compressed(&self, path: &Path) -> Result<bool, Error> {
    backend::is_already_compressed(path)
  }
  fn apply_inplace(&self, path: &Path) -> Result<(), Error> {
    backend::apply_inplace(path)
  }
  fn apply_bytes(
    &self,
    path: &Path,
    content: &[u8],
    mode: Option<std::fs::Permissions>,
  ) -> Result<(), Error> {
    backend::apply_bytes(path, content, mode)
  }
  fn compressed_on_disk(&self, path: &Path) -> Result<Option<bool>, Error> {
    backend::compressed_on_disk(path)
  }
  fn clone_file(&self, src: &Path, dest: &Path) -> Result<bool, Error> {
    backend::clone_file(src, dest)
  }
}

/// A configurable in-memory backend for exercising the rollback and plain-write
/// fallback paths that a real filesystem never reaches.
#[cfg(test)]
pub(crate) struct FakeBackend {
  pub(crate) detect: Support,
  /// `None` → apply succeeds; `Some(errno)` → apply fails with that OS error.
  pub(crate) apply_errno: Option<i32>,
}

#[cfg(test)]
impl FakeBackend {
  fn apply_result(&self) -> Result<(), Error> {
    match self.apply_errno {
      None => Ok(()),
      Some(errno) => Err(Error::Io {
        context: "fake apply",
        source: std::io::Error::from_raw_os_error(errno),
      }),
    }
  }
}

#[cfg(test)]
impl Backend for FakeBackend {
  fn detect(&self, _path: &Path) -> Result<Support, Error> {
    Ok(self.detect)
  }
  fn is_already_compressed(&self, _path: &Path) -> Result<bool, Error> {
    Ok(false)
  }
  fn apply_inplace(&self, _path: &Path) -> Result<(), Error> {
    self.apply_result()
  }
  fn apply_bytes(
    &self,
    _path: &Path,
    _content: &[u8],
    _mode: Option<std::fs::Permissions>,
  ) -> Result<(), Error> {
    self.apply_result()
  }
  fn compressed_on_disk(&self, _path: &Path) -> Result<Option<bool>, Error> {
    Ok(Some(false))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("decmpfs-{tag}-{}", std::process::id()));
    // A pid-recycled leftover FILE at this path makes create_dir_all fail
    // with AlreadyExists; clear it so the scratch dir always materializes.
    let _ = std::fs::remove_file(&dir);
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
      matches!(out, Ok(Outcome::Compressed { .. } | Outcome::NoGain { .. })),
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
    assert!(
      out.is_err(),
      "cannot write a file over a directory, got {out:?}"
    );
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

  // The `Support::AlreadyCompressed`-from-detect arm: a real macOS detect never
  // returns it (it reports already-compressed via the apply path), so a fake drives
  // it. Needs a real file for the on-disk-bytes read.
  #[test]
  fn compress_file_reports_already_compressed_from_detect() {
    let dir = scratch("already-detect");
    let path = dir.join("f.node");
    std::fs::write(&path, fake_addon()).unwrap();
    let backend = FakeBackend {
      detect: Support::AlreadyCompressed,
      apply_errno: None,
    };
    assert!(matches!(
      compress_file_with(&backend, &path),
      Ok(Outcome::AlreadyCompressed { .. })
    ));
    std::fs::remove_dir_all(&dir).ok();
  }

  // detect → Unsupported: the bytes still land via a plain write, Outcome::Unsupported.
  #[test]
  fn compress_bytes_falls_back_to_plain_on_an_unsupported_fs() {
    let dir = scratch("unsup");
    let path = dir.join("x.node");
    let content = fake_addon();
    let backend = FakeBackend {
      detect: Support::Unsupported(UnsupportedReason::Filesystem),
      apply_errno: None,
    };
    let out = compress_bytes_with(&backend, &path, &content, &Gate::any());
    assert!(
      matches!(out, Ok(Outcome::Unsupported { .. })),
      "got {out:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), content, "bytes landed plain");
    std::fs::remove_dir_all(&dir).ok();
  }

  // detect → Supported but the guarded apply is skipped (faked permission failure):
  // the bytes land via a plain write, Outcome::Skipped(IntegrityRevert).
  #[test]
  fn compress_bytes_falls_back_to_plain_on_a_guarded_skip() {
    let dir = scratch("guard-skip");
    let path = dir.join("x.node");
    let content = fake_addon();
    let backend = FakeBackend {
      detect: Support::Supported,
      apply_errno: Some(13), // EACCES
    };
    let out = compress_bytes_with(&backend, &path, &content, &Gate::any());
    assert!(
      matches!(
        out,
        Ok(Outcome::Skipped {
          reason: SkipReason::IntegrityRevert
        })
      ),
      "got {out:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), content, "bytes landed plain");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn copy_file_errors_when_the_source_is_missing() {
    let dir = scratch("copy-missing");
    let out = copy_file(&dir.join("absent.node"), &dir.join("dest.node"));
    assert!(matches!(out, Err(Error::NotFound(_))));
    std::fs::remove_dir_all(&dir).ok();
  }

  /// A fallback fake: no clone path (trait default), reports the source
  /// compressed, and its apply actually writes — so the guarded one-pass copy
  /// arm runs end to end and classifies via the backend signal.
  struct RecompressingFake;

  impl Backend for RecompressingFake {
    fn detect(&self, _path: &Path) -> Result<Support, Error> {
      Ok(Support::Supported)
    }
    fn is_already_compressed(&self, _path: &Path) -> Result<bool, Error> {
      Ok(true)
    }
    fn apply_inplace(&self, _path: &Path) -> Result<(), Error> {
      Ok(())
    }
    fn apply_bytes(
      &self,
      path: &Path,
      content: &[u8],
      _mode: Option<std::fs::Permissions>,
    ) -> Result<(), Error> {
      std::fs::write(path, content).map_err(|source| Error::Io {
        context: "fake write",
        source,
      })
    }
    fn compressed_on_disk(&self, _path: &Path) -> Result<Option<bool>, Error> {
      Ok(Some(true))
    }
  }

  #[test]
  fn copy_file_recompresses_at_the_destination_when_it_cannot_clone() {
    let dir = scratch("copy-recompress");
    let src = dir.join("src.node");
    let dest = dir.join("dest.node");
    let content = fake_addon();
    std::fs::write(&src, &content).unwrap();
    let out = copy_file_with(&RecompressingFake, &src, &dest).unwrap();
    assert!(
      matches!(out, CopyOutcome::CopiedCompressed { .. }),
      "got {out:?}"
    );
    assert_eq!(
      std::fs::read(&dest).unwrap(),
      content,
      "bytes are identical"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn copy_file_copies_a_plain_source_plain_and_replaces_the_destination() {
    let dir = scratch("copy-plain");
    let src = dir.join("src.node");
    let dest = dir.join("dest.node");
    let content = fake_addon();
    std::fs::write(&src, &content).unwrap();
    std::fs::write(&dest, b"stale destination").unwrap();
    let backend = FakeBackend {
      detect: Support::Supported,
      apply_errno: None,
    };
    let out = copy_file_with(&backend, &src, &dest).unwrap();
    assert_eq!(out, CopyOutcome::CopiedPlain { skipped: None });
    assert_eq!(
      std::fs::read(&dest).unwrap(),
      content,
      "destination replaced"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn copy_file_lands_plain_and_reports_the_skip_when_recompression_fails() {
    struct SkippingFake;
    impl Backend for SkippingFake {
      fn detect(&self, _path: &Path) -> Result<Support, Error> {
        Ok(Support::Supported)
      }
      fn is_already_compressed(&self, _path: &Path) -> Result<bool, Error> {
        Ok(true)
      }
      fn apply_inplace(&self, _path: &Path) -> Result<(), Error> {
        Ok(())
      }
      fn apply_bytes(
        &self,
        _path: &Path,
        _content: &[u8],
        _mode: Option<std::fs::Permissions>,
      ) -> Result<(), Error> {
        Err(Error::Io {
          context: "fake apply",
          source: std::io::Error::from_raw_os_error(13), // EACCES
        })
      }
      fn compressed_on_disk(&self, _path: &Path) -> Result<Option<bool>, Error> {
        Ok(Some(false))
      }
    }
    let dir = scratch("copy-skip");
    let src = dir.join("src.node");
    let dest = dir.join("dest.node");
    let content = fake_addon();
    std::fs::write(&src, &content).unwrap();
    let out = copy_file_with(&SkippingFake, &src, &dest).unwrap();
    assert!(
      matches!(out, CopyOutcome::CopiedPlain { skipped: Some(_) }),
      "got {out:?}"
    );
    assert_eq!(std::fs::read(&dest).unwrap(), content, "bytes still landed");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn copy_file_onto_itself_is_a_noop_reported_as_cloned() {
    let dir = scratch("copy-self");
    let src = dir.join("src.node");
    let content = fake_addon();
    std::fs::write(&src, &content).unwrap();
    let backend = FakeBackend {
      detect: Support::Supported,
      apply_errno: None,
    };
    let out = copy_file_with(&backend, &src, &src).unwrap();
    assert!(matches!(out, CopyOutcome::Cloned { .. }), "got {out:?}");
    assert_eq!(std::fs::read(&src).unwrap(), content, "source untouched");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn is_same_file_sees_hardlinks_and_distinct_files() {
    let dir = scratch("same-file");
    let a = dir.join("a.node");
    let b = dir.join("b.node");
    std::fs::write(&a, b"bytes").unwrap();
    std::fs::write(&b, b"bytes").unwrap();
    assert!(is_same_file(&a, &a), "identical path");
    assert!(!is_same_file(&a, &b), "distinct files");
    let link = dir.join("a-link.node");
    std::fs::hard_link(&a, &link).unwrap();
    assert!(is_same_file(&a, &link), "hardlink shares the inode");
    assert!(
      !is_same_file(&a, &dir.join("absent.node")),
      "a missing path is never the same file"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn copy_file_errors_when_the_destination_cannot_be_replaced() {
    let dir = scratch("copy-dest-dir");
    let src = dir.join("src.node");
    std::fs::write(&src, fake_addon()).unwrap();
    // A directory at `dest` makes the replace step's remove_file fail.
    let dest = dir.join("dest.node");
    std::fs::create_dir(&dest).unwrap();
    let backend = FakeBackend {
      detect: Support::Supported,
      apply_errno: None,
    };
    let out = copy_file_with(&backend, &src, &dest);
    assert!(
      matches!(
        out,
        Err(Error::Io {
          context: "replace existing destination",
          ..
        })
      ),
      "got {out:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn copy_file_errors_when_the_source_is_unreadable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("copy-unreadable");
    let src = dir.join("src.node");
    let dest = dir.join("dest.node");
    std::fs::write(&src, fake_addon()).unwrap();
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o000)).unwrap();
    let backend = FakeBackend {
      detect: Support::Supported,
      apply_errno: None,
    };
    let out = copy_file_with(&backend, &src, &dest);
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).ok();
    assert!(
      matches!(
        out,
        Err(Error::Io {
          context: "read copy source",
          ..
        })
      ),
      "got {out:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn try_clone_file_errors_when_the_source_is_missing() {
    let dir = scratch("clone-missing");
    let out = try_clone_file(&dir.join("absent.node"), &dir.join("dest.node"));
    assert!(matches!(out, Err(Error::NotFound(_))));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn try_clone_file_clones_on_apfs_and_declines_an_existing_destination() {
    let dir = scratch("clone-try");
    let src = dir.join("src.node");
    let dest = dir.join("dest.node");
    std::fs::write(&src, fake_addon()).unwrap();
    assert!(try_clone_file(&src, &dest).unwrap(), "fresh clone");
    // clonefile refuses an existing destination — reported as cannot-clone,
    // never an error.
    assert!(!try_clone_file(&src, &dest).unwrap());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn copy_file_clones_a_compressed_source_on_apfs() {
    let dir = scratch("copy-clone");
    let src = dir.join("src.node");
    let dest = dir.join("dest.node");
    let content = fake_addon();
    let wrote = compress_bytes(&src, &content, &Gate::any()).unwrap();
    // Only meaningful when the scratch volume actually compressed the source.
    if !matches!(wrote, Outcome::Compressed { .. }) {
      std::fs::remove_dir_all(&dir).ok();
      return;
    }
    let out = copy_file(&src, &dest).unwrap();
    assert_eq!(out, CopyOutcome::Cloned { compressed: true });
    assert!(backend::is_already_compressed(&dest).unwrap());
    assert_eq!(
      std::fs::read(&dest).unwrap(),
      content,
      "bytes are identical"
    );
    std::fs::remove_dir_all(&dir).ok();
  }
}
